"""The product service: webhook receiver with fulfillment, and the product's account API.

It pins its account's webhook keys from attestation, keeps its ledger in SQLite, and serves its
own account API, through which a user registers a workspace, gets a quote-first single-use
address, and reads its deposits, credits, and webhook events. It holds the product key and calls
the service on the user's behalf, as Phala Cloud's backend does.
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import secrets
import threading
import time
from collections.abc import AsyncIterator, Iterator, Mapping
from concurrent.futures import ThreadPoolExecutor
from contextlib import asynccontextmanager, contextmanager
from functools import partial
from http import HTTPStatus
from itertools import islice
from typing import Any
from urllib.parse import parse_qs, urlsplit

import httpx
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
from granian import Granian
from granian.constants import HTTPModes, Interfaces
from granian.http import HTTP1Settings
from granian.server.embed import Server as EmbeddedServer
from starlette.applications import Starlette
from starlette.requests import ClientDisconnect, Request
from starlette.responses import JSONResponse
from starlette.responses import Response as HttpResponse
from starlette.routing import Route

from topup_client.models import AttestationResponse, Quote
from topup_sdk import (
    ApiError,
    SignatureError,
    TopupClient,
    load_public_key,
    load_webhook_public_key,
    verify_attestation_binding,
    verify_request,
)
from topup_sdk.addresses import forwarder_address, quote_salt
from topup_sdk.ids import DEPOSIT, object_id, parse_id

from .config import (
    DRIVER_KEYID,
    EVM_ADDRESS,
    MissingProductKeyError,
    ProductConfig,
)
from .demo import DemoConsole
from .fulfillment import Answer, Fulfillment, PinnedKeys, TransientError, parse_decimal
from .ledger import ProductLedger
from .restore_records import export_restore_records
from .transport import OPERATION_TIMEOUT_SECONDS, operation_deadline

LOG = logging.getLogger(__name__)

MAX_BODY_BYTES = 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 30
DEMO_TIMEOUT_SECONDS = 8
HEADER_TIMEOUT_MILLISECONDS = 5000
CONNECTION_LIMIT = 32
LISTEN_BACKLOG = 128
SHUTDOWN_TIMEOUT_SECONDS = 35
# Workspace ids and lock references in the account API: URL path segments without escaping.
ACCOUNT_REF = re.compile(r"[A-Za-z0-9._-]{1,64}")
# The account API's path of the restore records, reserved among workspace ids.
RESTORE_RECORDS = "restore-records"


class ProductServer:
    """Serves `POST /webhooks`, `GET /healthz`, and, given an `AccountApi`, `/accounts`, and,
    given a `DemoConsole`, the demo's API for the website: `/api/`. It serves no web page."""

    def __init__(
        self,
        fulfillment: Fulfillment,
        accounts: AccountApi | None = None,
        demo: DemoConsole | None = None,
    ) -> None:
        self.fulfillment = fulfillment
        self.accounts = accounts
        self.demo = demo
        config = fulfillment.config
        base_path = urlsplit(config.public_url).path.rstrip("/")
        self._workers = ThreadPoolExecutor(max_workers=16, thread_name_prefix="product")
        # Keep timed-out synchronous work admitted until it actually finishes. A client timeout
        # cannot stop a Python thread or undo a mutation, and must not admit unlimited new work.
        self._capacity = threading.BoundedSemaphore(16)
        self._webhook_workers = ThreadPoolExecutor(max_workers=4, thread_name_prefix="webhook")
        self._webhook_capacity = threading.BoundedSemaphore(4)

        def dispatch(
            method: str, target: str, headers: dict[str, str], body: bytes
        ) -> HttpResponse:
            timeout = (
                DEMO_TIMEOUT_SECONDS
                if method == "GET" and self.demo is not None and self.demo.handles(target)
                else OPERATION_TIMEOUT_SECONDS
            )
            operation_deadline.set(time.monotonic() + timeout)
            if method == "POST" and urlsplit(target).path == base_path + "/webhooks":
                answer = self.fulfillment.handle(headers, body)
            elif method == "GET" and urlsplit(target).path == base_path + "/healthz":
                answer = Answer(HTTPStatus.OK, {"status": "ok"})
            elif (
                self.accounts is not None and self.accounts.handles(target) and method != "OPTIONS"
            ):
                answer = self.accounts.handle(method, target, headers, body)
            elif self.demo is not None and self.demo.handles(target):
                response = self.demo.handle(method, target, headers, body)
                return HttpResponse(response.body, response.status, headers=response.headers)
            else:
                answer = Answer(HTTPStatus.NOT_FOUND)
            if answer.body is None:
                return HttpResponse(status_code=answer.status)
            return JSONResponse(answer.body, status_code=answer.status)

        def error(request: Request, status: HTTPStatus) -> HttpResponse:
            headers = self.demo.cors(request.headers.get("origin")) if self.demo is not None else {}
            return JSONResponse({"code": status.name.lower()}, status_code=status, headers=headers)

        async def endpoint(request: Request) -> HttpResponse:
            try:
                async with asyncio.timeout(REQUEST_TIMEOUT_SECONDS):
                    # The HTTP/1 parser rejects malformed wire framing. Also validate ASGI requests,
                    # and bound streamed/chunked bodies regardless of Content-Length.
                    lengths = request.headers.getlist("content-length")
                    if lengths and (
                        len(lengths) != 1
                        or len(lengths[0]) > 20
                        or not re.fullmatch(r"[0-9]+", lengths[0])
                    ):
                        return error(request, HTTPStatus.BAD_REQUEST)
                    if lengths and int(lengths[0]) > MAX_BODY_BYTES:
                        return error(request, HTTPStatus.REQUEST_ENTITY_TOO_LARGE)
                    body = bytearray()
                    async for chunk in request.stream():
                        if len(body) + len(chunk) > MAX_BODY_BYTES:
                            return error(request, HTTPStatus.REQUEST_ENTITY_TOO_LARGE)
                        body.extend(chunk)
                    if lengths and len(body) != int(lengths[0]):
                        return error(request, HTTPStatus.BAD_REQUEST)
                    target = request.scope["raw_path"].decode("ascii")
                    if request.url.query:
                        target += "?" + request.url.query
                    webhook = (
                        request.method == "POST" and request.url.path == base_path + "/webhooks"
                    )
                    capacity = self._webhook_capacity if webhook else self._capacity
                    workers = self._webhook_workers if webhook else self._workers
                    if not capacity.acquire(blocking=False):
                        return error(request, HTTPStatus.SERVICE_UNAVAILABLE)
                    try:
                        future = workers.submit(
                            dispatch,
                            request.method,
                            target,
                            dict(request.headers),
                            bytes(body),
                        )
                    except RuntimeError:
                        capacity.release()
                        raise
                    future.add_done_callback(lambda _: capacity.release())
                    return await asyncio.wrap_future(future)
            except TimeoutError:
                return error(request, HTTPStatus.REQUEST_TIMEOUT)
            except ClientDisconnect:
                return error(request, HTTPStatus.BAD_REQUEST)
            except Exception:
                LOG.exception("product request failed")
                return error(request, HTTPStatus.INTERNAL_SERVER_ERROR)

        self._ready = threading.Event()
        self._loop: asyncio.AbstractEventLoop | None = None

        @asynccontextmanager
        async def lifespan(_: Starlette) -> AsyncIterator[None]:
            self._ready.set()
            try:
                yield
            finally:
                # Drain actual work, not merely cancelled asyncio wrappers. The production
                # supervisor kills the worker at 35s if noncooperative sync code cannot drain.
                await asyncio.to_thread(self.close)

        self.app = Starlette(
            routes=[Route("/{path:path}", endpoint, methods=["GET", "POST", "OPTIONS"])],
            lifespan=lifespan,
        )
        self._server = EmbeddedServer(
            self.app,
            address=config.listen_host,
            port=config.listen_port,
            **_server_options(),
        )
        self._thread = threading.Thread(target=self._run, name="product-server")

    def _run(self) -> None:
        async def run() -> None:
            self._loop = asyncio.get_running_loop()
            await self._server.serve()

        asyncio.run(run())

    def __enter__(self) -> ProductServer:
        self._thread.start()
        deadline = time.monotonic() + 10
        while not self._ready.is_set():
            if not self._thread.is_alive() or time.monotonic() >= deadline:
                self.__exit__()
                raise RuntimeError("product server did not start")
            time.sleep(0.01)
        return self

    def close(self) -> None:
        self._workers.shutdown(wait=True, cancel_futures=True)
        self._webhook_workers.shutdown(wait=True, cancel_futures=True)
        if self.accounts is not None:
            self.accounts.close()
        if self.demo is not None:
            self.demo.close()

    def __exit__(self, *_: object) -> None:
        if self._loop is not None and self._thread.is_alive():
            self._loop.call_soon_threadsafe(self._server.stop)
        self._thread.join()


