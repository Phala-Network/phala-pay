from __future__ import annotations

import json
import uuid
from collections.abc import Callable

import httpx
import pytest

from topup_client import AuthenticatedClient
from topup_client.api.account import get_account
from topup_client.models import ErrorResponse, Payment
from topup_sdk import (
    AddressMismatchError,
    ApiError,
    TopupClient,
    UnpinnedTreasuryWarning,
    quote_address,
)

API_KEY = "ppay_sk_test_" + "A" * 43 + "000000"
LIVE_KEY = "ppay_rk_live_" + "A" * 43 + "000000"
ACCOUNT = "acct_" + "0a" * 16
NOW = 1_790_000_000
FACTORY = "0x" + "aa" * 20
IMPLEMENTATION = "0x" + "bb" * 20
TREASURY = "0x" + "cc" * 20
QUOTE_ID = "qt_" + "0c" * 16


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


def _quote(**fields: object) -> dict[str, object]:
    address = quote_address(
        FACTORY,
        IMPLEMENTATION,
        str(fields.get("treasury", TREASURY)),
        account=ACCOUNT,
        client_reference_id="ws 1",
        quote_id=QUOTE_ID,
    )
    return {
        "id": QUOTE_ID,
        "object": "quote",
        "livemode": False,
        "client_reference_id": "ws 1",
        "treasury": TREASURY,
        "metadata": {},
        "amount": 2500,
        "currency": "usd",
        "chain_id": 11155111,
        "asset": "pha",
        "amount_atomic": "100",
        "exchange_rate": "25.00000000",
        "address": address,
        "payment_uri": f"ethereum:0x{'22' * 20}@11155111/transfer?address={address}&uint256=100",
        "status": "open",
        "expires_at": NOW + 900,
        "created": NOW,
        "payment": None,
        "deposit": None,
        "terms": QUOTE_TERMS,
        **fields,
    }


class FakeService:
    """Checks each request's API key like the service and records it."""

    def __init__(self, respond: Callable[[httpx.Request, int], httpx.Response]) -> None:
        self.respond = respond
        self.requests: list[httpx.Request] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        if request.headers.get("authorization") not in (f"Bearer {API_KEY}", f"Bearer {LIVE_KEY}"):
            return _error(401, "api_key_invalid")
        if request.method == "GET" and request.url.raw_path == b"/v1/account":
            return httpx.Response(
                200,
                json={
                    "id": ACCOUNT,
                    "object": "account",
                    "livemode": False,
                    "name": "Acme",
                    "charges_enabled": False,
                    "paused_scopes": [],
                    "webhook_keys": [{"version": 1, "expires_at": None}],
                    "created": NOW,
                },
            )
        self.requests.append(request)
        return self.respond(request, len(self.requests))


def _error(status: int, code: str, **fields: str) -> httpx.Response:
    error_type = "api_error" if status >= 500 else "invalid_request_error"
    doc_url = f"https://phala-network.github.io/phala-pay/#section/Errors/{code}"
    body = {"type": error_type, "code": code, "message": code, "doc_url": doc_url, **fields}
    return httpx.Response(status, json={"error": body})


def _client(
    service: FakeService,
    *,
    pinned: bool = True,
    max_attempts: int = 4,
    treasuries: dict[int, str] | None = {11155111: TREASURY},  # noqa: B006 (never mutated)
    api_key: str = API_KEY,
    account: str | None = None,
) -> TopupClient:
    return TopupClient(
        "http://service.test:8080",
        api_key,
        account=account,
        forwarder=(FACTORY, IMPLEMENTATION) if pinned else None,
        treasuries=treasuries if pinned else None,
        transport=httpx.MockTransport(service),
        sleep=lambda _: None,
        max_attempts=max_attempts,
    )


def test_quotes_send_the_key_and_an_idempotency_key() -> None:
    service = FakeService(lambda request, _: httpx.Response(200, json=_quote()))
    with _client(service) as client:
        quote = client.create_quote("ws 1", 2500, chain_id=11155111, asset="pha")
        # The address check read the account id once.
        assert client.account_id() == ACCOUNT
    assert quote.id == QUOTE_ID
    request = service.requests[0]
    assert request.url.raw_path == b"/v1/quotes"
    assert json.loads(request.content) == {
        "client_reference_id": "ws 1",
        "amount": 2500,
        "currency": "usd",
        "chain_id": 11155111,
        "asset": "pha",
    }
    assert uuid.UUID(request.headers["idempotency-key"].strip('"'))
    assert request.headers["authorization"] == f"Bearer {API_KEY}"
    assert "signature" not in request.headers


