"""The Phala Pay demo's API: CORS for the website, account cookies, request policy, the pins, both
collection methods, refunds, sweeps, and the timeline's and ledger's states."""

from __future__ import annotations

import json
import re
import sys
import threading
import time
from collections.abc import Iterator
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import replace
from http import HTTPStatus
from pathlib import Path
from typing import Any

import httpx
import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from reference_product import demo as demo_module
from reference_product.config import (
    ChainConfig,
    MintableToken,
    MissingProductKeyError,
    ProductConfig,
)
from reference_product.demo import ApiRecorder, DemoConsole
from reference_product.fulfillment import Fulfillment, PinnedKeys
from reference_product.ledger import ProductLedger
from reference_product.server import AccountApi
from reference_product.transport import operation_deadline
from topup_sdk import (
    ApiError,
    credited_event_id,
    deposit_address_salt,
    forwarder_address,
    quote_salt,
    sign_webhook,
)
from topup_sdk.errors import ResponseValidationError, TransportError

NOW = 1_790_000_000
ACCOUNT = "acct_" + "ac" * 16
QUOTE = "qt_" + "0c" * 16
DEPOSIT = "dep_" + "0d" * 16
REFUND = "re_" + "0e" * 16
ADDRESS_ID = "da_" + "0a" * 16
TOKEN = "0x" + "44" * 20
BASE_TOKEN = "0x" + "77" * 20
BASE_USDT = "0x" + "66" * 20
BASE_FAUCET = "0x" + "99" * 20
TREASURY = "0x" + "cc" * 20
CONFIG = ProductConfig(
    service_url="https://service.test",
    account=ACCOUNT,
    # Two chains with the same treasury; the service serves only Sepolia at first.
    chains=(
        ChainConfig(
            chain_id=11155111,
            name="Sepolia",
            rpc_url="https://rpc.sepolia.test",
            treasury=TREASURY,
            test_tokens=(MintableToken("PHA", TOKEN),),
        ),
        ChainConfig(
            chain_id=84532,
            name="Base Sepolia",
            rpc_url="https://rpc.base-sepolia.test",
            treasury=TREASURY,
            test_tokens=(
                MintableToken("PHA", BASE_TOKEN),
                MintableToken("USDT", BASE_USDT, minter=BASE_FAUCET),
            ),
        ),
    ),
    factory="0x" + "aa" * 20,
    implementation="0x" + "bb" * 20,
    public_url="https://api.acme.example",
    web_origin="https://acme.example",
    bonus_bps={"pha": 1000},
)
WEBSITE = {"Origin": "https://acme.example"}


# The terms a quote was issued with (`Quote.terms`), as the service resolves them from a route's
# defaults.
QUOTE_TERMS = {
    "quote_ttl_seconds": 900,
    "quote_spread_bps": 50,
    "quote_tolerance_bps": 100,
    "quote_amount_decimals": 4,
    "min_amount": 100,
    "min_deposit_atomic": "0",
    "max_deposit_atomic": "1000000000000000000000000",
    "min_refund_atomic": "1",
    "confirmations": "2",
}


def _quote(customer: str = "acct", **fields: Any) -> dict[str, Any]:
    address = forwarder_address(
        CONFIG.factory, CONFIG.implementation, TREASURY, quote_salt(ACCOUNT, customer, QUOTE)
    )
    return {
        "id": QUOTE,
        "object": "quote",
        "livemode": False,
        "client_reference_id": customer,
        "treasury": TREASURY,
        "metadata": {"order_id": "order_1"},
        "amount": 2500,
        "currency": "usd",
        "chain_id": 11155111,
        "asset": "pha",
        "amount_atomic": "100",
        "exchange_rate": "25.00000000",
        "address": address,
        "payment_uri": f"ethereum:0x{'44' * 20}@11155111/transfer?address={address}&uint256=100",
        "status": "open",
        "expires_at": NOW + 900,
        "created": NOW,
        "payment": None,
        "deposit": None,
        "terms": QUOTE_TERMS,
        **fields,
    }


def _deposit_address(customer: str, *, address: str | None = None) -> dict[str, Any]:
    salt = deposit_address_salt(ACCOUNT, livemode=False, client_reference_id=customer, version=1)
    derived = forwarder_address(CONFIG.factory, CONFIG.implementation, TREASURY, salt)
    at = address or derived
    return {
        "id": ADDRESS_ID,
        "object": "deposit_address",
        "livemode": False,
        "client_reference_id": customer,
        "address": at,
        "version": 1,
        "salt": "0x" + salt.hex(),
        "status": "active",
        "created": NOW,
        "retired_at": None,
        "metadata": {"workspace": customer},
        "networks": [
            {
                "chain_id": 11155111,
                "address": at,
                "treasury": TREASURY,
                "assets": [
                    {
                        "asset": "pha",
                        "contract": TOKEN,
                        "decimals": 18,
                        "payment_uri": f"ethereum:{TOKEN}@11155111/transfer?address={at}",
                    }
                ],
            }
        ],
        "payments": [],
    }


def _deposit(customer: str = "acct", **fields: Any) -> dict[str, Any]:
    return {
        "id": DEPOSIT,
        "object": "deposit",
        "livemode": False,
        "client_reference_id": customer,
        "quote": QUOTE,
        "deposit_address": None,
        "status": "credited",
        "final": True,
        "final_at": NOW + 780,
        "swept": False,
        "metadata": {"order_id": "order_1"},
        "rejection_reason": None,
        "chain_id": 11155111,
        "asset": "pha",
        "asset_contract": "0x" + "44" * 20,
        "amount_atomic": "100",
        "amount": 2500,
        "currency": "usd",
        "exchange_rate": "25.00000000",
        "price_source": "quote",
        "valued_at": NOW,
        "address": "0x" + "11" * 20,
        "from_address": "0x" + "33" * 20,
        "tx_hash": "0x" + "ab" * 32,
        "receipt_log_index": 0,
        "revision": 0,
        "log_index": 0,
        "block_number": 1,
        "block_hash": "0x" + "10" * 32,
        "block_time": NOW,
        "amount_refunded_atomic": "0",
        "refunded": False,
        "amount_refunded": 0,
        "amount_reversed": 0,
        "created": NOW,
        **fields,
    }


def _list(url: str, data: list[dict[str, Any]]) -> dict[str, Any]:
    return {"object": "list", "url": url, "has_more": False, "data": data}


def _config_asset(
    asset: str, chain_id: int, contract: str, pricing: str = "spot"
) -> dict[str, Any]:
    return {
        "asset": asset,
        "chain_id": chain_id,
        "confirmations": "2",
        "contract": contract,
        "decimals": 18,
        "max_deposit_atomic": "1" + "0" * 24,
        "min_amount": 100,
        "min_deposit_atomic": "0",
        "min_refund_atomic": "1" + "0" * 18,
        "pricing": pricing,
        "quote_spread_bps": 50,
        "quote_tolerance_bps": 100,
        "quote_amount_decimals": 4,
        "quote_ttl_seconds": 900,
        "typical_credit_seconds": 30,
        "typical_finality_seconds": 900,
    }


