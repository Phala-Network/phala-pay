from __future__ import annotations

import base64
import json
from typing import Any

import httpx
import pytest
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

from topup_client.models import AttestationResponse
from topup_sdk import (
    AttestationError,
    TopupClient,
    attestation_report_data,
    verify_attestation_binding,
)

NONCE = bytes(range(16))
ACCOUNT = "acct_0123456789abcdef0123456789abcdef"
CURRENT = bytes([0x42] * 32)
PREVIOUS = bytes([0x24] * 32)
# The known vectors of `report_data_matches_the_published_vector` in
# crates/adapters/src/attestation.rs.
REPORT_DATA_LIVE_ROTATING = "86da5cb5cfe64def5578b7e416e6354930cd47cf85231f8227c8eb6e1748eda0"
REPORT_DATA_TEST = "919730430f98ac5d7dc845ded45abd8946e7ee4e38035cefb16567324fcbe622"


def _whpk(raw: bytes) -> str:
    return "whpk_" + base64.b64encode(raw).decode()


def _response(**overrides: Any) -> dict[str, Any]:
    body: dict[str, Any] = {
        "object": "attestation",
        "account": ACCOUNT,
        "livemode": True,
        "webhook_keys": [
            {"version": 2, "public_key": _whpk(CURRENT), "expires_at": None},
            {"version": 1, "public_key": _whpk(PREVIOUS), "expires_at": 1_790_000_000},
        ],
        "report_data": REPORT_DATA_LIVE_ROTATING,
        "tdx_quote": "",
    }
    body.update(overrides)
    return body


def test_report_data_matches_the_rust_vectors() -> None:
    rotating = attestation_report_data(NONCE, ACCOUNT, True, [(2, CURRENT), (1, PREVIOUS)])
    assert rotating.hex() == REPORT_DATA_LIVE_ROTATING
    assert attestation_report_data(NONCE, ACCOUNT, False, [(2, CURRENT)]).hex() == REPORT_DATA_TEST


def test_binding_returns_the_keys_current_first() -> None:
    keys = verify_attestation_binding(
        AttestationResponse.from_dict(_response()),
        NONCE,
        expected_account=ACCOUNT,
        expected_livemode=True,
    )
    raw = [key.public_bytes(Encoding.Raw, PublicFormat.Raw) for key in keys]
    assert raw == [CURRENT, PREVIOUS]


@pytest.mark.parametrize(
    "overrides",
    [
        {"account": "acct_" + "f" * 32},
        {"livemode": False},
        {"webhook_keys": [{"version": 2, "public_key": _whpk(bytes([0x43] * 32))}]},
        {"webhook_keys": [{"version": 2, "public_key": _whpk(CURRENT)}]},
        {"webhook_keys": [{"version": 2, "public_key": _whpk(CURRENT[:31])}]},
        # A key in any other form than `whpk_` is malformed.
        {
            "webhook_keys": [
                {"version": 2, "public_key": CURRENT.hex()},
                {"version": 1, "public_key": _whpk(PREVIOUS)},
            ]
        },
        {"webhook_keys": []},
        {"report_data": "00" * 32},
        {"report_data": "not hex"},
    ],
)
def test_bindings_that_do_not_match_are_rejected(overrides: dict[str, Any]) -> None:
    with pytest.raises(AttestationError):
        verify_attestation_binding(AttestationResponse.from_dict(_response(**overrides)), NONCE)


@pytest.mark.parametrize(
    ("expected_account", "expected_livemode", "match"),
    [("acct_" + "f" * 32, True, "another account"), (ACCOUNT, False, "other mode")],
)
def test_an_attestation_of_another_account_or_mode_is_refused(
    expected_account: str, expected_livemode: bool, match: str
) -> None:
    with pytest.raises(AttestationError, match=match):
        verify_attestation_binding(
            AttestationResponse.from_dict(_response()),
            NONCE,
            expected_account=expected_account,
            expected_livemode=expected_livemode,
        )


def test_client_attestation_is_authenticated_and_verifies_the_binding() -> None:
    forged = _response(webhook_keys=[{"version": 2, "public_key": _whpk(bytes([0x43] * 32))}])
    bodies = [_response(), forged]
    api_key = "ppay_sk_live_" + "C" * 43 + "000000"

    def respond(request: httpx.Request) -> httpx.Response:
        assert request.url.params["nonce"] == NONCE.hex()
        assert request.headers["authorization"] == f"Bearer {api_key}"
        return httpx.Response(200, json=bodies.pop(0))

    with TopupClient(
        "https://service.test:8080", api_key, transport=httpx.MockTransport(respond)
    ) as client:
        evidence = client.attestation(NONCE)
        assert evidence.account == ACCOUNT
        assert evidence.webhook_keys[0].public_key == _whpk(CURRENT)
        with pytest.raises(AttestationError):
            client.attestation(NONCE)


def test_roll_webhook_key_posts_the_overlap_once_with_an_idempotency_key() -> None:
    seen: list[httpx.Request] = []
    account = {
        "id": ACCOUNT,
        "object": "account",
        "livemode": False,
        "name": "Acme",
        "charges_enabled": False,
        "paused_scopes": [],
        "webhook_keys": [
            {"version": 2, "expires_at": None},
            {"version": 1, "expires_at": 1_790_003_600},
        ],
        "created": 1_790_000_000,
    }

    def respond(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, json=account)

    api_key = "ppay_sk_test_" + "C" * 43 + "000000"
    with TopupClient(
        "https://service.test:8080", api_key, transport=httpx.MockTransport(respond)
    ) as client:
        rolled = client.roll_webhook_key(expires_in=3600)
    assert [key.version for key in rolled.webhook_keys] == [2, 1]
    (request,) = seen
    assert request.url.path == "/v1/account/webhook_keys/roll"
    assert json.loads(request.content) == {"expires_in": 3600}
    assert request.headers["idempotency-key"]
