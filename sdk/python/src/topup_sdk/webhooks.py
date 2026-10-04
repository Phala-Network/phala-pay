"""Standard Webhooks verification for events signed with an account's webhook key.

Events carry `webhook-id`, `webhook-timestamp`, and `webhook-signature` headers. The signature
is the asymmetric `v1a` scheme: ed25519 over `{id}.{timestamp}.{body}` with the account's key in
the event's mode (design D11), pinned from attestation; during a key rotation a delivery carries
one signature per key, and any pinned key may verify it. The body is Stripe's Event object,
`{id, object: "event", account, livemode, type, created, data: {object}}`, where `data.object` is
the object the event is about. Receivers deduplicate by `webhook-id`, the event's `evt_` id;
`deposit.credited` is the fulfillment event (`topup_sdk.fulfillment`).
"""

from __future__ import annotations

import base64
import binascii
import json
import math
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from .errors import SignatureError

DEFAULT_TOLERANCE_SECONDS = 300

PublicKeys = Ed25519PublicKey | Sequence[Ed25519PublicKey]
"""One pinned webhook key, or several while a rotation overlaps."""

WEBHOOK_PUBLIC_KEY_PREFIX = "whpk_"


def webhook_public_key_bytes(encoded: str) -> bytes:
    """The 32 raw bytes of a webhook public key in Standard Webhooks' form, `whpk_` and the
    standard base64 of the key, as `GET /v1/attestation` lists it; raises `ValueError` on any
    other form."""
    if not encoded.startswith(WEBHOOK_PUBLIC_KEY_PREFIX):
        raise ValueError("a webhook public key is whpk_ and the base64 of 32 bytes")
    try:
        raw = base64.b64decode(encoded.removeprefix(WEBHOOK_PUBLIC_KEY_PREFIX), validate=True)
    except binascii.Error as error:
        raise ValueError("a webhook public key is whpk_ and the base64 of 32 bytes") from error
    if len(raw) != 32:
        raise ValueError("a webhook public key is whpk_ and the base64 of 32 bytes")
    return raw


def load_webhook_public_key(encoded: str) -> Ed25519PublicKey:
    """Parses a webhook public key in Standard Webhooks' form (`webhook_public_key_bytes`)."""
    return Ed25519PublicKey.from_public_bytes(webhook_public_key_bytes(encoded))


@dataclass(frozen=True)
class WebhookEvent:
    """A verified event of one account and mode: its `evt_` id, type, creation time in Unix
    seconds, and `data`, whose `object` is the object the event is about."""

    id: str
    account: str
    livemode: bool
    type: str
    created: int
    data: dict[str, Any]

    @property
    def object(self) -> dict[str, Any] | None:
        """The object the event is about."""
        value = self.data.get("object")
        return value if isinstance(value, dict) else None


def verify_webhook(
    headers: Mapping[str, str],
    body: bytes,
    public_keys: PublicKeys,
    *,
    expected_account: str,
    expected_livemode: bool,
    tolerance_seconds: int | float = DEFAULT_TOLERANCE_SECONDS,
    now: int | float | None = None,
) -> WebhookEvent:
    """Verifies a delivery and returns its parsed envelope, raising `SignatureError` otherwise.

    Fails closed unless a signature verifies with one of `public_keys`, the account's keys
    pinned from attestation, and the event's `account` and `livemode` are `expected_account` and
    `expected_livemode`: a key is per account and mode, and this check also refuses an event of
    another account or mode that verified with a key pinned by mistake. Invalid time options
    raise `ValueError`: `now` must be finite and tolerance finite and non-negative.
    """
    webhook_id = verify_webhook_signature(
        headers, body, public_keys, tolerance_seconds=tolerance_seconds, now=now
    )
    try:
        envelope = json.loads(body)
        if (
            not isinstance(envelope, dict)
            or envelope.get("object") != "event"
            or not isinstance(envelope["id"], str)
            or not isinstance(envelope["type"], str)
            or not isinstance(envelope["data"], dict)
            or not isinstance(envelope["data"].get("object"), dict)
            or type(envelope["created"]) is not int
            or not isinstance(envelope["account"], str)
            or type(envelope["livemode"]) is not bool
        ):
            raise ValueError("not an event object")
        event = WebhookEvent(
            id=envelope["id"],
            account=envelope["account"],
            livemode=envelope["livemode"],
            type=envelope["type"],
            created=envelope["created"],
            data=envelope["data"],
        )
    except (ValueError, KeyError, TypeError) as error:
        raise SignatureError("webhook body malformed") from error
    if event.id != webhook_id:
        raise SignatureError("webhook id does not match the envelope")
    if event.account != expected_account:
        raise SignatureError("webhook event is for another account")
    if event.livemode != expected_livemode:
        raise SignatureError("webhook event is for the other mode")
    return event


