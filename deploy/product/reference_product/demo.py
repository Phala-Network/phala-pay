"""The Phala Pay demo's API: a cloud console's "Billing → Add credits" page, on staging.

The website, deploy/product/web, is served elsewhere (pay.phala.com, on Cloudflare) and calls this
JSON API at `{public_url}/api/` from its origin, `web_origin`, the only origin the API allows
(CORS, with the visitor's cookie). The demo shows both ways to collect a payment, as the product's
backend runs them with its API key:

- `GET api/account`: the visitor's demo account (a random id in a cookie; no other data is kept),
  its balance from this product's ledger, the ledger lines behind it (credits, claw-backs, and the
  product's own bonus lines), and its payments;
- `GET api/assets`: the networks a customer can pay on, each with its explorer, gas faucet, and
  treasury, and its tokens (the service's `GET /v1/config` `assets` on the chains the product has
  pins for, so a chain appears once the service serves it there), each with its faucet or
  whether its test token mints, and the product's bonus rate for it;
- `POST api/quotes` `{"amount", "chain_id", "asset"}` (cents, and a network and token of
  `api/assets`): creates a locked-price quote with the SDK, with an order id in its `metadata`,
  and returns its `client_secret`, the `expected_address` the SDK recomputed from the pins, for
  `<Checkout>`, and its locked `exchange_rate` and `expires_at`;
- `POST api/deposit_address`: the visitor's single deposit address, for every token on every
  network (`POST /v1/deposit_addresses`), recomputed by the SDK from the pins, with a fresh
  `client_secret` for `<DepositAddress>`; `GET api/deposit_address` reads it and its payments;
- `GET api/quotes/{id}`, `GET api/deposits/{id}`: a payment's timeline, built only from real
  data: the service's quote, deposit, refunds, and sweeps (read with the product's API key), the
  chain's block times, and this product's verified webhook events and ledger rows; with the
  service requests behind it;
- `POST api/refunds` `{"deposit", "amount_atomic", "destination_address"}`,
  `POST api/refunds/{id}/mark_paid` `{"transaction_hash", "receipt_log_index"?}`, and
  `POST api/refunds/{id}/cancel`: the refund flow, for the visitor's own deposits; the visitor
  plays the merchant's finance team, which pays refunds from the treasury;
- `GET api/sweeps`: per network and token, the account's unswept balance, the `factory.flush`
  call and Safe Transaction Builder batch the SDK builds for the merchant to sign, and the
  finalized sweeps;
- `GET api/trust`: the service's attestation, with the report-data binding checked by the SDK, and
  the app id and compose hash of its TLS evidence.

The browser never holds a key: the product sends every service request itself, and the developer
view shows those requests with the API key redacted to its prefix. Balances move only through the
`deposit.*` webhooks (reference_product.fulfillment), exactly as for any other account. The
product holds no wallet key: it never sweeps or pays a refund.
"""

from __future__ import annotations

import json
import logging
import re
import secrets
import threading
import time
import uuid
from collections import deque
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from http import HTTPStatus
from http.cookies import CookieError, SimpleCookie
from itertools import islice
from typing import Any
from urllib.parse import urlsplit

import httpx

from topup_client.models import AttestationResponse, DepositAddress, Payment
from topup_sdk import (
    AddressMismatchError,
    ApiError,
    AttestationError,
    TopupClient,
    TopupError,
    flush_transactions,
    forwarder_address,
    safe_batch,
)
from topup_sdk.addresses import same_address
from topup_sdk.errors import ResponseValidationError, TransportError

from .config import EVM_ADDRESS, MissingProductKeyError, ProductConfig
from .ledger import ORDER_FLOW_CODE, DepositView, ProductLedger
from .transport import DeadlineTransport, operation_deadline

LOG = logging.getLogger(__name__)

ACCOUNT_COOKIE = "demo_account"
ACCOUNT_ID = re.compile(r"demo-[0-9a-f]{24}")
QUOTE_ID = re.compile(r"qt_[0-9a-f]{32}")
DEPOSIT_ID = re.compile(r"dep_[0-9a-f]{32}")
REFUND_ID = re.compile(r"re_[0-9a-f]{32}")
TX_HASH = re.compile(r"0x[0-9a-fA-F]{64}")
ATOMIC = re.compile(r"[1-9][0-9]{0,77}")
PRESETS = [500, 2000, 5000]
MIN_AMOUNT = 100
MAX_AMOUNT = 100_000
# The forwarders one flush call may name (topup_sdk.sweeps.MAX_SALTS_PER_FLUSH).
MAX_SWEEP_FORWARDERS = 200
EXPLORERS = {
    1: "https://etherscan.io",
    8453: "https://basescan.org",
    11155111: "https://sepolia.etherscan.io",
    84532: "https://sepolia.basescan.org",
}
MAINNETS = {1, 8453}
# Where the visitor's wallet gets gas: ethereum.org's list of Sepolia faucets, and Base's page
# pointing to its Base Sepolia faucets.
GAS_FAUCETS = {
    11155111: "https://ethereum.org/en/developers/docs/networks/#sepolia",
    84532: "https://docs.base.org/get-started/get-funds#testnet-base-sepolia",
}
# Testnet tokens the visitor cannot mint, by asset: Circle's faucet serves its test USDC on every
# testnet it supports (the visitor picks the network there).
TOKEN_FAUCETS = {"usdc": "https://faucet.circle.com"}
VERIFY_DOCS = (
    "https://github.com/Phala-Network/phala-pay/blob/main/deploy/README.md"
    "#attestation-ingress-and-egress"
)
# A refund's `failure_reason`, explained to the visitor.
REFUND_FAILURES = {
    "sender_mismatch": "The transfer was not sent from the treasury the deposit's address pays.",
    "destination_mismatch": "The transfer did not pay the refund's destination address.",
    "amount_mismatch": "The transfer's amount is not the refund's amount.",
    "transfer_not_found": "The transaction has no transfer of the deposit's token.",
    "transaction_failed": "The transaction reverted.",
    "transfer_already_used": "Another refund already used that transfer.",
    "transaction_dropped": "The transaction left the chain and its sender's nonce was reused.",
    "transaction_not_found": "No provider returned the transaction within 24 hours.",
}


@dataclass(frozen=True)
class Response:
    status: HTTPStatus
    body: bytes = b""
    headers: dict[str, str] = field(default_factory=dict)


class RateLimiter:
    """At most `limit` events per key in any `window` seconds."""

    def __init__(self, limit: int, window: float, clock: Callable[[], float]) -> None:
        self.limit = limit
        self.window = window
        self._clock = clock
        self._hits: dict[str, deque[float]] = {}
        self._lock = threading.Lock()

    def allow(self, key: str) -> bool:
        now = self._clock()
        with self._lock:
            hits = self._hits.setdefault(key, deque())
            while hits and hits[0] <= now - self.window:
                hits.popleft()
            if len(hits) >= self.limit:
                return False
            hits.append(now)
            if len(self._hits) > 10_000:
                self._hits = {
                    k: v for k, v in self._hits.items() if v and v[-1] > now - self.window
                }
            return True