def test_transient_failures_are_retried_with_one_idempotency_key() -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        if count == 1:
            return _error(503, "unavailable")
        if count == 2:
            return _error(409, "idempotency_key_in_use")
        return httpx.Response(200, json=_quote())

    service = FakeService(respond)
    with _client(service) as client:
        client.create_quote("ws 1", 2500, chain_id=11155111, asset="pha", idempotency_key="k-1")
    assert len(service.requests) == 3
    assert {request.headers["idempotency-key"] for request in service.requests} == {'"k-1"'}


def test_business_errors_are_raised_without_retry() -> None:
    service = FakeService(lambda request, _: _error(409, "exposure_cap_exceeded", param="amount"))
    with _client(service) as client, pytest.raises(ApiError) as raised:
        client.create_quote("ws 1", 2500, chain_id=11155111, asset="pha")
    error = raised.value
    assert (error.status_code, error.code, error.param) == (409, "exposure_cap_exceeded", "amount")
    assert error.error_type == "invalid_request_error"
    assert len(service.requests) == 1


def test_retries_stop_after_max_attempts() -> None:
    service = FakeService(lambda request, _: _error(503, "unavailable"))
    with _client(service, max_attempts=2) as client, pytest.raises(ApiError):
        client.get_quote(QUOTE_ID)
    assert len(service.requests) == 2


def test_open_quotes_must_have_the_derived_address() -> None:
    forged = _quote(address="0x" + "11" * 20)
    service = FakeService(lambda request, _: httpx.Response(200, json=forged))
    with _client(service) as client, pytest.raises(AddressMismatchError):
        client.get_quote(QUOTE_ID)
    # A closed quote's address is not shown, and without a pinned forwarder nothing is checked.
    closed = FakeService(
        lambda request, _: httpx.Response(200, json={**forged, "status": "expired"})
    )
    with _client(closed) as client:
        assert client.get_quote(QUOTE_ID).status == "expired"
    with _client(
        FakeService(lambda request, _: httpx.Response(200, json=forged)), pinned=False
    ) as client:
        assert client.get_quote(QUOTE_ID).address == forged["address"]


def test_only_secret_and_restricted_keys_are_accepted() -> None:
    for key in ["sk_test_123", "rk_live_" + "A" * 49, "ppay_pk_test_" + "A" * 49, "acme/v1"]:
        with pytest.raises(ValueError, match="restricted key"):
            TopupClient("http://service.test", key)
    assert TopupClient("https://service.test", LIVE_KEY).livemode
    assert not TopupClient("https://service.test", API_KEY).livemode


def _live_quote(treasury: str = TREASURY) -> dict[str, object]:
    return {**_quote(treasury=treasury), "livemode": True, "chain_id": 1}


def test_live_address_checks_fail_closed_without_every_pin() -> None:
    service = FakeService(lambda request, _: httpx.Response(200, json=_live_quote()))
    for pins in (
        {"pinned": False, "account": ACCOUNT},
        {"treasuries": None, "account": ACCOUNT},
        # The account is pinned too: a live client never takes it from the service.
        {"treasuries": {1: TREASURY}},
    ):
        with (
            _client(service, api_key=LIVE_KEY, **pins) as client,
            pytest.raises(AddressMismatchError, match="live mode requires"),
        ):
            client.get_quote(QUOTE_ID)
    with _client(service, api_key=LIVE_KEY, treasuries={1: TREASURY}, account=ACCOUNT) as client:
        assert client.get_quote(QUOTE_ID).address == _live_quote()["address"]
    assert all(request.url.raw_path != b"/v1/account" for request in service.requests)


