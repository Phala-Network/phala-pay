from __future__ import annotations

import json
from collections.abc import Mapping, Sequence

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from topup_sdk import (
    SignatureError,
    WebhookEvent,
    sign_webhook,
    verify_webhook,
    verify_webhook_signature,
)

from ._support import RUST_BODY, RUST_ID, RUST_KEY, RUST_SIGNATURE, RUST_TIMESTAMP

# Fixed vector from crates/topup/src/outbox/signature.rs (seed [7; 32]).


def _rust_headers(signature: str = RUST_SIGNATURE) -> dict[str, str]:
    return {
        "Webhook-Id": RUST_ID,
        "Webhook-Timestamp": str(RUST_TIMESTAMP),
        "Webhook-Signature": signature,
    }


def test_rust_signed_delivery_verifies() -> None:
    webhook_id = verify_webhook_signature(
        _rust_headers(), RUST_BODY, RUST_KEY.public_key(), now=RUST_TIMESTAMP
    )
    assert webhook_id == RUST_ID


def test_any_listed_v1a_signature_may_match() -> None:
    headers = _rust_headers(f"v1,c29tZXRoaW5n v1a,AAAA {RUST_SIGNATURE}")
    verify_webhook_signature(headers, RUST_BODY, RUST_KEY.public_key(), now=RUST_TIMESTAMP)


@pytest.mark.parametrize(
    ("headers", "body", "now"),
    [
        (_rust_headers(), RUST_BODY + b" ", RUST_TIMESTAMP),
        (_rust_headers(), RUST_BODY, RUST_TIMESTAMP + 301),
        (_rust_headers(), RUST_BODY, RUST_TIMESTAMP - 301),
        ({**_rust_headers(), "Webhook-Id": "other"}, RUST_BODY, RUST_TIMESTAMP),
        (_rust_headers(RUST_SIGNATURE.replace("v1a,", "v1,")), RUST_BODY, RUST_TIMESTAMP),
        ({"Webhook-Id": RUST_ID}, RUST_BODY, RUST_TIMESTAMP),
        # Non-ASCII digits pass str.isdigit() but are not a Unix timestamp.
        ({**_rust_headers(), "Webhook-Timestamp": "\u0661\u0662"}, RUST_BODY, RUST_TIMESTAMP),
    ],
)
def test_tampered_or_stale_deliveries_are_rejected(
    headers: dict[str, str], body: bytes, now: int
) -> None:
    with pytest.raises(SignatureError):
        verify_webhook_signature(headers, body, RUST_KEY.public_key(), now=now)


ACCOUNT = "acct_" + "a1" * 16
OTHER_KEY = Ed25519PrivateKey.from_private_bytes(bytes([8] * 32))


def _signed(
    envelope: Mapping[str, object],
    webhook_id: str,
    keys: Sequence[Ed25519PrivateKey] = (RUST_KEY,),
) -> tuple[dict[str, str], bytes]:
    body = json.dumps(envelope).encode()
    headers = sign_webhook(keys, webhook_id, RUST_TIMESTAMP, body)
    return headers, body


EVENT_ID = "evt_26a20351ab10595a852f9c1aa0372d73"
ENVELOPE: dict[str, object] = {
    "id": EVENT_ID,
    "object": "event",
    "account": ACCOUNT,
    "livemode": True,
    "type": "deposit.credited",
    "created": 1_790_410_321,
    "actor": "system",
    "request": None,
    "data": {"object": {"id": "dep_3f1c2b9e6a8d5c479e210b7d4f6a8c13", "object": "deposit"}},
}


def _verify(
    headers: Mapping[str, str],
    body: bytes,
    keys: Ed25519PublicKey | Sequence[Ed25519PublicKey] | None = None,
    *,
    account: str = ACCOUNT,
    livemode: bool = True,
) -> WebhookEvent:
    return verify_webhook(
        headers,
        body,
        RUST_KEY.public_key() if keys is None else keys,
        expected_account=account,
        expected_livemode=livemode,
        now=RUST_TIMESTAMP,
    )


def test_event_is_parsed_and_bound_to_the_webhook_id() -> None:
    headers, body = _signed(ENVELOPE, EVENT_ID)
    event = _verify(headers, body)
    assert (event.id, event.type, event.created) == (EVENT_ID, "deposit.credited", 1_790_410_321)
    assert (event.account, event.livemode) == (ACCOUNT, True)
    assert event.object == ENVELOPE["data"]["object"]  # type: ignore[index]

    headers, body = _signed({**ENVELOPE, "id": "evt_" + "0" * 32}, EVENT_ID)
    with pytest.raises(SignatureError, match="does not match"):
        _verify(headers, body)
    headers, body = _signed({**ENVELOPE, "object": "deposit"}, EVENT_ID)
    with pytest.raises(SignatureError, match="malformed"):
        _verify(headers, body)