def _server_options() -> dict[str, Any]:
    return {
        "interface": Interfaces.ASGI,
        "http": HTTPModes.http1,
        "websockets": False,
        # Granian v2.8.4 acquires a permit before accept and holds it for the connection.
        "backpressure": CONNECTION_LIMIT,
        # Granian clamps backlog to at least 128; the OS may clamp it further.
        "backlog": LISTEN_BACKLOG,
        "http1_settings": HTTP1Settings(
            header_read_timeout=HEADER_TIMEOUT_MILLISECONDS,
            max_buffer_size=16 * 1024,
            keep_alive=False,
        ),
        "log_access": False,
    }


class AccountApi:
    """The product's account API; the deposit driver uses it as a signed-in user would.

    - `POST /accounts` `{"account_id"}` registers a workspace (`register_team`);
    - `POST /accounts/{id}/quotes` `{"amount_minor", "chain_id"?, "asset"?}` creates a quote
      (`create_quote`) on a configured chain (the first by default) in an asset (that chain's
      first test token by default) and returns the service's quote;
    - `POST /accounts/{id}/deposits/{deposit_id}/refunds` `{"destination_address",
      "amount_atomic"}` requests a refund of one of the workspace's deposits (`create_refund`);
    - `GET /accounts/{id}` returns the workspace's deposits (from the service), its credits
      (from the ledger), and the verified webhook events for those deposits and its quotes;
    - `GET /accounts/restore-records?since=` returns the records a service restore asks the
      merchant for (reference_product.restore_records), client secrets included, for the
      operator's `fetch-restore-records`; `restore-records` is not a workspace id.

    The product calls the service with its own key on the user's behalf, as Phala Cloud's
    backend does. Requests must carry an RFC 9421 signature by the pinned driver key
    (`driver_public_key`, key id `driver/v1`), which stands in for user sessions and cannot sign
    service requests. A signature is fresh for five minutes, so a captured request can be replayed
    within them: every `POST` must carry an `Idempotency-Key` the signature covers, and quote and
    refund creation pass it to the service, whose replay of the first response makes a replayed
    request create nothing new. Registration and reads are idempotent in themselves.
    """

    def __init__(self, config: ProductConfig, ledger: ProductLedger, driver_key: Ed25519PublicKey):
        self.config = config
        self.ledger = ledger
        self.driver_key = driver_key
        self.accounts_path = urlsplit(config.public_url).path.rstrip("/") + "/accounts"
        self._client: TopupClient | None = None
        self._client_lock = threading.Lock()

    def handles(self, target: str) -> bool:
        path = urlsplit(target).path
        return path == self.accounts_path or path.startswith(self.accounts_path + "/")

    def handle(self, method: str, target: str, headers: Mapping[str, str], body: bytes) -> Answer:
        public = urlsplit(self.config.public_url)
        try:
            verified = verify_request(
                method=method,
                target_uri=f"{public.scheme}://{public.netloc}{target}",
                headers=headers,
                body=body,
                public_key=self.driver_key,
                keyid=DRIVER_KEYID,
                require_idempotency_key=method == "POST",
            )
        except SignatureError:
            return Answer(HTTPStatus.UNAUTHORIZED)
        parts = urlsplit(target).path.removeprefix(self.accounts_path).split("/")[1:]
        try:
            if method == "POST" and not parts:
                team = _account_ref(_json_object(body).get("account_id"))
                if team == RESTORE_RECORDS:
                    raise ValueError(f"{RESTORE_RECORDS} is not a workspace id")
                register_team(self.ledger, team)
                return Answer(HTTPStatus.OK, {"account_id": team})
            if len(parts) == 2 and parts[1] == "quotes" and method == "POST":
                team = _account_ref(parts[0])
                request = _json_object(body)
                amount_minor = request.get("amount_minor")
                if type(amount_minor) is not int or amount_minor <= 0:
                    raise ValueError("amount_minor must be a positive integer")
                chain_id, asset = request.get("chain_id"), request.get("asset")
                if chain_id is not None and type(chain_id) is not int:
                    raise ValueError("chain_id must be an integer")
                if asset is not None and not isinstance(asset, str):
                    raise ValueError("asset must be a string")
                chain = self.config.chain(chain_id)
                if self.ledger.team_suspended(team) is None:
                    return Answer(HTTPStatus.NOT_FOUND)
                quote = create_quote(
                    self.config,
                    self._service(),
                    self.ledger,
                    team,
                    amount_minor=amount_minor,
                    chain_id=chain.chain_id,
                    asset=asset or chain.test_token.symbol.lower(),
                    idempotency_key=verified.idempotency_key,
                )
                return Answer(HTTPStatus.OK, quote.to_dict())
            if (
                len(parts) == 4
                and parts[1] == "deposits"
                and parts[3] == "refunds"
                and method == "POST"
            ):
                team = _account_ref(parts[0])
                deposit = object_id(DEPOSIT, parse_id(DEPOSIT, parts[2]))
                request = _json_object(body)
                destination = request.get("destination_address")
                amount = parse_decimal(request.get("amount_atomic"))
                if not isinstance(destination, str) or not EVM_ADDRESS.fullmatch(destination):
                    raise ValueError("destination_address must be a 0x-prefixed 20-byte address")
                if amount is None or amount <= 0:
                    raise ValueError("amount_atomic must be a positive decimal string")
                if self.ledger.team_suspended(team) is None or not any(
                    item.id == deposit
                    for item in self._service().list_deposits(client_reference_id=team)
                ):
                    return Answer(HTTPStatus.NOT_FOUND)
                refund = self._service().create_refund(
                    deposit, destination, amount, idempotency_key=verified.idempotency_key
                )
                return Answer(HTTPStatus.OK, refund.to_dict())
            if parts == [RESTORE_RECORDS] and method == "GET":
                since = _since(urlsplit(target).query)
                # The records hold client secrets: logged as exported, never their content.
                LOG.warning("restore records exported through the account API (since %s)", since)
                records = export_restore_records(self.config.account, self.ledger, since=since)
                return Answer(HTTPStatus.OK, records)
            if len(parts) == 1 and method == "GET":
                team = _account_ref(parts[0])
                if self.ledger.team_suspended(team) is None:
                    return Answer(HTTPStatus.NOT_FOUND)
                return Answer(HTTPStatus.OK, self._account_view(team))
        except ValueError:
            return Answer(HTTPStatus.BAD_REQUEST)
        except MissingProductKeyError:
            LOG.warning("account API unavailable: the product key is not configured")
            return Answer(HTTPStatus.SERVICE_UNAVAILABLE)
        except ApiError as error:
            # The service's documented error code is public; nothing else is passed on.
            LOG.warning("service answered %s %s", error.status_code, error.code)
            return Answer(
                HTTPStatus.BAD_GATEWAY,
                {"service_status": error.status_code, "service_code": error.code},
            )
        except httpx.HTTPError:
            LOG.warning("service unavailable for the account API")
            return Answer(HTTPStatus.SERVICE_UNAVAILABLE)
        except RuntimeError:
            LOG.exception("account API request failed")
            return Answer(HTTPStatus.INTERNAL_SERVER_ERROR)
        return Answer(HTTPStatus.NOT_FOUND)

    def _account_view(self, team: str) -> dict[str, Any]:
        deposits = list(
            islice(self._service().list_deposits(client_reference_id=team, page_size=100), 100)
        )
        ids = {deposit.id for deposit in deposits}
        return {
            "account_id": team,
            "deposits": [deposit.to_dict() for deposit in deposits],
            "credits": [
                {"provider_order_id": key, "amount_minor": amount}
                for key, amount in self.ledger.credits_for(team)
            ],
            "orders": self.ledger.orders_for(team),
            "balance_minor": self.ledger.balance_for(team),
            "adjustments": [
                {"provider_order_id": key, "amount_minor": amount, "reason": reason}
                for key, amount, reason in self.ledger.adjustments_for(team)
            ],
            "bonuses": [
                {"provider_order_id": key, "amount_minor": amount, "reason": reason}
                for key, amount, reason in self.ledger.bonuses_for(team)
            ],
            "events": [
                event
                for event in self.ledger.events_for(ids | {team})
                if (event["data"].get("object") or {}).get("id") in ids
                or (event["data"].get("object") or {}).get("client_reference_id") == team
            ],
        }

    def _service(self) -> TopupClient:
        with self._client_lock:
            if self._client is None:
                self._client = self.config.client()
            return self._client

    def close(self) -> None:
        with self._client_lock:
            if self._client is not None:
                self._client.close()


