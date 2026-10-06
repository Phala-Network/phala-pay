"""The product ledger: SQLite standing in for Phala Cloud's database."""

from __future__ import annotations

import json
import os
import sqlite3
import threading
import time
from collections.abc import Callable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ORDER_FLOW_CODE = "crypto-top-up"
ORDER_PROVIDER = "crypto_topup"


def _backfill_event_refs(db: sqlite3.Connection) -> None:
    for event_id, data in db.execute("SELECT id, data FROM webhook_events"):
        ProductLedger._record_event_refs(db, event_id, json.loads(data))


def _backfill_quote_statuses(db: sqlite3.Connection) -> None:
    for event_type, data in db.execute(
        "SELECT type, data FROM webhook_events WHERE type LIKE 'quote.%' ORDER BY received_at, id"
    ):
        try:
            event_data = json.loads(data)
        except (json.JSONDecodeError, TypeError):
            continue
        if isinstance(event_data, Mapping):
            ProductLedger._record_quote_webhook_status(db, event_type, event_data)
    for quote_id, response in db.execute("SELECT id, response FROM quote_records"):
        try:
            quote = json.loads(response)
        except (json.JSONDecodeError, TypeError):
            continue
        if isinstance(quote, Mapping):
            ProductLedger._record_quote_status(db, quote_id, quote.get("status"))


