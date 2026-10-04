"""The FastAPI example: quote creation for the checkout and idempotent webhook fulfillment."""

from __future__ import annotations

import json
import sqlite3
import sys
import time
from pathlib import Path

import httpx
import pytest
from fastapi.testclient import TestClient

from phala_pay import PhalaPay
from topup_sdk import sign_webhook

from .test_phala_pay import (
    ACCOUNT,
    API_KEY,
    EVENT_ID,
    FACTORY,
    IMPLEMENTATION,
    QUOTE_ID,
    SERVICE_KEY,
    SERVICE_PUBLIC_KEY,
    _deposit,
)
from .test_phala_pay import _quote as quote_object

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "examples"))

from fastapi_app import create_app

SECRET = f"{QUOTE_ID}_secret_{'ab' * 24}"


@pytest.fixture
def app(tmp_path: Path) -> tuple[TestClient, list[httpx.Request], Path]:
    requests: list[httpx.Request] = []

    def service(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        body = json.loads(request.content)
        if body["amount"] < 100:
            return httpx.Response(
                400,
                json={
                    "error": {
                        "type": "invalid_request_error",
                        "code": "amount_too_small",
                        "message": "internal detail",
                        "param": "amount",
                        "doc_url": "https://phala-network.github.io/phala-pay/#section/Errors/amount_too_small",
                    }
                },
            )
        return httpx.Response(200, json=quote_object(client_secret=SECRET))

    client = PhalaPay(
        "https://service.test",
        API_KEY,
        account=ACCOUNT,
        forwarder=(FACTORY, IMPLEMENTATION),
        transport=httpx.MockTransport(service),
    )
    database = tmp_path / "product.sqlite3"
    api = create_app(
        client,
        [SERVICE_PUBLIC_KEY],
        str(database),
        account=ACCOUNT,
        livemode=False,
        chain_id=11155111,
        asset="pha",
    )
    return TestClient(api), requests, database


def test_topup_returns_the_client_secret_and_keys_the_quote_by_order(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, requests, _ = app
    response = http.post("/topups", json={"amount": 2500}, headers={"x-team-id": "team-42"})
    assert response.status_code == 200
    assert response.json()["client_secret"] == SECRET
    assert response.json()["expected_address"] == quote_object()["address"]
    sent = requests[0]
    assert json.loads(sent.content)["client_reference_id"] == "team-42"
    assert json.loads(sent.content)["metadata"] == {"order_id": response.json()["order_id"]}
    assert sent.headers["idempotency-key"] == f'"{response.json()["order_id"]}"'


def test_topup_passes_on_only_the_error_code(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, _, _ = app
    response = http.post("/topups", json={"amount": 99}, headers={"x-team-id": "team-42"})
    assert response.status_code == 400
    assert response.json() == {"detail": {"code": "amount_too_small"}}
    assert http.post("/topups", json={"amount": 0}, headers={"x-team-id": "t"}).status_code == 422
    assert http.post("/topups", json={"amount": 100}).status_code == 422


def _delivery(
    event_type: str, event_id: str = EVENT_ID, account: str = ACCOUNT, **deposit: object
) -> tuple[bytes, dict[str, str]]:
    body = json.dumps(
        {
            "id": event_id,
            "object": "event",
            "account": account,
            "livemode": False,
            "type": event_type,
            "created": 1_790_000_321,
            "actor": "system",
            "request": None,
            "data": {"object": _deposit() | deposit},
        }
    ).encode()
    return body, sign_webhook(SERVICE_KEY, event_id, int(time.time()), body)


def _credited(body_amount: int = 2500, account: str = ACCOUNT) -> tuple[bytes, dict[str, str]]:
    return _delivery("deposit.credited", account=account, amount=body_amount)


def _deliver(http: TestClient, delivery: tuple[bytes, dict[str, str]]) -> None:
    body, headers = delivery
    response = http.post("/webhooks/phala-pay", content=body, headers=headers)
    assert response.status_code == 200


def _balance(database: Path) -> int:
    with sqlite3.connect(database) as db:
        row = db.execute("SELECT amount FROM balances WHERE team = 'team-42'").fetchone()
    return 0 if row is None else int(row[0])


def _event_id(number: int) -> str:
    return f"evt_{number:032x}"


def test_webhook_credits_each_deposit_once(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, _, database = app
    for _ in range(2):
        _deliver(http, _credited())
    with sqlite3.connect(database) as db:
        assert db.execute("SELECT team, amount FROM balances").fetchall() == [("team-42", 2500)]
        assert db.execute("SELECT count(*) FROM deposits").fetchone() == (1,)


def test_partial_refunds_take_back_their_share_in_any_order(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, _, database = app
    credited = _credited()
    third = _delivery(
        "deposit.refunded", _event_id(1), amount_refunded_atomic="33", amount_refunded=825
    )
    whole = _delivery(
        "deposit.refunded",
        _event_id(2),
        amount_refunded_atomic="100",
        amount_refunded=2500,
        refunded=True,
    )
    # The first refund's snapshot arrives before the credit it takes back from.
    _deliver(http, third)
    assert _balance(database) == 2500 - 825
    _deliver(http, credited)
    assert _balance(database) == 2500 - 825
    _deliver(http, whole)
    assert _balance(database) == 0
    # A late or repeated older snapshot never gives the refunded credit back.
    for delivery in (third, credited, third):
        _deliver(http, delivery)
    assert _balance(database) == 0


def test_a_reversal_delivered_before_the_credit_nets_to_zero(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, _, database = app
    reversed_ = _delivery("deposit.reversed", _event_id(3), status="reversed", amount_reversed=2500)
    _deliver(http, reversed_)
    assert _balance(database) == 0
    _deliver(http, _credited())
    assert _balance(database) == 0
    with sqlite3.connect(database) as db:
        assert db.execute("SELECT status, applied FROM deposits").fetchall() == [("reversed", 0)]


def test_a_reversal_after_the_credit_takes_it_back(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, _, database = app
    _deliver(http, _credited())
    assert _balance(database) == 2500
    _deliver(
        http,
        _delivery("deposit.reversed", _event_id(4), status="reversed", amount_reversed=2500),
    )
    assert _balance(database) == 0


def test_webhook_refuses_a_forged_delivery(
    app: tuple[TestClient, list[httpx.Request], Path],
) -> None:
    http, _, database = app
    body, headers = _credited()
    forged = body.replace(b'"amount": 2500', b'"amount": 999999')
    assert http.post("/webhooks/phala-pay", content=forged, headers=headers).status_code == 400
    assert http.post("/webhooks/phala-pay", content=body).status_code == 400
    # Signed with the pinned key, but another account's event: refused.
    other_body, other_headers = _credited(account="acct_" + "b2" * 16)
    response = http.post("/webhooks/phala-pay", content=other_body, headers=other_headers)
    assert response.status_code == 400
    with sqlite3.connect(database) as db:
        assert db.execute("SELECT count(*) FROM balances").fetchone() == (0,)
