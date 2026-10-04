from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from topup_sdk import (
    deposit_address,
    deposit_address_salt,
    load_webhook_public_key,
    quote_address,
    quote_salt,
    verify_webhook,
)
from topup_sdk.errors import SignatureError

FIXTURES = Path(__file__).resolve().parents[2] / "fixtures"


def load(name: str) -> dict[str, Any]:
    value = json.loads((FIXTURES / name).read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    assert value["schema_version"] == 1
    if name != "manifest.json":
        assert isinstance(value["group"], str)
    return value


def test_manifest_declares_implemented_and_pending_groups() -> None:
    manifest = load("manifest.json")
    groups = manifest["groups"]
    assert set(groups) == {"pins", "addresses", "webhooks", "ledger", "transport"}
    assert groups["addresses"]["python"] is True
    assert groups["webhooks"]["python"] is True
    assert all(groups[name]["python"] is False for name in ("pins", "ledger", "transport"))


def test_address_fixtures_match_existing_derivation() -> None:
    vectors = load("addresses-v1.json")
    for vector in vectors["quote"]:
        salt = quote_salt(vector["account"], vector["client_reference_id"], vector["quote_id"])
        assert "0x" + salt.hex() == vector["salt"]
        assert (
            quote_address(
                vectors["factory"],
                vectors["implementation"],
                vector["treasury"],
                account=vector["account"],
                client_reference_id=vector["client_reference_id"],
                quote_id=vector["quote_id"],
            )
            == vector["predicted_address"]
        )
    for vector in vectors["deposit_address"]:
        inputs = {
            key: vector[key] for key in ("account", "livemode", "client_reference_id", "version")
        }
        salt = deposit_address_salt(**inputs)
        assert "0x" + salt.hex() == vector["salt"]
        assert (
            deposit_address(
                vectors["factory"], vectors["implementation"], vector["treasury"], **inputs
            )
            == vector["predicted_address"]
        )


def test_webhook_fixture_cases_match_current_verifier() -> None:
    vectors = load("webhooks-v1.json")
    public_key = load_webhook_public_key(vectors["public_key"])
    for case in vectors["cases"]:
        headers = case["headers"]
        body = case["body"].encode()
        kwargs = {"now": case["now"], "tolerance_seconds": case.get("tolerance", 300)}
        if case["outcome"] == "accept":
            event = verify_webhook(
                headers,
                body,
                public_key,
                expected_account=case["expected_account"],
                expected_livemode=case["expected_livemode"],
                **kwargs,
            )
            assert event.id == headers["webhook-id"]
        else:
            with pytest.raises((SignatureError, ValueError)):
                verify_webhook(
                    headers,
                    body,
                    public_key,
                    expected_account=case["expected_account"],
                    expected_livemode=case["expected_livemode"],
                    **kwargs,
                )


def test_pending_fixture_groups_have_schema() -> None:
    pins = load("pins-v1.json")
    assert pins["canonical_encoding"].startswith("ppay_pins_v1.")
    assert len(pins["rejections"]) == 11
    ledger = load("ledger-v1.json")
    assert len(ledger["convergence_cases"]) == 5
    transport = load("transport-v1.json")
    assert len(transport["scenarios"]) == 3
