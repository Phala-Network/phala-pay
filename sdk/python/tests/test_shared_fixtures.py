from __future__ import annotations

from dataclasses import replace
from email.utils import formatdate
from types import SimpleNamespace
from typing import Any

import httpx
import pytest

from phala_pay import (
    ConfigurationError,
    LedgerSnapshotError,
    PhalaPay,
    balance_delta,
    deposit_net_amount,
    encode_pins,
    parse_pins,
)
from topup_sdk import (
    deposit_address,
    deposit_address_salt,
    load_webhook_public_key,
    quote_address,
    quote_salt,
    verify_webhook,
)
from topup_sdk.client import TopupClient, _ErrorResponseError, _seconds
from topup_sdk.errors import ApiError, SignatureError

from ._support import load


def test_manifest_declares_implemented_and_pending_groups() -> None:
    manifest = load("manifest.json")
    groups = manifest["groups"]
    assert set(groups) == {"pins", "addresses", "webhooks", "ledger", "transport"}
    assert groups["addresses"]["python"] is True
    assert groups["webhooks"]["python"] is True
    assert all(groups[name]["python"] is True for name in ("pins", "ledger", "transport"))


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


def test_pins_fixtures_execute() -> None:
    vectors = load("pins-v1.json")
    pins = parse_pins(vectors["canonical_encoding"])
    assert encode_pins(pins) == vectors["canonical_encoding"]
    for rejection in vectors["rejections"]:
        value = rejection["input"]
        if rejection["name"] == "oversize":
            value = "ppay_pins_v1." + "A" * 22000
        if rejection["name"] == "mode_mismatch":
            with pytest.raises(ConfigurationError):
                PhalaPay("ppay_sk_live_" + "x" * 50, pins=replace(pins, livemode=False))
            continue
        with pytest.raises((ValueError, ConfigurationError)):
            parse_pins(value)


def test_ledger_fixtures_execute() -> None:
    vectors = load("ledger-v1.json")
    for case in vectors["deposit_net_amount_cases"]:
        assert deposit_net_amount(case["deposit"]) == case["expected"]
    for case in vectors["convergence_cases"]:
        previous = None
        total = 0
        try:
            for event in case["events"]:
                change = balance_delta(previous, event)
                total += change.delta
                assert change.contribution == deposit_net_amount(change.snapshot)
                previous = change.snapshot
        except LedgerSnapshotError:
            assert case.get("error") == "LedgerSnapshotError"
        else:
            assert "error" not in case
            assert total == case["expected_delta"]


def test_ledger_reference_validation() -> None:
    base = {"id": "dep_x", "livemode": False, "client_reference_id": "c", "currency": "usd"}
    for status in ("pending", "rejected", "reversed"):
        assert (
            deposit_net_amount(
                {
                    **base,
                    "status": status,
                    "amount": None,
                    "amount_refunded": 0,
                    "amount_reversed": 0,
                }
            )
            == 0
        )
    with pytest.raises(LedgerSnapshotError):
        deposit_net_amount(
            {
                **base,
                "status": "credited",
                "amount": None,
                "amount_refunded": 0,
                "amount_reversed": 0,
            }
        )
    valued = {
        **base,
        "status": "pending",
        "amount": None,
        "amount_refunded": 0,
        "amount_reversed": 0,
    }
    assert balance_delta(valued, {**valued, "amount": 100}).snapshot["amount"] == 100
    with pytest.raises(LedgerSnapshotError):
        balance_delta({**valued, "amount": 100}, {**valued, "amount": 101})
    with pytest.raises(LedgerSnapshotError):
        deposit_net_amount(
            {
                **base,
                "status": "credited",
                "amount": 100,
                "amount_refunded": 1,
                "amount_reversed": 1,
            }
        )
    with pytest.raises(LedgerSnapshotError):
        deposit_net_amount(
            {
                **base,
                "status": "reversed",
                "amount": 100,
                "amount_refunded": 0,
                "amount_reversed": 1,
            }
        )


def test_transport_fixtures_execute_with_mock_transport() -> None:
    for scenario in load("transport-v1.json")["scenarios"]:
        seen: list[httpx.Request] = []
        responses = list(scenario["responses"])

        def handler(request: httpx.Request) -> httpx.Response:
            seen.append(request)  # noqa: B023
            status = responses.pop(0)  # noqa: B023
            return httpx.Response(
                status,
                request=request,
                headers={"retry-after": str(scenario.get("retry_after_seconds"))}  # noqa: B023
                if status == 503 and scenario.get("retry_after_seconds")  # noqa: B023
                else {},
            )

        client = TopupClient(
            "http://127.0.0.1",
            "ppay_sk_test_" + "x" * 50,
            transport=httpx.MockTransport(handler),
            request_deadline=scenario.get("deadline_seconds", 60),
            sleep=lambda _: None,
        )

        def operation() -> Any:
            response = client._client.get_httpx_client().request(scenario["method"], "/v1/test")  # noqa: B023
            if response.status_code >= 400:
                raise _ErrorResponseError(response)
            if response.status_code < 400:
                return SimpleNamespace(parsed=object(), status_code=200, headers={})
            return response

        try:
            if scenario["method"] == "DELETE" or scenario.get("retry_after_seconds"):
                with pytest.raises(ApiError):
                    client._call(operation, object, retryable=scenario["method"] != "DELETE")
            else:
                client._call(operation, object)
        finally:
            client.close()
        assert len(seen) == scenario["expected_attempts"]


def test_transport_origin_date_retry_and_cancellation_rules() -> None:
    assert _seconds("3") == 3
    assert _seconds(formatdate()) is not None
    for value in (
        "http://user:pass@example.com",
        "https://example.com/v1?x=1",
        "https://example.com/#x",
        "https://example.com/prefix",
    ):
        with pytest.raises(ValueError, match="api_base"):
            TopupClient(value, "ppay_sk_live_" + "x" * 50)
    client = TopupClient("http://127.0.0.1", "ppay_sk_test_" + "x" * 50, sleep=lambda _: None)
    try:

        def cancelled() -> Any:
            raise KeyboardInterrupt

        with pytest.raises(KeyboardInterrupt):
            client._call(cancelled, object)
    finally:
        client.close()