def test_a_spoofed_treasury_with_its_valid_address_is_refused_in_live_mode() -> None:
    # A compromised service returns an attacker's treasury and the CREATE2 address that really
    # derives from it: recomputing over the response's treasury would pass.
    attacker = "0x" + "ee" * 20
    spoofed = _live_quote(treasury=attacker)
    assert spoofed["address"] == quote_address(
        FACTORY,
        IMPLEMENTATION,
        attacker,
        account=ACCOUNT,
        client_reference_id="ws 1",
        quote_id=QUOTE_ID,
    )
    service = FakeService(lambda request, _: httpx.Response(200, json=spoofed))
    with (
        _client(service, api_key=LIVE_KEY, treasuries={1: TREASURY}, account=ACCOUNT) as client,
        pytest.raises(AddressMismatchError, match="not the pinned one"),
    ):
        client.get_quote(QUOTE_ID)
    # The response naming the pinned treasury while showing the attacker's address fails too.
    lying = {**spoofed, "treasury": TREASURY}
    service = FakeService(lambda request, _: httpx.Response(200, json=lying))
    with (
        _client(service, api_key=LIVE_KEY, treasuries={1: TREASURY}, account=ACCOUNT) as client,
        pytest.raises(AddressMismatchError, match="cannot derive"),
    ):
        client.get_quote(QUOTE_ID)


def test_restricted_keys_are_created_with_their_permissions() -> None:
    key = {
        "id": "key_" + "01" * 16,
        "object": "api_key",
        "livemode": False,
        "type": "restricted",
        "name": "checkout",
        "permissions": ["quotes.read", "quotes.write"],
        "secret": "ppay_rk_test_" + "B" * 49,
        "redacted": "ppay_rk_test_…BBBB",
        "status": "active",
        "created": NOW,
        "expires_at": None,
        "last_used": None,
    }
    service = FakeService(lambda request, _: httpx.Response(200, json=key))
    with _client(service) as client:
        created = client.create_api_key(name="checkout", permissions=["quotes.write"])
    assert created.type_ == "restricted"
    assert created.permissions == ["quotes.read", "quotes.write"]
    assert json.loads(service.requests[0].content) == {
        "name": "checkout",
        "type": "restricted",
        "permissions": ["quotes.write"],
    }


def _deposit(index: int) -> dict[str, object]:
    return {
        "id": f"dep_{index:032x}",
        "object": "deposit",
        "livemode": False,
        "client_reference_id": "ws 1",
        "quote": QUOTE_ID,
        "deposit_address": None,
        "status": "credited",
        "final": True,
        "swept": False,
        "metadata": {},
        "rejection_reason": None,
        "chain_id": 11155111,
        "asset": "pha",
        "asset_contract": "0x" + "22" * 20,
        "amount_atomic": "1",
        "amount": 1,
        "currency": "usd",
        "exchange_rate": "1.00000000",
        "price_source": "quote",
        "valued_at": NOW,
        "address": "0x" + "11" * 20,
        "from_address": "0x" + "33" * 20,
        "tx_hash": "0x" + "ab" * 32,
        "receipt_log_index": index,
        "revision": 0,
        "log_index": index,
        "block_number": 1,
        "block_hash": "0x" + "cd" * 32,
        "block_time": NOW,
        "amount_refunded_atomic": "0",
        "refunded": False,
        "amount_refunded": 0,
        "amount_reversed": 0,
        "created": NOW,
    }


def test_list_deposits_follows_stripe_cursors() -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        params = request.url.params
        assert request.url.raw_path.startswith(b"/v1/deposits")
        assert params["client_reference_id"] == "ws 1"
        assert params["status"] == "credited"
        if count == 1:
            assert "starting_after" not in params
            return httpx.Response(
                200,
                json={
                    "object": "list",
                    "url": "/v1/deposits",
                    "has_more": True,
                    "data": [_deposit(1)],
                },
            )
        assert params["starting_after"] == f"dep_{1:032x}"
        return httpx.Response(
            200,
            json={
                "object": "list",
                "url": "/v1/deposits",
                "has_more": False,
                "data": [_deposit(2)],
            },
        )

    service = FakeService(respond)
    with _client(service) as client:
        deposits = list(client.list_deposits(client_reference_id="ws 1", status="credited"))
    assert [deposit.log_index for deposit in deposits] == [1, 2]
    assert deposits[0].quote == QUOTE_ID


REFUND_ID = "re_" + "0e" * 16
REFUND = {
    "id": REFUND_ID,
    "object": "refund",
    "livemode": False,
    "metadata": {},
    "deposit": f"dep_{1:032x}",
    "amount_atomic": "1",
    "destination_address": "0x" + "44" * 20,
    "treasury": "0x" + "7e" * 20,
    "status": "pending",
    "failure_reason": None,
    "transaction_hash": None,
    "receipt_log_index": None,
    "created": NOW,
}


