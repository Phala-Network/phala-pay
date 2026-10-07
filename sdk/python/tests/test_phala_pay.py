from __future__ import annotations

import base64
import json
import time
from dataclasses import replace
from typing import Any

import httpx
import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

from phala_pay import (
    AddressMismatchError,
    Deposit,
    Event,
    EventRequest,
    PhalaPay,
    Quote,
    Refund,
    SignatureVerificationError,
    Webhook,
)
from topup_client.models import DepositMetadata, QuoteMetadata
from topup_sdk import deposit_address, load_webhook_public_key, sign_webhook

from ._support import (
    ACCOUNT,
    ADDRESS,
    DEPOSIT_ADDRESS_ID,
    EVENT_ID,
    FACTORY,
    IMPLEMENTATION,
    KEY,
    QUOTE_ID,
    REFUND_ID,
    SERVICE_KEY,
    SERVICE_PUBLIC_KEY,
    TREASURY,
    _delivery,
    _deposit,
    _deposit_address,
    _network,
    _quote,
    pins,
)

# Resources ---------------------------------------------------------------------------------------


def _client(handler: httpx.MockTransport) -> PhalaPay:
    return PhalaPay(
        KEY,
        pins=pins(),
        transport=handler,
    )


def test_quotes_create_returns_the_client_secret_to_a_request_with_the_key() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.headers["authorization"] == f"Bearer {KEY}"
        assert request.headers["idempotency-key"]
        seen.append(request)
        return httpx.Response(200, json=_quote(client_secret=f"{QUOTE_ID}_secret_{'ab' * 24}"))

    with _client(httpx.MockTransport(handler)) as client:
        quote = client.quotes.create(
            client_reference_id="team-42",
            amount=2500,
            chain_id=11155111,
            asset="pha",
            idempotency_key="o-1",
        )
    assert quote.client_secret == f"{QUOTE_ID}_secret_{'ab' * 24}"
    assert seen[0].headers["idempotency-key"] == '"o-1"'
    assert json.loads(seen[0].content)["client_reference_id"] == "team-42"


def test_metadata_is_sent_on_create_and_merged_by_update() -> None:
    seen: list[httpx.Request] = []
    refund: dict[str, object] = {
        "id": REFUND_ID,
        "object": "refund",
        "livemode": False,
        "deposit": f"dep_{1:032x}",
        "amount_atomic": "100",
        "destination_address": ADDRESS,
        "treasury": "0x" + "7e" * 20,
        "status": "pending",
        "failure_reason": None,
        "transaction_hash": None,
        "receipt_log_index": None,
        "created": 1_790_000_400,
        "metadata": {},
    }

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        body = json.loads(request.content) if request.content else {}
        metadata = body.get("metadata")
        merged = metadata if isinstance(metadata, dict) else {}
        if request.url.path.startswith("/v1/quotes"):
            return httpx.Response(200, json=_quote(metadata=merged))
        if request.url.path.startswith("/v1/deposits"):
            return httpx.Response(200, json={**_deposit(), "metadata": merged})
        return httpx.Response(200, json={**refund, "metadata": merged})

    with _client(httpx.MockTransport(handler)) as client:
        quote = client.quotes.create(
            client_reference_id="team-42",
            amount=2500,
            chain_id=11155111,
            asset="pha",
            metadata={"order_id": "6735"},
        )
        assert isinstance(quote.metadata, QuoteMetadata)
        assert quote.metadata.to_dict() == {"order_id": "6735"}
        client.quotes.update(QUOTE_ID, metadata={"order_id": "", "cart": "9"})
        deposit = client.deposits.update(f"dep_{1:032x}", metadata="")
        assert isinstance(deposit.metadata, DepositMetadata)
        assert deposit.metadata.to_dict() == {}
        client.refunds.create(
            deposit=f"dep_{1:032x}", destination_address=ADDRESS, metadata={"ticket": "T-1"}
        )
        client.refunds.update(REFUND_ID, metadata={"ticket": "T-2"})
        client.quotes.update(QUOTE_ID)
        with pytest.raises(ValueError, match="unset every key"):
            client.quotes.update(QUOTE_ID, metadata="x")  # type: ignore[arg-type]
    sent = [(r.method, r.url.path, json.loads(r.content)) for r in seen]
    assert sent == [
        ("POST", "/v1/quotes", {**json.loads(seen[0].content), "metadata": {"order_id": "6735"}}),
        ("POST", f"/v1/quotes/{QUOTE_ID}", {"metadata": {"order_id": "", "cart": "9"}}),
        ("POST", f"/v1/deposits/dep_{1:032x}", {"metadata": ""}),
        ("POST", "/v1/refunds", {**json.loads(seen[3].content), "metadata": {"ticket": "T-1"}}),
        ("POST", f"/v1/refunds/{REFUND_ID}", {"metadata": {"ticket": "T-2"}}),
        ("POST", f"/v1/quotes/{QUOTE_ID}", {}),
    ]