@pytest.mark.parametrize(
    ("account", "livemode", "match"),
    [
        ("acct_" + "b2" * 16, True, "another account"),
        (ACCOUNT, False, "other mode"),
    ],
)
def test_an_event_of_another_account_or_mode_is_refused(
    account: str, livemode: bool, match: str
) -> None:
    headers, body = _signed(ENVELOPE, EVENT_ID)
    with pytest.raises(SignatureError, match=match):
        _verify(headers, body, account=account, livemode=livemode)


def test_an_envelope_without_account_or_mode_fails_closed() -> None:
    for missing in ("account", "livemode"):
        envelope = {key: value for key, value in ENVELOPE.items() if key != missing}
        headers, body = _signed(envelope, EVENT_ID)
        with pytest.raises(SignatureError, match="malformed"):
            _verify(headers, body)


def test_another_accounts_key_does_not_verify() -> None:
    headers, body = _signed(ENVELOPE, EVENT_ID, keys=(OTHER_KEY,))
    with pytest.raises(SignatureError, match="no valid webhook signature"):
        _verify(headers, body)
    with pytest.raises(SignatureError, match="no webhook public key"):
        _verify(headers, body, [])


def test_during_a_rotation_either_pinned_key_verifies_and_then_only_the_new_one() -> None:
    # The service signs with the new key and the old one until the old one's overlap ends.
    headers, body = _signed(ENVELOPE, EVENT_ID, keys=(OTHER_KEY, RUST_KEY))
    assert len(headers["webhook-signature"].split()) == 2
    for pinned in (RUST_KEY, OTHER_KEY):
        assert _verify(headers, body, pinned.public_key()).id == EVENT_ID
    both = [OTHER_KEY.public_key(), RUST_KEY.public_key()]
    assert _verify(headers, body, both).id == EVENT_ID

    headers, body = _signed(ENVELOPE, EVENT_ID, keys=(OTHER_KEY,))
    with pytest.raises(SignatureError, match="no valid webhook signature"):
        _verify(headers, body, RUST_KEY.public_key())
    assert _verify(headers, body, both).id == EVENT_ID


@pytest.mark.parametrize("tolerance", [None, 0, 0.5, 30])
@pytest.mark.parametrize("direction", [-1, 1])
def test_signed_event_timestamp_window_is_inclusive(
    tolerance: int | float | None, direction: int
) -> None:
    headers, body = _signed(ENVELOPE, EVENT_ID)
    options = {} if tolerance is None else {"tolerance_seconds": tolerance}
    window = 300 if tolerance is None else tolerance
    assert (
        verify_webhook_signature(
            headers, body, RUST_KEY.public_key(), now=RUST_TIMESTAMP + direction * window, **options
        )
        == EVENT_ID
    )
    with pytest.raises(SignatureError, match="outside tolerance"):
        verify_webhook_signature(
            headers,
            body,
            RUST_KEY.public_key(),
            now=RUST_TIMESTAMP + direction * (window + 1),
            **options,
        )


@pytest.mark.parametrize("invalid", [float("nan"), float("inf"), float("-inf"), True])
def test_invalid_time_options_refuse_an_authentic_stale_event(invalid: float) -> None:
    headers, body = _signed(ENVELOPE, EVENT_ID)
    with pytest.raises(ValueError, match="must be finite"):
        verify_webhook_signature(
            headers,
            body,
            RUST_KEY.public_key(),
            now=RUST_TIMESTAMP + 301,
            tolerance_seconds=invalid,
        )
    with pytest.raises(ValueError, match="must be finite"):
        verify_webhook_signature(headers, body, RUST_KEY.public_key(), now=invalid)


def test_negative_tolerance_is_a_configuration_error() -> None:
    headers, body = _signed(ENVELOPE, EVENT_ID)
    with pytest.raises(ValueError, match="non-negative"):
        verify_webhook_signature(
            headers, body, RUST_KEY.public_key(), now=RUST_TIMESTAMP, tolerance_seconds=-1
        )


def test_signed_timestamp_outside_interoperable_integer_range_is_rejected() -> None:
    headers = sign_webhook(RUST_KEY, EVENT_ID, 2**53, RUST_BODY)
    with pytest.raises(SignatureError, match="timestamp malformed"):
        verify_webhook_signature(headers, RUST_BODY, RUST_KEY.public_key(), now=2**53)


@pytest.mark.parametrize(
    "overrides",
    [
        {"id": 123},
        {"type": ["deposit.credited"]},
        {"data": [["object", {"object": "deposit"}]]},
        {"data": {"object": []}},
        {"data": {"object": [["amount", 1234]]}},
    ],
)
def test_authentic_malformed_envelope_is_not_coerced(overrides: dict[str, object]) -> None:
    envelope = {**ENVELOPE, **overrides}
    # Use the coerced id as the signed header too: id binding alone must not validate a number.
    headers, body = _signed(envelope, str(envelope["id"]))
    with pytest.raises(SignatureError, match="body malformed"):
        _verify(headers, body)