MIGRATIONS: tuple[tuple[tuple[str, ...], Callable[[sqlite3.Connection], None] | None], ...] = (
    (
        (
            """
            CREATE TABLE IF NOT EXISTS teams (
                id TEXT PRIMARY KEY,
                suspended INTEGER NOT NULL DEFAULT 0
            );
            """,
            """
            -- team_id is NULL only for credits held because they name no known workspace.
            CREATE TABLE IF NOT EXISTS orders (
                id TEXT PRIMARY KEY,
                team_id TEXT REFERENCES teams (id),
                provider TEXT NOT NULL,
                order_flow_code TEXT NOT NULL,
                provider_order_id TEXT NOT NULL,
                payload TEXT NOT NULL,
                status TEXT NOT NULL,
                reason TEXT,
                credit_transaction_id TEXT,
                created_at REAL NOT NULL
            );
            """,
            """
            -- Phala Cloud's partial unique index; the key is checked across teams before insert.
            CREATE UNIQUE INDEX IF NOT EXISTS orders_crypto_topup_provider_order
                ON orders (team_id, provider_order_id) WHERE order_flow_code = 'crypto-top-up';
            """,
            """
            CREATE INDEX IF NOT EXISTS orders_flow_provider_order
                ON orders (order_flow_code, provider_order_id);
            """,
            """
            CREATE INDEX IF NOT EXISTS orders_team ON orders (team_id);
            """,
            """
            CREATE TABLE IF NOT EXISTS credit_transactions (
                id TEXT PRIMARY KEY,
                team_id TEXT NOT NULL REFERENCES teams (id),
                order_id TEXT NOT NULL UNIQUE REFERENCES orders (id),
                amount_minor INTEGER NOT NULL CHECK (amount_minor > 0),
                funding_source TEXT NOT NULL,
                created_at REAL NOT NULL
            );
            """,
            """
            -- The latest merged snapshot of each deposit (`deposit.*` carries the whole deposit):
            -- the later status and the larger cumulative claw-backs win, regardless of event order.
            CREATE TABLE IF NOT EXISTS deposit_snapshots (
                provider_order_id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                amount_refunded_minor INTEGER NOT NULL CHECK (amount_refunded_minor >= 0),
                amount_reversed_minor INTEGER NOT NULL CHECK (amount_reversed_minor >= 0)
            );
            """,
            """
            -- Changes to a credited order after its credit: refunds and reversals take it back.
            CREATE TABLE IF NOT EXISTS credit_adjustments (
                id TEXT PRIMARY KEY,
                team_id TEXT NOT NULL REFERENCES teams (id),
                order_id TEXT NOT NULL REFERENCES orders (id),
                amount_minor INTEGER NOT NULL CHECK (amount_minor <> 0),
                reason TEXT NOT NULL,
                created_at REAL NOT NULL
            );
            """,
            """
            -- The product's own promotion on an accepted order (`bonus_bps`), beside the credit:
            -- the grant, then its claw-backs as refunds and reversals net the credit down.
            -- An order's rows sum to its current bonus. This is the product's amount;
            -- the service never sees it.
            CREATE TABLE IF NOT EXISTS bonus_credits (
                id TEXT PRIMARY KEY,
                team_id TEXT NOT NULL REFERENCES teams (id),
                order_id TEXT NOT NULL REFERENCES orders (id),
                amount_minor INTEGER NOT NULL CHECK (amount_minor <> 0),
                reason TEXT NOT NULL,
                created_at REAL NOT NULL
            );
            """,
            """
            CREATE INDEX IF NOT EXISTS credit_adjustments_order ON credit_adjustments (order_id);
            """,
            """
            CREATE INDEX IF NOT EXISTS credit_adjustments_team ON credit_adjustments (team_id);
            """,
            """
            CREATE INDEX IF NOT EXISTS bonus_credits_order ON bonus_credits (order_id);
            """,
            """
            CREATE INDEX IF NOT EXISTS bonus_credits_team ON bonus_credits (team_id);
            """,
            """
            -- The webhook inbox: every verified delivery once, by its `webhook-id` (the event's
            -- `evt_` id), committed with its ledger effect. `data` is the event's parsed `data`,
            -- for the product's own reads. `body` and the three Standard Webhooks headers preserve
            -- the delivery exactly as received, as evidence for a service restore
            -- (deploy/runbooks/restore.md, step 5).
            CREATE TABLE IF NOT EXISTS webhook_events (
                id TEXT PRIMARY KEY,
                type TEXT NOT NULL,
                data TEXT NOT NULL,
                received_at REAL NOT NULL,
                body BLOB NOT NULL,
                webhook_timestamp TEXT NOT NULL,
                webhook_signature TEXT NOT NULL
            );
            """,
            """
            -- Each quote and deposit address the product created, as the service returned it,
            -- including its `client_secret`: the merchant's records for re-issuing in a restore
            -- (deploy/runbooks/restore.md, step 4). A client secret is a capability:
            -- the ledger file is readable by its owner only, and nothing logs it.
            -- `recorded_at` is when the product last got the response.
            CREATE TABLE IF NOT EXISTS quote_records (
                id TEXT PRIMARY KEY,
                team_id TEXT NOT NULL REFERENCES teams (id),
                response TEXT NOT NULL,
                recorded_at REAL NOT NULL
            );
            """,
            """
            CREATE TABLE IF NOT EXISTS deposit_address_records (
                id TEXT PRIMARY KEY,
                team_id TEXT NOT NULL REFERENCES teams (id),
                response TEXT NOT NULL,
                recorded_at REAL NOT NULL
            );
            """,
            """
            -- The demo console's (reference_product.demo): each visitor's quotes and their
            -- creation request in `api` for the developer view; deposit address; and refunds.
            CREATE TABLE IF NOT EXISTS demo_quotes (
                id TEXT PRIMARY KEY,
                account TEXT NOT NULL REFERENCES teams (id),
                amount INTEGER NOT NULL,
                amount_atomic TEXT NOT NULL,
                exchange_rate TEXT NOT NULL,
                address TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                created INTEGER NOT NULL,
                api TEXT NOT NULL,
                asset TEXT NOT NULL,
                chain_id INTEGER NOT NULL
            );
            """,
            """
            CREATE INDEX IF NOT EXISTS demo_quotes_account ON demo_quotes (account, created);
            """,
            """
            CREATE TABLE IF NOT EXISTS demo_deposit_addresses (
                account TEXT PRIMARY KEY REFERENCES teams (id),
                id TEXT NOT NULL,
                created INTEGER NOT NULL
            );
            """,
            """
            CREATE TABLE IF NOT EXISTS demo_refunds (
                id TEXT PRIMARY KEY,
                account TEXT NOT NULL REFERENCES teams (id),
                deposit TEXT NOT NULL,
                created INTEGER NOT NULL
            );
            """,
        ),
        None,
    ),
    (
        (
            """
            CREATE TABLE IF NOT EXISTS event_refs (
                ref TEXT NOT NULL,
                event_id TEXT NOT NULL REFERENCES webhook_events (id),
                PRIMARY KEY (ref, event_id)
            );
            """,
        ),
        _backfill_event_refs,
    ),
    (
        (
            """
            -- The latest quote state learned from a service response or verified quote webhook.
            -- Non-terminal `open` remains useful evidence, while the account view still falls
            -- back to the stored expiry when no terminal state has been learned.
            CREATE TABLE IF NOT EXISTS quote_statuses (
                quote_id TEXT PRIMARY KEY,
                status TEXT NOT NULL
            );
            """,
        ),
        _backfill_quote_statuses,
    ),
)
SCHEMA_VERSION = len(MIGRATIONS)


