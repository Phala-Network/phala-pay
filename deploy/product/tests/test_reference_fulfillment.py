"""The reference product credits each `deposit.credited` once and holds what it refuses, keeps
each delivery as received, and exports the records a service restore asks for."""

from __future__ import annotations

import json
import sqlite3
import sys
import threading
import time
import uuid
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import create_autospec

import httpx
import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from starlette.testclient import TestClient

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from reference_product import server
from reference_product.__main__ import write_records
from reference_product.config import (
    DRIVER_KEYID,
    ChainConfig,
    MintableToken,
    MissingProductKeyError,
    ProductConfig,
)
from reference_product.fulfillment import Answer, Fulfillment, PinnedKeys, TransientError
from reference_product.ledger import MIGRATIONS, SCHEMA_VERSION, Delivery, ProductLedger
from reference_product.restore_records import export_restore_records
from reference_product.server import (
    AccountApi,
    ProductServer,
    WebhookKeys,
    _make_product,
    pin_webhook_keys,
)
from topup_sdk import (
    RequestSigner,
    TopupClient,
    credited_event_id,
    load_public_key,
    sign_webhook,
    verify_webhook_signature,
)
from topup_sdk.addresses import deposit_id
from topup_sdk.signing import sf_string

TEAM = "team-1"
SERVICE_KEY = Ed25519PrivateKey.from_private_bytes(bytes([3] * 32))

CHAIN_ID = 11155111
TOKEN = "0x" + "44" * 20
CONFIG = ProductConfig(
    service_url="http://service.test",
    account="acct_" + "ac" * 16,
    api_key_file="unused",
    chains=(
        ChainConfig(
            chain_id=CHAIN_ID,
            name="Sepolia",
            rpc_url="https://rpc.test",
            treasury="0x0000000000000000000000000000000000007EA5",
            test_tokens=(MintableToken("PHA", TOKEN),),
        ),
    ),
    factory="0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d",
    implementation="0xfeb1871c9897251C74b39DFC74e577888290faE6",
    listen_host="127.0.0.1",
    listen_port=0,
    public_url="https://acme.example/topup",
    payer="0x" + "55" * 20,
    per_deposit_cap_minor=10_000,
    per_period_cap_minor=15_000,
)


# The terms a quote was issued with (`Quote.terms`), as the service resolves them from a route's
# defaults.
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


def _create_ledger_schema(db: sqlite3.Connection, *, version: int) -> None:
    for statements, backfill in MIGRATIONS[:version]:
        for statement in statements:
            db.execute(statement)
        if backfill is not None:
            backfill(db)


def _fulfillment(
    *, suspended: bool = False, config: ProductConfig = CONFIG, ledger: ProductLedger | None = None
) -> Fulfillment:
    if ledger is None:
        ledger = ProductLedger()
        ledger.add_team(TEAM, suspended=suspended)
    pinned = PinnedKeys(livemode=False, keys=[SERVICE_KEY.public_key()])
    return Fulfillment(config, ledger, lambda: pinned)


def _credited(
    number: int = 1,
    amount_minor: int = 2_500,
    team: str = TEAM,
    *,
    account: str = CONFIG.account,
    livemode: bool = False,
    event_type: str = "deposit.credited",
    **fields: object,
) -> tuple[dict[str, str], bytes]:
    """A signed `deposit.*` delivery about deposit `number`; `fields` override its object."""
    tx_hash = "0x" + f"{number:02x}" * 32
    deposit = deposit_id(CHAIN_ID, tx_hash, 0)
    event_id = (
        credited_event_id(deposit)
        if event_type == "deposit.credited"
        else "evt_" + uuid.uuid4().hex
    )
    return _delivery(
        event_type,
        {
            "id": deposit,
            "object": "deposit",
            "livemode": livemode,
            "client_reference_id": team,
            "quote": "qt_" + "0c" * 16,
            "deposit_address": None,
            "status": "credited",
            "final": False,
            "swept": False,
            "metadata": {},
            "rejection_reason": None,
            "chain_id": CHAIN_ID,
            "asset": "pha",
            "asset_contract": TOKEN,
            "amount_atomic": "25000000000000000000",
            "amount": amount_minor,
            "currency": "usd",
            "exchange_rate": "0.10000000",
            "price_source": "quote",
            "valued_at": 1_790_410_320,
            "address": "0x" + "66" * 20,
            "from_address": "0x" + "77" * 20,
            "tx_hash": tx_hash,
            "receipt_log_index": 0,
            "revision": 0,
            "log_index": 0,
            "block_number": 100,
            "block_hash": "0x" + "10" * 32,
            "block_time": 1_790_410_290,
            "amount_refunded_atomic": "0",
            "refunded": False,
            "amount_refunded": 0,
            "amount_reversed": 0,
            "created": 1_790_410_300,
        }
        | fields,
        event_id=event_id,
        account=account,
        livemode=livemode,
    )


def _delivery(
    event_type: str,
    obj: dict[str, Any],
    *,
    event_id: str | None = None,
    account: str = CONFIG.account,
    livemode: bool = False,
    created: int = 1_790_410_321,
) -> tuple[dict[str, str], bytes]:
    """A signed delivery of an event about `obj`, as the service sends it."""
    event_id = event_id or "evt_" + uuid.uuid4().hex
    body = json.dumps(
        {
            "id": event_id,
            "object": "event",
            "account": account,
            "livemode": livemode,
            "type": event_type,
            "created": created,
            "actor": "system",
            "request": None,
            "data": {"object": obj},
        }
    ).encode()
    return sign_webhook(SERVICE_KEY, event_id, int(time.time()), body), body


def test_a_credit_is_applied_once_across_redeliveries() -> None:
    fulfillment = _fulfillment()
    headers, body = _credited()
    for _ in range(3):
        assert fulfillment.handle(headers, body).status == 204
    [(key, amount)] = fulfillment.ledger.credits_for(TEAM)
    assert amount == 2_500
    assert key.startswith("dep_")
    assert len(fulfillment.ledger.events("deposit.credited")) == 1


# The product's own promotion: +10% on credits paid in PHA.
BONUS = replace(CONFIG, bonus_bps={"pha": 1000})


def test_a_pha_credit_earns_a_separate_bonus_line_once() -> None:
    fulfillment = _fulfillment(config=BONUS)
    headers, body = _credited(amount_minor=2_505)
    for _ in range(3):
        assert fulfillment.handle(headers, body).status == 204
    ledger = fulfillment.ledger
    # The credit is the service's exact USD value; the bonus, rounded down to cents, is its own
    # line.
    [(key, credit)] = ledger.credits_for(TEAM)
    assert credit == 2_505
    assert ledger.bonuses_for(TEAM) == [(key, 250, "PHA bonus +10%")]
    assert ledger.balance_for(TEAM) == 2_505 + 250


