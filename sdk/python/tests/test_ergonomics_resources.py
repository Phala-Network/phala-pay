"""Exercise every public resource route and its request controls through real generated calls."""

from __future__ import annotations

import json
from typing import Any

import httpx
import pytest

from phala_pay import ApiError, Deposit

from .test_ergonomics_transport import pay, record_response
from .test_phala_pay import _deposit, _quote

OPERATIONS = [
    (
        "quotes",
        "create",
        "/v1/quotes",
        "POST",
        (),
        {"client_reference_id": "team-42", "amount": 2500, "chain_id": 11155111, "asset": "pha"},
    ),
    ("quotes", "retrieve", "/v1/quotes/qt_1", "GET", ("qt_1",), {}),
    ("quotes", "update", "/v1/quotes/qt_1", "POST", ("qt_1",), {}),
    ("quotes", "cancel", "/v1/quotes/qt_1/cancel", "POST", ("qt_1",), {}),
    (
        "deposit_addresses",
        "create",
        "/v1/deposit_addresses",
        "POST",
        (),
        {"client_reference_id": "team-42"},
    ),
    ("deposit_addresses", "retrieve", "/v1/deposit_addresses/da_1", "GET", ("da_1",), {}),
    ("deposit_addresses", "update", "/v1/deposit_addresses/da_1", "POST", ("da_1",), {}),
    ("deposit_addresses", "rotate", "/v1/deposit_addresses/da_1/rotate", "POST", ("da_1",), {}),
    ("deposits", "retrieve", "/v1/deposits/dep_1", "GET", ("dep_1",), {}),
    ("deposits", "update", "/v1/deposits/dep_1", "POST", ("dep_1",), {}),
    (
        "refunds",
        "create",
        "/v1/refunds",
        "POST",
        (),
        {"deposit": "dep_1", "destination_address": "0x" + "11" * 20},
    ),
    ("refunds", "retrieve", "/v1/refunds/re_1", "GET", ("re_1",), {}),
    ("refunds", "update", "/v1/refunds/re_1", "POST", ("re_1",), {}),
    ("refunds", "cancel", "/v1/refunds/re_1/cancel", "POST", ("re_1",), {}),
    (
        "refunds",
        "mark_paid",
        "/v1/refunds/re_1/mark_paid",
        "POST",
        ("re_1",),
        {"transaction_hash": "0x" + "22" * 32},
    ),
    ("payment_settings", "retrieve", "/v1/payment_settings", "GET", (), {}),
    ("payment_settings", "update", "/v1/payment_settings", "POST", (), {}),
    ("config", "retrieve", "/v1/config", "GET", (), {}),
    ("balance", "retrieve", "/v1/balance", "GET", (), {}),
    ("account", "retrieve", "/v1/account", "GET", (), {}),
    ("account", "pause_quotes", "/v1/account/pause", "POST", (), {}),
    ("account", "resume_quotes", "/v1/account/resume", "POST", (), {}),
    ("account", "roll_webhook_key", "/v1/account/webhook_keys/roll", "POST", (), {}),
    (
        "treasuries",
        "challenge",
        "/v1/treasuries/challenge",
        "POST",
        (),
        {"chain_id": 1, "address": "0x" + "11" * 20},
    ),
    (
        "treasuries",
        "create",
        "/v1/treasuries",
        "POST",
        (),
        {"chain_id": 1, "message": "signed challenge", "signature": "0x12"},
    ),
    ("treasuries", "retrieve", "/v1/treasuries/tr_1", "GET", ("tr_1",), {}),
    ("treasuries", "cancel", "/v1/treasuries/tr_1/cancel", "POST", ("tr_1",), {}),
    ("treasuries", "pause", "/v1/treasuries/tr_1/pause", "POST", ("tr_1",), {}),
    ("treasuries", "resume", "/v1/treasuries/tr_1/resume", "POST", ("tr_1",), {}),
    ("api_keys", "create", "/v1/api_keys", "POST", (), {}),
    ("api_keys", "retrieve", "/v1/api_keys/key_1", "GET", ("key_1",), {}),
    ("api_keys", "roll", "/v1/api_keys/key_1/roll", "POST", ("key_1",), {}),
    ("api_keys", "revoke", "/v1/api_keys/key_1", "DELETE", ("key_1",), {}),
    (
        "webhook_endpoints",
        "create",
        "/v1/webhook_endpoints",
        "POST",
        (),
        {"url": "https://merchant.test/webhook", "enabled_events": ["deposit.credited"]},
    ),
    ("webhook_endpoints", "retrieve", "/v1/webhook_endpoints/we_1", "GET", ("we_1",), {}),
    ("webhook_endpoints", "update", "/v1/webhook_endpoints/we_1", "POST", ("we_1",), {}),
    ("webhook_endpoints", "delete", "/v1/webhook_endpoints/we_1", "DELETE", ("we_1",), {}),
    ("webhook_endpoints", "test", "/v1/webhook_endpoints/we_1/test", "POST", ("we_1",), {}),
    ("events", "retrieve", "/v1/events/evt_1", "GET", ("evt_1",), {}),
    (
        "events",
        "resend",
        "/v1/events/evt_1/resend",
        "POST",
        ("evt_1",),
        {"webhook_endpoint": "we_1"},
    ),
]