@dataclass(frozen=True)
class Delivery:
    """A webhook delivery as the receiver got it: the Standard Webhooks headers and the raw body,
    byte for byte. Only this, not a re-serialized event, verifies against the service's key."""

    webhook_id: str
    webhook_timestamp: str
    webhook_signature: str
    body: bytes


# How far each deposit status is along the deposit's life; a snapshot never moves it back.
STATUS_RANK = {"pending": 0, "credited": 1, "rejected": 1, "reversed": 2}
QUOTE_STATUSES = frozenset({"open", "complete", "expired", "canceled"})
TERMINAL_QUOTE_STATUSES = frozenset({"complete", "expired", "canceled"})


@dataclass(frozen=True)
class DepositView:
    """A deposit's merged snapshot: its furthest status and cumulative claw-backs, in cents."""

    status: str
    amount_refunded: int
    amount_reversed: int

    def merge(self, other: DepositView) -> DepositView:
        status = (
            other.status
            if STATUS_RANK.get(other.status, 0) > STATUS_RANK.get(self.status, 0)
            else self.status
        )
        return DepositView(
            status,
            max(self.amount_refunded, other.amount_refunded),
            max(self.amount_reversed, other.amount_reversed),
        )

    def contribution(self, credited: int) -> int:
        """What a credit of `credited` cents nets to: less the claw-backs while the deposit is
        `credited` or `reversed` (a reversal takes back all of it), nothing otherwise."""
        if self.status not in ("credited", "reversed"):
            return 0
        return max(0, credited - self.amount_refunded - self.amount_reversed)


@dataclass(frozen=True)
class StoredOrder:
    provider_order_id: str
    team_id: str | None
    payload: dict[str, Any]
    status: str
    reason: str | None
    credit_transaction_id: str | None
    id: str


