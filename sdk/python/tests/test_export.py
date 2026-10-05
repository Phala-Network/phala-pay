"""`export_account`: every list endpoint, paged through into one JSON file per resource."""

from __future__ import annotations

import json
from pathlib import Path

import httpx

from topup_sdk import TopupClient, export_account

from ._support import CLIENT_ACCOUNT as ACCOUNT
from ._support import CLIENT_API_KEY as API_KEY
from ._support import EPOCH
from ._support import _client_deposit as _deposit

LISTS = {
    "/v1/quotes",
    "/v1/deposits",
    "/v1/refunds",
    "/v1/deposit_addresses",
    "/v1/forwarders",
    "/v1/sweeps",
    "/v1/webhook_endpoints",
    "/v1/events",
}
FORWARDER = {
    "id": "fwd_" + "0f" * 16,
    "object": "forwarder",
    "livemode": False,
    "chain_id": 11155111,
    "address": "0x" + "11" * 20,
    "factory": "0x" + "aa" * 20,
    "salt": "0x" + "01" * 32,
    "treasury": "0x" + "cc" * 20,
    "quote": "qt_" + "0c" * 16,
    "deposit_address": None,
    "superseded_at": None,
}


def _service(request: httpx.Request) -> httpx.Response:
    path = request.url.path
    after = request.url.params.get("starting_after")
    if path == "/v1/account":
        return httpx.Response(
            200,
            json={
                "id": ACCOUNT,
                "object": "account",
                "livemode": False,
                "name": "Acme",
                "charges_enabled": False,
                "paused_scopes": [],
                "webhook_keys": [],
                "created": EPOCH,
            },
        )
    if path == "/v1/payment_settings":
        return httpx.Response(
            200,
            json={
                "object": "payment_settings",
                "livemode": False,
                "status": "unconfigured",
                "revision": "psrev_" + "05" * 16,
                "updated": EPOCH,
                "quote_creations_per_customer_per_minute": None,
                "chains": [],
                "available": [],
            },
        )
    if path == "/v1/config":
        return httpx.Response(
            200,
            json={
                "object": "config",
                "livemode": False,
                "currency": "usd",
                "max_open_quotes": 1,
                "quote_creations_per_customer_per_minute": 10,
                "max_open_amount_per_account": 1,
                "max_open_amount_per_customer": 1,
                "assets": [],
            },
        )
    if path == "/v1/balance":
        return httpx.Response(200, json={"object": "balance", "livemode": False, "unswept": []})
    if path in ("/v1/api_keys", "/v1/treasuries"):
        return httpx.Response(
            200, json={"object": "list", "url": path, "has_more": False, "data": []}
        )
    assert path in LISTS, path
    data: list[object] = []
    has_more = False
    if path == "/v1/deposits":
        # Two pages: the export follows `has_more`.
        data = [_deposit(2)] if after is None else [_deposit(1)]
        has_more = after is None
    if path == "/v1/forwarders":
        data = [FORWARDER]
    return httpx.Response(
        200, json={"object": "list", "url": path, "has_more": has_more, "data": data}
    )


def test_every_resource_is_written_and_paged_through(tmp_path: Path) -> None:
    client = TopupClient("https://service.test", API_KEY, transport=httpx.MockTransport(_service))
    counts = export_account(client, tmp_path / "export")
    assert (counts["deposits"], counts["forwarders"], counts["account"]) == (2, 1, 1)
    assert counts["payment_settings"] == 1
    written = {path.name for path in (tmp_path / "export").iterdir()}
    assert written == {f"{name}.json" for name in counts}
    forwarders = json.loads((tmp_path / "export" / "forwarders.json").read_text())
    assert forwarders == [FORWARDER]
    deposits = json.loads((tmp_path / "export" / "deposits.json").read_text())
    assert [deposit["log_index"] for deposit in deposits] == [2, 1]
