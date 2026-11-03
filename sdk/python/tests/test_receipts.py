"""Receipt contracts: preserve credited amounts, references, and cumulative adjustments."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from topup_client.models.deposit import Deposit
from topup_client.models.quote import Quote
from topup_sdk import (
    address_url,
    stripe_receipt_credit_note,
    stripe_receipt_invoice,
    stripe_receipt_invoice_item,
    transaction_url,
)
from topup_sdk.chains import network_name

ROOT = Path(__file__).resolve().parents[3]
API = json.loads((ROOT / "crates/topup/openapi.json").read_text())


def deposit(**changes: Any) -> Deposit:
    data = {**API["components"]["schemas"]["Deposit"]["example"], **changes}
    return Deposit.from_dict(data)


def test_explorer_parity() -> None:
    # The JS-server test also checks this fixture against the browser's viem registry.
    networks = json.loads((ROOT / "sdk/fixtures/explorers.json").read_text())
    tx_hash = "0x" + "aB" * 32
    address = "0x" + "aB" * 20
    for network in networks:
        chain_id = network["chain_id"]
        assert transaction_url(chain_id, tx_hash) == f"{network['explorer']}/tx/{tx_hash}"
        assert address_url(chain_id, address) == f"{network['explorer']}/address/{address}"
        assert network_name(chain_id) == network["name"]
    assert transaction_url(999, tx_hash) is None
    assert address_url(999, address) is None


def test_explorer_rejects_malformed_transaction_hash() -> None:
    valid = "0x" + "aB" * 32
    for value in (
        "",
        "0xabc",
        valid[:-1],
        valid + "a",
        valid + "\n",
        "0x" + "g" * 64,
        valid + "/../other",
        "0X" + "a" * 64,
    ):
        assert transaction_url(8453, value) is None


def test_explorer_rejects_malformed_address() -> None:
    valid = "0x" + "aB" * 20
    for value in (
        "",
        "0x123",
        valid[:-1],
        valid + "a",
        valid + "\n",
        "0x" + "g" * 40,
        valid + "?url=https://example.com",
        "0X" + "a" * 40,
    ):
        assert address_url(8453, value) is None


@pytest.mark.parametrize("livemode", [True, False])
def test_quote_receipt_is_manual_and_has_transaction(livemode: bool) -> None:
    dep = deposit(chain_id=8453, asset="usdc", livemode=livemode)
    invoice = stripe_receipt_invoice(dep, customer="cus_123")
    item = stripe_receipt_invoice_item(dep, invoice="in_123", customer="cus_123")
    assert invoice["collection_method"] == "send_invoice"
    assert invoice["auto_advance"] is False
    assert invoice["days_until_due"] == 0
    assert invoice["customer"] == item["customer"] == "cus_123"
    assert invoice["currency"] == item["currency"] == dep.currency
    assert item["invoice"] == "in_123"
    assert item["amount"] == dep.amount
    assert item["tax_behavior"] == "inclusive"
    assert item["discountable"] is False
    assert "USDC on Base" in item["description"]
    assert dep.tx_hash in item["description"]
    assert invoice["metadata"] == item["metadata"]
    assert invoice["metadata"]["livemode"] == str(livemode).lower()
    assert invoice["metadata"]["phala_pay_deposit"] == dep.id
    assert invoice["metadata"]["phala_pay_quote"] == dep.quote
    assert invoice["metadata"]["tx_hash"] == dep.tx_hash
    url = transaction_url(dep.chain_id, dep.tx_hash)
    assert url is not None
    assert url in invoice["footer"]
    assert {field["name"] for field in invoice["custom_fields"]} == {
        "Network",
        "Token",
        "Transaction",
        "Explorer",
    }


def test_spot_unknown_chain_and_expanded_quote() -> None:
    dep = deposit(quote=None, price_source="spot", chain_id=999, asset=None)
    invoice = stripe_receipt_invoice(dep, customer="cus_123", description="Account credits")
    assert invoice["description"] == "Account credits"
    assert "phala_pay_quote" not in invoice["metadata"]
    assert invoice["metadata"]["asset"] == dep.asset_contract
    assert "Chain 999" in str(invoice["custom_fields"])
    assert "Explorer" not in {field["name"] for field in invoice["custom_fields"]}
    assert dep.tx_hash in invoice["footer"]
    assert "https://" not in invoice["footer"]
    dep.quote = Quote.from_dict(API["components"]["schemas"]["Quote"]["example"])
    assert (
        stripe_receipt_invoice(dep, customer="cus_123")["metadata"]["phala_pay_quote"]
        == dep.quote.id
    )


def test_stripe_custom_field_limits_keep_full_reference() -> None:
    dep = deposit(asset="a" * 200, tx_hash="0x" + "b" * 200)
    invoice = stripe_receipt_invoice(dep, customer="cus_123")
    # Malformed hash has no explorer URL; footer retains the hash.
    assert len(invoice["custom_fields"]) == 3
    for field in invoice["custom_fields"]:
        assert len(field["name"]) <= 40
        assert len(field["value"]) <= 140
    assert dep.tx_hash in invoice["footer"]
    assert invoice["metadata"]["tx_hash"] == dep.tx_hash


def test_refund_growth_replay_stale_snapshot_and_later_growth() -> None:
    dep = deposit(amount=2500, amount_refunded=600)
    credit = stripe_receipt_credit_note(
        dep, invoice="in_123", previous_refunded=200, previous_reversed=0
    )
    assert credit is not None
    assert credit["invoice"] == "in_123"
    assert credit["amount"] == credit["out_of_band_amount"] == 400
    assert credit["metadata"]["amount_refunded"] == "600"
    assert credit["metadata"]["refund_growth"] == "400"
    assert credit["metadata"]["phala_pay_deposit"] == dep.id
    for previous in (600, 800):
        assert (
            stripe_receipt_credit_note(
                dep, invoice="in_123", previous_refunded=previous, previous_reversed=0
            )
            is None
        )
    dep.amount_refunded = 900
    next_credit = stripe_receipt_credit_note(
        dep, invoice="in_123", previous_refunded=800, previous_reversed=0
    )
    assert next_credit is not None
    assert next_credit["amount"] == 100


def test_reversal_growth() -> None:
    dep = deposit(amount=2500, status="reversed", amount_reversed=2500)
    credit = stripe_receipt_credit_note(
        dep, invoice="in_123", previous_refunded=0, previous_reversed=0
    )
    assert credit is not None
    assert credit["amount"] == credit["out_of_band_amount"] == 2500
    assert credit["metadata"]["reversal_growth"] == "2500"
    assert (
        stripe_receipt_credit_note(
            dep, invoice="in_123", previous_refunded=0, previous_reversed=2500
        )
        is None
    )


@pytest.mark.parametrize(
    "changes",
    [
        {"status": "pending"},
        {"status": "rejected"},
        {"amount": None},
        {"amount": -1},
        {"amount": True},
        {"currency": "USD"},
        {"tx_hash": "a" * 501},
    ],
)
def test_unreceiptable_deposit_is_rejected(changes: dict[str, Any]) -> None:
    with pytest.raises(ValueError, match=r"amount|currency|metadata|receipt|previous_refunded"):
        stripe_receipt_invoice(deposit(**changes), customer="cus_123")


@pytest.mark.parametrize("previous", [-1, True, 2600])
def test_invalid_adjustment_totals_are_rejected(previous: int) -> None:
    with pytest.raises(ValueError, match=r"amount|currency|metadata|receipt|previous_refunded"):
        stripe_receipt_credit_note(
            deposit(amount=2500), invoice="in_123", previous_refunded=previous, previous_reversed=0
        )