class Service:
    """The service's merchant API, as much of it as the demo reads."""

    def __init__(self) -> None:
        self.quote = _quote()
        self.deposits: list[dict[str, Any]] = []
        self.refunds: list[dict[str, Any]] = []
        self.forwarders: list[dict[str, Any]] = []
        self.sweeps: list[dict[str, Any]] = []
        self.wrong_address: str | None = None
        self.customer = "acct"
        self.requests: list[httpx.Request] = []
        self.fail_sweeps: set[str] = set()
        self.sweeps_block = False
        self.sweeps_started = threading.Event()
        self.sweeps_release = threading.Event()
        # Test PHA and a second token on the product's chain, and a token on a chain the
        # product's pins do not cover.
        self.assets = [
            _config_asset("pha", 11155111, TOKEN),
            _config_asset("usdc", 11155111, "0x" + "55" * 20, "stablecoin"),
            _config_asset("pha", 1, "0x" + "66" * 20),
        ]

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        path = request.url.path
        body = json.loads(request.content) if request.content else {}
        if path == "/v1/config":
            config = {
                "object": "config",
                "livemode": False,
                "currency": "usd",
                "assets": self.assets,
                "max_open_amount_per_account": 1_000_000,
                "max_open_amount_per_customer": 500_000,
                "max_open_quotes": 100,
                "quote_creations_per_customer_per_minute": 10,
            }
            return httpx.Response(200, json=config)
        if path == "/v1/quotes":
            self.quote = _quote(
                body["client_reference_id"],
                metadata=body["metadata"],
                chain_id=body["chain_id"],
                asset=body["asset"],
            )
            quote = {**self.quote, "client_secret": f"{QUOTE}_secret_{'ab' * 24}"}
            return httpx.Response(200, json=quote)
        if path.startswith("/v1/quotes/"):
            return httpx.Response(200, json=self.quote)
        if path == "/v1/deposit_addresses":
            self.customer = body["client_reference_id"]
            address = _deposit_address(self.customer, address=self.wrong_address)
            return httpx.Response(200, json={**address, "client_secret": f"{ADDRESS_ID}_secret_ab"})
        if path == f"/v1/deposit_addresses/{ADDRESS_ID}":
            return httpx.Response(200, json=_deposit_address(self.customer))
        if path == "/v1/deposits":
            return httpx.Response(200, json=_list(path, self.deposits))
        if path.startswith("/v1/deposits/"):
            found = [d for d in self.deposits if d["id"] == path.rsplit("/", 1)[1]]
            if not found:
                return _error(404, "resource_missing")
            return httpx.Response(200, json=found[0])
        if path == "/v1/refunds" and request.method == "POST":
            refund = {
                "id": REFUND,
                "object": "refund",
                "livemode": False,
                "deposit": body["deposit"],
                "amount_atomic": body["amount_atomic"],
                "destination_address": body["destination_address"],
                "treasury": TREASURY,
                "status": "pending",
                "failure_reason": None,
                "transaction_hash": None,
                "receipt_log_index": None,
                "created": NOW,
                "metadata": body.get("metadata", {}),
            }
            self.refunds.append(refund)
            return httpx.Response(200, json=refund)
        if path == "/v1/refunds":
            return httpx.Response(200, json=_list(path, self.refunds))
        if path.endswith("/mark_paid"):
            self.refunds[0].update(transaction_hash=body["transaction_hash"])
            return httpx.Response(200, json=self.refunds[0])
        if path.endswith("/cancel"):
            self.refunds[0].update(status="canceled")
            return httpx.Response(200, json=self.refunds[0])
        if path == "/v1/balance":
            if self.sweeps_block:
                self.sweeps_started.set()
                self.sweeps_release.wait(1)
            amount = {
                "chain_id": 11155111,
                "token": TOKEN,
                "asset": "pha",
                "amount_atomic": "300",
                "final_amount_atomic": "200",
            }
            return httpx.Response(
                200, json={"object": "balance", "livemode": False, "unswept": [amount]}
            )
        if path == "/v1/forwarders":
            forwarders = self.forwarders
            for name in ("quote", "deposit_address"):
                if value := request.url.params.get(name):
                    forwarders = [item for item in forwarders if item.get(name) == value]
            return httpx.Response(200, json=_list(path, forwarders))
        if path == "/v1/sweeps":
            forwarder = request.url.params.get("forwarder")
            if forwarder is not None and not re.fullmatch(r"fwd_[0-9a-f]{32}", forwarder):
                return _error(400, "invalid_forwarder")
            if request.url.params.get("token", "").lower() in self.fail_sweeps:
                return _error(503, "screening_unavailable")
            sweeps = self.sweeps
            if forwarder is not None:
                sweeps = [item for item in sweeps if item["forwarder"] == forwarder]
            return httpx.Response(200, json=_list(path, sweeps))
        return _error(404, "not_here")


def _error(status: int, code: str) -> httpx.Response:
    error = {
        "type": "invalid_request_error",
        "code": code,
        "message": code,
        "doc_url": f"https://phala-network.github.io/phala-pay/#section/Errors/{code}",
    }
    return httpx.Response(status, json={"error": error})


def _rpc(request: httpx.Request) -> httpx.Response:
    method = json.loads(request.content)["method"]
    result: Any = {"blockNumber": "0x1"}
    if method == "eth_getBlockByNumber":
        result = {"timestamp": hex(NOW - 12)}
    return httpx.Response(200, json={"jsonrpc": "2.0", "id": 1, "result": result})


@pytest.fixture
def demo(tmp_path: Path) -> Iterator[tuple[DemoConsole, Service]]:
    (tmp_path / "product.key").write_text("ppay_rk_test_" + "A" * 43 + "000000\n")
    service = Service()
    console = DemoConsole(
        replace(CONFIG, api_key_file=str(tmp_path / "product.key")),
        ProductLedger(),
        recorder=ApiRecorder(httpx.MockTransport(service)),
        http=httpx.Client(transport=httpx.MockTransport(_rpc)),
        clock=lambda: NOW,
    )
    try:
        yield console, service
    finally:
        console.close()


def _account(console: DemoConsole) -> str:
    response = console.handle("GET", "/api/account", WEBSITE, b"")
    assert response.status == HTTPStatus.OK
    # Host-only (no Domain) on the API's origin, sent with the same-site website's requests.
    cookie = response.headers["set-cookie"]
    name, *attributes = cookie.split("; ")
    assert re.fullmatch(r"demo_account=demo-[0-9a-f]{24}", name)
    assert sorted(attributes) == sorted(
        ["Path=/", f"Max-Age={30 * 86_400}", "HttpOnly", "SameSite=Lax", "Secure"]
    )
    return name


@pytest.mark.parametrize("path", ["assets", "account", "trust"])
def test_service_timeouts_are_unavailable(demo: tuple[DemoConsole, Service], path: str) -> None:
    console, _ = demo

    def timeout(request: httpx.Request) -> httpx.Response:
        raise httpx.ReadTimeout("service timeout", request=request)

    console.recorder._inner = httpx.MockTransport(timeout)
    response = console.handle("GET", f"/api/{path}", WEBSITE, b"")
    assert response.status == HTTPStatus.SERVICE_UNAVAILABLE
    assert json.loads(response.body) == {"code": "unavailable"}
    assert response.headers["retry-after"] == "2"


