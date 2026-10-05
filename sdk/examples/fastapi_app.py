"""A minimal product backend on FastAPI with Phala Pay: quotes for the checkout, and webhook
fulfillment.

- `POST /topups` `{"amount": 2500}` creates a quote for the signed-in team and returns its
  `client_secret` and the `expected_address` the SDK recomputed, which the browser passes to
  `<Checkout clientSecret apiBase expectedAddress />`. The order id rides along as the quote's
  `metadata`, so it arrives in the deposit's `metadata` in `deposit.credited`.
- `POST /webhooks/phala-pay` verifies each delivery with `pay.webhooks.construct_event`, which
  fails closed unless it is signed by your account's key in the key's mode and names your account
  and mode, then applies every `deposit.*` event to the team's balance before answering `200`.

The balance is driven by the deposit snapshot each `deposit.*` event carries, never by the event
type or its arrival order: events may arrive out of order, repeated, or after a later one. Per
deposit, serially (one transaction, the deposit's row locked), the stored view and the snapshot
are merged: the later `status` wins (`pending`, then `credited` or `rejected`, then `reversed`)
and the larger cumulative `amount_refunded` and `amount_reversed` win. The deposit's contribution
is then `amount - amount_refunded - amount_reversed` cents while it is `credited` or `reversed`,
and 0 otherwise; the balance moves by the difference from what the deposit contributed before. A
partial refund takes back its pro-rata share of the credit, a reversal all of it, and a
`deposit.reversed` delivered before `deposit.credited` nets to zero at once.

Run it against staging (install with `uv add phala-pay fastapi uvicorn`):

    PHALA_PAY_API_KEY=ppay_rk_test_... \\
    PHALA_PAY_PINS=ppay_pins_v1.... \\
    uvicorn --factory fastapi_app:app_from_env
"""

from __future__ import annotations

import logging
import os
import sqlite3
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Annotated

import httpx
from fastapi import Depends, FastAPI, Header, HTTPException, Request
from pydantic import BaseModel, Field

from phala_pay import ApiError, Deposit, PhalaPay, SignatureVerificationError

LOG = logging.getLogger(__name__)

# How far each status is along a deposit's life; a snapshot never moves a deposit back.
STATUS_RANK = {"pending": 0, "credited": 1, "rejected": 1, "reversed": 2}

SCHEMA = """
CREATE TABLE IF NOT EXISTS orders (
    id TEXT PRIMARY KEY, team TEXT NOT NULL, amount INTEGER NOT NULL, quote TEXT
);
-- One row per deposit: the merged snapshot, and the cents it contributes to the team's balance.
CREATE TABLE IF NOT EXISTS deposits (
    deposit TEXT PRIMARY KEY, team TEXT NOT NULL, status TEXT NOT NULL,
    amount INTEGER NOT NULL, amount_refunded INTEGER NOT NULL, amount_reversed INTEGER NOT NULL,
    applied INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS balances (team TEXT PRIMARY KEY, amount INTEGER NOT NULL);
"""


class TopupRequest(BaseModel):
    amount: int = Field(gt=0, le=10_000_000, description="US cents")


class TopupResponse(BaseModel):
    order_id: str
    client_secret: str
    expected_address: str


def apply_deposit(db: sqlite3.Connection, deposit: Deposit) -> int:
    """Merges a deposit snapshot into the stored view and moves the team's balance by the change
    in the deposit's contribution; returns that change. Run it in the transaction that locks the
    deposit's row (SQLite's `BEGIN IMMEDIATE` locks the database; on PostgreSQL, insert the row if
    missing and `SELECT ... FOR UPDATE` it), so deliveries of one deposit apply one at a time."""
    amount = deposit.amount if isinstance(deposit.amount, int) else 0
    row = db.execute(
        "SELECT status, amount, amount_refunded, amount_reversed, applied FROM deposits "
        "WHERE deposit = ?",
        (deposit.id,),
    ).fetchone()
    status, stored_amount, refunded, reversed_, applied = row or ("pending", 0, 0, 0, 0)
    if STATUS_RANK.get(deposit.status, 0) > STATUS_RANK.get(status, 0):
        status = deposit.status
    # The credit is fixed once valued: keep the first amount seen.
    amount = stored_amount or amount
    refunded = max(refunded, deposit.amount_refunded)
    reversed_ = max(reversed_, deposit.amount_reversed)
    contribution = amount - refunded - reversed_ if status in ("credited", "reversed") else 0
    db.execute(
        "INSERT INTO deposits (deposit, team, status, amount, amount_refunded, amount_reversed, "
        "applied) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (deposit) DO UPDATE SET "
        "status = excluded.status, amount = excluded.amount, "
        "amount_refunded = excluded.amount_refunded, amount_reversed = excluded.amount_reversed, "
        "applied = excluded.applied",
        (
            deposit.id,
            deposit.client_reference_id,
            status,
            amount,
            refunded,
            reversed_,
            contribution,
        ),
    )
    change = contribution - applied
    if change:
        db.execute(
            "INSERT INTO balances (team, amount) VALUES (?, ?) "
            "ON CONFLICT (team) DO UPDATE SET amount = amount + excluded.amount",
            (deposit.client_reference_id, change),
        )
    return change