def test_refunds_send_an_idempotency_key_and_default_to_the_remainder() -> None:
    service = FakeService(lambda request, _: httpx.Response(200, json=REFUND))
    with _client(service) as client:
        created = client.create_refund(f"dep_{1:032x}", "0x" + "44" * 20)
    assert created.status == "pending"
    assert created.treasury == "0x" + "7e" * 20
    request = service.requests[0]
    assert request.url.raw_path == b"/v1/refunds"
    assert json.loads(request.content) == {
        "deposit": f"dep_{1:032x}",
        "destination_address": "0x" + "44" * 20,
    }
    assert request.headers["idempotency-key"].startswith('"')


def test_refunds_are_marked_paid_and_canceled_by_id() -> None:
    marked = {**REFUND, "transaction_hash": "0x" + "dd" * 32, "receipt_log_index": 1}
    service = FakeService(lambda request, _: httpx.Response(200, json=marked))
    with _client(service) as client:
        refund = client.mark_refund_paid(REFUND_ID, "0x" + "dd" * 32, receipt_log_index=1)
        client.mark_refund_paid(REFUND_ID, "0x" + "dd" * 32)
        client.cancel_refund(REFUND_ID)
    assert refund.transaction_hash == "0x" + "dd" * 32
    assert refund.receipt_log_index == 1
    paid, unnamed, canceled = service.requests
    assert paid.url.raw_path == f"/v1/refunds/{REFUND_ID}/mark_paid".encode()
    assert json.loads(paid.content) == {
        "transaction_hash": "0x" + "dd" * 32,
        "receipt_log_index": 1,
    }
    assert json.loads(unnamed.content) == {"transaction_hash": "0x" + "dd" * 32}
    assert canceled.method == "POST"
    assert canceled.url.raw_path == f"/v1/refunds/{REFUND_ID}/cancel".encode()


def test_quote_payment_is_optional_and_parsed() -> None:
    payment = {
        "status": "seen",
        "chain_id": 11155111,
        "asset": "pha",
        "tx_hash": "0x" + "ab" * 32,
        "amount_atomic": "100",
        "confirmations": 1,
        "estimated_final_at": NOW + 900,
        "matches_quote": True,
        "deposit": "dep_" + "08" * 16,
    }

    def respond(_: httpx.Request, count: int) -> httpx.Response:
        return httpx.Response(200, json=_quote() if count == 1 else _quote(payment=payment))

    with _client(FakeService(respond)) as client:
        unpaid = client.get_quote(QUOTE_ID)
        seen = client.get_quote(QUOTE_ID)
    assert not isinstance(unpaid.payment, Payment)
    assert isinstance(seen.payment, Payment)
    assert seen.payment.status == "seen"
    assert seen.payment.confirmations == 1
    assert seen.payment.matches_quote


def test_a_pinned_treasury_is_the_only_one_a_quote_may_pay() -> None:
    other = "0x" + "dd" * 20
    # The address derives from the quote's own treasury, so only the pin catches another one.
    moved = _quote(treasury=other)
    service = FakeService(lambda request, _: httpx.Response(200, json=moved))
    # Test mode without pinned treasuries trusts the service's, and says so.
    with (
        _client(service, treasuries=None) as client,
        pytest.warns(UnpinnedTreasuryWarning, match="test mode only"),
    ):
        assert client.get_quote(QUOTE_ID).treasury == other
    for pins in ({11155111: TREASURY}, {1: other}):
        with _client(service, treasuries=pins) as client, pytest.raises(AddressMismatchError):
            client.get_quote(QUOTE_ID)
    with _client(service, treasuries={11155111: other}) as client:
        assert client.get_quote(QUOTE_ID).address == moved["address"]
    with pytest.raises(ValueError, match="forwarder"):
        TopupClient("http://service.test", API_KEY, treasuries={1: TREASURY})


def test_errors_carry_the_request_id_and_doc_url() -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        response = _error(404, "resource_missing")
        response.headers["request-id"] = f"req_{count}"
        return response

    with _client(FakeService(respond)) as client:
        for expected in ("req_1", "req_2"):
            with pytest.raises(ApiError) as raised:
                client.get_refund(REFUND_ID)
            assert raised.value.request_id == expected
            assert expected in str(raised.value)
            assert raised.value.doc_url == (
                "https://phala-network.github.io/phala-pay/#section/Errors/resource_missing"
            )