def verify_webhook_signature(
    headers: Mapping[str, str],
    body: bytes,
    public_keys: PublicKeys,
    *,
    tolerance_seconds: int | float = DEFAULT_TOLERANCE_SECONDS,
    now: int | float | None = None,
) -> str:
    """Checks that a Standard Webhooks `v1a` signature verifies with one of `public_keys` and the
    timestamp is within tolerance; returns the `webhook-id`.

    `now` must be finite and `tolerance_seconds` finite and non-negative; invalid options raise
    `ValueError`. Zero requires an exact timestamp match; the default window is 300 seconds.
    """
    now = int(time.time()) if now is None else now
    if not _finite_seconds(now) or not _finite_seconds(tolerance_seconds) or tolerance_seconds < 0:
        raise ValueError("webhook now must be finite and tolerance finite and non-negative")
    keys = [public_keys] if isinstance(public_keys, Ed25519PublicKey) else list(public_keys)
    if not keys:
        raise SignatureError("no webhook public key pinned")
    entries = headers.multi_items() if hasattr(headers, "multi_items") else headers.items()
    lowered: dict[str, str] = {}
    for name, value in entries:
        key = name.lower()
        if key in {"webhook-id", "webhook-timestamp", "webhook-signature"} and key in lowered:
            raise SignatureError("duplicate webhook signing header")
        lowered[key] = value.strip()
    try:
        webhook_id = lowered["webhook-id"]
        timestamp = lowered["webhook-timestamp"]
        signatures = lowered["webhook-signature"]
    except KeyError as error:
        raise SignatureError("webhook headers missing") from error
    if not (timestamp.isascii() and timestamp.isdigit()):
        raise SignatureError("webhook timestamp malformed")
    try:
        timestamp_seconds = int(timestamp)
    except ValueError as error:
        raise SignatureError("webhook timestamp malformed") from error
    if timestamp_seconds > 2**53 - 1:
        raise SignatureError("webhook timestamp malformed")
    if abs(now - timestamp_seconds) > tolerance_seconds:
        raise SignatureError("webhook timestamp outside tolerance")

    content = f"{webhook_id}.{timestamp}.".encode() + body
    entries = signatures.split()
    if not any(_matches(entry, content, key) for entry in entries for key in keys):
        raise SignatureError("no valid webhook signature")
    return webhook_id


def _finite_seconds(value: object) -> bool:
    return type(value) is int or (type(value) is float and math.isfinite(value))


def sign_webhook(
    private_keys: Ed25519PrivateKey | Sequence[Ed25519PrivateKey],
    webhook_id: str,
    timestamp: int,
    body: bytes,
) -> dict[str, str]:
    """Returns Standard Webhooks `v1a` headers, as the service signs a delivery: one signature
    per key, as during a rotation.

    For test senders only: a merchant never holds its account's webhook key.
    """
    keys = [private_keys] if isinstance(private_keys, Ed25519PrivateKey) else private_keys
    content = f"{webhook_id}.{timestamp}.".encode() + body
    signatures = [base64.b64encode(key.sign(content)).decode("ascii") for key in keys]
    return {
        "webhook-id": webhook_id,
        "webhook-timestamp": str(timestamp),
        "webhook-signature": " ".join(f"v1a,{signature}" for signature in signatures),
    }


def _matches(entry: str, content: bytes, public_key: Ed25519PublicKey) -> bool:
    version, _, encoded = entry.partition(",")
    if version != "v1a":
        return False
    try:
        public_key.verify(base64.b64decode(encoded, validate=True), content)
    except (InvalidSignature, binascii.Error, ValueError):
        return False
    return True