def test_refunds_claw_the_bonus_back_in_proportion() -> None:
    fulfillment = _fulfillment(config=BONUS)
    third = _credited(event_type="deposit.refunded", amount_refunded=833)
    whole = _credited(event_type="deposit.refunded", amount_refunded=2_500, refunded=True)
    assert fulfillment.handle(*_credited()).status == 204
    for _ in range(2):
        assert fulfillment.handle(*third).status == 204
    ledger = fulfillment.ledger
    # Nets to 1667: its bonus is 166, so the refund takes back 84 of the 250.
    assert [(amount, reason) for _, amount, reason in ledger.bonuses_for(TEAM)] == [
        (250, "PHA bonus +10%"),
        (-84, "deposit.refunded"),
    ]
    assert ledger.balance_for(TEAM) == 1_667 + 166
    for _ in range(2):
        assert fulfillment.handle(*whole).status == 204
    assert sum(amount for _, amount, _ in ledger.bonuses_for(TEAM)) == 0
    assert ledger.balance_for(TEAM) == 0


def test_a_reversal_takes_the_whole_bonus_back_in_either_order() -> None:
    reversal = _credited(event_type="deposit.reversed", status="reversed", amount_reversed=2_500)
    after = _fulfillment(config=BONUS)
    assert after.handle(*_credited()).status == 204
    assert after.handle(*reversal).status == 204
    assert [amount for _, amount, _ in after.ledger.bonuses_for(TEAM)] == [250, -250]
    assert after.ledger.balance_for(TEAM) == 0
    before = _fulfillment(config=BONUS)
    assert before.handle(*reversal).status == 204
    assert before.handle(*_credited()).status == 204
    assert before.ledger.bonuses_for(TEAM) == []
    assert before.ledger.balance_for(TEAM) == 0


def test_a_refund_delivered_before_the_credit_grants_only_the_netted_bonus() -> None:
    fulfillment = _fulfillment(config=BONUS)
    refund = _credited(event_type="deposit.refunded", amount_refunded=500)
    assert fulfillment.handle(*refund).status == 204
    assert fulfillment.handle(*_credited()).status == 204
    assert [amount for _, amount, _ in fulfillment.ledger.bonuses_for(TEAM)] == [200]
    assert fulfillment.ledger.balance_for(TEAM) == 2_000 + 200


def test_other_assets_and_held_credits_earn_no_bonus() -> None:
    usdc = _fulfillment(config=BONUS)
    assert usdc.handle(*_credited(asset="usdc")).status == 204
    assert usdc.ledger.bonuses_for(TEAM) == []
    assert usdc.ledger.balance_for(TEAM) == 2_500
    held = _fulfillment(config=BONUS, suspended=True)
    assert held.handle(*_credited()).status == 204
    assert held.ledger.bonuses_for(TEAM) == []
    assert held.ledger.balance_for(TEAM) == 0


def test_a_bonus_keeps_the_rate_it_was_granted_at() -> None:
    granted = _fulfillment(config=BONUS)
    assert granted.handle(*_credited()).status == 204
    # The promotion ends; a later refund still claws back at the granted 10%.
    ended = _fulfillment(ledger=granted.ledger)
    refund = _credited(event_type="deposit.refunded", amount_refunded=1_250)
    assert ended.handle(*refund).status == 204
    assert [amount for _, amount, _ in ended.ledger.bonuses_for(TEAM)] == [250, -125]
    # And a deposit credited after it ends earns none.
    assert ended.handle(*_credited(2)).status == 204
    assert len(ended.ledger.bonuses_for(TEAM)) == 2


def test_partial_refunds_take_back_their_share_of_the_credit() -> None:
    fulfillment = _fulfillment()
    third = _credited(event_type="deposit.refunded", amount_refunded=833)
    whole = _credited(event_type="deposit.refunded", amount_refunded=2_500, refunded=True)
    assert fulfillment.handle(*_credited()).status == 204
    assert fulfillment.handle(*third).status == 204
    assert fulfillment.ledger.balance_for(TEAM) == 2_500 - 833
    # A repeated or late older snapshot never gives a claw-back back.
    for delivery in (third, _credited(), third):
        fulfillment.handle(*delivery)
    assert fulfillment.ledger.balance_for(TEAM) == 2_500 - 833
    fulfillment.handle(*whole)
    assert fulfillment.ledger.balance_for(TEAM) == 0
    [(key, amount)] = fulfillment.ledger.credits_for(TEAM)
    assert amount == 2_500
    assert fulfillment.ledger.adjustments_for(TEAM) == [
        (key, -833, "deposit.refunded"),
        (key, -1_667, "deposit.refunded"),
    ]


def test_a_refund_delivered_before_the_credit_applies_with_it() -> None:
    fulfillment = _fulfillment()
    fulfillment.handle(*_credited(event_type="deposit.refunded", amount_refunded=1_000))
    assert fulfillment.ledger.credits_for(TEAM) == []
    fulfillment.handle(*_credited())
    assert fulfillment.ledger.balance_for(TEAM) == 1_500


def test_a_reversal_takes_the_credit_back_in_either_order() -> None:
    reversal = _credited(event_type="deposit.reversed", status="reversed", amount_reversed=2_500)
    after = _fulfillment()
    after.handle(*_credited())
    after.handle(*reversal)
    assert after.ledger.balance_for(TEAM) == 0
    assert [amount for _, amount, _ in after.ledger.adjustments_for(TEAM)] == [-2_500]

    # Reversed before the credit arrives: nothing is ever credited.
    before = _fulfillment()
    before.handle(*reversal)
    before.handle(*_credited())
    assert before.ledger.balance_for(TEAM) == 0
    assert before.ledger.credits_for(TEAM) == []
    assert before.ledger.orders_for(TEAM) == []


def test_claw_backs_leave_a_held_credit_alone() -> None:
    fulfillment = _fulfillment(suspended=True)
    fulfillment.handle(*_credited())
    fulfillment.handle(*_credited(event_type="deposit.refunded", amount_refunded=2_500))
    assert fulfillment.ledger.adjustments_for(TEAM) == []
    assert [order["status"] for order in fulfillment.ledger.orders_for(TEAM)] == ["held"]