@pytest.mark.parametrize(("resource", "method", "path", "verb", "args", "params"), OPERATIONS)
def test_every_resource_method_uses_single_transport_and_explicit_controls(  # noqa: PLR0917
    resource: str, method: str, path: str, verb: str, args: tuple[str, ...], params: dict[str, Any]
) -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(400, json={"error": {"code": "controlled"}})

    controls: dict[str, Any] = {"request_deadline": 0.25, "upgrade_tolerance": True}
    if verb == "POST":
        controls["idempotency_key"] = "order-1"
    with pay(handler) as client:
        operation = getattr(getattr(client, resource), method)
        with pytest.raises(TypeError, match="unexpected keyword argument"):
            operation(*args, **params, **controls, unexpected_option=True)
        assert requests == []
        with pytest.raises(ApiError):
            operation(*args, **params, **controls)
    assert len(requests) == 1
    assert requests[0].method == verb
    assert requests[0].url.path == path
    assert 0 < requests[0].extensions["timeout"]["read"] <= 0.25
    if verb == "POST":
        assert requests[0].headers["idempotency-key"] == '"order-1"'


@pytest.mark.parametrize(
    "resource",
    [
        "quotes",
        "deposit_addresses",
        "deposits",
        "refunds",
        "treasuries",
        "api_keys",
        "webhook_endpoints",
        "events",
        "sweeps",
        "forwarders",
    ],
)
def test_every_list_page_passes_limit_cursor_and_deadline(resource: str) -> None:
    requests: list[httpx.Request] = []
    with pay(
        lambda r: record_response(
            requests,
            r,
            httpx.Response(
                200, json={"object": "list", "url": r.url.path, "has_more": False, "data": []}
            ),
        )
    ) as client:
        target = getattr(client, resource)
        for method in (target.list, target.list_page):
            with pytest.raises(TypeError, match="unexpected keyword argument"):
                method(unexpected_option=True)
        assert requests == []
        assert target.list_page(
            limit=7, starting_after="cursor /?", request_deadline=0.25, upgrade_tolerance=True
        ) == {
            "data": [],
            "has_more": False,
        }
        result = target.list(
            limit=7, starting_after="cursor /?", request_deadline=0.25, upgrade_tolerance=True
        )
        if resource in {"api_keys", "treasuries"}:
            assert isinstance(result, list)
        else:
            assert iter(result) is result
        assert list(result) == []
    assert len(requests) == 2
    for request in requests:
        assert request.url.path == f"/v1/{resource}"
        assert request.url.params["limit"] == "7"
        assert request.url.params["starting_after"] == "cursor /?"
        assert 0 < request.extensions["timeout"]["read"] <= 0.25


@pytest.mark.parametrize("permissions", [None, [], ["quotes.write"]])
def test_omitted_permissions_create_secret_key_and_lists_create_restricted_keys(
    permissions: list[str] | None,
) -> None:
    requests: list[httpx.Request] = []
    body = {
        "object": "api_key",
        "id": "key_1",
        "livemode": False,
        "created": 1,
        "name": "runtime",
        "redacted": "prefix-only",
        "status": "active",
        "type": "secret" if permissions is None else "restricted",
        "permissions": permissions,
    }
    with pay(lambda r: record_response(requests, r, httpx.Response(200, json=body))) as client:
        key = client.api_keys.create(permissions=permissions)
    sent = json.loads(requests[0].content)
    if permissions is None:
        assert "permissions" not in sent
        assert "type" not in sent
        assert key.type_ == "secret"
    else:
        assert sent["permissions"] == permissions
        assert sent["type"] == "restricted"
        assert key.type_ == "restricted"


def test_nullable_absent_metadata_clears_and_expandables_are_preserved() -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        body = (
            _quote(deposit=_deposit())
            if request.url.path.startswith("/v1/quotes")
            else {
                "object": "payment_settings",
                "livemode": False,
                "status": "unconfigured",
                "revision": "psrev_1",
                "updated": 1,
                "quote_creations_per_customer_per_minute": None,
                "chains": [],
                "available": [],
            }
        )
        return httpx.Response(200, json=body)

    with pay(handler) as client:
        quote = client.quotes.retrieve("qt_1", expand=["deposit"])
        assert requests[0].url.params["expand[]"] == "deposit"
        assert isinstance(quote.deposit, Deposit)
        assert quote.deposit.amount == 2500
        client.payment_settings.update(quote_creations_per_customer_per_minute=None)
        client.payment_settings.update()
        client.quotes.update("qt_1", metadata="")
        client.quotes.update("qt_1", metadata={"old": ""})
    assert json.loads(requests[1].content) == {"quote_creations_per_customer_per_minute": None}
    assert json.loads(requests[2].content) == {}
    assert json.loads(requests[3].content) == {"metadata": ""}
    assert json.loads(requests[4].content) == {"metadata": {"old": ""}}
