"""Shared read-only fixtures and constructor-based transport hooks for SDK tests."""

from __future__ import annotations

import base64
import json
import time
import zlib
from collections.abc import Callable
from functools import partial
from pathlib import Path
from typing import Any
from unittest import mock

import httpx
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

from phala_pay import PhalaPay, Pins
from topup_sdk import TopupClient, deposit_address, quote_address, sign_webhook

API_KEY = "ppay_sk_test_" + "B" * 43 + "000000"
SERVICE_KEY = Ed25519PrivateKey.from_private_bytes(bytes([9] * 32))
SERVICE_PUBLIC_KEY = (
    "whpk_"
    + base64.b64encode(
        SERVICE_KEY.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    ).decode()
)
QUOTE_ID = "qt_" + "0c" * 16
EVENT_ID = "evt_" + "26" * 16
REFUND_ID = "re_" + "0d" * 16
ADDRESS = "0x" + "11" * 20
QUOTE_TERMS = {
    "quote_ttl_seconds": 900,
    "quote_spread_bps": 50,
    "quote_tolerance_bps": 100,
    "quote_amount_decimals": 4,
    "min_amount": 100,
    "min_deposit_atomic": "0",
    "max_deposit_atomic": "1000000000000000000000000",
    "min_refund_atomic": "1",
    "confirmations": "2",
}
ACCOUNT = "acct_" + "0c" * 16
FACTORY = "0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d"
IMPLEMENTATION = "0xfeb1871c9897251C74b39DFC74e577888290faE6"
TREASURY = "0x0000000000000000000000000000000000007EA5"
DEPOSIT_ADDRESS_ID = "da_" + "0d" * 16
SECRET = QUOTE_ID + "_secret_" + "ab" * 24
FIXTURES = Path(__file__).resolve().parents[2] / "fixtures"
CLIENT_API_KEY = "ppay_sk_test_" + "A" * 43 + "000000"
CLIENT_ACCOUNT = "acct_" + "0a" * 16
RUST_KEY = Ed25519PrivateKey.from_private_bytes(bytes([7] * 32))
RUST_ID = "evt_018d5f8e8a7b7d65bc442c4f5f0a6d31"
RUST_TIMESTAMP = 1_674_087_231
RUST_BODY = b'{"type":"deposit.confirmed","data":{"deposit_id":"dep_123"}}'
RUST_SIGNATURE = (
    "v1a,YuPb4kzXzDJqX8EcTFjrfDziMBFmzlPS3V/ISzdG/7R3KS7G1TVLRBF7DOJGnAtOjjvfeFm1G32KO67JiiY0BQ=="
)
EPOCH = 1_790_000_000


def _quote(**fields: object) -> dict[str, object]:
    address = quote_address(
        FACTORY,
        IMPLEMENTATION,
        TREASURY,
        account=ACCOUNT,
        client_reference_id="team-42",
        quote_id=QUOTE_ID,
    )
    return {
        "id": QUOTE_ID,
        "object": "quote",
        "livemode": False,
        "client_reference_id": "team-42",
        "treasury": TREASURY.lower(),
        "metadata": {},
        "amount": 2500,
        "currency": "usd",
        "chain_id": 11155111,
        "asset": "pha",
        "amount_atomic": "100",
        "exchange_rate": "25.00000000",
        "address": address,
        "payment_uri": f"ethereum:0x{'22' * 20}@11155111/transfer?address={address}&uint256=100",
        "status": "open",
        "expires_at": 1_790_000_900,
        "created": 1_790_000_000,
        "payment": None,
        "deposit": None,
        "terms": QUOTE_TERMS,
        **fields,
    }


def _deposit(index: int = 1) -> dict[str, object]:
    return {
        "id": f"dep_{index:032x}",
        "object": "deposit",
        "livemode": False,
        "client_reference_id": "team-42",
        "quote": QUOTE_ID,
        "deposit_address": None,
        "status": "credited",
        "final": False,
        "swept": False,
        "rejection_reason": None,
        "chain_id": 11155111,
        "asset": "pha",
        "asset_contract": "0x" + "22" * 20,
        "amount_atomic": "100",
        "amount": 2500,
        "currency": "usd",
        "exchange_rate": "25.00000000",
        "price_source": "quote",
        "valued_at": 1_790_000_300,
        "address": ADDRESS,
        "from_address": "0x" + "33" * 20,
        "tx_hash": "0x" + "ab" * 32,
        "receipt_log_index": index,
        "revision": 0,
        "log_index": index,
        "block_number": 1,
        "block_hash": "0x" + "cd" * 32,
        "block_time": 1_790_000_290,
        "amount_refunded_atomic": "0",
        "refunded": False,
        "amount_refunded": 0,
        "amount_reversed": 0,
        "created": 1_790_000_300,
        "metadata": {"order_id": "6735"},
    }


def _network(chain_id: int, address: str, treasury: str = TREASURY) -> dict[str, object]:
    return {
        "chain_id": chain_id,
        "address": address,
        "treasury": treasury.lower(),
        "assets": [
            {
                "asset": "pha",
                "contract": "0x" + "22" * 20,
                "decimals": 18,
                "payment_uri": f"ethereum:0x{'22' * 20}@{chain_id}/transfer?address={address}",
            }
        ],
    }