def test_malformed_service_responses_are_bad_gateway(
    demo: tuple[DemoConsole, Service],
    monkeypatch: pytest.MonkeyPatch,
    caplog: pytest.LogCaptureFixture,
) -> None:
    console, _ = demo

    def malformed(*args: Any) -> Any:
        raise ResponseValidationError("private detail")

    monkeypatch.setattr(console, "_api", malformed)
    response = console.handle("GET", "/api/assets", WEBSITE, b"")
    assert response.status == HTTPStatus.BAD_GATEWAY
    assert json.loads(response.body) == {"code": "bad_gateway"}
    assert "service response validation failed" in caplog.text
    assert "private detail" not in caplog.text


def _customer(cookie: str) -> str:
    return cookie.split("=", 1)[1]


def _post(console: DemoConsole, cookie: str, path: str, body: dict[str, Any]) -> Any:
    headers = {"Cookie": cookie, "Content-Type": "application/json"}
    response = console.handle("POST", f"/api/{path}", headers, json.dumps(body).encode())
    return response.status, json.loads(response.body)


def _get(console: DemoConsole, cookie: str, path: str) -> Any:
    response = console.handle("GET", f"/api/{path}", {"Cookie": cookie}, b"")
    return response.status, json.loads(response.body)


def _create_quote(console: DemoConsole, cookie: str) -> dict[str, Any]:
    request = {"amount": 2500, "chain_id": 11155111, "asset": "pha"}
    status, body = _post(console, cookie, "quotes", request)
    assert status == HTTPStatus.OK
    assert body["client_secret"].startswith(QUOTE)
    # The address the SDK recomputed from the pins, for `<Checkout expectedAddress>`.
    assert re.fullmatch(r"0x[0-9a-fA-F]{40}", body["expected_address"])
    # The console's order id travels in the quote's metadata.
    assert body["api"][0]["request"]["body"]["metadata"]["order_id"] == body["order_id"]
    # The developer view shows only the key's prefix and masks the client secret.
    assert body["api"][0]["request"]["headers"]["authorization"] == "Bearer ppay_rk_test_…"
    assert ("A" * 43 + "000000") not in json.dumps(body["api"])
    assert f"{QUOTE}_secret_" not in json.dumps(body["api"])
    return dict(body)


def _steps(console: DemoConsole, cookie: str, path: str = f"quotes/{QUOTE}") -> dict[str, str]:
    status, body = _get(console, cookie, path)
    assert status == HTTPStatus.OK
    return {step["key"]: step["state"] for step in body["steps"]}