@pytest.mark.parametrize(
    ("status", "body", "code", "doc_url"),
    [
        # Every field but `code` is optional to the SDK, `doc_url` included.
        (
            400,
            {"error": {"type": "invalid_request_error", "code": "paused", "message": "paused"}},
            "paused",
            None,
        ),
        # An unknown `type` is kept as sent.
        (
            404,
            {"error": {"type": "novel_error", "code": "resource_missing"}},
            "resource_missing",
            None,
        ),
        # Not the error object: a proxy's page, or JSON without it.
        (404, "<html>Not Found</html>", "unexpected_response", None),
        (400, {"message": "bad"}, "unexpected_response", None),
    ],
)
def test_an_error_body_missing_fields_is_still_an_api_error(
    status: int, body: object, code: str, doc_url: str | None
) -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        if isinstance(body, str):
            return httpx.Response(status, text=body, headers={"request-id": "req_1"})
        return httpx.Response(status, json=body, headers={"request-id": "req_1"})

    with _client(FakeService(respond)) as client, pytest.raises(ApiError) as raised:
        client.get_refund(REFUND_ID)
    assert (raised.value.status_code, raised.value.code) == (status, code)
    assert raised.value.doc_url == doc_url
    assert raised.value.request_id == "req_1"


def test_a_gateway_error_page_is_retried() -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        if count == 1:
            return httpx.Response(502, text="<html>Bad Gateway</html>")
        return httpx.Response(200, json=REFUND)

    service = FakeService(respond)
    with _client(service) as client:
        assert client.get_refund(REFUND_ID).id == REFUND_ID
    assert len(service.requests) == 2


def test_forwarders_filter_by_quote_and_deposit_address() -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        return httpx.Response(
            200, json={"object": "list", "url": "/v1/forwarders", "has_more": False, "data": []}
        )

    service = FakeService(respond)
    with _client(service) as client:
        assert list(client.list_forwarders(quote=QUOTE_ID)) == []
        assert list(client.list_forwarders(deposit_address="da_" + "0d" * 16, chain_id=1)) == []
    first, second = (dict(request.url.params) for request in service.requests)
    assert first == {"quote": QUOTE_ID, "limit": "100"}
    assert second == {"chain_id": "1", "deposit_address": "da_" + "0d" * 16, "limit": "100"}


def test_a_rate_limit_is_retried_after_its_retry_after() -> None:
    slept: list[float] = []

    def respond(request: httpx.Request, count: int) -> httpx.Response:
        if count == 1:
            response = _error(429, "customer_rate_limit")
            response.headers["retry-after"] = "7"
            return response
        return httpx.Response(200, json=REFUND)

    service = FakeService(respond)
    client = TopupClient(
        "http://service.test:8080",
        API_KEY,
        transport=httpx.MockTransport(service),
        sleep=slept.append,
    )
    with client:
        assert client.get_refund(REFUND_ID).id == REFUND_ID
    assert slept == [7.0]


def test_a_replayed_failure_is_not_retried() -> None:
    def respond(request: httpx.Request, count: int) -> httpx.Response:
        response = _error(500, "internal_error")
        response.headers["idempotent-replayed"] = "true"
        return response

    service = FakeService(respond)
    with _client(service) as client, pytest.raises(ApiError) as raised:
        client.cancel_refund(REFUND_ID)
    assert raised.value.status_code == 500
    assert len(service.requests) == 1