def test_a_forged_delivery_is_refused_without_a_credit() -> None:
    fulfillment = _fulfillment()
    headers, body = _credited()
    forged = sign_webhook(
        Ed25519PrivateKey.generate(), headers["webhook-id"], int(time.time()), body
    )
    assert fulfillment.handle(forged, body).status == 400
    assert fulfillment.handle(headers, body + b" ").status == 400
    assert fulfillment.ledger.credits_for(TEAM) == []


def test_a_repeat_with_another_amount_keeps_the_first_credit() -> None:
    fulfillment = _fulfillment()
    fulfillment.handle(*_credited(amount_minor=2_500))
    fulfillment.handle(*_credited(amount_minor=2_600))
    assert [amount for _, amount in fulfillment.ledger.credits_for(TEAM)] == [2_500]


@pytest.mark.parametrize(
    ("suspended", "amounts", "team", "reason"),
    [
        (True, [2_500], TEAM, "account_suspended"),
        (False, [10_001], TEAM, "per_deposit_cap"),
        (False, [10_000, 6_000], TEAM, "per_period_cap"),
    ],
)
def test_refused_credits_are_held_for_refund(
    suspended: bool, amounts: list[int], team: str, reason: str
) -> None:
    fulfillment = _fulfillment(suspended=suspended)
    for number, amount in enumerate(amounts, start=1):
        assert fulfillment.handle(*_credited(number, amount, team)).status == 204
    held = [order for order in fulfillment.ledger.orders_for(TEAM) if order["status"] == "held"]
    assert [order["reason"] for order in held] == [reason]
    credited = sum(amount for _, amount in fulfillment.ledger.credits_for(TEAM))
    assert credited == sum(amounts[:-1])


def test_a_credit_for_an_unknown_workspace_is_held() -> None:
    fulfillment = _fulfillment()
    assert fulfillment.handle(*_credited(team="team-unknown")).status == 204
    order = fulfillment.ledger.find_order(deposit_id(CHAIN_ID, "0x" + "01" * 32, 0))
    assert order is not None
    assert (order.status, order.reason, order.team_id) == ("held", "unknown_account", None)


def test_another_accounts_or_modes_event_is_refused_without_a_credit() -> None:
    fulfillment = _fulfillment()
    for delivery in (
        _credited(account="acct_" + "0b" * 16),
        _credited(livemode=True),
    ):
        assert fulfillment.handle(*delivery).status == 400
    assert fulfillment.ledger.credits_for(TEAM) == []


def test_the_inbox_keeps_each_delivery_as_received_once(caplog: pytest.LogCaptureFixture) -> None:
    fulfillment = _fulfillment()
    headers, body = _credited()
    # Header names arrive in any case; the body is kept byte for byte, not re-serialized.
    received = {name.upper(): value for name, value in headers.items()}
    spaced = body.replace(b'"object": "event"', b'"object":   "event"')
    received["WEBHOOK-SIGNATURE"] = sign_webhook(
        SERVICE_KEY, headers["webhook-id"], int(received["WEBHOOK-TIMESTAMP"]), spaced
    )["webhook-signature"]
    for _ in range(2):
        assert fulfillment.handle(received, spaced).status == 204
    [(delivery, _)] = fulfillment.ledger.deliveries(["deposit.credited"])
    assert delivery == Delivery(
        headers["webhook-id"],
        received["WEBHOOK-TIMESTAMP"],
        received["WEBHOOK-SIGNATURE"],
        spaced,
    )
    # A redelivery with another body (a service restored from backup re-valued the deposit)
    # changes nothing and is raised with the operator.
    assert fulfillment.handle(*_credited(amount_minor=2_600)).status == 204
    assert [kept for kept, _ in fulfillment.ledger.deliveries(["deposit.credited"])] == [delivery]
    assert [amount for _, amount in fulfillment.ledger.credits_for(TEAM)] == [2_500]
    assert "repeats with another body" in caplog.text


def test_a_delivery_is_kept_only_with_its_ledger_effect(monkeypatch: pytest.MonkeyPatch) -> None:
    fulfillment = _fulfillment()
    delivery = _credited()

    def unavailable(*_: object) -> None:
        raise sqlite3.OperationalError("disk I/O error")

    with monkeypatch.context() as patch:
        patch.setattr(fulfillment, "_credit", unavailable)
        with pytest.raises(sqlite3.OperationalError):
            fulfillment.handle(*delivery)
    assert fulfillment.ledger.deliveries(["deposit.credited"]) == []
    # The service retries; the retry credits and keeps the delivery.
    assert fulfillment.handle(*delivery).status == 204
    assert len(fulfillment.ledger.deliveries(["deposit.credited"])) == 1
    assert fulfillment.ledger.balance_for(TEAM) == 2_500


def test_a_new_ledger_file_is_its_owners_alone(tmp_path: Path) -> None:
    path = tmp_path / "ledger.sqlite3"
    ProductLedger(str(path)).add_team(TEAM)
    assert path.stat().st_mode & 0o777 == 0o600


def test_the_export_reads_the_ledger_without_changing_it(tmp_path: Path) -> None:
    path = tmp_path / "ledger.sqlite3"
    fulfillment = _fulfillment(ledger=ProductLedger(str(path)))
    fulfillment.ledger.add_team(TEAM)
    assert fulfillment.handle(*_credited()).status == 204
    before = path.read_bytes()
    reader = ProductLedger(str(path), read_only=True)
    assert len(export_restore_records(CONFIG.account, reader)["events"]) == 1
    with pytest.raises(sqlite3.OperationalError):
        reader.add_team("team-2")
    assert path.read_bytes() == before


def test_deliveries_wait_until_the_webhook_keys_are_pinned() -> None:
    def unpinned() -> PinnedKeys:
        raise MissingProductKeyError("not sealed yet")

    fulfillment = Fulfillment(CONFIG, ProductLedger(), unpinned)
    assert fulfillment.handle(*_credited()).status == 503


def test_live_mode_requires_a_preverified_webhook_pin(tmp_path: Path) -> None:
    key_file = tmp_path / "api-key"
    key_file.write_text("ppay_sk_live_" + "a" * 40)
    config = replace(CONFIG, api_key_file=str(key_file))
    with pytest.raises(MissingProductKeyError, match="pre-verified"):
        pin_webhook_keys(config)


def test_live_product_without_a_pin_fails_startup(tmp_path: Path) -> None:
    key_file = tmp_path / "api-key"
    key_file.write_text("ppay_sk_live_" + "a" * 40)
    config = replace(
        CONFIG,
        api_key_file=str(key_file),
        driver_public_key=DRIVER.public_key_base64(),
    )
    with pytest.raises(MissingProductKeyError, match="pre-verified"):
        _make_product(config)