def test_offers_the_services_tokens_by_network_on_the_products_chains(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    # Read before a demo account exists, like the attestation.
    response = console.handle("GET", "/api/assets", WEBSITE, b"")
    assert response.status == HTTPStatus.OK
    # The service also takes PHA on chain 1, which the product has no treasury pin for.
    # Base Sepolia is configured but not served yet: it is simply absent.
    [network] = json.loads(response.body)["networks"]
    assert {key: network[key] for key in network if key != "assets"} == {
        "chain_id": 11155111,
        "name": "Sepolia testnet",
        "testnet": True,
        "explorer": "https://sepolia.etherscan.io",
        "faucet": "https://ethereum.org/en/developers/docs/networks/#sepolia",
        "treasury": CONFIG.chain(11155111).treasury,
    }
    assert [a["symbol"] for a in network["assets"]] == ["PHA", "USDC"]
    # Test PHA mints from the visitor's wallet and earns the product's bonus; test USDC comes
    # from Circle's faucet.
    assert network["assets"][0] == {
        "asset": "pha",
        "symbol": "PHA",
        "contract": TOKEN,
        "decimals": 18,
        "pricing": "spot",
        "min_amount": 100,
        "quote_ttl_seconds": 900,
        "typical_credit_seconds": 30,
        "mintable": True,
        "minter": None,
        "faucet": None,
        "bonus_bps": 1000,
    }
    usdc = network["assets"][1]
    assert (usdc["mintable"], usdc["faucet"], usdc["bonus_bps"], usdc["pricing"]) == (
        False,
        "https://faucet.circle.com",
        0,
        "stablecoin",
    )
    # Cached: the service's config is read once.
    console.handle("GET", "/api/assets", WEBSITE, b"")
    assert [r.url.path for r in service.requests].count("/v1/config") == 1


def test_a_second_chain_appears_once_the_service_serves_it(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    service.assets.append(_config_asset("pha", 84532, BASE_TOKEN))
    service.assets.append(_config_asset("usdt", 84532, BASE_USDT, "stablecoin"))
    response = console.handle("GET", "/api/assets", WEBSITE, b"")
    networks = json.loads(response.body)["networks"]
    # In the config's order, each with its own explorer and gas faucet.
    assert [(n["chain_id"], n["name"]) for n in networks] == [
        (11155111, "Sepolia testnet"),
        (84532, "Base Sepolia testnet"),
    ]
    base = networks[1]
    assert base["explorer"] == "https://sepolia.basescan.org"
    assert base["faucet"] == "https://docs.base.org/get-started/get-funds#testnet-base-sepolia"
    # Test USDT mints through its faucet contract (Aave's on staging), from the visitor's wallet.
    assert [
        (a["asset"], a["mintable"], a["minter"], a["faucet"], a["bonus_bps"])
        for a in base["assets"]
    ] == [
        ("pha", True, None, None, 1000),
        ("usdt", True, BASE_FAUCET, None, 0),
    ]
    cookie = _account(console)
    status, _ = _post(
        console, cookie, "quotes", {"amount": 2500, "chain_id": 84532, "asset": "pha"}
    )
    assert status == HTTPStatus.OK
    assert json.loads(service.requests[-1].content)["chain_id"] == 84532
    _, account = _get(console, cookie, "account")
    assert account["payments"][0]["chain_id"] == 84532


def test_a_quote_is_for_an_offered_network_and_token_and_returns_its_locked_rate(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    for pair in (
        {"chain_id": 1, "asset": "pha"},  # the service's, on a chain without the product's pins
        {"chain_id": 84532, "asset": "pha"},  # the product's, not served there yet
        {"chain_id": 11155111, "asset": "dai"},  # no such token
        {"chain_id": "11155111", "asset": "pha"},
        {"asset": "pha"},
        {"chain_id": 11155111},
    ):
        status, body = _post(console, cookie, "quotes", {"amount": 2500, **pair})
        assert (status, body) == (HTTPStatus.BAD_REQUEST, {"code": "asset_invalid"})
    assert "/v1/quotes" not in [r.url.path for r in service.requests]
    request = {"amount": 2500, "chain_id": 11155111, "asset": "usdc"}
    status, body = _post(console, cookie, "quotes", request)
    assert status == HTTPStatus.OK
    sent = json.loads(service.requests[-1].content)
    assert (sent["chain_id"], sent["asset"]) == (11155111, "usdc")
    assert (body["chain_id"], body["asset"], body["exchange_rate"], body["expires_at"]) == (
        11155111,
        "usdc",
        "25.00000000",
        NOW + 900,
    )
    assert (body["amount"], body["amount_atomic"]) == (2500, "100")
    # The unpaid quote's row carries its asset and rate.
    _, account = _get(console, cookie, "account")
    [row] = account["payments"]
    assert (row["asset"], row["exchange_rate"]) == ("usdc", "25.00000000")


def test_an_expired_quote_without_payment_fails_at_the_transfer(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    _create_quote(console, cookie)
    service.quote = {**service.quote, "expires_at": NOW - 1}
    steps = _steps(console, cookie)
    assert steps["sent"] == "failed"
    assert steps["received"] == "upcoming"


def test_a_rejected_deposit_fails_at_the_credit(demo: tuple[DemoConsole, Service]) -> None:
    console, service = demo
    cookie = _account(console)
    _create_quote(console, cookie)
    customer = _customer(cookie)
    service.deposits = [
        _deposit(customer, status="rejected", rejection_reason="sanctioned", amount=None)
    ]
    steps = _steps(console, cookie)
    assert steps["sent"] == "complete"
    assert steps["received"] == "complete"
    assert steps["credited"] == "failed"
    assert steps["webhook_received"] == "upcoming"


def test_a_credited_deposit_waits_for_the_webhook_finality_and_the_sweep(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    _create_quote(console, cookie)
    service.deposits = [_deposit(_customer(cookie), final=False, final_at=None)]
    status, view = _get(console, cookie, f"quotes/{QUOTE}")
    assert status == HTTPStatus.OK
    steps = {step["key"]: step for step in view["steps"]}
    assert steps["credited"]["state"] == "complete"
    assert steps["webhook_received"]["state"] == "current"
    assert steps["final"]["state"] == "current"
    assert steps["swept"]["state"] == "upcoming"
    # Real times: the block's timestamp from the product's RPC, the service's valuation time.
    assert view["sent"]["at"] == NOW - 12
    assert steps["credited"]["at"] == NOW
    service.deposits = [_deposit(_customer(cookie), final=True)]
    steps = {step["key"]: step for step in _get(console, cookie, f"quotes/{QUOTE}")[1]["steps"]}
    assert steps["final"]["state"] == "complete"
    # The deposit's `final_at`, set by the service's finality watch.
    assert steps["final"]["at"] == NOW + 780
    assert steps["swept"]["state"] == "current"


def test_the_ledger_view_applies_the_snapshot_rule(demo: tuple[DemoConsole, Service]) -> None:
    console, service = demo
    cookie = _account(console)
    customer = _customer(cookie)
    service.deposits = [_deposit(customer, amount_refunded=625, amount_refunded_atomic="25")]
    status, view = _get(console, cookie, f"deposits/{DEPOSIT}")
    assert status == HTTPStatus.OK
    assert view["ledger"]["nets_to"] == 1875
    service.deposits = [_deposit(customer, status="reversed", amount_reversed=2500)]
    _, view = _get(console, cookie, f"deposits/{DEPOSIT}")
    assert view["ledger"]["nets_to"] == 0
    assert "reversed" in {step["key"] for step in view["steps"]}
    service.deposits = [_deposit(customer, status="rejected", amount=None)]
    _, view = _get(console, cookie, f"deposits/{DEPOSIT}")
    assert view["ledger"]["nets_to"] == 0


def test_the_deposit_address_is_shown_only_when_the_pins_derive_it(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    status, body = _post(console, cookie, "deposit_address", {})
    assert status == HTTPStatus.OK
    assert body["verified"] is True
    assert body["client_secret"].startswith(ADDRESS_ID)
    assert body["deposit_address"]["networks"][0]["chain_id"] == 11155111
    assert body["deposit_address"]["metadata"] == {"workspace": _customer(cookie)}
    status, body = _get(console, cookie, "deposit_address")
    assert status == HTTPStatus.OK
    assert body["deposit_address"]["id"] == ADDRESS_ID
    # A compromised service naming another address: the SDK refuses it, nothing is shown.
    service.wrong_address = "0x" + "99" * 20
    status, body = _post(console, cookie, "deposit_address", {})
    assert status == HTTPStatus.BAD_GATEWAY
    assert body == {"code": "address_not_derivable"}


def test_refunds_of_own_deposits_show_the_transfer_to_make(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    service.deposits = [_deposit("someone-else")]
    refund = {"deposit": DEPOSIT, "amount_atomic": "40", "destination_address": "0x" + "33" * 20}
    status, _ = _post(console, cookie, "refunds", refund)
    assert status == HTTPStatus.NOT_FOUND
    assert not service.refunds
    service.deposits = [_deposit(_customer(cookie))]
    status, body = _post(console, cookie, "refunds", refund)
    assert status == HTTPStatus.OK
    transfer = body["refund"]["transfer"]
    assert transfer["from"] == TREASURY
    assert transfer["token"] == "0x" + "44" * 20
    assert transfer["data"] == "0xa9059cbb" + ("33" * 20).rjust(64, "0") + "28".rjust(64, "0")
    for bad in (
        {"transaction_hash": "0x12"},
        {"transaction_hash": "0x" + "ab" * 32, "receipt_log_index": -1},
    ):
        status, body = _post(console, cookie, f"refunds/{REFUND}/mark_paid", bad)
        assert status == HTTPStatus.BAD_REQUEST
    status, body = _post(
        console, cookie, f"refunds/{REFUND}/mark_paid", {"transaction_hash": "0x" + "AB" * 32}
    )
    assert status == HTTPStatus.OK
    assert body["refund"]["transaction_hash"] == "0x" + "ab" * 32
    assert body["refund"]["transfer"] is None
    # Another browser can neither mark nor cancel it.
    other = _account(console)
    status, _ = _post(console, other, f"refunds/{REFUND}/cancel", {})
    assert status == HTTPStatus.NOT_FOUND
    status, body = _post(console, cookie, f"refunds/{REFUND}/cancel", {})
    assert (status, body["refund"]["status"]) == (HTTPStatus.OK, "canceled")


def test_the_sweep_is_built_only_from_forwarders_the_pins_derive(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    salt = quote_salt(ACCOUNT, "acct", QUOTE)
    good = {
        "id": "fwd_" + "01" * 16,
        "object": "forwarder",
        "livemode": False,
        "chain_id": 11155111,
        "address": forwarder_address(CONFIG.factory, CONFIG.implementation, TREASURY, salt),
        "factory": CONFIG.factory,
        "salt": "0x" + salt.hex(),
        "treasury": TREASURY,
    }
    # A forwarder over another treasury, or at an address its salt does not give, is refused.
    other_treasury = {**good, "id": "fwd_" + "02" * 16, "treasury": "0x" + "dd" * 20}
    wrong_address = {**good, "id": "fwd_" + "03" * 16, "address": "0x" + "ee" * 20}
    service.forwarders = [good, other_treasury, wrong_address]
    status, view = _get(console, cookie, "sweeps")
    assert status == HTTPStatus.OK
    # One group per network and token; only PHA on Sepolia has a final unswept balance.
    assert [(g["chain_id"], g["symbol"]) for g in view["groups"]] == [
        (11155111, "PHA"),
        (11155111, "USDC"),
    ]
    group, usdc = view["groups"]
    assert (group["sweepable_forwarders"], group["refused_forwarders"]) == (1, 2)
    assert group["final_unswept_atomic"] == "200"
    [flush] = group["flush"]
    assert flush["to"].lower() == CONFIG.factory.lower()
    assert flush["data"].startswith("0x")
    assert salt.hex() in flush["data"]
    assert group["safe_batch"]["meta"]["createdFromSafeAddress"].lower() == TREASURY
    assert group["safe_batch"]["transactions"] == [flush]
    assert (usdc["unswept_atomic"], usdc["flush"], usdc["safe_batch"]) == ("0", [], None)
    forwarder_reads = [r for r in service.requests if r.url.path == "/v1/forwarders"]
    assert [r.url.params["sweepable"] for r in forwarder_reads] == [TOKEN]


def test_stale_sweeps_return_the_cached_view_and_start_one_refresh(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    now = [NOW]
    console._clock = lambda: now[0]
    cookie = _account(console)
    status, first = _get(console, cookie, "sweeps")
    assert status == HTTPStatus.OK
    service.sweeps_block = True
    now[0] += 11
    status, cached = _get(console, cookie, "sweeps")
    assert status == HTTPStatus.OK
    assert cached == first
    assert service.sweeps_started.wait(1)
    # A second stale request observes the in-flight refresh and does not start another one.
    _, still_cached = _get(console, cookie, "sweeps")
    assert still_cached == first
    service.sweeps_release.set()
    deadline = time.monotonic() + 1
    while console._sweeps_refreshing and time.monotonic() < deadline:
        time.sleep(0.01)
    assert not console._sweeps_refreshing
    assert [r.url.path for r in service.requests].count("/v1/balance") == 2


def test_one_unavailable_sweep_group_does_not_hide_the_others(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    service.fail_sweeps.add(TOKEN.lower())
    cookie = _account(console)
    status, view = _get(console, cookie, "sweeps")
    assert status == HTTPStatus.OK
    groups = {group["asset"]: group for group in view["groups"]}
    assert groups["pha"]["unavailable"] is True
    assert groups["pha"]["unswept_atomic"] == "0"
    assert groups["usdc"]["unavailable"] is False


def test_requests_need_the_cookie_and_posts_need_json(demo: tuple[DemoConsole, Service]) -> None:
    console, _ = demo
    no_cookie = console.handle("GET", f"/api/quotes/{QUOTE}", {}, b"")
    assert no_cookie.status == HTTPStatus.UNAUTHORIZED
    forged = console.handle("GET", "/api/account", {"Cookie": "demo_account=admin"}, b"")
    assert "set-cookie" in forged.headers
    cookie = _account(console)
    form = {"Cookie": cookie, "Content-Type": "application/x-www-form-urlencoded"}
    for path in ("quotes", "deposit_address", "refunds"):
        post = console.handle("POST", f"/api/{path}", form, b"amount=2500")
        assert post.status == HTTPStatus.UNSUPPORTED_MEDIA_TYPE
    # Another browser's deposit is not found.
    status, _ = _get(console, cookie, f"deposits/{DEPOSIT}")
    assert status == HTTPStatus.NOT_FOUND


def test_the_website_origin_reads_the_api_with_credentials(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, _ = demo
    cors = {
        "access-control-allow-origin": "https://acme.example",
        "access-control-allow-credentials": "true",
        "vary": "Origin",
    }
    cookie = _account(console)
    ok = console.handle("GET", "/api/account", {**WEBSITE, "Cookie": cookie}, b"")
    assert ok.status == HTTPStatus.OK
    assert {key: ok.headers[key] for key in cors} == cors
    # Errors too, so the page can read their codes.
    for response in (
        console.handle("GET", f"/api/quotes/{QUOTE}", WEBSITE, b""),
        console.handle("GET", "/api/unknown", {**WEBSITE, "Cookie": cookie}, b""),
        console.handle("POST", "/api/quotes", {**WEBSITE, "Cookie": cookie}, b"amount=1"),
    ):
        assert response.status >= HTTPStatus.BAD_REQUEST
        assert {key: response.headers[key] for key in cors} == cors


def test_answers_the_preflight_of_the_website_only(demo: tuple[DemoConsole, Service]) -> None:
    console, _ = demo
    request = {
        **WEBSITE,
        "Access-Control-Request-Method": "POST",
        "Access-Control-Request-Headers": "content-type",
    }
    preflight = console.handle("OPTIONS", "/api/quotes", request, b"")
    assert preflight.status == HTTPStatus.NO_CONTENT
    assert preflight.body == b""
    assert preflight.headers == {
        "access-control-allow-origin": "https://acme.example",
        "access-control-allow-credentials": "true",
        "access-control-allow-methods": "GET, POST",
        "access-control-allow-headers": "content-type",
        "access-control-max-age": "600",
        "vary": "Origin",
    }
    for origin in (
        "https://evil.example",
        "http://acme.example",
        "https://acme.example.evil",
        "null",
    ):
        other = console.handle("OPTIONS", "/api/quotes", {**request, "Origin": origin}, b"")
        assert other.headers == {"vary": "Origin"}


def test_other_origins_get_no_cors_headers(demo: tuple[DemoConsole, Service]) -> None:
    console, _ = demo
    for headers in ({"Origin": "https://evil.example"}, {"Origin": "null"}, {}):
        response = console.handle("GET", "/api/account", headers, b"")
        assert not any(key.startswith("access-control-") for key in response.headers)
        assert response.headers["vary"] == "Origin"
    assert console.cors("https://evil.example") == {"vary": "Origin"}


def test_serves_only_the_api(demo: tuple[DemoConsole, Service]) -> None:
    console, _ = demo
    assert console.handles("/api/account")
    # The website is on Cloudflare: the API's origin serves no page and no asset.
    for path in ["/", "/index.html", "/assets/index-0a1b2c.js", "/webhooks", "/healthz"]:
        assert not console.handles(path)
        assert console.handle("GET", path, WEBSITE, b"").status == HTTPStatus.NOT_FOUND
    for path in ["/api/../index.html", "/api/unknown", "/api/assets/index-0a1b2c.js"]:
        assert console.handle("GET", path, {}, b"").status in (
            HTTPStatus.NOT_FOUND,
            HTTPStatus.UNAUTHORIZED,
        )


def test_the_config_requires_the_website_origin() -> None:
    for origin in (
        "https://pay.phala.com/",
        "https://pay.phala.com/app",
        "http://pay.phala.com",
        "https://Pay.phala.com",
        "https://*.phala.com",
        "*",
    ):
        with pytest.raises(ValueError, match="web_origin"):
            replace(CONFIG, web_origin=origin)
    for origin in ("https://pay.phala.com", "https://pay.phala.com:8443", "http://127.0.0.1:4173"):
        assert replace(CONFIG, web_origin=origin).web_origin == origin
    with pytest.raises(ValueError, match="web_origin"):
        DemoConsole(replace(CONFIG, web_origin=None), ProductLedger())


def test_the_config_requires_an_account_id() -> None:
    with pytest.raises(ValueError, match="acct_"):
        replace(CONFIG, account="phala-cloud")


def test_the_ledger_lines_show_the_bonus_apart_from_the_credit(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    customer = _customer(cookie)
    deposit = _deposit(customer, final=False)
    service.deposits = [deposit]
    key = Ed25519PrivateKey.generate()
    fulfillment = Fulfillment(
        console.config, console.ledger, lambda: PinnedKeys(False, [key.public_key()])
    )
    event_id = credited_event_id(DEPOSIT)
    body = json.dumps(
        {
            "id": event_id,
            "object": "event",
            "account": ACCOUNT,
            "livemode": False,
            "type": "deposit.credited",
            "created": NOW,
            "actor": "system",
            "request": None,
            "data": {"object": deposit},
        }
    ).encode()
    answer = fulfillment.handle(sign_webhook(key, event_id, int(time.time()), body), body)
    assert answer.status == HTTPStatus.NO_CONTENT
    _, account = _get(console, cookie, "account")
    assert account["balance"] == 2750
    assert {(line["kind"], line["reason"], line["amount"]) for line in account["ledger"]} == {
        ("credit", "deposit.credited", 2500),
        ("bonus", "PHA bonus +10%", 250),
    }
    assert account["payments"][0]["bonus"] == 250
    _, view = _get(console, cookie, f"deposits/{DEPOSIT}")
    assert view["ledger"]["product"]["bonus"] == 250
    # The rates are the service's decimals, for the page to format.
    credited = next(step for step in view["steps"] if step["key"] == "credited")
    assert {"label": "Rate", "value": "25.00000000", "kind": "rate", "unit": "PHA"} in credited[
        "details"
    ]


def test_the_config_validates_its_chains() -> None:
    sepolia = CONFIG.chain(11155111)
    # Addresses are normalised to their checksum; a wrong mixed-case checksum is refused.
    assert sepolia.treasury == "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"
    assert CONFIG.factory == "0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa"
    with pytest.raises(ValueError, match="checksum"):
        replace(sepolia, treasury="0xCCcCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC")
    for url in (
        "http://rpc.example.org",
        "https://sepolia.infura.io/v3/0123456789abcdef0123456789abcdef",
        "https://user:secret@rpc.example.org",
        "https://rpc.example.org/?key=abc",
        "wss://rpc.example.org",
    ):
        with pytest.raises(ValueError, match="rpc_url"):
            replace(sepolia, rpc_url=url)
    # Local chains: the loopback host, or a compose service's name.
    for url in ("http://127.0.0.1:8545", "http://anvil:8545"):
        assert replace(sepolia, rpc_url=url).rpc_url == url
    with pytest.raises(ValueError, match="repeat"):
        replace(CONFIG, chains=(sepolia, sepolia))
    with pytest.raises(ValueError, match="at least one"):
        replace(CONFIG, chains=())
    with pytest.raises(ValueError, match="distinct"):
        replace(sepolia, test_tokens=(MintableToken("PHA", TOKEN), MintableToken("PHA", TOKEN)))
    # The deposit driver mints the first test token itself.
    with pytest.raises(ValueError, match="must mint itself"):
        replace(sepolia, test_tokens=(MintableToken("USDT", TOKEN, minter=BASE_FAUCET),))
    bonuses: list[dict[str, Any]] = [
        {"PHA": 1000},
        {"pha": 0},
        {"pha": 10_001},
        {"pha": True},
        {"pha": 10.5},
    ]
    for bonus in bonuses:
        with pytest.raises(ValueError, match="bonus_bps"):
            replace(CONFIG, bonus_bps=bonus)
    with pytest.raises(ValueError, match="not configured"):
        CONFIG.chain(1)


def test_account_stops_at_one_full_page(demo: tuple[DemoConsole, Service]) -> None:
    console, service = demo
    requests: list[httpx.Request] = []

    def paginated(request: httpx.Request) -> httpx.Response:
        if request.url.path != "/v1/deposits":
            return service(request)
        requests.append(request)
        assert request.url.params["limit"] == "50"
        assert "starting_after" not in request.url.params
        deposits = [_deposit(id=f"dep_{index:032x}") for index in range(50)]
        return httpx.Response(200, json={**_list("/v1/deposits", deposits), "has_more": True})

    console.recorder._inner = httpx.MockTransport(paginated)
    _account(console)
    assert len(requests) == 1


def _forwarder_for(deposit: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": "fwd_" + "33" * 16,
        "object": "forwarder",
        "livemode": False,
        "chain_id": deposit["chain_id"],
        "address": deposit["address"],
        "factory": CONFIG.factory,
        "salt": "0x" + "44" * 32,
        "treasury": TREASURY,
        "quote": deposit.get("quote"),
        "deposit_address": deposit.get("deposit_address"),
    }


def _sweep(deposit: dict[str, Any], block_number: int) -> dict[str, Any]:
    return {
        "id": f"sw_{block_number:032x}",
        "object": "sweep",
        "livemode": False,
        "chain_id": deposit["chain_id"],
        "address": deposit["address"],
        "token": TOKEN,
        "treasury": TREASURY,
        "amount_atomic": "100",
        "tx_hash": "0x" + "22" * 32,
        "block_number": block_number,
        "forwarder": "fwd_" + "33" * 16,
        "log_index": 0,
        "created": NOW,
    }


@pytest.mark.parametrize("origin", ["quote", "deposit_address"])
def test_sweep_lookup_filters_by_resolved_forwarder_id(
    demo: tuple[DemoConsole, Service], origin: str
) -> None:
    console, service = demo
    deposit = _deposit()
    if origin == "deposit_address":
        deposit.update(quote=None, deposit_address=ADDRESS_ID)
    service.forwarders = [_forwarder_for(deposit)]
    service.sweeps = [_sweep(deposit, deposit["block_number"] + 1)]
    result = console._sweep_of(deposit)
    assert result is not None
    assert result["address"] == deposit["address"]
    [lookup, sweeps] = service.requests
    assert lookup.url.params[origin] == deposit[origin]
    assert lookup.url.params["chain_id"] == str(deposit["chain_id"])
    assert sweeps.url.params["forwarder"] == service.forwarders[0]["id"]
    assert sweeps.url.params["limit"] == "100"


def test_sweep_lookup_returns_the_earliest_sweep_after_a_reused_address_deposit(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    deposit = _deposit(quote=None, deposit_address=ADDRESS_ID, block_number=10)
    service.forwarders = [_forwarder_for(deposit)]
    service.sweeps = [_sweep(deposit, block) for block in (40, 30, 20, 5)]

    def paginated(request: httpx.Request) -> httpx.Response:
        assert "starting_after" not in request.url.params
        response = service(request)
        if request.url.path == "/v1/sweeps":
            return httpx.Response(200, json={**response.json(), "has_more": True})
        return response

    console.recorder._inner = httpx.MockTransport(paginated)
    result = console._sweep_of(deposit)
    assert result is not None
    assert result["block_number"] == 20


def test_sweeps_service_rejects_address_filters(demo: tuple[DemoConsole, Service]) -> None:
    console, _ = demo
    with pytest.raises(ApiError, match="invalid_forwarder"):
        next(console._service().list_sweeps(forwarder=_deposit()["address"]))


@pytest.mark.parametrize("resolved", [True, False])
@pytest.mark.parametrize("reaches_older", [True, False])
def test_sweep_lookup_reads_at_most_one_full_page(
    demo: tuple[DemoConsole, Service], resolved: bool, reaches_older: bool
) -> None:
    console, service = demo
    deposit = _deposit(block_number=1)
    service.forwarders = [_forwarder_for(deposit)] if resolved else []
    requests: list[httpx.Request] = []

    def paginated(request: httpx.Request) -> httpx.Response:
        if request.url.path != "/v1/sweeps":
            return service(request)
        requests.append(request)
        assert "starting_after" not in request.url.params
        assert request.url.params["limit"] == "100"
        if resolved:
            assert request.url.params["forwarder"].startswith("fwd_")
        else:
            assert "forwarder" not in request.url.params
        sweeps = [_sweep(deposit, block) for block in range(200, 100, -1)]
        if reaches_older:
            sweeps[-1] = _sweep(deposit, 0)
        return httpx.Response(200, json={**_list("/v1/sweeps", sweeps), "has_more": True})

    console.recorder._inner = httpx.MockTransport(paginated)
    result = console._sweep_of(deposit)
    if not resolved and not reaches_older:
        assert result is None
    else:
        assert result is not None
        assert result["block_number"] == (102 if reaches_older else 101)
    assert len(requests) == 1


@pytest.mark.parametrize("kind", ["trust", "networks"])
def test_caches_fetch_single_flight_and_serve_stale_on_failure(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch, kind: str
) -> None:
    console, _ = demo
    now = [NOW]
    console._clock = lambda: now[0]
    value: Any = (
        {"attestation": {"binding_verified": True}, "tls_evidence": {"app_id": "test"}}
        if kind == "trust"
        else [{"chain_id": 11155111}]
    )
    started, release = threading.Event(), threading.Event()
    calls: list[int] = []
    fail = [False]

    def fetch(*args: Any) -> Any:
        calls.append(1)
        started.set()
        assert release.wait(2)
        if fail[0]:
            raise TransportError("timeout")
        return value

    method = console._trust_view if kind == "trust" else console._payable_networks
    monkeypatch.setattr(
        console, "_fetch_trust_view" if kind == "trust" else "_fetch_payable_networks", fetch
    )
    with ThreadPoolExecutor(max_workers=5) as workers:
        futures = [workers.submit(method) for _ in range(5)]
        try:
            assert started.wait(1)
        finally:
            release.set()
        assert [future.result(timeout=2) for future in futures] == [value] * 5
    assert len(calls) == 1
    now[0] += 301
    fail[0] = True
    assert method() == value
    assert method() == value
    assert len(calls) == 2
    now[0] += 31
    fail[0] = False
    assert method() == value
    assert len(calls) == 3


@pytest.mark.parametrize("kind", ["trust", "networks"])
def test_cold_cache_failures_retry_after_thirty_seconds(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch, kind: str
) -> None:
    console, _ = demo
    now = [NOW]
    console._clock = lambda: now[0]
    calls: list[int] = []

    def fetch(*args: Any) -> Any:
        calls.append(1)
        raise TransportError("timeout")

    monkeypatch.setattr(
        console, "_fetch_trust_view" if kind == "trust" else "_fetch_payable_networks", fetch
    )
    for _ in range(2):
        response = console.handle(
            "GET", f"/api/{'trust' if kind == 'trust' else 'assets'}", {}, b""
        )
        assert response.status == HTTPStatus.SERVICE_UNAVAILABLE
    assert len(calls) == 1
    now[0] += 31
    assert (
        console.handle("GET", f"/api/{'trust' if kind == 'trust' else 'assets'}", {}, b"").status
        == 503
    )
    assert len(calls) == 2


def test_incomplete_trust_evidence_retries_after_thirty_seconds(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch
) -> None:
    console, _ = demo
    now = [NOW]
    console._clock = lambda: now[0]
    calls: list[int] = []

    def fetch() -> dict[str, Any]:
        calls.append(1)
        return {"attestation": {"binding_verified": False}, "tls_evidence": None}

    monkeypatch.setattr(console, "_fetch_trust_view", fetch)
    assert console._trust_view()["tls_evidence"] is None
    now[0] += 29
    console._trust_view()
    assert len(calls) == 1
    now[0] += 2
    console._trust_view()
    assert len(calls) == 2


def test_rpc_failures_are_cached_for_thirty_seconds(demo: tuple[DemoConsole, Service]) -> None:
    console, _ = demo
    now = [NOW]
    console._clock = lambda: now[0]
    calls: list[httpx.Request] = []
    failing = [True]

    def rpc(request: httpx.Request) -> httpx.Response:
        calls.append(request)
        assert request.extensions["timeout"]["read"] == 2
        if failing[0]:
            raise httpx.ReadTimeout("RPC timeout", request=request)
        return _rpc(request)

    console._http.close()
    console._http = httpx.Client(transport=httpx.MockTransport(rpc))
    tx_hash = "0x" + "11" * 32
    assert console._block_time(11155111, tx_hash) is None
    assert console._block_time(11155111, tx_hash) is None
    assert len(calls) == 1
    now[0] += 31
    failing[0] = False
    assert console._block_time(11155111, tx_hash) == {
        "tx_hash": tx_hash,
        "block_number": 1,
        "at": NOW - 12,
    }
    now[0] += 300
    assert console._block_time(11155111, tx_hash) is not None
    assert len(calls) == 3


def test_block_time_cache_evicts_the_least_recently_used_entry(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch
) -> None:
    console, _ = demo
    calls: list[str] = []

    def rpc(chain_id: int, method: str, *params: Any) -> dict[str, str]:
        if method == "eth_getTransactionReceipt":
            calls.append(params[0])
            return {"blockNumber": "0x1"}
        return {"timestamp": hex(NOW)}

    monkeypatch.setattr(console, "_rpc", rpc)
    hashes = [f"0x{index:064x}" for index in range(1025)]
    for tx_hash in hashes[:1024]:
        console._block_time(11155111, tx_hash)
    console._block_time(11155111, hashes[0])
    console._block_time(11155111, hashes[-1])
    console._block_time(11155111, hashes[0])
    assert len(calls) == 1025
    console._block_time(11155111, hashes[1])
    assert len(calls) == 1026
    assert len(console._block_times) == 1024


def test_cold_sweeps_timeout_keeps_one_build_running_with_its_own_deadline(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch
) -> None:
    console, service = demo
    cookie = _account(console)
    service.sweeps_block = True
    monkeypatch.setattr(demo_module, "SWEEPS_WAIT_SECONDS", 0.02)
    deadlines: list[float | None] = []
    build = console._build_sweeps_view

    def record_deadline(now: float) -> dict[str, Any]:
        deadlines.append(operation_deadline.get())
        return build(now)

    monkeypatch.setattr(console, "_build_sweeps_view", record_deadline)
    caller_deadline = time.monotonic() + 0.1
    token = operation_deadline.set(caller_deadline)
    try:
        status, body = _get(console, cookie, "sweeps")
        assert (status, body) == (HTTPStatus.SERVICE_UNAVAILABLE, {"code": "unavailable"})
        assert service.sweeps_started.is_set()
        assert console._sweeps_refreshing
        assert deadlines[0] is not None
        assert deadlines[0] > caller_deadline + 24
        assert _get(console, cookie, "sweeps")[0] == HTTPStatus.SERVICE_UNAVAILABLE
        assert len(deadlines) == 1
    finally:
        operation_deadline.reset(token)
        service.sweeps_release.set()
        assert console._sweeps_thread is not None
        console._sweeps_thread.join(timeout=2)
    assert console._sweeps is not None
    assert not console._sweeps_refreshing
    assert _get(console, cookie, "sweeps")[0] == HTTPStatus.OK
    console.close()


def test_sweeps_older_than_a_minute_return_immediately_with_stale_marker(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    now = [NOW]
    console._clock = lambda: now[0]
    cookie = _account(console)
    _, first = _get(console, cookie, "sweeps")
    now[0] += 61
    service.sweeps_block = True
    try:
        started = time.monotonic()
        status, cached = _get(console, cookie, "sweeps")
        assert time.monotonic() - started < 0.5
        assert status == HTTPStatus.OK
        assert cached == {**first, "stale": True}
        assert service.sweeps_started.wait(1)
        assert _get(console, cookie, "sweeps")[1] == cached
    finally:
        service.sweeps_release.set()
        console.close()


def test_fresh_sweeps_cache_does_not_start_background_builds(
    demo: tuple[DemoConsole, Service],
) -> None:
    console, service = demo
    cookie = _account(console)
    status, first = _get(console, cookie, "sweeps")
    assert status == HTTPStatus.OK
    assert console._sweeps_thread is not None
    console._sweeps_thread.join(timeout=2)
    for _ in range(5):
        assert _get(console, cookie, "sweeps") == (HTTPStatus.OK, first)
    assert not console._sweeps_refreshing
    assert [request.url.path for request in service.requests].count("/v1/balance") == 1


@pytest.mark.parametrize("kind", ["demo", "account"])
def test_product_closes_owned_http_threads(demo: tuple[DemoConsole, Service], kind: str) -> None:
    fixture_console, _ = demo
    assert not [thread for thread in threading.enumerate() if thread.name == "product-http"]
    if kind == "demo":
        console = DemoConsole(fixture_console.config, ProductLedger())
        try:
            console._service()
            console._sweeps_service()
            assert len([t for t in threading.enumerate() if t.name == "product-http"]) == 2
        finally:
            console.close()
        console.close()
    else:
        driver_key = Ed25519PrivateKey.from_private_bytes(bytes([7] * 32)).public_key()
        api = AccountApi(fixture_console.config, ProductLedger(), driver_key)
        try:
            api._service()
            assert len([t for t in threading.enumerate() if t.name == "product-http"]) == 1
        finally:
            api.close()
        api.close()
    assert not [thread for thread in threading.enumerate() if thread.name == "product-http"]


@pytest.mark.parametrize("kind", ["trust", "networks"])
@pytest.mark.parametrize("failure", ["transport", "api", "validation"])
def test_cached_failures_raise_fresh_exceptions_for_concurrent_readers(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch, kind: str, failure: str
) -> None:
    console, _ = demo
    original: TransportError | ApiError | ResponseValidationError
    if failure == "transport":
        original = TransportError("timeout", "service timeout")
    elif failure == "api":
        original = ApiError(503, "service_unavailable", "service failed", request_id="request-test")
    else:
        original = ResponseValidationError("malformed response", status_code=200, request_id="test")
    calls: list[int] = []

    def fetch(*args: Any) -> Any:
        calls.append(1)
        raise original

    monkeypatch.setattr(
        console, "_fetch_trust_view" if kind == "trust" else "_fetch_payable_networks", fetch
    )
    method = console._trust_view if kind == "trust" else console._payable_networks
    with pytest.raises(type(original)) as first:
        method()

    def read_failure() -> Exception:
        try:
            method()
        except (TransportError, ApiError, ResponseValidationError) as error:
            return error
        raise AssertionError("expected cached failure")

    with ThreadPoolExecutor(max_workers=5) as workers:
        errors = [
            future.result(timeout=2) for future in [workers.submit(read_failure) for _ in range(5)]
        ]
    assert len({id(error) for error in [first.value, *errors]}) == 6
    assert len(calls) == 1
    for error in errors:
        assert type(error) is type(original)
        assert str(error) == str(original)
        if isinstance(error, TransportError):
            assert error.code == "timeout"
        elif isinstance(error, ApiError):
            assert error.status_code == 503
            assert error.code == "service_unavailable"
            assert error.request_id == "request-test"
        elif isinstance(error, ResponseValidationError):
            assert error.status_code == 200
            assert error.request_id == "test"


@pytest.mark.parametrize("failure", ["transport", "http", "missing_key"])
def test_cold_sweeps_waiters_receive_distinct_refresh_exceptions(
    demo: tuple[DemoConsole, Service], monkeypatch: pytest.MonkeyPatch, failure: str
) -> None:
    console, _ = demo
    waiting = threading.Barrier(3)
    started, release = threading.Event(), threading.Event()
    pending_ids: list[int] = []
    error: TransportError | httpx.HTTPError | MissingProductKeyError
    if failure == "transport":
        error = TransportError("timeout", "refresh failed")
    elif failure == "http":
        error = httpx.HTTPError("refresh failed")
    else:
        error = MissingProductKeyError("refresh failed")

    class WaitingFuture(Future[dict[str, Any]]):
        def result(self, timeout: float | None = None) -> dict[str, Any]:
            pending_ids.append(id(self))
            waiting.wait(timeout=2)
            return super().result(timeout=timeout)

    def build(now: float) -> dict[str, Any]:
        started.set()
        assert release.wait(3)
        raise error

    monkeypatch.setattr(demo_module, "Future", WaitingFuture)
    monkeypatch.setattr(console, "_build_sweeps_view", build)
    with ThreadPoolExecutor(max_workers=2) as workers:
        futures = [workers.submit(console._sweeps_view) for _ in range(2)]
        try:
            assert started.wait(1)
            waiting.wait(timeout=2)
        finally:
            release.set()
        with pytest.raises(type(error)) as first:
            futures[0].result(timeout=3)
        with pytest.raises(type(error)) as second:
            futures[1].result(timeout=3)
    assert len(pending_ids) == 2
    assert len(set(pending_ids)) == 1
    assert first.value is not second.value
    assert first.value is not error
    assert second.value is not error
    assert str(first.value) == str(second.value) == str(error)