class ProductLedger:
    """SQLite stand-in for the product database; every write is one serialized transaction."""

    def __init__(self, path: str = ":memory:", *, read_only: bool = False) -> None:
        """Opens the ledger at `path`, creating its tables if it has none; with `read_only`, opens
        an existing ledger for reading only (the restore export next to a running product): no
        file is created or changed."""
        self._lock = threading.RLock()
        self.events_changed = threading.Condition(self._lock)
        if read_only:
            uri = Path(path).absolute().as_uri() + "?mode=ro"
            self._connection = sqlite3.connect(uri, uri=True, check_same_thread=False)
            return
        if path != ":memory:":
            # The ledger holds client secrets: owner-only, and SQLite gives its journal the
            # database file's mode.
            os.close(os.open(path, os.O_CREAT | os.O_RDWR, 0o600))
            os.chmod(path, 0o600)
        self._connection = sqlite3.connect(path, check_same_thread=False, isolation_level=None)
        self._connection.execute("PRAGMA foreign_keys = ON")
        try:
            self._migrate()
        except BaseException:
            self._connection.close()
            raise

    def _migrate(self) -> None:
        """Upgrade unversioned and versioned ledgers atomically, under SQLite's write lock."""
        with self.transaction() as db:
            version = db.execute("PRAGMA user_version").fetchone()[0]
            if version > SCHEMA_VERSION:
                raise ValueError("ledger schema is newer than this product")
            for target_version, (statements, backfill) in enumerate(MIGRATIONS, start=1):
                if target_version <= version:
                    continue
                for statement in statements:
                    db.execute(statement)
                if backfill is not None:
                    backfill(db)
                # target_version comes only from the local migration tuple, not external input.
                db.execute(f"PRAGMA user_version = {target_version}")

    @contextmanager
    def transaction(self) -> Iterator[sqlite3.Connection]:
        with self._lock:
            self._connection.execute("BEGIN IMMEDIATE")
            try:
                yield self._connection
            except BaseException:
                self._connection.execute("ROLLBACK")
                raise
            self._connection.execute("COMMIT")

    def add_team(self, team_id: str, *, suspended: bool = False) -> None:
        with self.transaction() as db:
            db.execute(
                "INSERT INTO teams (id, suspended) VALUES (?, ?) "
                "ON CONFLICT (id) DO UPDATE SET suspended = excluded.suspended",
                (team_id, int(suspended)),
            )

    def team_suspended(self, team_id: str) -> bool | None:
        with self._lock:
            row = self._connection.execute(
                "SELECT suspended FROM teams WHERE id = ?", (team_id,)
            ).fetchone()
        return None if row is None else bool(row[0])

    def record_quote(self, team_id: str, quote: Mapping[str, Any]) -> None:
        """Records a quote the service created for the workspace, as it returned it (with its
        `client_secret`)."""
        with self.transaction() as db:
            db.execute(
                "INSERT INTO quote_records (id, team_id, response, recorded_at) "
                "VALUES (?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
                (quote["id"], team_id, json.dumps(quote, sort_keys=True), time.time()),
            )
            self._record_quote_status(db, quote.get("id"), quote.get("status"))

    def record_quote_status(self, quote: Mapping[str, Any]) -> None:
        """Records a quote status from a service response already fetched by the product."""
        with self.transaction() as db:
            self._record_quote_status(db, quote.get("id"), quote.get("status"))

    @staticmethod
    def _record_quote_status(db: sqlite3.Connection, quote_id: object, status: object) -> None:
        if not isinstance(quote_id, str) or not isinstance(status, str):
            return
        if status not in QUOTE_STATUSES:
            return
        current = db.execute(
            "SELECT status FROM quote_statuses WHERE quote_id = ?", (quote_id,)
        ).fetchone()
        # A quote's terminal state cannot be undone by a stale open object fetched while a
        # webhook is being delivered. Terminal-to-terminal updates are retained in receive order.
        if current is not None and current[0] in TERMINAL_QUOTE_STATUSES and status == "open":
            return
        db.execute(
            "INSERT INTO quote_statuses (quote_id, status) VALUES (?, ?) "
            "ON CONFLICT (quote_id) DO UPDATE SET status = excluded.status",
            (quote_id, status),
        )

    def record_deposit_address(self, team_id: str, address: Mapping[str, Any]) -> None:
        """Records a deposit address the service returned for the workspace, as it returned it.
        A later response (a fresh `client_secret`) replaces it."""
        with self.transaction() as db:
            db.execute(
                "INSERT INTO deposit_address_records (id, team_id, response, recorded_at) "
                "VALUES (?, ?, ?, ?) ON CONFLICT (id) DO UPDATE SET "
                "response = excluded.response, recorded_at = excluded.recorded_at",
                (address["id"], team_id, json.dumps(address, sort_keys=True), time.time()),
            )

    def quote_records(self) -> list[tuple[dict[str, Any], float]]:
        """Every recorded quote response, with when it was recorded, oldest first."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT response, recorded_at FROM quote_records ORDER BY recorded_at, id"
            ).fetchall()
        return [(json.loads(row[0]), float(row[1])) for row in rows]

    def deposit_address_records(self) -> list[tuple[dict[str, Any], float]]:
        """Every recorded deposit address response, with when it was last recorded, oldest
        first."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT response, recorded_at FROM deposit_address_records ORDER BY recorded_at, id"
            ).fetchall()
        return [(json.loads(row[0]), float(row[1])) for row in rows]

    def find_order(self, provider_order_id: str) -> StoredOrder | None:
        with self._lock:
            return self._find_order(self._connection, provider_order_id)

    def credited_since(self, db: sqlite3.Connection, team_id: str, since: float) -> int:
        row = db.execute(
            "SELECT COALESCE(SUM(amount_minor), 0) FROM credit_transactions "
            "WHERE team_id = ? AND created_at >= ?",
            (team_id, since),
        ).fetchone()
        return int(row[0])

    def credits_for(self, team_id: str) -> list[tuple[str, int]]:
        with self._lock:
            rows = self._connection.execute(
                "SELECT o.provider_order_id, c.amount_minor FROM credit_transactions c "
                "JOIN orders o ON o.id = c.order_id WHERE c.team_id = ? ORDER BY c.created_at",
                (team_id,),
            ).fetchall()
        return [(str(key), int(amount)) for key, amount in rows]

    def balance_for(self, team_id: str) -> int:
        """The workspace's crypto top-up balance: its credits less their claw-backs, and its
        bonuses less theirs."""
        with self._lock:
            row = self._connection.execute(
                "SELECT (SELECT COALESCE(SUM(amount_minor), 0) FROM credit_transactions "
                "WHERE team_id = ?) + (SELECT COALESCE(SUM(amount_minor), 0) "
                "FROM credit_adjustments WHERE team_id = ?) + (SELECT "
                "COALESCE(SUM(amount_minor), 0) FROM bonus_credits WHERE team_id = ?)",
                (team_id, team_id, team_id),
            ).fetchone()
        return int(row[0])

    def bonuses_for(self, team_id: str) -> list[tuple[str, int, str]]:
        """The workspace's bonus lines: `(provider_order_id, amount_minor, reason)`, oldest
        first."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT o.provider_order_id, b.amount_minor, b.reason FROM bonus_credits b "
                "JOIN orders o ON o.id = b.order_id WHERE b.team_id = ? ORDER BY b.created_at",
                (team_id,),
            ).fetchall()
        return [(str(key), int(amount), str(reason)) for key, amount, reason in rows]

    def adjustments_for(self, team_id: str) -> list[tuple[str, int, str]]:
        """The workspace's claw-backs: `(provider_order_id, amount_minor, reason)`, oldest first."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT o.provider_order_id, a.amount_minor, a.reason FROM credit_adjustments a "
                "JOIN orders o ON o.id = a.order_id WHERE a.team_id = ? ORDER BY a.created_at",
                (team_id,),
            ).fetchall()
        return [(str(key), int(amount), str(reason)) for key, amount, reason in rows]

    @staticmethod
    def merge_snapshot(
        db: sqlite3.Connection, provider_order_id: str, snapshot: DepositView
    ) -> DepositView:
        """Merges `snapshot` into the deposit's stored view and returns the result."""
        row = db.execute(
            "SELECT status, amount_refunded_minor, amount_reversed_minor FROM deposit_snapshots "
            "WHERE provider_order_id = ?",
            (provider_order_id,),
        ).fetchone()
        merged = snapshot if row is None else DepositView(row[0], row[1], row[2]).merge(snapshot)
        db.execute(
            "INSERT INTO deposit_snapshots (provider_order_id, status, amount_refunded_minor, "
            "amount_reversed_minor) VALUES (?, ?, ?, ?) ON CONFLICT (provider_order_id) DO UPDATE "
            "SET status = excluded.status, amount_refunded_minor = excluded.amount_refunded_minor, "
            "amount_reversed_minor = excluded.amount_reversed_minor",
            (provider_order_id, merged.status, merged.amount_refunded, merged.amount_reversed),
        )
        return merged

    @staticmethod
    def order_amounts(db: sqlite3.Connection, order_id: str) -> tuple[int, int]:
        """An order's credit and what it nets to after its adjustments, in cents."""
        row = db.execute(
            "SELECT (SELECT COALESCE(SUM(amount_minor), 0) FROM credit_transactions "
            "WHERE order_id = ?), (SELECT COALESCE(SUM(amount_minor), 0) FROM credit_adjustments "
            "WHERE order_id = ?)",
            (order_id, order_id),
        ).fetchone()
        return int(row[0]), int(row[0]) + int(row[1])

    @staticmethod
    def order_bonus(db: sqlite3.Connection, order_id: str) -> int:
        """An order's current bonus, in cents: its grant less its claw-backs."""
        row = db.execute(
            "SELECT COALESCE(SUM(amount_minor), 0) FROM bonus_credits WHERE order_id = ?",
            (order_id,),
        ).fetchone()
        return int(row[0])

    def orders_for(self, team_id: str) -> list[dict[str, Any]]:
        """The workspace's crypto top-up orders: `accepted` (credited) or `held` (refused)."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT provider_order_id, status, reason FROM orders "
                "WHERE team_id = ? AND order_flow_code = ? ORDER BY created_at",
                (team_id, ORDER_FLOW_CODE),
            ).fetchall()
        return [
            {"provider_order_id": key, "status": status, "reason": reason}
            for key, status, reason in rows
        ]

    @staticmethod
    def stored_body(db: sqlite3.Connection, webhook_id: str) -> bytes | None:
        """The raw body of the delivery recorded as `webhook_id`, or `None` when there is none."""
        row = db.execute("SELECT body FROM webhook_events WHERE id = ?", (webhook_id,)).fetchone()
        return None if row is None else bytes(row[0])

    def record_delivery(
        self, db: sqlite3.Connection, delivery: Delivery, event_type: str, data: Mapping[str, Any]
    ) -> None:
        """Adds a verified delivery to the inbox, in `db`'s transaction with its ledger effect."""
        db.execute(
            "INSERT INTO webhook_events (id, type, data, received_at, body, webhook_timestamp, "
            "webhook_signature) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                delivery.webhook_id,
                event_type,
                json.dumps(data, sort_keys=True),
                time.time(),
                delivery.body,
                delivery.webhook_timestamp,
                delivery.webhook_signature,
            ),
        )
        self._record_quote_webhook_status(db, event_type, data)
        self._record_event_refs(db, delivery.webhook_id, data)
        self.events_changed.notify_all()

    @classmethod
    def _record_quote_webhook_status(
        cls, db: sqlite3.Connection, event_type: str, data: Mapping[str, Any]
    ) -> None:
        obj = data.get("object")
        if not isinstance(obj, Mapping):
            return
        quote_id = obj.get("id")
        if not isinstance(quote_id, str):
            return
        status: object = obj.get("status")
        if event_type == "quote.canceled":
            status = "canceled"
        elif event_type == "quote.expired":
            status = "expired"
        if event_type.startswith("quote."):
            cls._record_quote_status(db, quote_id, status)

    @staticmethod
    def _record_event_refs(db: sqlite3.Connection, event_id: str, data: object) -> None:
        if not isinstance(data, Mapping):
            return
        inner = data.get("object")
        sources = [data, inner] if isinstance(inner, dict) else [data]
        refs = {
            value
            for source in sources
            for key in ("deposit_id", "id", "quote", "deposit", "client_reference_id")
            if isinstance(value := source.get(key), str)
        }
        db.executemany(
            "INSERT OR IGNORE INTO event_refs (ref, event_id) VALUES (?, ?)",
            [(ref, event_id) for ref in refs],
        )

    @staticmethod
    def event_rows(db: sqlite3.Connection, refs: set[str]) -> list[Any]:
        return db.execute(
            "SELECT id, type, data, received_at FROM webhook_events "
            "WHERE id IN (SELECT event_id FROM event_refs "
            "WHERE ref IN (SELECT value FROM json_each(?))) ORDER BY received_at",
            (json.dumps(sorted(refs)),),
        ).fetchall()

    def events_for(self, refs: set[str]) -> list[dict[str, Any]]:
        """Stored events naming these object ids or workspace references, oldest first."""
        with self._lock:
            rows = self.event_rows(self._connection, refs)
        return [{"type": row[1], "data": json.loads(row[2])} for row in rows]

    def deliveries(self, event_types: Sequence[str]) -> list[tuple[Delivery, float]]:
        """The inbox's deliveries of `event_types` with their raw evidence and when each was
        received, oldest first."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT id, webhook_timestamp, webhook_signature, body, received_at "
                "FROM webhook_events "
                "WHERE type IN (SELECT value FROM json_each(?)) "
                "ORDER BY received_at, id",
                (json.dumps(list(event_types)),),
            ).fetchall()
        return [(Delivery(row[0], row[1], row[2], bytes(row[3])), float(row[4])) for row in rows]

    def events(self, event_type: str) -> list[dict[str, Any]]:
        with self._lock:
            rows = self._connection.execute(
                "SELECT data FROM webhook_events WHERE type = ? ORDER BY received_at", (event_type,)
            ).fetchall()
        return [json.loads(row[0]) for row in rows]

    def all_events(self) -> list[dict[str, Any]]:
        """Every stored webhook event as `{"type", "data"}`, oldest first."""
        with self._lock:
            rows = self._connection.execute(
                "SELECT type, data FROM webhook_events ORDER BY received_at"
            ).fetchall()
        return [{"type": row[0], "data": json.loads(row[1])} for row in rows]

    def wait_for_event(
        self, event_type: str, matches: Callable[[dict[str, Any]], bool], timeout: float
    ) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        with self.events_changed:
            while True:
                for event in self.events(event_type):
                    if matches(event):
                        return event
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"no matching {event_type} webhook within {timeout:.0f}s")
                self.events_changed.wait(remaining)

    @staticmethod
    def _find_order(db: sqlite3.Connection, provider_order_id: str) -> StoredOrder | None:
        row = db.execute(
            "SELECT provider_order_id, team_id, payload, status, reason, credit_transaction_id, id "
            "FROM orders WHERE order_flow_code = ? AND provider_order_id = ?",
            (ORDER_FLOW_CODE, provider_order_id),
        ).fetchone()
        if row is None:
            return None
        return StoredOrder(row[0], row[1], json.loads(row[2]), row[3], row[4], row[5], row[6])
