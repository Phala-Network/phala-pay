"""`Webhook.construct_event`: verify a delivery and return the typed event, as Stripe's does."""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any, get_args

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

from topup_client.models import Deposit, Quote, Refund
from topup_sdk import SignatureError, load_webhook_public_key, verify_webhook_signature
from topup_sdk._secrets import protect

from ._types import EventType

DEFAULT_TOLERANCE = 300


class SignatureVerificationError(SignatureError):
    """The delivery's signature, timestamp, or id did not verify; answer `400` and do nothing."""


@dataclass(frozen=True)
class EventData:
    """`object` is the resource as it was when the event happened, rendered with the change and
    never re-rendered: a `Deposit` for `deposit.*`, a `Quote` for `quote.*`, a `Refund` for
    `refund.*`, and the raw object for any other type. `previous_attributes`, on `*.updated`
    events, holds the former values of the fields that changed."""

    object: Deposit | Quote | Refund | dict[str, Any]
    previous_attributes: dict[str, Any] | None = None


@dataclass(frozen=True)
class EventRequest:
    """The API request that caused an event: its `Request-Id` and the `Idempotency-Key` it sent."""

    id: str
    idempotency_key: str | None


@dataclass(frozen=True)
class Event:
    """A verified event of `account` in one mode (`EventType` lists them), such as
    `deposit.credited`, `deposit.rejected`, `deposit.reversed`, `deposit.refunded`,
    `refund.created`, `refund.updated`, `refund.failed`, `quote.canceled`, `quote.expired`, or an
    account event (`account.updated`, `api_key.*`, `treasury.*`, `webhook_endpoint.*`). Its `id`
    is stable across retries and replays; process each id once. Claw back the credit of a
    `deposit.reversed` deposit as for `deposit.refunded`. `request` is the API request that
    caused it, or `None` when the service's own workers did; `actor` who caused it: an API key id
    (`key_…`), `admin`, or `system`."""

    id: str
    account: str
    livemode: bool
    type: str
    created: int
    actor: str
    request: EventRequest | None
    data: EventData

    @property
    def deposit(self) -> Deposit:
        """`data.object` of a `deposit.*` event."""
        if not isinstance(self.data.object, Deposit):
            raise TypeError(f"{self.type} does not carry a deposit")
        return self.data.object

    @property
    def quote(self) -> Quote:
        """`data.object` of a `quote.*` event."""
        if not isinstance(self.data.object, Quote):
            raise TypeError(f"{self.type} does not carry a quote")
        return self.data.object

    @property
    def refund(self) -> Refund:
        """`data.object` of a `refund.*` event."""
        if not isinstance(self.data.object, Refund):
            raise TypeError(f"{self.type} does not carry a refund")
        return self.data.object


class Webhook:
    @staticmethod
    def construct_event(
        payload: bytes | str,
        headers: Mapping[str, str],
        public_key: str | Ed25519PublicKey | Sequence[str | Ed25519PublicKey],
        expected_account: str,
        *,
        expected_livemode: bool,
        tolerance: int | float = DEFAULT_TOLERANCE,
    ) -> Event:
        """Verifies a delivery and returns its event, failing closed.

        `payload` is the raw request body, before any JSON parsing; `headers` are the request
        headers (`webhook-id`, `webhook-timestamp`, `webhook-signature`); `public_key` is your
        account's webhook key in the mode you receive, in Standard Webhooks' `whpk_` form as
        pinned from `GET /v1/attestation`, or a list of keys while a rotation overlaps.
        `expected_account` is your `acct_…` id and `expected_livemode` the mode of the endpoint.

        Raises `SignatureVerificationError` when no signature verifies with a given key, the
        timestamp is more than `tolerance` seconds away, the body's id differs from `webhook-id`,
        or the event's `account` or `livemode` is not the expected one; and `ValueError` when a
        verified body is not an event or `tolerance` is not finite and non-negative. Zero
        requires an exact timestamp match; the default window is 300 seconds.
        """
        if not expected_account:
            raise ValueError("expected_account is required")
        body = payload.encode() if isinstance(payload, str) else payload
        candidates = [public_key] if isinstance(public_key, str | Ed25519PublicKey) else public_key
        keys = [load_webhook_public_key(key) if isinstance(key, str) else key for key in candidates]
        try:
            webhook_id = verify_webhook_signature(headers, body, keys, tolerance_seconds=tolerance)
        except SignatureError as error:
            raise SignatureVerificationError(str(error)) from error

        envelope = json.loads(body)
        if not isinstance(envelope, dict) or envelope.get("object") != "event":
            raise ValueError("webhook body is not an event")
        event_id, event_type, created = (
            envelope.get("id"),
            envelope.get("type"),
            envelope.get("created"),
        )
        account, livemode = envelope.get("account"), envelope.get("livemode")
        actor, request = envelope.get("actor"), envelope.get("request")
        data = envelope.get("data")
        if (
            not isinstance(event_id, str)
            or not isinstance(event_type, str)
            or type(created) is not int
            or not isinstance(account, str)
            or type(livemode) is not bool
            or not isinstance(data, dict)
            or not isinstance(data.get("object"), dict)
            or not isinstance(actor, str)
            or "request" not in envelope
            or not (request is None or _is_request(request))
        ):
            raise ValueError("webhook body is not an event")
        if event_id != webhook_id:
            raise SignatureVerificationError("webhook id does not match the event")
        if account != expected_account:
            raise SignatureVerificationError("webhook event is for another account")
        if livemode != expected_livemode:
            raise SignatureVerificationError("webhook event is for the other mode")
        previous = data.get("previous_attributes")
        return Event(
            event_id,
            account,
            livemode,
            event_type,
            created,
            actor,
            None if request is None else EventRequest(request["id"], request["idempotency_key"]),
            EventData(
                _resource(event_type, data["object"]),
                previous if isinstance(previous, dict) else None,
            ),
        )


def _is_request(value: object) -> bool:
    """Whether `value` is an event's `request`: a `Request-Id` and the `Idempotency-Key` sent,
    or `null` for none."""
    if not isinstance(value, dict) or not isinstance(value.get("id"), str):
        return False
    key = value.get("idempotency_key", False)
    return key is None or isinstance(key, str)


def _resource(  # noqa: PLR0912
    event_type: str, value: dict[str, Any]
) -> Deposit | Quote | Refund | dict[str, Any]:
    if event_type not in get_args(EventType):
        return protect(value)
    resource = event_type.partition(".")[0]
    if resource in {"deposit", "quote", "refund"}:
        if value.get("object") != resource or type(value.get("livemode")) is not bool:
            raise ValueError("malformed webhook resource")
        for field in ("id", "status"):
            if not isinstance(value.get(field), str) or not value[field]:
                raise ValueError("malformed webhook resource")
        if resource in {"deposit", "quote"}:
            amount = value.get("amount")
            if (type(amount) is not int and amount is not None) or (
                resource == "quote" and amount is None
            ):
                raise ValueError("malformed webhook resource")
            for field in ("client_reference_id", "currency", "amount_atomic"):
                if not isinstance(value.get(field), str):
                    raise ValueError("malformed webhook resource")
        if "metadata" in value and not isinstance(value["metadata"], dict):
            raise ValueError("malformed webhook resource")
    try:
        if resource == "deposit":
            return protect(Deposit.from_dict(value))
        if resource == "quote":
            return protect(Quote.from_dict(value))
        if resource == "refund":
            return protect(Refund.from_dict(value))
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError(f"{event_type} carries a malformed {resource}") from error
    return value