def test_account_settings_keys_endpoints_and_events_use_their_paths() -> None:
    account = {
        "id": ACCOUNT,
        "object": "account",
        "livemode": False,
        "name": "Acme",
        "charges_enabled": False,
        "paused_scopes": ["quotes"],
        "webhook_keys": [{"version": 1, "expires_at": None}],
        "created": NOW,
    }
    settings = {
        "object": "payment_settings",
        "livemode": False,
        "status": "configured",
        "revision": "psrev_" + "04" * 16,
        "updated": NOW,
        "quote_creations_per_customer_per_minute": None,
        "chains": [
            {"chain_id": 11155111, "confirmations": "finalized", "assets": [{"asset": "usdc"}]}
        ],
        "available": [],
    }
    key = {
        "id": "key_" + "01" * 16,
        "object": "api_key",
        "livemode": False,
        "type": "secret",
        "name": "ci",
        "redacted": "ppay_sk_test_…AAAA",
        "status": "active",
        "created": NOW,
        "expires_at": None,
        "last_used": None,
    }
    event = {
        "id": "evt_" + "02" * 16,
        "object": "event",
        "account": ACCOUNT,
        "livemode": False,
        "type": "deposit.credited",
        "created": NOW,
        "actor": "system",
        "data": {"object": {}},
        "pending_webhooks": 0,
    }
    answers = {
        b"/v1/account/pause": account,
        b"/v1/account/resume": account,
        b"/v1/account": account,
        b"/v1/payment_settings": settings,
        b"/v1/api_keys": key,
        f"/v1/api_keys/{key['id']}/roll".encode(): key,
        b"/v1/account/webhook_keys/roll": account,
        f"/v1/events/{event['id']}/resend".encode(): event,
    }

    def respond(request: httpx.Request, _: int) -> httpx.Response:
        if request.url.raw_path == b"/v1/events?type=deposit.reversed&limit=100":
            return httpx.Response(
                200,
                json={"object": "list", "url": "/v1/events", "has_more": False, "data": [event]},
            )
        return httpx.Response(200, json=answers[request.url.raw_path])

    service = FakeService(respond)
    with _client(service) as client:
        updated = client.update_payment_settings(
            chains=[
                {"chain_id": 11155111, "confirmations": "finalized", "assets": [{"asset": "usdc"}]}
            ]
        )
        reset = client.update_payment_settings(quote_creations_per_customer_per_minute=None)
        client.pause_quotes()
        client.resume_quotes()
        client.create_api_key(name="ci")
        client.roll_api_key(str(key["id"]), expires_in=3600)
        client.roll_webhook_key()
        assert [e.id for e in client.list_events(type="deposit.reversed")] == [event["id"]]
        client.resend_event(str(event["id"]), webhook_endpoint="we_" + "03" * 16)
    assert updated.status == reset.status == "configured"
    assert updated.chains[0].confirmations == "finalized"
    settings_update, rate_reset, pause, resume, created, rolled, webhook_roll, _, resent = (
        service.requests
    )
    # A webhook key roll keeps the old key for the 48 hours a live roll needs, by default.
    assert json.loads(webhook_roll.content) == {"expires_in": 172_800}
    # `chains` replaces the list; a parameter not given is not sent, and `None` restores a default.
    assert json.loads(settings_update.content) == {
        "chains": [
            {"chain_id": 11155111, "confirmations": "finalized", "assets": [{"asset": "usdc"}]}
        ]
    }
    assert json.loads(rate_reset.content) == {"quote_creations_per_customer_per_minute": None}
    assert json.loads(pause.content) == json.loads(resume.content) == {"scopes": ["quotes"]}
    assert json.loads(created.content) == {"name": "ci"}
    assert json.loads(rolled.content) == {"expires_in": 3600}
    assert json.loads(resent.content) == {"webhook_endpoint": "we_" + "03" * 16}
    for request in (settings_update, rate_reset, pause, resume, created, rolled, resent):
        assert request.method == "POST"
        assert request.headers["idempotency-key"].startswith('"')


@pytest.mark.parametrize(
    ("status", "code", "retry_after"),
    [
        (403, "permission_denied", None),
        (429, "rate_limit", "1"),
        (503, "unavailable", "1"),
        (503, "service_restoring", "300"),
        (503, "unavailable", None),
    ],
)
def test_generated_shared_errors_preserve_body_and_optional_retry_delay(
    status: int, code: str, retry_after: str | None
) -> None:
    def respond(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/account"
        response = _error(status, code)
        response.headers["Cache-Control"] = "no-store"
        if retry_after is not None:
            response.headers["Retry-After"] = retry_after
        return response

    with httpx.Client(
        base_url="https://service.test", transport=httpx.MockTransport(respond)
    ) as http:
        client = AuthenticatedClient(
            base_url="https://service.test", token=API_KEY, raise_on_unexpected_status=True
        ).set_httpx_client(http)
        response = get_account.sync_detailed(client=client)
    assert response.status_code == status
    assert isinstance(response.parsed, ErrorResponse)
    assert response.parsed.error.code == code
    assert response.headers.get("Retry-After") == retry_after
    assert response.headers["Cache-Control"] == "no-store"
