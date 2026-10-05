from __future__ import annotations

import json
from typing import Any

import httpx
import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from topup_sdk import (
    CREDITED_EVENT,
    CreditedDeposit,
    FulfillmentError,
    SignatureError,
    WebhookEvent,
    credited_event_id,
    sign_webhook,
    verify_webhook,
)
from topup_sdk.__main__ import send_test_event

from ._support import RUST_BODY, RUST_ID, RUST_KEY, RUST_SIGNATURE, RUST_TIMESTAMP

DEPOSIT = "dep_3f1c2b9e6a8d5c479e210b7d4f6a8c13"


def _deposit(**overrides: Any) -> dict[str, Any]:
    deposit: dict[str, Any] = {
        "id": DEPOSIT,
        "object": "deposit",
        "livemode": False,
        "client_reference_id": "team-42",
        "quote": "qt_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10",
        "deposit_address": None,
        "status": "credited",
        "final": False,
        "swept": False,
        "metadata": {},
        "rejection_reason": None,
        "chain_id": 1,
        "asset": "pha",
        "asset_contract": "0x" + "22" * 20,
        "amount_atomic": "1000000000000000000",
        "amount": 1234,
        "currency": "usd",
        "exchange_rate": "0.12345678",
        "price_source": "quote",
        "valued_at": 1_790_410_321,
        "address": "0x" + "11" * 20,
        "from_address": "0x" + "44" * 20,
        "tx_hash": "0x" + "33" * 32,
        "log_index": 12,
        "block_number": 100,
        "amount_refunded_atomic": "0",
        "refunded": False,
        "amount_refunded": 0,
        "amount_reversed": 0,
        "created": 1_790_410_300,
    }
    deposit.update(overrides)
    return deposit


ACCOUNT = "acct_" + "a1" * 16


def _event(deposit: dict[str, Any], event_type: str = CREDITED_EVENT) -> WebhookEvent:
    return WebhookEvent(
        id=credited_event_id(DEPOSIT),
        account=ACCOUNT,
        livemode=False,
        type=event_type,
        created=1_790_410_321,
        data={"object": deposit},
    )


def test_credited_event_id_is_derived_from_the_deposit_id() -> None:
    # The service derives the same value (crates/core, credited_event_id vector).
    assert credited_event_id(DEPOSIT) == "evt_26a20351ab10595a852f9c1aa0372d73"


def test_credited_event_parses_into_a_typed_credit() -> None:
    credit = CreditedDeposit.from_event(_event(_deposit()))
    assert credit.deposit_id == DEPOSIT
    assert credit.client_reference_id == "team-42"
    assert credit.amount == 1234
    assert credit.price_source == "quote"
    assert credit.quote == "qt_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10"
    assert credit.fulfillment_key == DEPOSIT


def test_spot_and_swept_credits_parse() -> None:
    credit = CreditedDeposit.from_event(
        _event(_deposit(price_source="spot", quote=None, swept=True))
    )
    assert credit.price_source == "spot"
    assert credit.quote is None


@pytest.mark.parametrize(
    "event",
    [
        _event(_deposit(), event_type="deposit.rejected"),
        _event({key: value for key, value in _deposit().items() if key != "client_reference_id"}),
        _event(_deposit(status="rejected")),
        _event(_deposit(status="swept")),
        _event(_deposit(object="quote")),
        _event(_deposit(price_source="lock")),
        _event(_deposit(amount="1234")),
        _event(_deposit(amount=None)),
        _event(_deposit(id="3f1c2b9e-6a8d-5c47-9e21-0b7d4f6a8c13")),
        _event(_deposit(quote="q-981")),
        _event(_deposit(log_index=-1)),
        _event(_deposit(chain_id=True)),
        # A flat payload without `object`.
        WebhookEvent(
            id="26a20351-ab10-595a-852f-9c1aa0372d73",
            account=ACCOUNT,
            livemode=False,
            type=CREDITED_EVENT,
            created=0,
            data={"deposit_id": "3f1c2b9e-6a8d-5c47-9e21-0b7d4f6a8c13", "state": "credited"},
        ),
    ],
)
def test_other_shapes_are_refused(event: WebhookEvent) -> None:
    with pytest.raises(FulfillmentError):
        CreditedDeposit.from_event(event)


def test_sign_webhook_reproduces_the_service_signature() -> None:
    headers = sign_webhook(RUST_KEY, RUST_ID, RUST_TIMESTAMP, RUST_BODY)
    assert headers["webhook-signature"] == RUST_SIGNATURE


def _receiver(key: Ed25519PrivateKey, credits: dict[str, int]) -> httpx.MockTransport:
    def handle(request: httpx.Request) -> httpx.Response:
        try:
            event = verify_webhook(
                request.headers,
                request.content,
                key.public_key(),
                expected_account=ACCOUNT,
                expected_livemode=False,
            )
        except SignatureError:
            return httpx.Response(400)
        credit = CreditedDeposit.from_event(event)
        credits.setdefault(credit.fulfillment_key, credit.amount)
        return httpx.Response(204)

    return httpx.MockTransport(handle)


def test_send_test_event_passes_a_verifying_deduplicating_receiver() -> None:
    key = Ed25519PrivateKey.generate()
    credits: dict[str, int] = {}
    report = send_test_event(
        "https://product.example/webhooks",
        key,
        account=ACCOUNT,
        client_reference_id="team-42",
        amount=250,
        transport=_receiver(key, credits),
    )
    assert report["passed"], json.dumps(report)
    assert [result["status"] for result in report["results"]] == [204, 204, 400, 400]
    assert credits == {report["deposit_id"]: 250}
    assert report["event_id"] == credited_event_id(report["deposit_id"])


def test_send_test_event_fails_a_receiver_that_skips_verification() -> None:
    report = send_test_event(
        "https://product.example/webhooks",
        Ed25519PrivateKey.generate(),
        account=ACCOUNT,
        client_reference_id="team-42",
        amount=250,
        transport=httpx.MockTransport(lambda _request: httpx.Response(200)),
    )
    assert not report["passed"]
    assert report["results"][2] == {"case": "foreign_signature", "status": 200, "ok": False}