class ApiRecorder(httpx.BaseTransport):
    """The product's transport to the service; records the exchanges of the current request for
    the developer view, with the API key redacted to its prefix and client secrets masked."""

    def __init__(self, inner: httpx.BaseTransport | None = None) -> None:
        self._inner = inner or DeadlineTransport()
        self._local = threading.local()

    @contextmanager
    def capture(self) -> Iterator[list[dict[str, Any]]]:
        calls: list[dict[str, Any]] = []
        self._local.calls = calls
        try:
            yield calls
        finally:
            self._local.calls = None

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        response = self._inner.handle_request(request)
        calls: list[dict[str, Any]] | None = getattr(self._local, "calls", None)
        if calls is not None:
            response.read()
            calls.append(_exchange(request, response))
        return response

    def close(self) -> None:
        self._inner.close()


class DemoConsole:
    """Serves the demo's API to the website; see the module docstring."""

    def __init__(
        self,
        config: ProductConfig,
        ledger: ProductLedger,
        *,
        recorder: ApiRecorder | None = None,
        http: httpx.Client | None = None,
        clock: Callable[[], float] = time.time,
    ) -> None:
        self.config = config
        self.ledger = ledger
        self.root = urlsplit(config.public_url).path.rstrip("/")
        self.api = self.root + "/api/"
        if config.web_origin is None:
            raise ValueError("the demo's API needs web_origin, the website's origin")
        self.web_origin = config.web_origin
        self.secure_cookie = urlsplit(config.public_url).scheme == "https"
        self.recorder = recorder or ApiRecorder()
        self._http = http or httpx.Client(
            timeout=5, follow_redirects=False, transport=DeadlineTransport()
        )
        self._clock = clock
        self._client: TopupClient | None = None
        self._sweeps_client: TopupClient | None = None
        self._lock = threading.Lock()
        self._cache_changed = threading.Condition(self._lock)
        self._new_accounts = RateLimiter(30, 60, clock)
        self._quotes_per_account = RateLimiter(3, 60, clock)
        self._quotes_per_day = RateLimiter(20, 86_400, clock)
        self._quotes = RateLimiter(30, 60, clock)
        self._writes_per_account = RateLimiter(10, 60, clock)
        self._writes = RateLimiter(60, 60, clock)
        self._reads = RateLimiter(120, 60, clock)
        self._trust: tuple[float, dict[str, Any]] | None = None
        self._networks: tuple[float, list[dict[str, Any]]] | None = None
        self._trust_refreshing = False
        self._networks_refreshing = False
        self._trust_expires = 0.0
        self._networks_expires = 0.0
        self._trust_error: TopupError | httpx.HTTPError | MissingProductKeyError | None = None
        self._networks_error: TopupError | httpx.HTTPError | MissingProductKeyError | None = None
        self._sweeps: tuple[float, dict[str, Any]] | None = None
        self._sweeps_refreshing = False
        self._block_times: dict[tuple[int, str], tuple[int, int]] = {}
        self._chain_ids = set(config.treasuries())

    # Routing ------------------------------------------------------------------------------------

    def handles(self, target: str) -> bool:
        return urlsplit(target).path.startswith(self.api)

    def handle(self, method: str, target: str, headers: dict[str, str], body: bytes) -> Response:
        path = urlsplit(target).path
        if not path.startswith(self.api):
            return Response(HTTPStatus.NOT_FOUND)
        lowered = {key.lower(): value for key, value in headers.items()}
        origin = lowered.get("origin")
        if method == "OPTIONS":
            response = Response(HTTPStatus.NO_CONTENT)
            if origin == self.web_origin:
                # The page's requests: GETs, and POSTs with a JSON body.
                response.headers["access-control-allow-methods"] = "GET, POST"
                response.headers["access-control-allow-headers"] = "content-type"
                response.headers["access-control-max-age"] = "600"
        else:
            response = self._handle_api(method, path.removeprefix(self.api), lowered, body)
        response.headers.update(self.cors(origin))
        return response

    def cors(self, origin: str | None) -> dict[str, str]:
        """The CORS headers of every API response, errors included: credentialed requests from
        the website's origin only; other origins get none."""
        if origin != self.web_origin:
            return {"vary": "Origin"}
        return {
            "access-control-allow-origin": self.web_origin,
            "access-control-allow-credentials": "true",
            "vary": "Origin",
        }

    def _handle_api(self, method: str, name: str, headers: dict[str, str], body: bytes) -> Response:
        try:
            return self._api(method, name, headers, body)
        except ApiError as error:
            # The service's documented code is public; its message and everything else are not.
            LOG.warning("demo: service answered %s %s", error.status_code, error.code)
            if error.status_code == 429:
                status = HTTPStatus.TOO_MANY_REQUESTS
            elif 400 <= error.status_code < 500:
                status = HTTPStatus.BAD_REQUEST
            else:
                status = HTTPStatus.BAD_GATEWAY
            return _json(status, {"code": error.code})
        except AddressMismatchError:
            # The SDK refused an address the product cannot derive from its pins: never shown.
            LOG.error("demo: the service returned an address the pins do not derive")
            return _json(HTTPStatus.BAD_GATEWAY, {"code": "address_not_derivable"})
        except TransportError:
            LOG.warning("demo: service transport unavailable")
            response = _json(HTTPStatus.SERVICE_UNAVAILABLE, {"code": "unavailable"})
            response.headers["retry-after"] = "2"
            return response
        except ResponseValidationError:
            LOG.warning("demo: service response validation failed")
            return _json(HTTPStatus.BAD_GATEWAY, {"code": "bad_gateway"})
        except (httpx.HTTPError, MissingProductKeyError):
            LOG.warning("demo: service unavailable", exc_info=True)
            return _json(HTTPStatus.SERVICE_UNAVAILABLE, {"code": "unavailable"})

    def _api(self, method: str, name: str, headers: dict[str, str], body: bytes) -> Response:
        # The same for every visitor, cached, and read before a demo account exists.
        if name == "trust" and method == "GET":
            return _json(HTTPStatus.OK, self._trust_view())
        if name == "assets" and method == "GET":
            return _json(HTTPStatus.OK, {"networks": self._payable_networks()})
        account = _cookie_account(headers.get("cookie", ""))
        if name == "account" and method == "GET":
            cookie = None
            if account is None:
                if not self._new_accounts.allow("global"):
                    return _json(HTTPStatus.TOO_MANY_REQUESTS, {"code": "rate_limited"})
                account = f"demo-{secrets.token_hex(12)}"
                cookie = self._set_cookie(account)
            self._ensure_account(account)
            response = _json(HTTPStatus.OK, self._account(account))
            if cookie is not None:
                response.headers["set-cookie"] = cookie
            return response
        if account is None:
            return _json(HTTPStatus.UNAUTHORIZED, {"code": "no_demo_account"})
        self._ensure_account(account)
        if method == "POST":
            # A JSON body forces a CORS preflight, which this API allows only for the website.
            if not headers.get("content-type", "").startswith("application/json"):
                return _json(HTTPStatus.UNSUPPORTED_MEDIA_TYPE, {"code": "json_required"})
            request = _json_body(body)
            if request is None:
                return _json(HTTPStatus.BAD_REQUEST, {"code": "json_object_required"})
            if name == "quotes":
                return self._create_quote(account, request)
            if not (self._writes.allow("global") and self._writes_per_account.allow(account)):
                return _json(HTTPStatus.TOO_MANY_REQUESTS, {"code": "rate_limited"})
            if name == "deposit_address":
                return self._create_deposit_address(account)
            if name == "refunds":
                return self._create_refund(account, request)
            refund, _, action = name.removeprefix("refunds/").partition("/")
            if name.startswith("refunds/") and REFUND_ID.fullmatch(refund):
                if action == "mark_paid":
                    return self._mark_refund_paid(account, refund, request)
                if action == "cancel":
                    return self._cancel_refund(account, refund)
            return _json(HTTPStatus.NOT_FOUND, {"code": "not_found"})
        if method != "GET":
            return _json(HTTPStatus.METHOD_NOT_ALLOWED, {"code": "method_not_allowed"})
        if not self._reads.allow(account):
            return _json(HTTPStatus.TOO_MANY_REQUESTS, {"code": "rate_limited"})
        view: dict[str, Any] | None = None
        if name == "deposit_address":
            view = self._deposit_address(account)
        elif name == "sweeps":
            view = self._sweeps_view()
        elif name.startswith("quotes/") and QUOTE_ID.fullmatch(name.removeprefix("quotes/")):
            view = self._quote_timeline(account, name.removeprefix("quotes/"))
        elif name.startswith("deposits/") and DEPOSIT_ID.fullmatch(name.removeprefix("deposits/")):
            view = self._deposit_timeline(account, name.removeprefix("deposits/"))
        if view is None:
            return _json(HTTPStatus.NOT_FOUND, {"code": "not_found"})
        return _json(HTTPStatus.OK, view)

    # Accounts -----------------------------------------------------------------------------------

    def _set_cookie(self, account: str) -> str:
        # Host-only on the API's origin. The website is same-site with it (both under phala.com),
        # so a Lax cookie goes with the page's credentialed requests and with no other site's.
        cookie = (
            f"{ACCOUNT_COOKIE}={account}; Path={self.root}/; Max-Age={30 * 86_400}; "
            "HttpOnly; SameSite=Lax"
        )
        return cookie + ("; Secure" if self.secure_cookie else "")

    def _ensure_account(self, account: str) -> None:
        if self.ledger.team_suspended(account) is None:
            self.ledger.add_team(account)

    def _account(self, account: str) -> dict[str, Any]:
        deposits = [
            deposit.to_dict()
            for deposit in _take(
                self._service().list_deposits(client_reference_id=account, page_size=50), 50
            )
        ]
        now = self._clock()
        with self.ledger.transaction() as db:
            quotes = db.execute(
                "SELECT id, amount, amount_atomic, exchange_rate, asset, chain_id, expires_at, "
                "created FROM demo_quotes WHERE account = ? ORDER BY created DESC LIMIT 20",
                (account,),
            ).fetchall()
            address = db.execute(
                "SELECT id FROM demo_deposit_addresses WHERE account = ?", (account,)
            ).fetchone()
            ledgers = {deposit["id"]: _ledger(db, deposit["id"]) for deposit in deposits}
            lines = _ledger_lines(db, account)
        paid_quotes = {d["quote"] for d in deposits if isinstance(d.get("quote"), str)}
        payments = [
            {
                "kind": "quote" if isinstance(deposit.get("quote"), str) else "address",
                "id": deposit["id"],
                "quote": deposit.get("quote") if isinstance(deposit.get("quote"), str) else None,
                "created": deposit["created"],
                "amount": deposit.get("amount"),
                "amount_atomic": deposit["amount_atomic"],
                "chain_id": deposit["chain_id"],
                "asset": deposit.get("asset"),
                "exchange_rate": deposit.get("exchange_rate"),
                "status": deposit["status"],
                "final": deposit["final"],
                "swept": deposit["swept"],
                "tx_hash": deposit["tx_hash"],
                "amount_refunded_atomic": deposit["amount_refunded_atomic"],
                "net": ledgers[deposit["id"]]["net"],
                "bonus": ledgers[deposit["id"]]["bonus"],
            }
            for deposit in deposits
        ]
        for quote_id, amount, amount_atomic, rate, asset, chain_id, expires_at, created in quotes:
            if quote_id in paid_quotes:
                continue
            payments.append(
                {
                    "kind": "quote",
                    "id": quote_id,
                    "quote": quote_id,
                    "created": created,
                    "amount": amount,
                    "amount_atomic": amount_atomic,
                    "chain_id": chain_id,
                    "asset": asset,
                    "exchange_rate": rate,
                    "status": "expired" if now >= expires_at else "awaiting_payment",
                    "final": False,
                    "swept": False,
                    "tx_hash": None,
                    "amount_refunded_atomic": "0",
                    "net": None,
                    "bonus": None,
                }
            )
        payments.sort(key=lambda payment: payment["created"], reverse=True)
        return {
            "account_id": account,
            "balance": self.ledger.balance_for(account),
            "ledger": lines,
            "presets": PRESETS,
            "min_amount": MIN_AMOUNT,
            "max_amount": MAX_AMOUNT,
            "api_base": self.config.service_url,
            "factory": self.config.factory,
            "deposit_address": None if address is None else address[0],
            "payments": payments,
        }

    # Quotes -------------------------------------------------------------------------------------

    def _create_quote(self, account: str, request: dict[str, Any]) -> Response:
        amount = request.get("amount")
        if type(amount) is not int or not MIN_AMOUNT <= amount <= MAX_AMOUNT:
            return _json(HTTPStatus.BAD_REQUEST, {"code": "amount_invalid"})
        # A token on a network the product offers, as the service's quote request names both.
        chain_id, asset = request.get("chain_id"), request.get("asset")
        offered = {
            (network["chain_id"], each["asset"])
            for network in self._payable_networks()
            for each in network["assets"]
        }
        pair = (chain_id, asset)
        if type(chain_id) is not int or not isinstance(asset, str) or pair not in offered:
            return _json(HTTPStatus.BAD_REQUEST, {"code": "asset_invalid"})
        if not (
            self._quotes.allow("global")
            and self._quotes_per_account.allow(account)
            and self._quotes_per_day.allow(account)
        ):
            return _json(HTTPStatus.TOO_MANY_REQUESTS, {"code": "rate_limited"})
        # The console's order id travels in `metadata`, to the deposit and its webhooks.
        order_id = f"order_{secrets.token_hex(6)}"
        with self.recorder.capture() as calls:
            quote = self._service().create_quote(
                account,
                amount,
                chain_id=chain_id,
                asset=asset,
                idempotency_key=str(uuid.uuid4()),
                metadata={"order_id": order_id, "workspace": account},
            )
        if not isinstance(quote.client_secret, str):
            return _json(HTTPStatus.BAD_GATEWAY, {"code": "unexpected_response"})
        self.ledger.record_quote(account, quote.to_dict())
        with self.ledger.transaction() as db:
            db.execute(
                "INSERT OR IGNORE INTO demo_quotes (id, account, amount, amount_atomic, "
                "exchange_rate, address, expires_at, created, api, asset, chain_id) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    quote.id,
                    account,
                    quote.amount,
                    quote.amount_atomic,
                    quote.exchange_rate,
                    quote.address,
                    quote.expires_at,
                    quote.created,
                    json.dumps(calls),
                    quote.asset,
                    quote.chain_id,
                ),
            )
        return _json(
            HTTPStatus.OK,
            {
                "quote": quote.id,
                "client_secret": quote.client_secret,
                # Recomputed from the pins by the SDK (it raises otherwise); `<Checkout>` shows
                # the quote only when the service's address is this one.
                "expected_address": quote.address,
                "order_id": order_id,
                "amount": quote.amount,
                # The price the quote locks until `expires_at`, in USD per token.
                "chain_id": quote.chain_id,
                "asset": quote.asset,
                "amount_atomic": quote.amount_atomic,
                "exchange_rate": quote.exchange_rate,
                "expires_at": quote.expires_at,
                "api": calls,
            },
        )

    def _quote_timeline(self, account: str, quote_id: str) -> dict[str, Any] | None:
        with self.ledger.transaction() as db:
            row = db.execute(
                "SELECT api FROM demo_quotes WHERE id = ? AND account = ?", (quote_id, account)
            ).fetchone()
        if row is None:
            return None
        with self.recorder.capture() as calls:
            quote = self._service().get_quote(quote_id)
            deposits = list(_take(self._service().list_deposits(quote=quote_id, page_size=10), 10))
            payment = quote.payment.to_dict() if isinstance(quote.payment, Payment) else None
            view = self._payment_view(
                deposits[0].to_dict() if deposits else None, payment, quote.to_dict()
            )
        view["api"] = [*json.loads(row[0]), *calls]
        return view

    # Deposit addresses --------------------------------------------------------------------------

    def _create_deposit_address(self, account: str) -> Response:
        with self.recorder.capture() as calls:
            # Returns the customer's active address, issuing it once; the SDK recomputes every
            # network's address from the pins and raises on a mismatch.
            address = self._service().create_deposit_address(
                account, metadata={"workspace": account}
            )
        if not isinstance(address.client_secret, str):
            return _json(HTTPStatus.BAD_GATEWAY, {"code": "unexpected_response"})
        with self.ledger.transaction() as db:
            db.execute(
                "INSERT INTO demo_deposit_addresses (account, id, created) VALUES (?, ?, ?) "
                "ON CONFLICT (account) DO UPDATE SET id = excluded.id",
                (account, address.id, address.created),
            )
        self.ledger.record_deposit_address(account, address.to_dict())
        return _json(
            HTTPStatus.OK,
            {
                "deposit_address": _deposit_address_view(address),
                "client_secret": address.client_secret,
                "verified": True,
                "api": calls,
            },
        )

    def _deposit_address(self, account: str) -> dict[str, Any] | None:
        with self.recorder.capture() as calls:
            address = self._account_address(account)
        if address is None:
            return None
        return {"deposit_address": _deposit_address_view(address), "verified": True, "api": calls}

    def _account_address(self, account: str) -> DepositAddress | None:
        """The visitor's deposit address, read with the SDK, which recomputes it from the pins."""
        with self.ledger.transaction() as db:
            row = db.execute(
                "SELECT id FROM demo_deposit_addresses WHERE account = ?", (account,)
            ).fetchone()
        return None if row is None else self._service().get_deposit_address(row[0])

    def _deposit_timeline(self, account: str, deposit_id: str) -> dict[str, Any] | None:
        """A deposit's timeline; before the service records it, the payment its address saw."""
        with self.recorder.capture() as calls:
            try:
                deposit: dict[str, Any] | None = self._service().get_deposit(deposit_id).to_dict()
            except ApiError as error:
                if error.status_code != 404:
                    raise
                deposit = None
            payment = None
            if deposit is None:
                address = self._account_address(account)
                payments = [] if address is None else address.to_dict()["payments"]
                payment = next((p for p in payments if p.get("deposit") == deposit_id), None)
                if payment is None:
                    return None
            elif deposit["client_reference_id"] != account:
                return None
            quote = None
            if deposit is not None and isinstance(deposit.get("quote"), str):
                quote = self._service().get_quote(deposit["quote"]).to_dict()
            view = self._payment_view(deposit, payment, quote)
        view["api"] = calls
        return view

    # Payments -----------------------------------------------------------------------------------

    def _payment_view(
        self,
        deposit: dict[str, Any] | None,
        payment: dict[str, Any] | None,
        quote: dict[str, Any] | None,
    ) -> dict[str, Any]:
        """A payment's timeline, refunds, ledger, and events; every value comes from the service,
        the chain, or this product's ledger."""
        refunds: list[dict[str, Any]] = []
        sweep = None
        if deposit is not None:
            refunds = [
                refund.to_dict()
                for refund in _take(
                    self._service().list_refunds(deposit=deposit["id"], page_size=20), 20
                )
            ]
            if deposit["swept"]:
                sweep = self._sweep_of(deposit)
        tx_hash = deposit["tx_hash"] if deposit else (payment["tx_hash"] if payment else None)
        source = deposit or payment or quote or {}
        chain_id = source.get("chain_id")
        sent = None
        if tx_hash is not None and isinstance(chain_id, int) and chain_id in self._chain_ids:
            sent = self._block_time(chain_id, tx_hash)
        keys = {deposit["id"]} if deposit else ({payment["deposit"]} if payment else set())
        keys |= {refund["id"] for refund in refunds}
        if quote is not None:
            keys.add(quote["id"])
        events = self._events(keys)
        with self.ledger.transaction() as db:
            ledger = None if deposit is None else _ledger(db, deposit["id"])
        return {
            "kind": "quote" if quote is not None else "address",
            "quote": None if quote is None else _quote_view(quote),
            "deposit": deposit,
            "payment": payment,
            "sent": sent,
            "steps": _steps(
                quote,
                deposit=deposit,
                payment=payment,
                events=events,
                ledger=ledger,
                sent=sent,
                sweep=sweep,
                now=self._clock(),
            ),
            "refunds": [_refund_view(refund, deposit) for refund in refunds],
            "ledger": None if deposit is None else _ledger_view(deposit, ledger),
            "events": events,
        }

    def _events(self, keys: set[str]) -> list[dict[str, Any]]:
        """This product's verified webhook events about the quote, the deposit, or its refunds."""
        with self.ledger.transaction() as db:
            rows = ProductLedger.event_rows(db, keys)
        events = []
        for event_id, event_type, data, received_at in rows:
            payload = json.loads(data)
            events.append(
                {
                    "id": event_id,
                    "type": event_type,
                    "received_at": received_at,
                    # Only events whose signature verified against the pinned key are stored.
                    "verified": True,
                    "data": payload,
                }
            )
        return events

    def _block_time(self, chain_id: int, tx_hash: str) -> dict[str, Any] | None:
        """The block and time of the transaction's block, from the product's RPC of its chain."""
        cached = self._block_times.get((chain_id, tx_hash))
        if cached is None:
            try:
                receipt = self._rpc(chain_id, "eth_getTransactionReceipt", tx_hash)
                if not isinstance(receipt, dict):
                    return None
                block = self._rpc(chain_id, "eth_getBlockByNumber", receipt["blockNumber"], False)
                cached = (int(receipt["blockNumber"], 16), int(block["timestamp"], 16))
            except (httpx.HTTPError, ValueError, KeyError, TypeError):
                LOG.warning("demo: block time lookup failed", exc_info=True)
                return None
            with self._lock:
                self._block_times[(chain_id, tx_hash)] = cached
        return {"tx_hash": tx_hash, "block_number": cached[0], "at": cached[1]}

    def _rpc(self, chain_id: int, method: str, *params: Any) -> Any:
        body = self._http.post(
            self.config.chain(chain_id).rpc_url,
            json={"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)},
        ).json()
        return body["result"]

    def _sweep_of(self, deposit: dict[str, Any]) -> dict[str, Any] | None:
        """The finalized sweep (`GET /v1/sweeps`) that moved the deposit: the first of its
        forwarder after its block."""
        sweeps = self._service().list_sweeps(
            chain_id=deposit["chain_id"],
            forwarder=deposit["address"],
            token=deposit["asset_contract"],
            page_size=1,
        )
        for sweep in sweeps:
            if same_address(sweep.address, deposit["address"]) and (
                sweep.block_number >= deposit["block_number"]
            ):
                return sweep.to_dict()
        return None

    # Refunds ------------------------------------------------------------------------------------

    def _create_refund(self, account: str, request: dict[str, Any]) -> Response:
        deposit_id = request.get("deposit")
        destination = request.get("destination_address")
        amount = request.get("amount_atomic")
        if not isinstance(deposit_id, str) or not DEPOSIT_ID.fullmatch(deposit_id):
            return _json(HTTPStatus.BAD_REQUEST, {"code": "deposit_invalid"})
        if not isinstance(destination, str) or not EVM_ADDRESS.fullmatch(destination):
            return _json(HTTPStatus.BAD_REQUEST, {"code": "destination_address_invalid"})
        if not isinstance(amount, str) or not ATOMIC.fullmatch(amount):
            return _json(HTTPStatus.BAD_REQUEST, {"code": "amount_atomic_invalid"})
        with self.recorder.capture() as calls:
            deposit = self._service().get_deposit(deposit_id)
            if deposit.client_reference_id != account:
                return _json(HTTPStatus.NOT_FOUND, {"code": "not_found"})
            refund = self._service().create_refund(
                deposit_id,
                destination,
                int(amount),
                idempotency_key=str(uuid.uuid4()),
                metadata={"workspace": account},
            )
        with self.ledger.transaction() as db:
            db.execute(
                "INSERT OR IGNORE INTO demo_refunds (id, account, deposit, created) "
                "VALUES (?, ?, ?, ?)",
                (refund.id, account, deposit_id, refund.created),
            )
        body = _refund_view(refund.to_dict(), deposit.to_dict())
        return _json(HTTPStatus.OK, {"refund": body, "api": calls})

    def _mark_refund_paid(self, account: str, refund_id: str, request: dict[str, Any]) -> Response:
        tx_hash = request.get("transaction_hash")
        index = request.get("receipt_log_index")
        if not isinstance(tx_hash, str) or not TX_HASH.fullmatch(tx_hash):
            return _json(HTTPStatus.BAD_REQUEST, {"code": "transaction_hash_invalid"})
        if index is not None and (type(index) is not int or not 0 <= index < 10_000):
            return _json(HTTPStatus.BAD_REQUEST, {"code": "receipt_log_index_invalid"})
        if not self._owns_refund(account, refund_id):
            return _json(HTTPStatus.NOT_FOUND, {"code": "not_found"})
        with self.recorder.capture() as calls:
            refund = self._service().mark_refund_paid(
                refund_id, tx_hash.lower(), receipt_log_index=index
            )
        return _json(HTTPStatus.OK, {"refund": _refund_view(refund.to_dict()), "api": calls})

    def _cancel_refund(self, account: str, refund_id: str) -> Response:
        if not self._owns_refund(account, refund_id):
            return _json(HTTPStatus.NOT_FOUND, {"code": "not_found"})
        with self.recorder.capture() as calls:
            refund = self._service().cancel_refund(refund_id)
        return _json(HTTPStatus.OK, {"refund": _refund_view(refund.to_dict()), "api": calls})

    def _owns_refund(self, account: str, refund_id: str) -> bool:
        with self.ledger.transaction() as db:
            row = db.execute(
                "SELECT 1 FROM demo_refunds WHERE id = ? AND account = ?", (refund_id, account)
            ).fetchone()
        return row is not None

    # Sweeps -------------------------------------------------------------------------------------

    def _sweeps_view(self) -> dict[str, Any]:
        """Per network and token the product offers: the account's unswept balance, the flush
        the merchant signs, and the finalized sweeps; the same for every visitor, cached for 10
        seconds."""
        now = self._clock()
        refresh = False
        synchronous = False
        with self._lock:
            if self._sweeps is not None:
                age = now - self._sweeps[0]
                if age < 10:
                    return self._sweeps[1]
                if age >= 60:
                    self._sweeps_refreshing = True
                    synchronous = True
                elif not self._sweeps_refreshing:
                    self._sweeps_refreshing = True
                    refresh = True
                view = self._sweeps[1]
            else:
                view = None
        if synchronous:
            try:
                return self._build_sweeps_view(now)
            finally:
                with self._lock:
                    self._sweeps_refreshing = False
        if view is not None:
            if refresh:
                threading.Thread(target=self._refresh_sweeps, args=(now,), daemon=True).start()
            return view
        return self._build_sweeps_view(now)

    def _refresh_sweeps(self, now: float) -> None:
        try:
            self._build_sweeps_view(now)
        except TopupError:
            LOG.warning("sweeps refresh failed", exc_info=True)
        finally:
            with self._lock:
                self._sweeps_refreshing = False

    def _build_sweeps_view(self, now: float) -> dict[str, Any]:
        service = self._sweeps_service()
        networks = self._payable_networks(service)
        with self.recorder.capture() as calls:
            unswept = service.get_balance().unswept
            groups = [
                self._sweep_group(network, asset, unswept, now, service)
                for network in networks
                for asset in network["assets"]
            ]
        view = {"factory": self.config.factory, "groups": groups, "api": calls}
        with self._lock:
            self._sweeps = (self._clock(), view)
        return view

    def _sweep_group(
        self,
        network: dict[str, Any],
        asset: dict[str, Any],
        unswept: list[Any],
        now: float,
        service: TopupClient,
    ) -> dict[str, Any]:
        """One token's sweep on one network."""
        chain_id, token, treasury = network["chain_id"], asset["contract"], network["treasury"]
        static = {
            "chain_id": chain_id,
            "network": network["name"],
            "asset": asset["asset"],
            "symbol": asset["symbol"],
            "decimals": asset["decimals"],
            "token": token,
            "treasury": treasury,
        }
        try:
            return self._sweep_group_data(static, service, unswept, now)
        except (ApiError, TransportError, ResponseValidationError) as error:
            LOG.warning("sweep group unavailable for chain %s asset %s: %s", chain_id, token, error)
            return {
                **static,
                "unavailable": True,
                "unswept_atomic": "0",
                "final_unswept_atomic": "0",
                "sweepable_forwarders": 0,
                "refused_forwarders": 0,
                "flush": [],
                "safe_batch": None,
                "sweeps": [],
            }

    def _sweep_group_data(
        self, static: dict[str, Any], service: TopupClient, unswept: list[Any], now: float
    ) -> dict[str, Any]:
        chain_id, token, treasury = static["chain_id"], static["token"], static["treasury"]
        amounts = next(
            (
                amount.to_dict()
                for amount in unswept
                if amount.chain_id == chain_id and same_address(amount.token, token)
            ),
            {"amount_atomic": "0", "final_amount_atomic": "0"},
        )
        # Only a final unswept balance has forwarders to flush.
        forwarders = []
        if amounts["final_amount_atomic"] != "0":
            forwarders = list(
                _take(
                    service.list_forwarders(
                        chain_id=chain_id, sweepable=token, page_size=min(MAX_SWEEP_FORWARDERS, 100)
                    ),
                    MAX_SWEEP_FORWARDERS,
                )
            )
        sweeps = [
            sweep.to_dict()
            for sweep in _take(
                service.list_sweeps(chain_id=chain_id, token=token, page_size=10), 10
            )
        ]
        # Flush only forwarders the pins derive: the service's word decides nothing here.
        derived = [
            forwarder
            for forwarder in forwarders
            if same_address(forwarder.factory, self.config.factory)
            and same_address(forwarder.treasury, treasury)
            and same_address(
                forwarder_address(
                    self.config.factory,
                    self.config.implementation,
                    treasury,
                    bytes.fromhex(forwarder.salt.removeprefix("0x")),
                ),
                forwarder.address,
            )
        ]
        calls_to_sign = flush_transactions(derived, token) if derived else []
        return {
            **static,
            "unavailable": False,
            "unswept_atomic": amounts["amount_atomic"],
            "final_unswept_atomic": amounts["final_amount_atomic"],
            "sweepable_forwarders": len(derived),
            "refused_forwarders": len(forwarders) - len(derived),
            "flush": calls_to_sign,
            "safe_batch": None
            if not calls_to_sign
            else safe_batch(
                chain_id,
                treasury,
                calls_to_sign,
                name="Phala Pay sweep",
                description=f"Sweep {static['symbol']} to the treasury",
                created_at_ms=int(now * 1000),
            ),
            "sweeps": sweeps,
        }

    # Networks -----------------------------------------------------------------------------------

    def _payable_networks(self, service: TopupClient | None = None) -> list[dict[str, Any]]:
        """The networks a customer can pay on, each with its tokens: the service's payable assets
        (`GET /v1/config`) on the configured chains (a quote on any other chain has no treasury to
        recompute its address from), in the config's order, then the service's. A configured
        chain the service does not serve yet is left out."""
        with self._cache_changed:
            while self._networks_refreshing:
                self._wait_for_cache()
            if self._clock() < self._networks_expires:
                if self._networks is not None:
                    return self._networks[1]
                if self._networks_error is not None:
                    raise self._networks_error
            self._networks_refreshing = True
        try:
            networks = self._fetch_payable_networks(service)
        except (TopupError, httpx.HTTPError, MissingProductKeyError) as error:
            with self._cache_changed:
                self._networks_error = error
                self._networks_expires = self._clock() + 30
                if self._networks is not None:
                    return self._networks[1]
            raise
        else:
            with self._cache_changed:
                self._networks = (self._clock(), networks)
                self._networks_expires = self._clock() + 300
                self._networks_error = None
            return networks
        finally:
            with self._cache_changed:
                self._networks_refreshing = False
                self._cache_changed.notify_all()

    def _fetch_payable_networks(self, service: TopupClient | None) -> list[dict[str, Any]]:
        offered = (service or self._service()).get_config().assets
        networks = []
        for chain in self.config.chains:
            testnet = chain.chain_id not in MAINNETS
            # Each mintable test token, with the faucet contract that mints it, if not its own.
            minters = {token.address.lower(): token.minter for token in chain.test_tokens}
            assets = [
                {
                    "asset": asset.asset,
                    "symbol": asset.asset.upper(),
                    "contract": asset.contract,
                    "decimals": asset.decimals,
                    "pricing": asset.pricing,
                    "min_amount": asset.min_amount,
                    "quote_ttl_seconds": asset.quote_ttl_seconds,
                    # The typical time from paying to the credit at the account's confirmation on
                    # the chain, for the page's timeline.
                    "typical_credit_seconds": asset.typical_credit_seconds,
                    # The page's faucet helper: the visitor's wallet mints a mintable test token;
                    # another testnet token may have its issuer's faucet.
                    "mintable": asset.contract.lower() in minters,
                    "minter": minters.get(asset.contract.lower()),
                    "faucet": TOKEN_FAUCETS.get(asset.asset) if testnet else None,
                    # The product's own promotion (reference_product.fulfillment).
                    "bonus_bps": self.config.bonus_bps.get(asset.asset.lower(), 0),
                }
                for asset in offered
                if asset.chain_id == chain.chain_id
            ]
            if not assets:
                continue
            networks.append(
                {
                    "chain_id": chain.chain_id,
                    "name": f"{chain.name} testnet" if testnet else chain.name,
                    "testnet": testnet,
                    "explorer": EXPLORERS.get(chain.chain_id),
                    "faucet": GAS_FAUCETS.get(chain.chain_id) if testnet else None,
                    "treasury": chain.treasury,
                    "assets": assets,
                }
            )
        return networks

    # Trust --------------------------------------------------------------------------------------

    def _trust_view(self) -> dict[str, Any]:
        with self._cache_changed:
            while self._trust_refreshing:
                self._wait_for_cache()
            if self._clock() < self._trust_expires:
                if self._trust is not None:
                    return self._trust[1]
                if self._trust_error is not None:
                    raise self._trust_error
            self._trust_refreshing = True
        try:
            view = self._fetch_trust_view()
        except (TopupError, httpx.HTTPError, MissingProductKeyError) as error:
            with self._cache_changed:
                self._trust_error = error
                self._trust_expires = self._clock() + 30
                if self._trust is not None:
                    return self._trust[1]
            raise
        else:
            failed = not view["attestation"]["binding_verified"] or view["tls_evidence"] is None
            with self._cache_changed:
                self._trust_expires = self._clock() + (30 if failed else 300)
                self._trust_error = None
                if failed and self._trust is not None:
                    return self._trust[1]
                self._trust = (self._clock(), view)
            return view
        finally:
            with self._cache_changed:
                self._trust_refreshing = False
                self._cache_changed.notify_all()

    def _wait_for_cache(self) -> None:
        deadline = operation_deadline.get()
        remaining = None if deadline is None else deadline - time.monotonic()
        if remaining is not None and remaining <= 0:
            raise TransportError("timeout")
        self._cache_changed.wait(timeout=remaining)

    def _fetch_trust_view(self) -> dict[str, Any]:
        attestation: dict[str, Any]
        try:
            evidence = self._service().attestation(secrets.token_bytes(32))
            attestation = _attestation_view(evidence)
        except AttestationError:
            attestation = {"binding_verified": False}
        view = {
            "attestation": attestation,
            "tls_evidence": self._tls_evidence(),
            "verify_docs": VERIFY_DOCS,
            "dstack_verifier": "https://github.com/Dstack-TEE/dstack/tree/master/verifier",
        }
        return view

    def _tls_evidence(self) -> dict[str, str] | None:
        """App id, compose hash, and OS image of the dstack-ingress certificate evidence quote
        (RTMR3 events), as deploy/verify-ingress-evidence.sh reads them."""
        url = self.config.service_url.rstrip("/") + "/evidences/quote.json"
        try:
            body = self._http.get(url).raise_for_status().json()
            log = body["event_log"]
            entries = json.loads(log) if isinstance(log, str) else log
        except (httpx.HTTPError, ValueError, KeyError, TypeError):
            return None
        names = {
            "app-id": "app_id",
            "compose-hash": "compose_hash",
            "os-image-hash": "os_image_hash",
        }
        found = {
            names[entry["event"]]: str(entry.get("event_payload", ""))
            for entry in entries
            if isinstance(entry, dict) and entry.get("imr") == 3 and entry.get("event") in names
        }
        return {**found, "url": url} if "app_id" in found else None

    # Service ------------------------------------------------------------------------------------

    def _service(self) -> TopupClient:
        with self._lock:
            if self._client is None:
                self._client = TopupClient(
                    self.config.service_url,
                    self.config.api_key(),
                    account=self.config.account,
                    forwarder=(self.config.factory, self.config.implementation),
                    treasuries=self.config.treasuries(),
                    transport=self.recorder,
                    timeout=5,
                    max_attempts=1,
                )
            return self._client

    def _sweeps_service(self) -> TopupClient:
        with self._lock:
            if self._sweeps_client is None:
                self._sweeps_client = TopupClient(
                    self.config.service_url,
                    self.config.api_key(),
                    account=self.config.account,
                    forwarder=(self.config.factory, self.config.implementation),
                    treasuries=self.config.treasuries(),
                    transport=self.recorder,
                    timeout=20,
                    max_attempts=1,
                )
            return self._sweeps_client

    def close(self) -> None:
        with self._lock:
            if self._client is not None:
                self._client.close()
            if self._sweeps_client is not None:
                self._sweeps_client.close()
        self._http.close()


# Views ------------------------------------------------------------------------------------------


def _steps(
    quote: dict[str, Any] | None,
    *,
    deposit: dict[str, Any] | None,
    payment: dict[str, Any] | None,
    events: list[dict[str, Any]],
    ledger: dict[str, Any] | None,
    sent: dict[str, Any] | None,
    sweep: dict[str, Any] | None,
    now: float,
) -> list[dict[str, Any]]:
    """The payment's timeline; every value and time comes from the service, the chain, or this
    product's ledger. `at` is Unix seconds, or `None` where no source reports a time."""
    status = None if deposit is None else deposit["status"]
    credited = (
        deposit is not None
        and status in ("credited", "reversed")
        and isinstance(deposit.get("valued_at"), int)
    )
    delivered = next((e for e in events if e["type"] == "deposit.credited"), None)
    reversal = next((e for e in events if e["type"] == "deposit.reversed"), None)
    expired = quote is not None and (
        quote["status"] in ("expired", "canceled")
        or (quote["status"] == "open" and now >= quote["expires_at"])
    )
    steps: list[dict[str, Any]] = []

    def step(key: str, state: str, at: float | None, details: list[dict[str, Any]]) -> None:
        steps.append({"key": key, "state": state, "at": at, "details": details})

    if quote is not None:
        step(
            "quote_created",
            "complete",
            quote["created"],
            [
                {"label": "Quote", "value": quote["id"], "mono": True},
                {
                    "label": "Locked price",
                    "value": quote["exchange_rate"],
                    "kind": "rate",
                    "unit": quote["asset"].upper(),
                },
                {"label": "Exact amount", "value": quote["amount_atomic"], "kind": "atomic"},
                {
                    "label": "Address (recomputed by the SDK)",
                    "value": quote["address"],
                    "kind": "address",
                },
                {"label": "Expires", "value": quote["expires_at"], "kind": "time"},
                {"label": "Metadata", "value": _metadata_text(quote.get("metadata"))},
            ],
        )

    # Sent: the transaction's block on chain.
    if sent is not None:
        step(
            "sent",
            "complete",
            sent["at"],
            [
                {"label": "Transaction", "value": sent["tx_hash"], "kind": "tx"},
                {"label": "Block", "value": sent["block_number"]},
            ],
        )
    else:
        step("sent", "failed" if expired else "current", None, [])

    # Received: the service saw the transfer (a `seen` payment), then recorded it.
    if deposit is not None:
        step(
            "received",
            "complete",
            deposit["created"],
            [
                {"label": "Deposit", "value": deposit["id"], "mono": True},
                {"label": "From", "value": deposit["from_address"], "kind": "address"},
                {"label": "Amount", "value": deposit["amount_atomic"], "kind": "atomic"},
            ],
        )
    elif payment is not None:
        details: list[dict[str, Any]] = [
            {"label": "Amount", "value": payment["amount_atomic"], "kind": "atomic"},
        ]
        if isinstance(payment.get("confirmations"), int):
            details.append({"label": "Confirmations", "value": payment["confirmations"]})
        if isinstance(payment.get("matches_quote"), bool):
            details.append(
                {"label": "Matches the quote", "value": "yes" if payment["matches_quote"] else "no"}
            )
        step("received", "complete", None, details)
    else:
        step("received", "upcoming", None, [])

    # Credited: valued and screened at the route's confirmation.
    if status == "rejected" and deposit is not None:
        reason = deposit.get("rejection_reason") or ""
        step("credited", "failed", None, [{"label": "Reason", "value": reason}])
    elif credited and deposit is not None:
        step(
            "credited",
            "complete",
            deposit["valued_at"],
            [
                {"label": "Credit", "value": deposit.get("amount"), "kind": "usd"},
                {
                    "label": "Priced at",
                    "value": "the quote's locked price"
                    if deposit.get("price_source") == "quote"
                    else "spot (market rate on arrival)",
                },
                {
                    "label": "Rate",
                    "value": deposit.get("exchange_rate"),
                    "kind": "rate",
                    "unit": (deposit.get("asset") or "").upper(),
                },
            ],
        )
    else:
        step("credited", "current" if payment or deposit else "upcoming", None, [])

    # The signed webhook, as this product's handler received and applied it.
    if delivered is not None:
        obj = delivered["data"].get("object") or {}
        details = [
            {"label": "Event", "value": delivered["id"], "mono": True},
            {"label": "Signature", "value": "verified (Standard Webhooks v1a, pinned key)"},
            {"label": "data.object.metadata", "value": _metadata_text(obj.get("metadata"))},
        ]
        if ledger is not None and ledger["credit"] is not None:
            details += [
                {"label": "Ledger order", "value": f"{ledger['status']} ({ledger['order_key']})"},
                {"label": "Credit", "value": ledger["credit"], "kind": "usd_delta"},
            ]
            if ledger["bonus"]:
                details.append(
                    {
                        "label": "Bonus (this product's)",
                        "value": ledger["bonus"],
                        "kind": "usd_delta",
                    }
                )
        step("webhook_received", "complete", delivered["received_at"], details)
    else:
        step("webhook_received", "current" if credited else "upcoming", None, [])

    # Final: the deposit's block is final on both providers; it can no longer be reversed.
    if deposit is not None and deposit["final"]:
        step(
            "final",
            "complete",
            deposit.get("final_at"),
            [{"label": "Block", "value": deposit["block_number"]}],
        )
    elif status != "reversed":
        step("final", "current" if deposit is not None else "upcoming", None, [])

    if status == "reversed" and deposit is not None:
        step(
            "reversed",
            "failed",
            None if reversal is None else reversal["received_at"],
            [
                {"label": "Taken back", "value": deposit["amount_reversed"], "kind": "usd"},
                {"label": "Event", "value": "deposit.reversed" if reversal else "not received"},
            ],
        )

    # Swept: the merchant's own flush, indexed by the service once finalized.
    if deposit is not None and deposit["swept"]:
        details = []
        if sweep is not None:
            details = [
                {"label": "Flush transaction", "value": sweep["tx_hash"], "kind": "tx"},
                {"label": "Treasury", "value": sweep["treasury"], "kind": "address"},
                {"label": "Moved", "value": sweep["amount_atomic"], "kind": "atomic"},
            ]
        step("swept", "complete", None if sweep is None else sweep["created"], details)
    elif status != "reversed":
        waiting = deposit is not None and deposit["final"]
        step("swept", "current" if waiting else "upcoming", None, [])
    return steps


def _quote_view(quote: dict[str, Any]) -> dict[str, Any]:
    return {
        key: quote.get(key)
        for key in (
            "id",
            "status",
            "amount",
            "asset",
            "chain_id",
            "amount_atomic",
            "exchange_rate",
            "address",
            "expires_at",
            "created",
            "metadata",
        )
    }


def _deposit_address_view(address: DepositAddress) -> dict[str, Any]:
    body = address.to_dict()
    return {
        "id": address.id,
        "client_reference_id": address.client_reference_id,
        "address": body.get("address"),
        "version": address.version,
        "status": address.status,
        "metadata": body.get("metadata", {}),
        "networks": body["networks"],
        "payments": body["payments"],
        "created": address.created,
    }


def _refund_view(refund: dict[str, Any], deposit: dict[str, Any] | None = None) -> dict[str, Any]:
    """The refund, with the exact transfer that pays it while it awaits one."""
    view = dict(refund)
    reason = refund.get("failure_reason")
    view["failure_explanation"] = None if reason is None else REFUND_FAILURES.get(reason, reason)
    token = None
    if deposit is not None:
        token = deposit["asset_contract"]
    elif isinstance(refund.get("deposit"), dict):
        token = refund["deposit"]["asset_contract"]
    view["transfer"] = None
    if refund["status"] == "pending" and refund.get("transaction_hash") is None and token:
        destination = refund["destination_address"]
        amount = int(refund["amount_atomic"])
        view["transfer"] = {
            "from": refund["treasury"],
            "token": token,
            "to": destination,
            "amount_atomic": refund["amount_atomic"],
            # ERC-20 transfer(to, amount), sent from the treasury to the token contract.
            "data": "0xa9059cbb"
            + destination.lower().removeprefix("0x").rjust(64, "0")
            + format(amount, "x").rjust(64, "0"),
        }
    return view


def _ledger(db: Any, deposit_id: str) -> dict[str, Any]:
    """The ledger's order for a deposit (its order key is the deposit id), its credit, and what it
    nets to after refunds and reversals."""
    row = db.execute(
        "SELECT o.id, o.provider_order_id, o.status, o.reason, c.id, c.amount_minor "
        "FROM orders o LEFT JOIN credit_transactions c ON c.order_id = o.id "
        "WHERE o.order_flow_code = ? AND o.provider_order_id = ?",
        (ORDER_FLOW_CODE, deposit_id),
    ).fetchone()
    snapshot = db.execute(
        "SELECT status, amount_refunded_minor, amount_reversed_minor FROM deposit_snapshots "
        "WHERE provider_order_id = ?",
        (deposit_id,),
    ).fetchone()
    view: dict[str, Any] = {
        "order_key": deposit_id,
        "status": None,
        "reason": None,
        "credit_transaction": None,
        "credit": None,
        "net": None,
        "bonus": None,
        "adjustments": [],
        "snapshot": None
        if snapshot is None
        else dict(zip(("status", "amount_refunded", "amount_reversed"), snapshot, strict=True)),
    }
    if row is None:
        return view
    order_id = row[0]
    view.update(status=row[2], reason=row[3], credit_transaction=row[4], credit=row[5])
    if row[5] is not None:
        view["net"] = ProductLedger.order_amounts(db, order_id)[1]
        view["bonus"] = ProductLedger.order_bonus(db, order_id)
    view["adjustments"] = [
        {"amount": amount, "reason": reason, "at": at}
        for amount, reason, at in db.execute(
            "SELECT amount_minor, reason, created_at FROM credit_adjustments WHERE order_id = ? "
            "ORDER BY created_at",
            (order_id,),
        ).fetchall()
    ]
    return view


def _ledger_view(deposit: dict[str, Any], ledger: dict[str, Any] | None) -> dict[str, Any]:
    """The snapshot rule on the service's deposit, beside what this product's ledger applied."""
    amount = deposit.get("amount") or 0
    rule = DepositView(
        deposit["status"], deposit["amount_refunded"], deposit["amount_reversed"]
    ).contribution(amount)
    return {
        "status": deposit["status"],
        "amount": deposit.get("amount"),
        "amount_refunded": deposit["amount_refunded"],
        "amount_reversed": deposit["amount_reversed"],
        "nets_to": rule,
        "product": ledger,
    }


def _ledger_lines(db: Any, account: str) -> list[dict[str, Any]]:
    """The workspace's balance, line by line, newest first: credits and their claw-backs, and
    the product's bonus lines (`kind` `bonus`: the grant, `PHA bonus +10%`, and its claw-backs,
    named by the event that took them back)."""
    rows = db.execute(
        "SELECT o.provider_order_id, c.amount_minor, 'deposit.credited', c.created_at, 'credit' "
        "FROM credit_transactions c JOIN orders o ON o.id = c.order_id WHERE c.team_id = ? "
        "UNION ALL SELECT o.provider_order_id, a.amount_minor, a.reason, a.created_at, 'credit' "
        "FROM credit_adjustments a JOIN orders o ON o.id = a.order_id WHERE a.team_id = ? "
        "UNION ALL SELECT o.provider_order_id, b.amount_minor, b.reason, b.created_at, 'bonus' "
        "FROM bonus_credits b JOIN orders o ON o.id = b.order_id WHERE b.team_id = ? "
        "ORDER BY 4 DESC, 5 DESC LIMIT 50",
        (account, account, account),
    ).fetchall()
    return [
        {"deposit": key, "amount": amount, "reason": reason, "at": at, "kind": kind}
        for key, amount, reason, at, kind in rows
    ]


def _metadata_text(metadata: Any) -> str:
    if not isinstance(metadata, dict) or not metadata:
        return "{}"
    return json.dumps(metadata, sort_keys=True)


def _attestation_view(evidence: AttestationResponse) -> dict[str, Any]:
    return {
        # TopupClient.attestation raises unless report_data binds the fresh nonce, the account,
        # the mode, and the account's webhook keys.
        "binding_verified": True,
        "account": evidence.account,
        "livemode": evidence.livemode,
        "webhook_public_key": evidence.webhook_keys[0].public_key,
        "report_data": evidence.report_data,
        "quote_bytes": len(evidence.tdx_quote) // 2,
    }


def _exchange(request: httpx.Request, response: httpx.Response) -> dict[str, Any]:
    # An allowlist: the API key never leaves the server, only its mode's prefix is shown.
    headers = {
        name: _redact_key(value) if name == "authorization" else value
        for name, value in request.headers.items()
        if name in ("authorization", "content-type", "idempotency-key")
    }
    return {
        "method": request.method,
        "url": str(request.url),
        "request": {"headers": headers, "body": _body(request.content)},
        "status": response.status_code,
        "response": _mask(_body(response.content)),
    }


def _body(content: bytes) -> Any:
    if not content:
        return None
    try:
        return json.loads(content)
    except ValueError:
        return "<non-JSON body>"


def _mask(value: Any) -> Any:
    if isinstance(value, dict):
        return {
            key: "…_secret_… (handed to this browser's page)"
            if key == "client_secret" and isinstance(item, str)
            else _mask(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [_mask(item) for item in value]
    return value


def _redact_key(authorization: str) -> str:
    """`Bearer ppay_rk_test_…`: the scheme and the key's prefix, never the key."""
    _, _, key = authorization.partition(" ")
    for prefix in ("ppay_sk_test_", "ppay_sk_live_", "ppay_rk_test_", "ppay_rk_live_"):
        if key.startswith(prefix):
            return f"Bearer {prefix}…"
    return "Bearer …"


def _take[T](items: Iterator[T], limit: int) -> Iterator[T]:
    return islice(items, limit)


def _json_body(body: bytes) -> dict[str, Any] | None:
    try:
        value = json.loads(body or b"{}")
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def _cookie_account(header: str) -> str | None:
    try:
        cookie = SimpleCookie(header)
    except CookieError:
        return None
    morsel = cookie.get(ACCOUNT_COOKIE)
    if morsel is None or not ACCOUNT_ID.fullmatch(morsel.value):
        return None
    return morsel.value


def _json(status: HTTPStatus, body: dict[str, Any]) -> Response:
    return Response(
        status,
        json.dumps(body).encode(),
        {"content-type": "application/json", "cache-control": "no-store"},
    )