def test_deposits_list_follows_every_page() -> None:
    pages = {None: [_deposit(3), _deposit(2)], "dep_" + f"{2:032x}": [_deposit(1)]}

    def handler(request: httpx.Request) -> httpx.Response:
        after = request.url.params.get("starting_after")
        data = pages[after]
        return httpx.Response(
            200,
            json={"object": "list", "url": "/v1/deposits", "has_more": after is None, "data": data},
        )

    with _client(httpx.MockTransport(handler)) as client:
        deposits = list(client.deposits.list(client_reference_id="team-42"))
    assert [d.log_index for d in deposits] == [3, 2, 1]


def test_deposit_addresses_create_rotate_and_list_check_every_active_address() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        if request.url.path == "/v1/deposit_addresses" and request.method == "POST":
            return httpx.Response(200, json=_deposit_address(metadata={"team": "42"}))
        if request.url.path == f"/v1/deposit_addresses/{DEPOSIT_ADDRESS_ID}":
            return httpx.Response(200, json=_deposit_address(metadata={}))
        if request.url.path.endswith("/rotate"):
            assert request.headers["idempotency-key"]
            return httpx.Response(200, json=_deposit_address(2))
        return httpx.Response(
            200,
            json={
                "object": "list",
                "url": "/v1/deposit_addresses",
                "has_more": False,
                "data": [
                    _deposit_address(2),
                    _deposit_address(
                        1,
                        id="da_" + "0e" * 16,
                        status="retired",
                        address=None,
                        networks=[_network(11155111, "0x" + "99" * 20, "0x" + "99" * 20)],
                    ),
                ],
            },
        )

    with PhalaPay(
        KEY,
        pins=pins(),
        transport=httpx.MockTransport(handler),
    ) as client:
        created = client.deposit_addresses.create(
            client_reference_id="team-42", metadata={"team": "42"}
        )
        cleared = client.deposit_addresses.update(created.id, metadata="")
        rotated = client.deposit_addresses.rotate(created.id)
        listed = list(client.deposit_addresses.list(client_reference_id="team-42"))
    assert created.version == 1
    assert [network.chain_id for network in created.networks] == [11155111, 84532]
    assert created.networks[0].assets[0].asset == "pha"
    assert rotated.version == 2
    assert [address.status for address in listed] == ["active", "retired"]
    assert json.loads(seen[0].content) == {
        "client_reference_id": "team-42",
        "metadata": {"team": "42"},
    }
    assert created.metadata.to_dict() == {"team": "42"}
    assert json.loads(seen[1].content) == {"metadata": ""}
    assert cleared.metadata.to_dict() == {}
    assert seen[3].url.params["client_reference_id"] == "team-42"


OTHER_TREASURY = "0x" + "99" * 20


def _derived(treasury: str) -> str:
    return deposit_address(
        FACTORY,
        IMPLEMENTATION,
        treasury,
        account=ACCOUNT,
        livemode=False,
        client_reference_id="team-42",
        version=1,
    )


@pytest.mark.parametrize(
    ("network", "treasuries"),
    [
        # Another address on one chain.
        (_network(84532, "0x" + "11" * 20), {11155111: TREASURY, 84532: TREASURY}),
        # The address of another treasury, which a pin of the account's treasuries refuses.
        (
            _network(84532, _derived(OTHER_TREASURY), OTHER_TREASURY),
            {11155111: TREASURY, 84532: TREASURY},
        ),
        # A chain without a pinned treasury.
        (_network(84532, _derived(TREASURY)), {11155111: TREASURY}),
    ],
)
def test_a_deposit_address_the_account_cannot_derive_is_refused(
    network: dict[str, object], treasuries: dict[int, str]
) -> None:
    body = _deposit_address()
    networks = body["networks"]
    assert isinstance(networks, list)

    def handler(_: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200, json={**body, "address": None, "networks": [networks[0], network]}
        )

    with (
        PhalaPay(
            KEY,
            pins=replace(pins(), treasuries=treasuries),
            transport=httpx.MockTransport(handler),
        ) as client,
        pytest.raises(AddressMismatchError, match="chain 84532"),
    ):
        client.deposit_addresses.retrieve(DEPOSIT_ADDRESS_ID)


def test_the_key_must_be_an_api_key() -> None:
    with pytest.raises(ValueError, match="invalid API key format"):
        PhalaPay("acme/v1", pins=pins())


# Webhooks ----------------------------------------------------------------------------------------


