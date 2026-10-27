"""The merchant's records a service restore asks for, from the product's ledger.

After the service is restored from backup, its operator asks every merchant for what it did or
received after the restore point (deploy/runbooks/restore.md, step 2). `export_restore_records`
answers with the product's own records, each already in the body of the admin request that brings
it back, without the operator's `reason`:

- `treasuries`: `POST /v1/admin/restore/treasuries/verify` bodies (step 3) of at most 100
  treasuries each, per mode: each treasury's latest object among the `treasury.*` deliveries the
  webhook inbox kept;
- `treasury_applications`: `POST /v1/admin/restore/treasuries/apply` bodies (step 3), one per
  kept delivery of a `treasury.updated` that announced a pending change becoming `active`, the
  only evidence a lost application is restored from;
- `deposit_addresses`: `POST /v1/admin/restore/deposit_addresses` bodies (step 4), from each
  deposit address response the product recorded, with its `client_secret`;
- `events`: `POST /v1/admin/restore/events` bodies (step 5) of at most 100 deliveries each, every
  `deposit.credited`, `deposit.rejected`, and `deposit.reversed` delivery the webhook inbox kept,
  its raw body and Standard Webhooks headers exactly as received, in the order of their deposits'
  receipt positions and revisions, so a reversed deposit comes right before the one that replaced
  it.

With `since` (the restore point, Unix seconds), a record is exported when the service created it
or the product last recorded it (a deposit address posted again gets a fresh `client_secret`) at
most `SINCE_MARGIN` seconds before, allowing for clock skew. Without `since`, everything is
exported,
which is safe: a record the restored service still holds is answered `reissued: false` or
`matches`. The export holds client secrets:
hand it to the operator over the incident's channel only, and delete it once the restore is done.
"""

from __future__ import annotations

import json
import logging
from collections.abc import Mapping
from typing import Any

from .ledger import Delivery, ProductLedger

LOG = logging.getLogger(__name__)

# The event types a restore re-derives from the chain, so imports (restore_mode.rs).
RESTORED_EVENT_TYPES = ("deposit.credited", "deposit.rejected", "deposit.reversed")
TREASURY_EVENT_TYPES = ("treasury.created", "treasury.updated", "treasury.canceled")
# Deliveries per `POST /v1/admin/restore/events`, and treasuries per `treasuries/verify`.
MAX_DELIVERIES = 100
# Clock-skew allowance around the restore point.
SINCE_MARGIN = 300
MAX_TREASURIES = 100
# The treasury fields `treasuries/verify` compares.
TREASURY_FIELDS = ("id", "status", "chain_id", "address", "crediting_paused_by")


def export_restore_records(
    account: str, ledger: ProductLedger, *, since: int | None = None
) -> dict[str, Any]:
    """The product's restore records for `account`, as the operator's admin requests take them."""

    def after(created: object, recorded_at: float) -> bool:
        """Whether the service created a record, or the product last recorded it, since."""
        if since is None:
            return True
        latest = max(created, recorded_at) if isinstance(created, int) else recorded_at
        return latest >= since - SINCE_MARGIN

    addresses = [
        deposit_address_request(account, response)
        for response, recorded_at in ledger.deposit_address_records()
        if after(response.get("created"), recorded_at)
    ]
    # Quotes share customer networks. Recover the address, never an old price window.
    known = {address["id"] for address in addresses}
    for response, recorded_at in ledger.quote_records():
        address_id = response.get("deposit_address")
        if address_id and address_id not in known and after(response.get("created"), recorded_at):
            addresses.append(
                {
                    "account": account,
                    "livemode": response["livemode"],
                    "client_reference_id": response["client_reference_id"],
                    "id": address_id,
                    "version": response["deposit_address_version"],
                    "address": response["address"],
                }
            )
            known.add(address_id)

    def kept(event_types: tuple[str, ...]) -> list[tuple[dict[str, Any], Delivery]]:
        """The inbox's deliveries of `event_types` since, each with its parsed event."""
        events = []
        for delivery, received_at in ledger.deliveries(event_types):
            event = json.loads(delivery.body)
            if after(event.get("created"), received_at):
                events.append((event, delivery))
        return events

    deposit_events = sorted(kept(RESTORED_EVENT_TYPES), key=lambda each: deposit_position(each[0]))
    deliveries = [
        request
        for _, delivery in deposit_events
        if (request := delivery_request(delivery)) is not None
    ]
    treasury_events = kept(TREASURY_EVENT_TYPES)
    return {
        "account": account,
        "since": since,
        "treasuries": treasury_verify_requests(account, [event for event, _ in treasury_events]),
        "treasury_applications": [
            {"delivery": request}
            for event, delivery in treasury_events
            if is_application(event) and (request := delivery_request(delivery)) is not None
        ],
        "deposit_addresses": addresses,
        "events": [
            {"deliveries": deliveries[start : start + MAX_DELIVERIES]}
            for start in range(0, len(deliveries), MAX_DELIVERIES)
        ],
    }