def _account_ref(value: object) -> str:
    if not isinstance(value, str) or not ACCOUNT_REF.fullmatch(value):
        raise ValueError("expected 1-64 letters, digits, '.', '_', or '-'")
    return value


def _since(query: str) -> int | None:
    """The restore records query's `since`, Unix seconds; `None` when absent."""
    values = parse_qs(query, keep_blank_values=True, strict_parsing=bool(query))
    if set(values) - {"since"}:
        raise ValueError("the only parameter is since")
    if "since" not in values:
        return None
    [since] = values["since"]
    if not (since.isascii() and since.isdigit()):
        raise ValueError("since must be Unix seconds")
    return int(since)


def _json_object(body: bytes) -> dict[str, Any]:
    value = json.loads(body)
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def pin_webhook_keys(config: ProductConfig, *, wait_s: float = 0) -> PinnedKeys:
    """Returns the account's webhook keys in the API key's mode, from attestation evidence bound
    to a fresh nonce and fetched with the product's API key.

    `verify_attestation_binding` checks that the report data binds the nonce, the account, the
    mode, and the keys. Production integrators must also verify the TDX quote with the dstack
    verifier and then pin the keys in configuration. Test mode may fetch keys after checking the
    report-data binding. While the service is unreachable this retries for up to `wait_s` seconds;
    without the API key it raises `MissingProductKeyError`.
    """
    livemode = config.livemode()
    if config.webhook_public_keys:
        return PinnedKeys(
            livemode, [load_webhook_public_key(key) for key in config.webhook_public_keys]
        )
    if livemode:
        raise MissingProductKeyError(
            "live mode requires pre-verified webhook_public_keys; refusing unpinned attestation"
        )
    deadline = time.monotonic() + wait_s
    while True:
        nonce = secrets.token_bytes(32)
        try:
            response = httpx.get(
                config.service_url.rstrip("/") + "/v1/attestation",
                params={"nonce": nonce.hex()},
                headers={"Authorization": f"Bearer {config.api_key()}"},
                timeout=30,
            )
            response.raise_for_status()
            evidence = AttestationResponse.from_dict(response.json())
            break
        except (httpx.HTTPError, ValueError, KeyError, TypeError) as error:
            if time.monotonic() >= deadline:
                raise TransientError("the service's attestation is unavailable") from error
            LOG.warning(
                "waiting for %s/v1/attestation: %s", config.service_url, type(error).__name__
            )
            time.sleep(5)
    keys = verify_attestation_binding(
        evidence, nonce, expected_account=config.account, expected_livemode=livemode
    )
    LOG.warning("pinned webhook keys from attestation; verify the quote before production")
    return PinnedKeys(livemode, keys)