def test_unsealed_test_product_starts_without_api_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("UNSEALED_TEST_KEY", raising=False)
    config = replace(
        CONFIG,
        api_key_file=None,
        api_key_env="UNSEALED_TEST_KEY",
        driver_public_key=DRIVER.public_key_base64(),
    )
    with _make_product(config):
        pass


DRIVER = RequestSigner.from_seed(DRIVER_KEYID, bytes([7] * 32))


def _signed(
    method: str, path: str, body: bytes, signer: RequestSigner = DRIVER
) -> tuple[str, dict[str, str]]:
    """The target and headers of a driver request; a `POST` carries a signed Idempotency-Key."""
    target = "/topup" + path
    key = sf_string(str(uuid.uuid4())) if method == "POST" else None
    headers = signer.sign(method, "https://acme.example" + target, body, idempotency_key=key)
    if key is not None:
        headers["Idempotency-Key"] = key
    return target, headers


def _account_call(
    api: AccountApi, method: str, path: str, body: bytes, signer: RequestSigner = DRIVER
) -> Answer:
    target, headers = _signed(method, path, body, signer)
    return api.handle(method, target, headers, body)


def test_account_api_requires_the_driver_key_and_valid_refs(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("ACME_SEED", raising=False)
    config = replace(CONFIG, api_key_file=None, api_key_env="ACME_SEED")
    api = AccountApi(config, ProductLedger(), load_public_key(DRIVER.public_key_base64()))
    register = json.dumps({"account_id": TEAM}).encode()
    other = RequestSigner.from_seed(DRIVER_KEYID, bytes([8] * 32))
    assert _account_call(api, "POST", "/accounts", register, other).status == 401
    unsigned = api.handle("POST", "/topup/accounts", {}, register)
    assert unsigned.status == 401
    # A POST's signature must cover an Idempotency-Key.
    without_key = DRIVER.sign("POST", "https://acme.example/topup/accounts", register)
    assert api.handle("POST", "/topup/accounts", without_key, register).status == 401
    bad_ref = json.dumps({"account_id": "a/b"}).encode()
    assert _account_call(api, "POST", "/accounts", bad_ref).status == 400
    assert _account_call(api, "GET", f"/accounts/{TEAM}", b"").status == 404
    # Registration is the product's own; a quote needs the service, and the product key is not
    # sealed yet: unavailable.
    assert _account_call(api, "POST", "/accounts", register).status == 200
    quote = json.dumps({"amount_minor": 2500}).encode()
    assert _account_call(api, "POST", f"/accounts/{TEAM}/quotes", quote).status == 503
    # A quote names a configured chain, if any.
    for chain in ({"chain_id": 1}, {"chain_id": "11155111"}, {"asset": 1}):
        body = json.dumps({"amount_minor": 2500, **chain}).encode()
        assert _account_call(api, "POST", f"/accounts/{TEAM}/quotes", body).status == 400


def test_the_account_view_lists_the_workspaces_quote_events() -> None:
    class Service:
        def list_deposits(
            self, *, client_reference_id: str, page_size: int
        ) -> list[SimpleNamespace]:
            assert page_size == 100
            return []

    fulfillment = _fulfillment()
    ledger = fulfillment.ledger
    for team in (TEAM, "team-2"):
        quote = {"id": f"qt_{team}", "client_reference_id": team}
        assert fulfillment.handle(*_delivery("quote.expired", quote)).status == 204
    client = create_autospec(TopupClient, instance=True, spec_set=True)
    service = Service()
    client.list_deposits.side_effect = service.list_deposits
    api = AccountApi(CONFIG, ledger, load_public_key(DRIVER.public_key_base64()), client=client)
    answer = _account_call(api, "GET", f"/accounts/{TEAM}", b"")
    assert answer.status == 200
    assert answer.body is not None
    assert [event["data"]["object"]["id"] for event in answer.body["events"]] == [f"qt_{TEAM}"]


def test_refund_requests_only_name_the_workspaces_own_deposits() -> None:
    own, other = "dep_" + uuid.uuid4().hex, "dep_" + uuid.uuid4().hex
    requested: list[tuple[str, str, int]] = []
    keys: list[str] = []

    class Service:
        def list_deposits(self, *, client_reference_id: str) -> list[SimpleNamespace]:
            return [SimpleNamespace(id=own)] if client_reference_id == TEAM else []

        def create_refund(
            self, deposit: str, to: str, amount: int, *, idempotency_key: str | None
        ) -> SimpleNamespace:
            requested.append((deposit, to, amount))
            assert idempotency_key is not None
            keys.append(idempotency_key)
            return SimpleNamespace(to_dict=lambda: {"id": "re_1", "status": "pending"})

    ledger = ProductLedger()
    ledger.add_team(TEAM)
    client = create_autospec(TopupClient, instance=True, spec_set=True)
    service = Service()
    client.list_deposits.side_effect = service.list_deposits
    client.create_refund.side_effect = service.create_refund
    api = AccountApi(CONFIG, ledger, load_public_key(DRIVER.public_key_base64()), client=client)
    to = "0x" + "66" * 20

    def refund(deposit: str, body: dict[str, Any]) -> Answer:
        path = f"/accounts/{TEAM}/deposits/{deposit}/refunds"
        return _account_call(api, "POST", path, json.dumps(body).encode())

    body = {"destination_address": to, "amount_atomic": "5"}
    assert refund(other, body).status == 404
    assert refund("not-a-deposit-id", body).status == 400
    assert refund(own, {**body, "destination_address": "0x12"}).status == 400
    assert refund(own, {**body, "amount_atomic": "0"}).status == 400
    assert requested == []
    answer = refund(own, body)
    assert (answer.status, answer.body) == (200, {"id": "re_1", "status": "pending"})
    assert requested == [(own, to, 5)]
    # A replayed request passes the same key on, so the service answers it with the first refund.
    payload = json.dumps(body).encode()
    target, headers = _signed("POST", f"/accounts/{TEAM}/deposits/{own}/refunds", payload)
    for _ in range(2):
        assert api.handle("POST", target, headers, payload).status == 200
    assert keys[1] == keys[2] != keys[0]


# Service restores: the product exports what the operator asks merchants for, as the admin
# requests take it (deploy/runbooks/restore.md).

QUOTE_ID = "qt_" + "0c" * 16
ADDRESS_ID = "da_" + "0d" * 16
QUOTE_SECRET = f"{QUOTE_ID}_secret_" + "ab" * 24
ADDRESS_SECRET = f"{ADDRESS_ID}_secret_" + "cd" * 24


def _quote(created: int = 1_790_000_000) -> dict[str, Any]:
    """A `POST /v1/quotes` response, whole."""
    return {
        "id": QUOTE_ID,
        "object": "quote",
        "livemode": False,
        "client_reference_id": TEAM,
        "amount": 2_500,
        "currency": "usd",
        "chain_id": 11155111,
        "asset": "pha",
        "amount_atomic": "25000000000000000000",
        "exchange_rate": "0.10000000",
        "address": "0x" + "66" * 20,
        "treasury": "0x" + "7e" * 20,
        "payment_uri": "ethereum:0x…",
        "status": "open",
        "expires_at": created + 900,
        "created": created,
        "payment": None,
        "deposit": None,
        "terms": QUOTE_TERMS,
        "client_secret": QUOTE_SECRET,
        "metadata": {"order_id": "order_1"},
    }


def _deposit_address(created: int = 1_790_000_000) -> dict[str, Any]:
    """A `POST /v1/deposit_addresses` response, whole."""
    return {
        "id": ADDRESS_ID,
        "object": "deposit_address",
        "livemode": False,
        "client_reference_id": TEAM,
        "version": 2,
        "status": "active",
        "salt": "0x" + "5a" * 32,
        "address": "0x" + "88" * 20,
        "networks": [
            {"chain_id": 11155111, "address": "0x" + "88" * 20, "treasury": "0x0", "assets": []}
        ],
        "payments": [],
        "metadata": {},
        "created": created,
        "retired_at": None,
        "client_secret": ADDRESS_SECRET,
    }


def test_the_export_is_the_admin_requests_bodies() -> None:
    fulfillment = _fulfillment()
    ledger = fulfillment.ledger
    ledger.record_quote(TEAM, _quote())
    ledger.record_deposit_address(TEAM, _deposit_address())
    credited = _credited()
    for delivery in (
        credited,
        _credited(2, event_type="deposit.reversed", status="reversed", amount_reversed=2_500),
        # Neither a refund nor any other event is re-derived by a restore, so none is imported.
        _credited(event_type="deposit.refunded", amount_refunded=100),
        _delivery("quote.expired", {"id": QUOTE_ID}),
    ):
        assert fulfillment.handle(*delivery).status == 204

    records = export_restore_records(CONFIG.account, ledger)
    assert records["deposit_addresses"] == [
        {
            "account": CONFIG.account,
            "livemode": False,
            "client_reference_id": TEAM,
            "id": ADDRESS_ID,
            "version": 2,
            "address": "0x" + "88" * 20,
            "client_secret": ADDRESS_SECRET,
        }
    ]
    # Only the fields the restore request takes (it refuses any other), the secret included.
    quote = _quote()
    assert records["quotes"] == [
        {"account": CONFIG.account}
        | {
            name: quote[name]
            for name in (
                "livemode",
                "id",
                "client_reference_id",
                "chain_id",
                "asset",
                "amount",
                "amount_atomic",
                "exchange_rate",
                "address",
                "created",
                "expires_at",
                "metadata",
                "client_secret",
            )
        }
    ]
    [batch] = records["events"]
    deliveries = batch["deliveries"]
    assert [json.loads(each["body"])["type"] for each in deliveries] == [
        "deposit.credited",
        "deposit.reversed",
    ]
    # The delivery exactly as received, so it verifies with the service's key as it did then.
    headers, body = credited
    assert deliveries[0] == {
        "webhook_id": headers["webhook-id"],
        "webhook_timestamp": headers["webhook-timestamp"],
        "webhook_signature": headers["webhook-signature"],
        "body": body.decode(),
    }
    for each in deliveries:
        signed = {
            "webhook-id": each["webhook_id"],
            "webhook-timestamp": each["webhook_timestamp"],
            "webhook-signature": each["webhook_signature"],
        }
        verify_webhook_signature(
            signed,
            each["body"].encode(),
            SERVICE_KEY.public_key(),
            now=int(each["webhook_timestamp"]),
        )


def _treasury(status: str, number: int = 1) -> dict[str, Any]:
    return {
        "id": f"trs_{number:032x}",
        "object": "treasury",
        "livemode": False,
        "chain_id": 11155111,
        "address": "0x" + f"{number:02x}" * 20,
        "kind": "eoa",
        "status": status,
        "crediting_paused_by": [],
    }


def test_the_export_verifies_the_latest_treasuries_and_restores_signed_applications() -> None:
    fulfillment = _fulfillment()
    pending = _delivery("treasury.created", _treasury("pending"), created=100)
    applied = _delivery("treasury.updated", _treasury("active"), created=300)
    # The application's event names the change becoming active from pending.
    applied_body = json.loads(applied[1])
    applied_body["data"]["previous_attributes"] = {"status": "pending"}
    body = json.dumps(applied_body).encode()
    applied = sign_webhook(SERVICE_KEY, applied_body["id"], int(time.time()), body), body
    paused = _delivery("treasury.updated", _treasury("active", 2), created=200)
    # Delivered out of order: the latest by `created` wins.
    for delivery in (applied, paused, pending):
        assert fulfillment.handle(*delivery).status == 204

    records = export_restore_records(CONFIG.account, fulfillment.ledger)
    assert records["treasuries"] == [
        {
            "account": CONFIG.account,
            "livemode": False,
            "treasuries": [
                {
                    name: _treasury(status, number)[name]
                    for name in ("id", "status", "chain_id", "address", "crediting_paused_by")
                }
                for status, number in (("active", 1), ("active", 2))
            ],
        }
    ]
    # Only the signed pending-to-active change is an application, exactly as received.
    headers, _ = applied
    assert records["treasury_applications"] == [
        {
            "delivery": {
                "webhook_id": headers["webhook-id"],
                "webhook_timestamp": headers["webhook-timestamp"],
                "webhook_signature": headers["webhook-signature"],
                "body": body.decode(),
            }
        }
    ]
    assert records["events"] == []


def test_the_export_starts_five_minutes_before_the_restore_point(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    restore_point = 1_790_000_000
    ledger = ProductLedger()
    ledger.add_team(TEAM)
    # Recorded an hour before the restore point.
    monkeypatch.setattr("reference_product.ledger.time.time", lambda: restore_point - 3_600)
    ledger.record_quote(TEAM, {**_quote(created=restore_point - 299), "id": QUOTE_ID})
    ledger.record_quote(TEAM, {**_quote(created=restore_point - 301), "id": "qt_" + "0e" * 16})
    ledger.record_deposit_address(TEAM, _deposit_address(created=restore_point - 3_600))
    records = export_restore_records(CONFIG.account, ledger, since=restore_point)
    # The service re-issues a quote it no longer holds created up to five minutes before.
    assert [quote["id"] for quote in records["quotes"]] == [QUOTE_ID]
    assert records["deposit_addresses"] == []
    # Posted again after the restore point, the address has a new client secret the service
    # must keep, though it was created long before.
    fresh = f"{ADDRESS_ID}_secret_" + "ef" * 24
    monkeypatch.setattr("reference_product.ledger.time.time", lambda: restore_point + 10)
    ledger.record_deposit_address(
        TEAM, {**_deposit_address(created=restore_point - 3_600), "client_secret": fresh}
    )
    records = export_restore_records(CONFIG.account, ledger, since=restore_point)
    assert [address["client_secret"] for address in records["deposit_addresses"]] == [fresh]
    everything = export_restore_records(CONFIG.account, ledger, since=None)
    assert len(everything["quotes"]) == 2


def test_the_export_orders_deliveries_by_deposit_position_and_batches_them() -> None:
    fulfillment = _fulfillment()
    # D1 (revision 1) arrives before D0, which it replaced, and another deposit between them.
    tx_hash = "0x" + "7c" * 32
    replaced, replacing = (
        _credited(
            9,
            event_type="deposit.reversed",
            status="reversed",
            tx_hash=tx_hash,
            receipt_log_index=0,
            revision=0,
        ),
        _credited(9, tx_hash=tx_hash, receipt_log_index=0, revision=1),
    )
    for delivery in (replacing, _credited(1, amount_minor=1), replaced):
        assert fulfillment.handle(*delivery).status == 204
    [batch] = export_restore_records(CONFIG.account, fulfillment.ledger)["events"]
    order = [json.loads(each["body"])["data"]["object"] for each in batch["deliveries"]]
    assert [(deposit["tx_hash"], deposit["revision"]) for deposit in order] == [
        ("0x" + "01" * 32, 0),
        (tx_hash, 0),
        (tx_hash, 1),
    ]
    for number in range(10, 110):
        assert fulfillment.handle(*_credited(number, amount_minor=1)).status == 204
    records = export_restore_records(CONFIG.account, fulfillment.ledger, since=None)
    assert [len(batch["deliveries"]) for batch in records["events"]] == [100, 3]


def test_a_later_deposit_address_response_replaces_the_record() -> None:
    ledger = ProductLedger()
    ledger.add_team(TEAM)
    ledger.record_deposit_address(TEAM, _deposit_address())
    fresh = f"{ADDRESS_ID}_secret_" + "ef" * 24
    ledger.record_deposit_address(TEAM, {**_deposit_address(), "client_secret": fresh})
    [record] = export_restore_records(CONFIG.account, ledger)["deposit_addresses"]
    assert record["client_secret"] == fresh


def test_an_export_file_is_new_and_its_owners_alone(tmp_path: Path) -> None:
    output = tmp_path / "records.json"
    write_records("{}\n", str(output))
    assert output.read_text() == "{}\n"
    assert output.stat().st_mode & 0o777 == 0o600
    with pytest.raises(FileExistsError):
        write_records("[]\n", str(output))
    # Never replaced, and no temporary file is left behind.
    assert output.read_text() == "{}\n"
    assert [path.name for path in tmp_path.iterdir()] == ["records.json"]


def test_the_account_api_serves_the_restore_records_to_the_driver_only() -> None:
    fulfillment = _fulfillment()
    ledger = fulfillment.ledger
    ledger.record_quote(TEAM, _quote())
    assert fulfillment.handle(*_credited()).status == 204
    api = AccountApi(CONFIG, ledger, load_public_key(DRIVER.public_key_base64()))
    answer = _account_call(api, "GET", "/accounts/restore-records", b"")
    assert (answer.status, answer.body) == (200, export_restore_records(CONFIG.account, ledger))
    answer = _account_call(api, "GET", "/accounts/restore-records?since=1790000000", b"")
    assert answer.body == export_restore_records(CONFIG.account, ledger, since=1_790_000_000)
    other = RequestSigner.from_seed(DRIVER_KEYID, bytes([8] * 32))
    assert _account_call(api, "GET", "/accounts/restore-records", b"", other).status == 401
    for query in ("?since=-1", "?since=x", "?since=1&since=2", "?other=1"):
        assert _account_call(api, "GET", "/accounts/restore-records" + query, b"").status == 400
    # Its path is not a workspace's.
    register = json.dumps({"account_id": "restore-records"}).encode()
    assert _account_call(api, "POST", "/accounts", register).status == 400


def test_restore_records_over_asgi_preserves_signed_query_and_payload() -> None:
    fulfillment = _fulfillment()
    ledger = fulfillment.ledger
    ledger.record_quote(TEAM, _quote())
    api = AccountApi(CONFIG, ledger, load_public_key(DRIVER.public_key_base64()))
    product = ProductServer(fulfillment, api)
    try:
        with TestClient(product.app) as client:
            for query, since in (("", None), ("?since=1790000000", 1_790_000_000)):
                target, headers = _signed("GET", "/accounts/restore-records" + query, b"")
                response = client.get(target, headers=headers)
                assert response.status_code == 200
                assert response.json() == export_restore_records(
                    CONFIG.account, ledger, since=since
                )
            assert client.get(target).status_code == 401
    finally:
        product.close()


def test_event_references_upgrade_once_and_deduplicate_matches(tmp_path: Path) -> None:
    path = tmp_path / "ledger.sqlite"
    data = {"object": {"id": "dep_test", "quote": "qt_test", "client_reference_id": TEAM}}
    with sqlite3.connect(path) as db:
        _create_ledger_schema(db, version=1)
        db.execute(
            "INSERT INTO webhook_events VALUES (?, ?, ?, ?, ?, ?, ?)",
            ("evt_test", "deposit.credited", json.dumps(data), 1, b"{}", "1", "signature"),
        )
    for _ in range(2):
        ledger = ProductLedger(str(path))
        try:
            assert ledger.events_for({"dep_test", "qt_test", TEAM}) == [
                {"type": "deposit.credited", "data": data}
            ]
            assert ledger.events_for({"other-team"}) == []
            with ledger.transaction() as db:
                assert db.execute("SELECT COUNT(*) FROM event_refs").fetchone()[0] == 3
                assert db.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
        finally:
            ledger._connection.close()


def test_event_references_commit_with_the_delivery() -> None:
    ledger = ProductLedger()
    delivery = Delivery("evt_test", "1", "signature", b"{}")
    data = {"id": "re_test", "deposit": "dep_test"}

    def record_and_fail() -> None:
        with ledger.transaction() as db:
            ledger.record_delivery(db, delivery, "refund.created", data)
            raise RuntimeError("rollback")

    with pytest.raises(RuntimeError, match="rollback"):
        record_and_fail()
    assert ledger.events_for({"re_test"}) == []
    with ledger.transaction() as db:
        ledger.record_delivery(db, delivery, "refund.created", data)
    assert ledger.events_for({"re_test", "dep_test"}) == [{"type": "refund.created", "data": data}]


def test_account_view_bounds_deposits_and_avoids_global_event_reads(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class Service:
        def list_deposits(
            self, *, client_reference_id: str, page_size: int
        ) -> Iterator[SimpleNamespace]:
            assert client_reference_id == TEAM
            assert page_size == 100
            for index in range(100):
                yield SimpleNamespace(id=f"dep_{index}", to_dict=lambda: {"object": "deposit"})
            raise AssertionError("read beyond the newest 100 deposits")

    fulfillment = _fulfillment()
    for team in (TEAM, "team-2"):
        assert (
            fulfillment.handle(
                *_delivery("quote.expired", {"id": f"qt_{team}", "client_reference_id": team})
            ).status
            == 204
        )
    client = create_autospec(TopupClient, instance=True, spec_set=True)
    service = Service()
    client.list_deposits.side_effect = service.list_deposits
    api = AccountApi(
        CONFIG, fulfillment.ledger, load_public_key(DRIVER.public_key_base64()), client=client
    )

    def global_events() -> list[dict[str, Any]]:
        raise AssertionError("account view must query references")

    monkeypatch.setattr(fulfillment.ledger, "all_events", global_events)
    view = api._account_view(TEAM)
    assert len(view["deposits"]) == 100
    assert [event["data"]["object"]["id"] for event in view["events"]] == [f"qt_{TEAM}"]


def test_webhook_key_fetch_failures_are_negative_cached(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    now = [100.0]
    calls: list[float] = []
    failing = [True]
    pinned = PinnedKeys(False, [SERVICE_KEY.public_key()])

    def pin(config: ProductConfig, *, wait_s: float = 0) -> PinnedKeys:
        calls.append(wait_s)
        if failing[0]:
            raise TransientError("attestation unavailable")
        return pinned

    monkeypatch.setattr(server, "pin_webhook_keys", pin)
    monkeypatch.setattr(time, "monotonic", lambda: now[0])
    keys = WebhookKeys(CONFIG)
    with ThreadPoolExecutor(max_workers=4) as workers:
        futures = [workers.submit(keys) for _ in range(4)]
        for future in futures:
            with pytest.raises(TransientError):
                future.result()
    assert len(calls) == 1
    now[0] += 29
    with pytest.raises(TransientError):
        keys()
    assert len(calls) == 1
    now[0] += 2
    failing[0] = False
    assert keys() is pinned
    assert keys() is pinned
    assert len(calls) == 2


def test_event_reference_backfill_tolerates_non_object_data(tmp_path: Path) -> None:
    path = tmp_path / "ledger.sqlite"
    with sqlite3.connect(path) as db:
        _create_ledger_schema(db, version=1)
        values: tuple[Any, ...] = (None, [], "text", 1)
        for index, value in enumerate(values):
            db.execute(
                "INSERT INTO webhook_events VALUES (?, ?, ?, ?, ?, ?, ?)",
                (f"evt_{index}", "test", json.dumps(value), 1, b"{}", "1", "signature"),
            )
    ledger = ProductLedger(str(path))
    try:
        with ledger.transaction() as db:
            assert db.execute("SELECT COUNT(*) FROM event_refs").fetchone()[0] == 0
            assert db.execute("SELECT COUNT(*) FROM webhook_events").fetchone()[0] == 4
    finally:
        ledger._connection.close()


def test_event_reference_backfill_rolls_back_non_sql_errors(
    tmp_path: Path,
) -> None:
    path = tmp_path / "ledger.sqlite"
    with sqlite3.connect(path) as db:
        _create_ledger_schema(db, version=1)
        db.execute(
            "INSERT INTO webhook_events VALUES (?, ?, ?, ?, ?, ?, ?)",
            ("evt_test", "test", "invalid JSON", 1, b"{}", "1", "signature"),
        )
    with pytest.raises(json.JSONDecodeError):
        ProductLedger(str(path))
    with sqlite3.connect(path) as connection:
        assert connection.execute("PRAGMA user_version").fetchone()[0] == 0
        assert (
            connection.execute("SELECT 1 FROM sqlite_master WHERE name = 'event_refs'").fetchone()
            is None
        )


def test_concurrent_event_reference_backfills_are_idempotent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "ledger.sqlite"
    data = {"id": "dep_test", "client_reference_id": TEAM}
    with sqlite3.connect(path) as db:
        _create_ledger_schema(db, version=1)
        db.execute(
            "INSERT INTO webhook_events VALUES (?, ?, ?, ?, ?, ?, ?)",
            ("evt_test", "test", json.dumps(data), 1, b"{}", "1", "signature"),
        )
    barrier = threading.Barrier(2)
    connect = sqlite3.connect

    def race(database: str, **kwargs: Any) -> sqlite3.Connection:
        connection: sqlite3.Connection = connect(database, **kwargs)
        barrier.wait(timeout=3)
        return connection

    monkeypatch.setattr(sqlite3, "connect", race)
    ledgers: list[ProductLedger] = []
    try:
        with ThreadPoolExecutor(max_workers=2) as workers:
            futures = [workers.submit(ProductLedger, str(path)) for _ in range(2)]
            for future in futures:
                ledgers.append(future.result(timeout=5))
        for ledger in ledgers:
            assert ledger.events_for({TEAM}) == [{"type": "test", "data": data}]
            with ledger.transaction() as db:
                assert db.execute("SELECT COUNT(*) FROM event_refs").fetchone()[0] == 2
    finally:
        for ledger in ledgers:
            ledger._connection.close()


@pytest.mark.parametrize("failure", ["timeout", "network", "malformed"])
def test_account_api_isolates_sdk_service_failures(
    failure: str, caplog: pytest.LogCaptureFixture
) -> None:
    fulfillment = _fulfillment()

    def service(request: httpx.Request) -> httpx.Response:
        if failure == "timeout":
            raise httpx.ReadTimeout("private service detail", request=request)
        if failure == "network":
            raise httpx.ConnectError("private service detail", request=request)
        return httpx.Response(200, json={"private service detail": "invalid list"})

    client = TopupClient(
        "https://service.test",
        "ppay_rk_test_" + "A" * 43 + "000000",
        account=CONFIG.account,
        transport=httpx.MockTransport(service),
        max_attempts=1,
    )
    api = AccountApi(
        CONFIG, fulfillment.ledger, load_public_key(DRIVER.public_key_base64()), client=client
    )
    try:
        answer = _account_call(api, "GET", f"/accounts/{TEAM}", b"")
        assert answer.status == (502 if failure == "malformed" else 503)
        assert answer.body == {"code": "bad_gateway" if failure == "malformed" else "unavailable"}
        assert "account API" in caplog.text
        assert "private service detail" not in caplog.text
    finally:
        api.close()
        client.close()


def test_account_api_leaves_injected_client_open() -> None:
    fulfillment = _fulfillment()
    client = create_autospec(TopupClient, instance=True, spec_set=True)
    client.list_deposits.return_value = iter([])
    api = AccountApi(
        CONFIG, fulfillment.ledger, load_public_key(DRIVER.public_key_base64()), client=client
    )
    try:
        assert _account_call(api, "GET", f"/accounts/{TEAM}", b"").status == 200
        api.close()
        client.close.assert_not_called()
    finally:
        api.close()
        fulfillment.ledger._connection.close()


@pytest.mark.parametrize("version", range(SCHEMA_VERSION))
def test_ledger_migrations_upgrade_legacy_and_versioned_schemas(
    tmp_path: Path, version: int
) -> None:
    path = tmp_path / "ledger.sqlite"
    with sqlite3.connect(path) as db:
        _create_ledger_schema(db, version=max(1, version))
        db.execute(f"PRAGMA user_version = {version}")
    ledger = ProductLedger(str(path))
    try:
        with ledger.transaction() as db:
            assert db.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
            assert db.execute("SELECT COUNT(*) FROM event_refs").fetchone()[0] == 0
    finally:
        ledger._connection.close()


def test_quote_status_migration_backfills_and_is_idempotent(tmp_path: Path) -> None:
    path = tmp_path / "ledger.sqlite"
    quote = _quote()
    expired_quote_id = "qt_" + "0d" * 16
    with sqlite3.connect(path) as db:
        _create_ledger_schema(db, version=2)
        db.execute("INSERT INTO teams (id) VALUES (?)", (TEAM,))
        db.execute(
            "INSERT INTO quote_records (id, team_id, response, recorded_at) VALUES (?, ?, ?, ?)",
            (quote["id"], TEAM, json.dumps({**quote, "status": "open"}), 1),
        )
        db.execute(
            "INSERT INTO quote_records (id, team_id, response, recorded_at) VALUES (?, ?, ?, ?)",
            (expired_quote_id, TEAM, json.dumps({**quote, "id": expired_quote_id}), 2),
        )
        for event_id, quote_id, event_type, received_at in (
            ("evt_canceled", quote["id"], "quote.canceled", 3),
            ("evt_expired", expired_quote_id, "quote.expired", 4),
        ):
            db.execute(
                "INSERT INTO webhook_events "
                "(id, type, data, received_at, body, webhook_timestamp, webhook_signature) "
                "VALUES (?, ?, ?, ?, ?, ?, ?)",
                (
                    event_id,
                    event_type,
                    json.dumps({"object": {"id": quote_id}}),
                    received_at,
                    b"{}",
                    "1",
                    "signature",
                ),
            )
        db.execute("PRAGMA user_version = 2")
    for _ in range(2):
        ledger = ProductLedger(str(path))
        try:
            with ledger.transaction() as db:
                assert db.execute(
                    "SELECT status FROM quote_statuses WHERE quote_id = ?", (quote["id"],)
                ).fetchone() == ("canceled",)
                assert db.execute(
                    "SELECT status FROM quote_statuses WHERE quote_id = ?", (expired_quote_id,)
                ).fetchone() == ("expired",)
                assert db.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
        finally:
            ledger._connection.close()


def test_quote_statuses_are_monotonic_and_duplicate_deliveries_are_idempotent() -> None:
    ledger = ProductLedger()
    complete_id = "qt_" + "01" * 16
    expired_id = "qt_" + "02" * 16
    equal_rank_id = "qt_" + "03" * 16
    ledger.record_quote_status({"id": complete_id, "status": "complete"})
    ledger.record_quote_status({"id": complete_id, "status": "expired"})
    ledger.record_quote_status({"id": expired_id, "status": "expired"})
    ledger.record_quote_status({"id": expired_id, "status": "complete"})
    ledger.record_quote_status({"id": equal_rank_id, "status": "complete"})
    ledger.record_quote_status({"id": equal_rank_id, "status": "canceled"})
    with ledger.transaction() as db:
        assert db.execute(
            "SELECT status FROM quote_statuses WHERE quote_id = ?", (complete_id,)
        ).fetchone() == ("complete",)
        assert db.execute(
            "SELECT status FROM quote_statuses WHERE quote_id = ?", (expired_id,)
        ).fetchone() == ("complete",)
        assert db.execute(
            "SELECT status FROM quote_statuses WHERE quote_id = ?", (equal_rank_id,)
        ).fetchone() == ("complete",)
    fulfillment = _fulfillment(ledger=ledger)
    delivery = _delivery("quote.expired", {"id": complete_id})
    assert fulfillment.handle(*delivery).status == 204
    assert fulfillment.handle(*delivery).status == 204
    with ledger.transaction() as db:
        assert db.execute(
            "SELECT status FROM quote_statuses WHERE quote_id = ?", (complete_id,)
        ).fetchone() == ("complete",)


def test_ledger_rejects_future_schema_without_changing_it(tmp_path: Path) -> None:
    path = tmp_path / "ledger.sqlite"
    with sqlite3.connect(path) as db:
        db.execute(f"PRAGMA user_version = {SCHEMA_VERSION + 1}")
    with pytest.raises(ValueError, match="schema is newer"):
        ProductLedger(str(path))
    with sqlite3.connect(path) as db:
        assert db.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION + 1
        assert db.execute("SELECT name FROM sqlite_master").fetchall() == []