def current_team(x_team_id: Annotated[str, Header(pattern=r"^[A-Za-z0-9._-]{1,64}$")]) -> str:
    """Stands in for the product's session: replace it with your authentication."""
    return x_team_id


def create_app(
    pay: PhalaPay,
    database: str,
    *,
    chain_id: int,
    asset: str,
) -> FastAPI:
    app = FastAPI()

    @contextmanager
    def transaction() -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(database, isolation_level="IMMEDIATE")
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    with transaction() as db:
        db.executescript(SCHEMA)

    @app.post("/topups")
    def create_topup(
        body: TopupRequest, team: Annotated[str, Depends(current_team)]
    ) -> TopupResponse:
        order_id = str(uuid.uuid4())
        with transaction() as db:
            db.execute(
                "INSERT INTO orders (id, team, amount) VALUES (?, ?, ?)",
                (order_id, team, body.amount),
            )
        try:
            # The order id as the Idempotency-Key: repeating the call within 24 hours replays the
            # same quote and client secret, for example to resume the checkout after a reload.
            quote = pay.quotes.create(
                client_reference_id=team,
                amount=body.amount,
                chain_id=chain_id,
                asset=asset,
                idempotency_key=order_id,
                metadata={"order_id": order_id},
            )
        except ApiError as error:
            # Codes such as `amount_too_small` are stable and safe to show; messages are not.
            # `503` (`unavailable`, or `service_restoring` after a restore) is worth a retry.
            status = error.status_code if error.status_code in (400, 409, 429, 503) else 502
            raise HTTPException(status, detail={"code": error.code}) from error
        except httpx.HTTPError as error:
            raise HTTPException(503, detail={"code": "unavailable"}) from error
        if not isinstance(quote.client_secret, str):
            raise HTTPException(502, detail={"code": "unexpected_response"})
        with transaction() as db:
            db.execute("UPDATE orders SET quote = ? WHERE id = ?", (quote.id, order_id))
        # `quote.address` was recomputed from the pinned forwarder; the page shows only it.
        return TopupResponse(
            order_id=order_id, client_secret=quote.client_secret, expected_address=quote.address
        )

    @app.post("/webhooks/phala-pay")
    async def webhook(request: Request) -> dict[str, bool]:
        payload = await request.body()
        try:
            event = pay.webhooks.construct_event(payload, request.headers)
        except (SignatureVerificationError, ValueError) as error:
            raise HTTPException(400) from error

        if event.type.startswith("deposit."):
            deposit = event.deposit
            if deposit.status == "credited" and not isinstance(deposit.amount, int):
                # A credited deposit always has an amount; a 5xx makes the service retry.
                raise HTTPException(500)
            with transaction() as db:
                change = apply_deposit(db, deposit)
            LOG.info("%s %s: balance %+d", event.type, deposit.id, change)
        return {"received": True}

    return app


def app_from_env() -> FastAPI:
    pay = PhalaPay.from_env()
    return create_app(
        pay,
        os.environ.get("DATABASE", "topups.sqlite3"),
        chain_id=int(os.environ.get("PHALA_PAY_CHAIN_ID", "11155111")),
        asset=os.environ.get("PHALA_PAY_ASSET", "pha"),
    )