class WebhookKeys:
    """Pins the account's webhook keys once, on first use, so the product starts before its API
    key is configured (a CVM before its secrets are sealed)."""

    def __init__(self, config: ProductConfig) -> None:
        self._config = config
        self._lock = threading.Lock()
        self._pinned: PinnedKeys | None = None

    def __call__(self, *, wait_s: float = 0) -> PinnedKeys:
        with self._lock:
            if self._pinned is None:
                self._pinned = pin_webhook_keys(self._config, wait_s=wait_s)
            return self._pinned


def register_team(ledger: ProductLedger, team: str, *, suspended: bool = False) -> None:
    """Registers a workspace; the service creates its account with the first quote."""
    ledger.add_team(team, suspended=suspended)


def create_quote(
    config: ProductConfig,
    client: TopupClient,
    ledger: ProductLedger,
    team: str,
    *,
    amount_minor: int,
    chain_id: int | None = None,
    asset: str | None = None,
    idempotency_key: str | None = None,
) -> Quote:
    """Creates a quote for the workspace on a configured chain (the first by default), in `asset`
    (the chain's first test token by default), and records it as the service returned it, its
    `client_secret` included, for a service restore; a repeat with the same `idempotency_key`
    returns the first quote.

    The client recomputes the address from the pinned forwarder, the chain's treasury, and the
    quote id, and raises before returning an address the product did not derive.
    """
    chain = config.chain(chain_id)
    quote = client.create_quote(
        team,
        amount_minor,
        chain_id=chain.chain_id,
        asset=asset or chain.test_token.symbol.lower(),
        idempotency_key=idempotency_key,
    )
    ledger.record_quote(team, quote.to_dict())
    return quote