def _construct(
    body: bytes,
    headers: dict[str, str],
    key: str | list[str] = SERVICE_PUBLIC_KEY,
    account: str = ACCOUNT,
    livemode: bool = False,
) -> Event:
    return Webhook.construct_event(body, headers, key, account, expected_livemode=livemode)


def test_construct_event_returns_the_typed_deposit() -> None:
    body, headers = _delivery()
    event = _construct(body, headers)
    assert (event.id, event.type, event.created) == (EVENT_ID, "deposit.credited", 1_790_000_321)
    assert (event.account, event.livemode) == (ACCOUNT, False)
    assert isinstance(event.data.object, Deposit)
    assert (event.deposit.id, event.deposit.client_reference_id, event.deposit.amount) == (
        f"dep_{1:032x}",
        "team-42",
        2500,
    )
    # The quote's metadata, copied to its deposit, arrives with the event.
    assert isinstance(event.deposit.metadata, DepositMetadata)
    assert event.deposit.metadata.to_dict() == {"order_id": "6735"}
    with pytest.raises(TypeError):
        _ = event.quote


def test_construct_event_parses_quote_events_and_accepts_text_and_any_header_case() -> None:
    body, headers = _delivery("quote.expired", _quote(status="expired"))
    event = Webhook.construct_event(
        body.decode(),
        {k.upper(): v for k, v in headers.items()},
        load_webhook_public_key(SERVICE_PUBLIC_KEY),
        ACCOUNT,
        expected_livemode=False,
    )
    assert isinstance(event.data.object, Quote)
    assert event.quote.status == "expired"


def test_construct_event_parses_a_failed_refund() -> None:
    refund: dict[str, object] = {
        "id": "re_" + "0e" * 16,
        "object": "refund",
        "livemode": False,
        "deposit": f"dep_{1:032x}",
        "amount_atomic": "100",
        "destination_address": "0x" + "44" * 20,
        "treasury": "0x" + "7e" * 20,
        "status": "failed",
        "failure_reason": "sender_mismatch",
        "transaction_hash": "0x" + "dd" * 32,
        "receipt_log_index": None,
        "created": 1_790_000_000,
        "metadata": {},
    }
    body, headers = _delivery("refund.failed", refund)
    event = _construct(body, headers)
    assert isinstance(event.data.object, Refund)
    assert event.refund.status == "failed"
    assert event.refund.failure_reason == "sender_mismatch"
    with pytest.raises(TypeError):
        _ = event.deposit


def test_construct_event_carries_the_request_and_previous_attributes() -> None:
    body, headers = _delivery()
    assert _construct(body, headers).request is None
    endpoint = {"id": "we_" + "0a" * 16, "object": "webhook_endpoint", "url": "https://b.example"}
    body, headers = _delivery(
        "webhook_endpoint.updated",
        extra={
            "request": {"id": "req_" + "0b" * 16, "idempotency_key": "update-1"},
            "data": {"object": endpoint, "previous_attributes": {"url": "https://a.example"}},
        },
    )
    event = _construct(body, headers)
    assert event.request == EventRequest("req_" + "0b" * 16, "update-1")
    assert event.data.object == endpoint
    assert event.data.previous_attributes == {"url": "https://a.example"}


def test_construct_event_keeps_unknown_types_raw() -> None:
    body, headers = _delivery("payout.paid", {"id": "po_1"})
    assert _construct(body, headers).data.object == {"id": "po_1"}


@pytest.mark.parametrize(
    ("case", "match"),
    [
        ("tampered", "no valid webhook signature"),
        ("other key", "no valid webhook signature"),
        ("stale", "outside tolerance"),
        ("id mismatch", "does not match"),
        ("no headers", "headers missing"),
    ],
)
def test_construct_event_rejects_forgeries(case: str, match: str) -> None:
    if case == "tampered":
        body, headers = _delivery()
        body = body.replace(b"2500", b"9999")
    elif case == "other key":
        body, headers = _delivery(key=Ed25519PrivateKey.generate())
    elif case == "stale":
        body, headers = _delivery(timestamp=int(time.time()) - 301)
    elif case == "id mismatch":
        body, headers = _delivery(webhook_id="evt_" + "00" * 16)
    else:
        body, headers = _delivery()
        headers = {}
    with pytest.raises(SignatureVerificationError, match=match):
        _construct(body, headers)