def deposit_position(event: Mapping[str, Any]) -> tuple[int, str, int, int]:
    """A deposit event's receipt position and revision (chain, transaction, receipt log index,
    revision), for ordering."""
    deposit = event["data"]["object"]
    return (
        int(deposit["chain_id"]),
        str(deposit["tx_hash"]).lower(),
        int(deposit["receipt_log_index"]),
        int(deposit["revision"]),
    )


def treasury_verify_requests(account: str, events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """`POST /v1/admin/restore/treasuries/verify` bodies: per mode, each treasury's object in its
    latest `treasury.*` event (by `created`, then as received)."""
    latest: dict[tuple[bool, str], dict[str, Any]] = {}
    for event in sorted(events, key=lambda event: event.get("created", 0)):
        treasury = (event.get("data") or {}).get("object")
        if isinstance(treasury, dict) and isinstance(treasury.get("id"), str):
            key = (bool(event.get("livemode")), treasury["id"])
            latest[key] = {name: treasury[name] for name in TREASURY_FIELDS if name in treasury}
    requests: list[dict[str, Any]] = []
    for livemode in (False, True):
        treasuries = [treasury for (mode, _), treasury in latest.items() if mode == livemode]
        requests.extend(
            {
                "account": account,
                "livemode": livemode,
                "treasuries": treasuries[start : start + MAX_TREASURIES],
            }
            for start in range(0, len(treasuries), MAX_TREASURIES)
        )
    return requests


def is_application(event: Mapping[str, Any]) -> bool:
    """Whether an event is the `treasury.updated` of a pending change becoming `active`."""
    data = event.get("data") or {}
    return (
        event.get("type") == "treasury.updated"
        and (data.get("object") or {}).get("status") == "active"
        and (data.get("previous_attributes") or {}).get("status") == "pending"
    )


def deposit_address_request(account: str, address: Mapping[str, Any]) -> dict[str, Any]:
    """A `POST /v1/admin/restore/deposit_addresses` body from a deposit address response: its id
    and version, and its address (the top-level one, or its first network's)."""
    networks = address.get("networks") or []
    request = {
        "account": account,
        "livemode": address["livemode"],
        "client_reference_id": address["client_reference_id"],
        "id": address["id"],
        "version": address["version"],
        "address": address.get("address") or (networks[0]["address"] if networks else None),
        "client_secret": address.get("client_secret"),
    }
    return {name: value for name, value in request.items() if value is not None}


def delivery_request(delivery: Delivery) -> dict[str, str] | None:
    """A delivery as `POST /v1/admin/restore/events` takes it; `None` for a body that is not
    UTF-8, which no JSON string can carry byte for byte."""
    try:
        body = delivery.body.decode("utf-8")
    except UnicodeDecodeError:
        LOG.warning("webhook %s: the body is not UTF-8; not exported", delivery.webhook_id)
        return None
    return {
        "webhook_id": delivery.webhook_id,
        "webhook_timestamp": delivery.webhook_timestamp,
        "webhook_signature": delivery.webhook_signature,
        "body": body,
    }