def _deposit_address(version: int = 1, **fields: object) -> dict[str, object]:
    address = deposit_address(
        FACTORY,
        IMPLEMENTATION,
        TREASURY,
        account=ACCOUNT,
        livemode=False,
        client_reference_id="team-42",
        version=version,
    ).lower()
    return {
        "id": DEPOSIT_ADDRESS_ID,
        "object": "deposit_address",
        "livemode": False,
        "client_reference_id": "team-42",
        "address": address,
        "version": version,
        "salt": "0x" + "00" * 32,
        "status": "active",
        "created": 1_790_000_000,
        "retired_at": None,
        "metadata": {},
        "networks": [_network(11155111, address), _network(84532, address)],
        "payments": [],
        **fields,
    }


def _delivery(
    event_type: str = "deposit.credited",
    obj: dict[str, object] | None = None,
    *,
    event_id: str = EVENT_ID,
    webhook_id: str = EVENT_ID,
    timestamp: int | None = None,
    key: Ed25519PrivateKey | list[Ed25519PrivateKey] = SERVICE_KEY,
    account: str = ACCOUNT,
    livemode: bool = False,
    extra: dict[str, object] | None = None,
) -> tuple[bytes, dict[str, str]]:
    data: dict[str, object] = {"object": _deposit() if obj is None else obj}
    body = json.dumps(
        {
            "id": event_id,
            "object": "event",
            "account": account,
            "livemode": livemode,
            "type": event_type,
            "created": 1_790_000_321,
            "actor": "system",
            "request": None,
            "data": data,
            **(extra or {}),
        }
    ).encode()
    stamp = int(time.time()) if timestamp is None else timestamp
    return body, sign_webhook(key, webhook_id, stamp, body)


def valid_key(*, live: bool = False, restricted: bool = False) -> str:
    body = f"ppay_{'rk' if restricted else 'sk'}_{'live' if live else 'test'}_" + "A" * 43
    crc = zlib.crc32(body.encode())
    alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    checksum = ""
    for _ in range(6):
        crc, digit = divmod(crc, 62)
        checksum = alphabet[digit] + checksum
    return body + checksum


KEY = valid_key()


def pins() -> Pins:
    return Pins(
        "https://service.test",
        ACCOUNT,
        False,
        FACTORY,
        IMPLEMENTATION,
        {11155111: TREASURY, 84532: TREASURY},
        ((1, SERVICE_PUBLIC_KEY),),
    )


def pay(handler: Callable[[httpx.Request], httpx.Response], **options: Any) -> PhalaPay:
    hooks: dict[str, Any] = {
        key: options.pop(key) for key in ("clock", "sleep", "rng", "wall_clock") if key in options
    }
    with mock.patch("phala_pay._client.TopupClient", partial(TopupClient, **hooks)):
        return PhalaPay(KEY, pins=pins(), transport=httpx.MockTransport(handler), **options)


def record_response(
    requests: list[httpx.Request],
    request: httpx.Request,
    response: httpx.Response,
) -> httpx.Response:
    requests.append(request)
    return response


class Clock:
    def __init__(self) -> None:
        self.now = 0.0
        self.delays: list[float] = []

    def __call__(self) -> float:
        return self.now

    def sleep(self, delay: float) -> None:
        self.delays.append(delay)
        self.now += delay


def error(status: int, code: str = "unavailable", **headers: str) -> httpx.Response:
    return httpx.Response(status, json={"error": {"code": code}}, headers=headers)


def load(name: str) -> dict[str, Any]:
    value = json.loads((FIXTURES / name).read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    assert value["schema_version"] == 1
    if name != "manifest.json":
        assert isinstance(value["group"], str)
    return value


def _client_deposit(index: int) -> dict[str, object]:
    return {
        "id": f"dep_{index:032x}",
        "object": "deposit",
        "livemode": False,
        "client_reference_id": "ws 1",
        "quote": QUOTE_ID,
        "deposit_address": None,
        "status": "credited",
        "final": True,
        "swept": False,
        "metadata": {},
        "rejection_reason": None,
        "chain_id": 11155111,
        "asset": "pha",
        "asset_contract": "0x" + "22" * 20,
        "amount_atomic": "1",
        "amount": 1,
        "currency": "usd",
        "exchange_rate": "1.00000000",
        "price_source": "quote",
        "valued_at": EPOCH,
        "address": "0x" + "11" * 20,
        "from_address": "0x" + "33" * 20,
        "tx_hash": "0x" + "ab" * 32,
        "receipt_log_index": index,
        "revision": 0,
        "log_index": index,
        "block_number": 1,
        "block_hash": "0x" + "cd" * 32,
        "block_time": EPOCH,
        "amount_refunded_atomic": "0",
        "refunded": False,
        "amount_refunded": 0,
        "amount_reversed": 0,
        "created": EPOCH,
    }


def merchant(
    handler: Callable[[httpx.Request], httpx.Response], clock: Clock, **options: Any
) -> PhalaPay:
    options.setdefault("clock", clock)
    options.setdefault("sleep", clock.sleep)
    options.setdefault("rng", lambda: 0.0)
    options.setdefault("wall_clock", lambda: EPOCH + clock.now)
    return pay(handler, **options)


def maintenance(*, headers: dict[str, str] | None = None) -> httpx.Response:
    return httpx.Response(
        503,
        json={"error": {"code": "service_maintenance", "message": "Restarting"}},
        headers=headers,
    )


def invoke(client: PhalaPay, method: str, **options: Any) -> None:
    if method == "GET":
        client.quotes.retrieve(QUOTE_ID, **options)
    else:
        client.quotes.create(
            client_reference_id="order-42", amount=2500, chain_id=11155111, asset="pha", **options
        )
