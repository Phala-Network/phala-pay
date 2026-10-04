from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any

from ._errors import LedgerSnapshotError

_STATUSES = {"pending", "credited", "rejected", "reversed"}
_RANK = {"pending": 0, "credited": 1, "rejected": 1, "reversed": 2}


def _get(value: Any, key: str, default: Any = None) -> Any:
    if isinstance(value, Mapping):
        return value.get(key, default)
    return getattr(value, key, default)


def _validated(value: Any) -> dict[str, Any]:
    status = _get(value, "status")
    if status not in _STATUSES:
        raise LedgerSnapshotError(f"unknown deposit status: {status!r}")
    amount = _get(value, "amount")
    refunded = _get(value, "amount_refunded", 0) or 0
    reversed_amount = _get(value, "amount_reversed", 0) or 0
    if amount is not None and (type(amount) is not int or amount < 0):
        raise LedgerSnapshotError("amount must be a non-negative integer or null")
    if any(type(x) is not int or x < 0 for x in (refunded, reversed_amount)):
        raise LedgerSnapshotError("deductions must be non-negative integers")
    if amount is None:
        if refunded or reversed_amount:
            raise LedgerSnapshotError("unvalued snapshots must have zero deductions")
    elif refunded + reversed_amount > amount:
        raise LedgerSnapshotError("deductions exceed valuation")
    if status == "credited" and amount is None:
        raise LedgerSnapshotError("credited snapshots require a valuation")
    if status == "reversed" and amount is not None and reversed_amount != amount:
        raise LedgerSnapshotError("valued reversed snapshots must reverse the full amount")
    if refunded and reversed_amount:
        raise LedgerSnapshotError("refund and reversal cannot coexist")
    base = {
        key: _get(value, key)
        for key in ("id", "livemode", "client_reference_id", "currency", "status", "amount")
    }
    return base | {"amount_refunded": refunded, "amount_reversed": reversed_amount}


def deposit_net_amount(deposit: Any) -> int:
    snapshot = _validated(deposit)
    if snapshot["status"] != "credited" or snapshot["amount"] is None:
        return 0
    return int(snapshot["amount"] - snapshot["amount_refunded"] - snapshot["amount_reversed"])


@dataclass(frozen=True)
class BalanceDelta:
    snapshot: dict[str, Any]
    contribution: int
    delta: int


def balance_delta(previous: Any, deposit: Any) -> BalanceDelta:
    current = _validated(deposit)
    current_net = deposit_net_amount(current)
    if previous is None:
        return BalanceDelta(current, current_net, current_net)
    old = _validated(previous)
    for key in ("id", "livemode", "client_reference_id", "currency"):
        if old.get(key) != current.get(key):
            raise LedgerSnapshotError(f"conflicting {key}")
    if (
        old["amount"] is not None
        and current["amount"] is not None
        and old["amount"] != current["amount"]
    ):
        raise LedgerSnapshotError("valuation changed")
    if {old["status"], current["status"]} == {"credited", "rejected"}:
        raise LedgerSnapshotError("credited cannot become rejected")
    amount = old["amount"] if old["amount"] is not None else current["amount"]
    status = (
        old["status"] if _RANK[old["status"]] >= _RANK[current["status"]] else current["status"]
    )
    refunded = max(old["amount_refunded"], current["amount_refunded"])
    reversed_amount = max(old["amount_reversed"], current["amount_reversed"])
    merged = {
        **current,
        "amount": amount,
        "status": status,
        "amount_refunded": refunded,
        "amount_reversed": reversed_amount,
    }
    merged = _validated(merged)
    contribution = deposit_net_amount(merged)
    delta = contribution - deposit_net_amount(old)
    return BalanceDelta(merged, contribution, delta)