def test_construct_event_rejects_a_verified_body_that_is_not_an_event() -> None:
    body = json.dumps({"id": EVENT_ID, "type": "deposit.credited", "data": {}}).encode()
    headers = sign_webhook(SERVICE_KEY, EVENT_ID, int(time.time()), body)
    with pytest.raises(ValueError, match="not an event"):
        _construct(body, headers)
    # An envelope without its actor or request, or with a malformed request, is not one either.
    complete = json.loads(_delivery()[0])
    without_actor = {key: value for key, value in complete.items() if key != "actor"}
    without_request = {key: value for key, value in complete.items() if key != "request"}
    for envelope in (without_actor, without_request, {**complete, "request": {"id": 7}}):
        body = json.dumps(envelope).encode()
        headers = sign_webhook(SERVICE_KEY, EVENT_ID, int(time.time()), body)
        with pytest.raises(ValueError, match="not an event"):
            _construct(body, headers)


@pytest.mark.parametrize(
    ("delivery", "account", "livemode", "match"),
    [
        ({"account": "acct_" + "b2" * 16}, ACCOUNT, False, "another account"),
        ({}, "acct_" + "b2" * 16, False, "another account"),
        ({"livemode": True}, ACCOUNT, False, "other mode"),
        ({}, ACCOUNT, True, "other mode"),
    ],
)
def test_construct_event_fails_closed_for_another_account_or_mode(
    delivery: dict[str, Any], account: str, livemode: bool, match: str
) -> None:
    body, headers = _delivery(**delivery)
    with pytest.raises(SignatureVerificationError, match=match):
        _construct(body, headers, account=account, livemode=livemode)


def test_construct_event_requires_the_expected_account() -> None:
    body, headers = _delivery()
    with pytest.raises(ValueError, match="expected_account"):
        _construct(body, headers, account="")
    with pytest.raises(TypeError):
        Webhook.construct_event(body, headers, SERVICE_PUBLIC_KEY)  # type: ignore[call-arg]


def test_construct_event_accepts_either_pinned_key_during_a_rotation() -> None:
    new_key = Ed25519PrivateKey.from_private_bytes(bytes([10] * 32))
    new_public = (
        "whpk_"
        + base64.b64encode(
            new_key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
        ).decode()
    )
    body, headers = _delivery(key=[new_key, SERVICE_KEY])
    for pinned in (SERVICE_PUBLIC_KEY, new_public, [new_public, SERVICE_PUBLIC_KEY]):
        assert _construct(body, headers, pinned).id == EVENT_ID
    # After the overlap only the new key signs: the old pin alone no longer verifies.
    body, headers = _delivery(key=new_key)
    with pytest.raises(SignatureVerificationError, match="no valid webhook signature"):
        _construct(body, headers)
    assert _construct(body, headers, [new_public, SERVICE_PUBLIC_KEY]).id == EVENT_ID


def test_construct_event_accepts_a_public_key_only_in_the_whpk_form() -> None:
    raw = SERVICE_KEY.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    body, headers = _delivery()
    for refused in (
        raw.hex(),
        base64.b64encode(raw).decode(),
        f"whpk_{raw.hex()}",
        f"whpk_{base64.b64encode(raw[:16]).decode()}",
    ):
        with pytest.raises(ValueError, match="whpk_"):
            _construct(body, headers, refused)


@pytest.mark.parametrize("tolerance", [float("nan"), float("inf"), float("-inf"), -1])
def test_construct_event_rejects_invalid_tolerance(tolerance: float) -> None:
    body, headers = _delivery(timestamp=int(time.time()) - 301)
    with pytest.raises(ValueError, match="finite and non-negative"):
        Webhook.construct_event(
            body, headers, SERVICE_PUBLIC_KEY, ACCOUNT, expected_livemode=False, tolerance=tolerance
        )


def test_transaction_hints_forward_hash_and_deposit_address_chain_without_payment_result() -> None:
    requests: list[httpx.Request] = []
    transaction_hash = "0x" + "ab" * 32

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(
            202,
            json={
                "object": "transaction_submission",
                "transaction_hash": transaction_hash,
                "status": "received",
            },
        )

    with _client(httpx.MockTransport(handler)) as client:
        quote_hint = client.quotes.submit_transaction(
            QUOTE_ID, transaction_hash=transaction_hash, request_deadline=2
        )
        address_hint = client.deposit_addresses.submit_transaction(
            DEPOSIT_ADDRESS_ID, transaction_hash=transaction_hash, chain_id=84532
        )
    assert quote_hint.transaction_hash == transaction_hash
    assert address_hint.transaction_hash == transaction_hash
    assert requests[0].url.path == f"/v1/quotes/{QUOTE_ID}/transactions"
    assert requests[1].url.path == f"/v1/deposit_addresses/{DEPOSIT_ADDRESS_ID}/transactions"
    assert json.loads(requests[0].content) == {"transaction_hash": transaction_hash}
    assert json.loads(requests[1].content) == {
        "transaction_hash": transaction_hash,
        "chain_id": 84532,
    }
    assert all(request.headers["authorization"] == f"Bearer {KEY}" for request in requests)