def quote_address(config: ProductConfig, team: str, quote_id: str, chain_id: int) -> str:
    return forwarder_address(
        config.factory,
        config.implementation,
        config.chain(chain_id).treasury,
        quote_salt(config.account, team, quote_id),
    )


def _make_product(config: ProductConfig, *, pin_wait_s: float = 0) -> ProductServer:
    """Runs the product: webhook receiver with fulfillment, account API, and the demo's API."""
    if config.driver_public_key is None:
        raise ValueError("driver_public_key is required to serve the account API")
    webhook_keys = WebhookKeys(config)
    try:
        webhook_keys(wait_s=pin_wait_s)
    except MissingProductKeyError:
        # A CVM starts before its secret is sealed.  An actually configured live key still
        # requires a pre-verified pin; an absent key has no mode to enforce yet and is allowed
        # to start so the provisioning health check can run.
        try:
            key = config.api_key()
        except MissingProductKeyError:
            key = ""
        if key.startswith(("ppay_sk_live_", "ppay_rk_live_")):
            raise
        LOG.warning("the product key is not configured; webhook keys are pinned once it is")
    ledger = ProductLedger(config.ledger_path)
    fulfillment = Fulfillment(config, ledger, webhook_keys)
    accounts = AccountApi(config, ledger, load_public_key(config.driver_public_key))
    demo = None if config.web_origin is None else DemoConsole(config, ledger)
    return ProductServer(fulfillment, accounts, demo)


@contextmanager
def product_service(config: ProductConfig, *, pin_wait_s: float = 0) -> Iterator[ProductServer]:
    """Runs the product in process for the sandbox and SDK examples."""
    with _make_product(config, pin_wait_s=pin_wait_s) as server:
        yield server


def _load_application(config: ProductConfig) -> Starlette:
    # Construct SQLite connections and in-memory rate limits inside the single worker.
    return _make_product(config, pin_wait_s=600).app


def serve(config: ProductConfig) -> None:
    """Serve under Granian's standard signal handling and bounded worker shutdown."""
    Granian(
        "reference_product.server",
        address=config.listen_host,
        port=config.listen_port,
        workers=1,
        workers_kill_timeout=SHUTDOWN_TIMEOUT_SECONDS,
        **_server_options(),
    ).serve(target_loader=partial(_load_application, config), wrap_loader=False)
