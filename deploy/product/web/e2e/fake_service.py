"""A stand-in for the Phala Pay service in the demo's end-to-end test.

It answers the merchant API the demo uses, in the shapes of crates/topup/openapi.json: the config
(the tokens of each chain in `--chains`: test PHA on Sepolia and Base Sepolia, a 6-decimal test
USDC priced as a stablecoin on Sepolia, and a 6-decimal test USDT on Base Sepolia), quotes and
their public view, deposit addresses and their public view, deposits, refunds (`mark_paid`
verified on chain), forwarders, the balance, sweeps, attestation, and the TLS evidence. It follows
real payments on each chain's Anvil the way the service does, compressed in time (one block a
second):

- a transfer to an issued address is a `seen` payment as soon as it is in a block;
- at `CREDIT_DEPTH` confirmations it is recorded as a deposit, valued (the quote's locked price
  when it pays an open quote exactly, otherwise spot), and credited with a signed
  `deposit.credited` webhook;
- at `FINAL_DEPTH` confirmations the deposit is `final`, and refunds may be requested;
- a refund marked paid is verified once its transaction is at `FINAL_DEPTH`: a `Transfer` of the
  deposit's token from the address's treasury to the destination for exactly the amount, in a log
  no other refund used; then `succeeded` with `deposit.refunded`, or `failed` with its
  `failure_reason` and `refund.failed`;
- the service never sweeps: `Flushed` events of the factory, from whoever sent the flush, are
  indexed once at `FINAL_DEPTH` as sweeps and mark the forwarder's earlier deposits `swept`.

`POST /_test/quotes/{id}/expire` and `/cancel` end an unpaid quote, as its window's end or the
merchant's cancel does. `POST /_test/deposits/{id}/reverse` makes a deposit `reversed`, as the
service's finality watch does for a proven-dropped transaction, and sends `deposit.reversed`.
`POST /_test/mining/pause` and `/resume` stop and restart the block a second it mines on each
chain, so a test can hold a transaction pending (with Anvil's automine off too).
Addresses, deposit ids, the webhook signature, and the attestation binding use the SDK's own
helpers, so the product checks them exactly as it checks the real service.

    python fake_service.py --port 8545 --chains '[{"chain_id": 11155111, "rpc": "http://…",
        "tokens": [{"asset": "pha", "contract": "0x…", "decimals": 18, "price": "0.25000000",
        "pricing": "spot"}]}]' \\
        --product-webhook http://127.0.0.1:8089/webhooks --webhook-seed <64 hex> \\
        --factory 0x… --implementation 0x… --account acct_… --treasury 0x…
"""

from __future__ import annotations

import argparse
import base64
import json
import logging
import secrets
import threading
import time
import uuid
from fractions import Fraction
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlsplit

import httpx
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

from topup_sdk import attestation_report_data, credited_event_id, sign_webhook
from topup_sdk.addresses import (
    deposit_address_salt,
    deposit_id,
    forwarder_address,
    keccak256,
    quote_salt,
)

LOG = logging.getLogger("fake_service")
TRANSFER_TOPIC = "0x" + keccak256(b"Transfer(address,address,uint256)").hex()
FLUSHED_TOPIC = "0x" + keccak256(b"Flushed(bytes32,address,address,address,uint256)").hex()
CREDIT_DEPTH = 2
# The service's typical credit time at that depth: half a 12 s slot to inclusion, then the blocks.
CREDIT_SECONDS = CREDIT_DEPTH * 12 + 6
FINAL_DEPTH = 12
MIN_REFUND_TOKENS = 20  # the staging PHA route's min_refund_atomic, in whole tokens
NAMESPACE = uuid.UUID("5b0c6f7e-0f3a-4c9e-9d1e-2f3a4b5c6d7e")
DOCS = "https://phala-network.github.io/phala-pay/#section/Errors/"


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


class RefusedError(Exception):
    """A request the service answers with a documented error."""

    def __init__(self, status: HTTPStatus, code: str, param: str | None = None) -> None:
        super().__init__(code)
        self.status = status
        self.code = code
        self.param = param


def _topic(address: str) -> str:
    return "0x" + "0" * 24 + address.lower().removeprefix("0x")


def _address(topic: str) -> str:
    return "0x" + topic[-40:].lower()


def _public(value: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in value.items() if not k.startswith("_")}


def _page(url: str, data: list[dict[str, Any]]) -> dict[str, Any]:
    return {"object": "list", "url": url, "has_more": False, "data": data}


class FakeTopup:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.treasury = args.treasury.lower()
        # Each chain's RPC, its tokens by lowercase contract, and how far it has been scanned.
        self.chains: dict[int, dict[str, Any]] = {
            int(chain["chain_id"]): {
                "rpc": chain["rpc"],
                "tokens": {token["contract"].lower(): token for token in chain["tokens"]},
                "scanned": 0,
                "flushes_scanned": 0,
            }
            for chain in json.loads(args.chains)
        }
        self.key = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(args.webhook_seed))
        self.rpc = httpx.Client(timeout=10)
        self.stop = threading.Event()
        # Whether the watch mines its block a second; changed under `mining_lock`, which the watch
        # holds while it mines, so a pause returns only once no block is being mined.
        self.mining = True
        self.mining_lock = threading.Lock()
        self.lock = threading.RLock()
        self.quotes: dict[str, dict[str, Any]] = {}
        self.addresses: dict[str, dict[str, Any]] = {}  # deposit addresses by id
        self.secrets: dict[str, list[str]] = {}
        self.forwarders: dict[str, dict[str, Any]] = {}  # by `chain_id:lowercase address`
        self.seen: dict[str, dict[str, Any]] = {}  # transfers by deposit id
        self.deposits: dict[str, dict[str, Any]] = {}
        self.refunds: dict[str, dict[str, Any]] = {}
        self.sweeps: list[dict[str, Any]] = []
        self.outbox: list[dict[str, Any]] = []
        self.idempotent: dict[str, dict[str, Any]] = {}

    # Chain ------------------------------------------------------------------------------------

    def call(self, chain_id: int, method: str, *params: Any) -> Any:
        body = self.rpc.post(
            self.chains[chain_id]["rpc"],
            json={"jsonrpc": "2.0", "id": 1, "method": method, "params": params},
        ).json()
        if "error" in body:
            raise RuntimeError(f"{method}: {body['error']}")
        return body["result"]

    def head(self, chain_id: int) -> int:
        return int(self.call(chain_id, "eth_blockNumber"), 16)

    def token(self, chain_id: int, asset: str) -> dict[str, Any]:
        """The chain's token for `asset`; refuses a pair the config does not offer."""
        for token in self.chains.get(chain_id, {"tokens": {}})["tokens"].values():
            if token["asset"] == asset:
                return dict(token)
        raise RefusedError(HTTPStatus.BAD_REQUEST, "asset_unsupported", "asset")

    def watch(self) -> None:
        while not self.stop.wait(1.0):
            try:
                with self.mining_lock:
                    if self.mining:
                        for chain_id in self.chains:
                            self.call(chain_id, "evm_mine")
                self.step()
            except (httpx.HTTPError, RuntimeError):
                LOG.exception("watch step failed")

    def step(self) -> None:
        heads = {chain_id: self.head(chain_id) for chain_id in self.chains}
        for chain_id, head in heads.items():
            self.detect(chain_id, head)
        with self.lock:
            for transfer in list(self.seen.values()):
                if (
                    transfer["id"] not in self.deposits
                    and self.depth(transfer, heads[transfer["chain_id"]]) >= CREDIT_DEPTH
                ):
                    self.record(transfer)
            for deposit in self.deposits.values():
                deep = heads[deposit["chain_id"]] - deposit["block_number"] + 1 >= FINAL_DEPTH
                if deep and deposit["status"] != "reversed" and not deposit["final"]:
                    deposit["final"] = True
                    deposit["final_at"] = int(time.time())
        self.verify_refunds(heads)
        for chain_id, head in heads.items():
            self.index_sweeps(chain_id, head)
        self.deliver()

    @staticmethod
    def depth(transfer: dict[str, Any], head: int) -> int:
        return head - int(transfer["block"]) + 1

    def detect(self, chain_id: int, head: int) -> None:
        """Every transfer of a chain's token to an issued address there, in the blocks not
        scanned yet."""
        chain = self.chains[chain_id]
        with self.lock:
            watched = [
                _topic(forwarder["address"])
                for forwarder in self.forwarders.values()
                if forwarder["chain_id"] == chain_id
            ]
        if not watched or head <= chain["scanned"]:
            return
        logs = self.call(
            chain_id,
            "eth_getLogs",
            {
                "address": list(chain["tokens"]),
                "fromBlock": hex(chain["scanned"] + 1),
                "toBlock": hex(head),
                "topics": [TRANSFER_TOPIC, None, watched],
            },
        )
        for log in logs:
            token = chain["tokens"][log["address"].lower()]
            receipt = self.call(chain_id, "eth_getTransactionReceipt", log["transactionHash"])
            position = next(
                index
                for index, item in enumerate(receipt["logs"])
                if item["logIndex"] == log["logIndex"]
            )
            key = deposit_id(chain_id, log["transactionHash"], position)
            with self.lock:
                self.seen.setdefault(
                    key,
                    {
                        "id": key,
                        "chain_id": chain_id,
                        "asset": token["asset"],
                        "contract": token["contract"].lower(),
                        "decimals": token["decimals"],
                        "address": _address(log["topics"][2]),
                        "from": _address(log["topics"][1]),
                        "amount_atomic": str(int(log["data"], 16)),
                        "tx_hash": log["transactionHash"],
                        "receipt_log_index": position,
                        "block": int(log["blockNumber"], 16),
                        "block_hash": log["blockHash"],
                        "log_index": int(log["logIndex"], 16),
                        "created": int(time.time()),
                    },
                )
        chain["scanned"] = head

    def record(self, transfer: dict[str, Any]) -> None:
        """Records a transfer at the credit depth as a deposit, values it, and credits it."""
        forwarder = self.forwarders[f"{transfer['chain_id']}:{transfer['address']}"]
        quote = self.quotes.get(forwarder["quote"] or "")
        owner = quote or self.addresses[forwarder["deposit_address"]]
        token = self.chains[transfer["chain_id"]]["tokens"][transfer["contract"]]
        atomic = int(transfer["amount_atomic"])
        at_quote = (
            quote is not None
            and quote["status"] == "open"
            and time.time() < quote["expires_at"]
            and transfer["contract"] == quote["_contract"]
            and transfer["amount_atomic"] == quote["amount_atomic"]
        )
        spot = int(atomic * Fraction(token["price"]) * 100 / 10 ** token["decimals"])
        now = int(time.time())
        deposit = {
            "id": transfer["id"],
            "object": "deposit",
            "livemode": False,
            "client_reference_id": owner["client_reference_id"],
            "quote": None if quote is None else quote["id"],
            "deposit_address": forwarder["deposit_address"],
            "status": "credited",
            "final": False,
            "final_at": None,
            "swept": False,
            "metadata": dict(owner["metadata"]),
            "rejection_reason": None,
            "chain_id": transfer["chain_id"],
            "asset": token["asset"],
            "asset_contract": transfer["contract"],
            "amount_atomic": transfer["amount_atomic"],
            "amount": quote["amount"] if at_quote and quote else spot,
            "currency": "usd",
            "exchange_rate": token["price"],
            "price_source": "quote" if at_quote else "spot",
            "valued_at": now,
            "address": transfer["address"],
            "from_address": transfer["from"],
            "tx_hash": transfer["tx_hash"],
            "receipt_log_index": transfer["receipt_log_index"],
            "revision": 0,
            "log_index": transfer["log_index"],
            "block_number": transfer["block"],
            "block_hash": transfer["block_hash"],
            "block_time": transfer["created"],
            "amount_refunded_atomic": "0",
            "refunded": False,
            "amount_refunded": 0,
            "amount_reversed": 0,
            "replaces": None,
            "replaced_by": None,
            "created": transfer["created"],
        }
        self.deposits[deposit["id"]] = deposit
        if at_quote and quote is not None:
            quote.update(status="complete", deposit=deposit["id"])
        self.emit("deposit.credited", deposit, event_id=credited_event_id(deposit["id"]))

    def verify_refunds(self, heads: dict[int, int]) -> None:
        with self.lock:
            marked = [
                r
                for r in self.refunds.values()
                if r["status"] == "pending" and r["transaction_hash"]
            ]
        for refund in marked:
            chain_id = self.deposits[refund["deposit"]]["chain_id"]
            receipt = self.call(chain_id, "eth_getTransactionReceipt", refund["transaction_hash"])
            final = heads[chain_id] - FINAL_DEPTH + 1
            if receipt is None or int(receipt["blockNumber"], 16) > final:
                continue
            with self.lock:
                reason = self.match(refund, receipt)
                if reason is None:
                    self.succeed(refund)
                else:
                    refund.update(status="failed", failure_reason=reason)
                    self.emit("refund.updated", refund)
                    self.emit("refund.failed", refund)

    def match(self, refund: dict[str, Any], receipt: dict[str, Any]) -> str | None:
        """The refund's `failure_reason` for the finalized receipt, or `None` when it pays it."""
        if int(receipt["status"], 16) != 1:
            return "transaction_failed"
        deposit = self.deposits[refund["deposit"]]
        logs = [
            (position, log)
            for position, log in enumerate(receipt["logs"])
            if log["address"].lower() == deposit["asset_contract"]
            and log["topics"]
            and log["topics"][0] == TRANSFER_TOPIC
        ]
        if refund["receipt_log_index"] is not None:
            logs = [(p, log) for p, log in logs if p == refund["receipt_log_index"]]
        if not logs:
            return "transfer_not_found"
        position, log = logs[0]
        if _address(log["topics"][1]) != refund["treasury"]:
            return "sender_mismatch"
        if _address(log["topics"][2]) != refund["destination_address"]:
            return "destination_mismatch"
        if str(int(log["data"], 16)) != refund["amount_atomic"]:
            return "amount_mismatch"
        used = {
            (other["transaction_hash"], other["receipt_log_index"])
            for other in self.refunds.values()
            if other["status"] == "succeeded"
        }
        if (refund["transaction_hash"], position) in used:
            return "transfer_already_used"
        refund["receipt_log_index"] = position
        return None

    def succeed(self, refund: dict[str, Any]) -> None:
        deposit = self.deposits[refund["deposit"]]
        refunded = int(deposit["amount_refunded_atomic"]) + int(refund["amount_atomic"])
        atomic = int(deposit["amount_atomic"])
        deposit.update(
            amount_refunded_atomic=str(refunded),
            amount_refunded=(deposit["amount"] or 0) * refunded // atomic,
            refunded=refunded == atomic,
        )
        refund["status"] = "succeeded"
        self.emit("refund.updated", refund)
        self.emit("deposit.refunded", deposit)

    def index_sweeps(self, chain_id: int, head: int) -> None:
        """Indexes the factory's finalized `Flushed` events on a chain, whoever sent the flush."""
        chain = self.chains[chain_id]
        final = head - FINAL_DEPTH + 1
        if final <= chain["flushes_scanned"]:
            return
        logs = self.call(
            chain_id,
            "eth_getLogs",
            {
                "address": self.args.factory,
                "fromBlock": hex(chain["flushes_scanned"] + 1),
                "toBlock": hex(final),
                "topics": [FLUSHED_TOPIC],
            },
        )
        with self.lock:
            for log in logs:
                address = _address(log["topics"][2])
                forwarder = self.forwarders.get(f"{chain_id}:{address}")
                token = chain["tokens"].get(_address(log["topics"][3]))
                if forwarder is None or token is None:
                    continue
                data = log["data"].removeprefix("0x")
                block, index = int(log["blockNumber"], 16), int(log["logIndex"], 16)
                sweep = {
                    "id": "sw_" + uuid.uuid5(NAMESPACE, f"{log['transactionHash']}:{index}").hex,
                    "object": "sweep",
                    "livemode": False,
                    "chain_id": chain_id,
                    "forwarder": forwarder["id"],
                    "address": address,
                    "token": _address(log["topics"][3]),
                    "asset": token["asset"],
                    "treasury": _address(data[:64]),
                    "amount_atomic": str(int(data[64:128], 16)),
                    "tx_hash": log["transactionHash"],
                    "log_index": index,
                    "block_number": block,
                    "created": int(time.time()),
                }
                self.sweeps.insert(0, sweep)
                for deposit in self.deposits.values():
                    if (
                        deposit["chain_id"] == chain_id
                        and deposit["address"] == address
                        and deposit["asset_contract"] == token["contract"].lower()
                        and (deposit["block_number"], deposit["log_index"]) < (block, index)
                    ):
                        deposit["swept"] = True
        chain["flushes_scanned"] = final

    # Webhooks ---------------------------------------------------------------------------------

    def emit(self, event_type: str, obj: dict[str, Any], *, event_id: str | None = None) -> None:
        """Queues an event whose `data.object` is rendered now, as the service's outbox does."""
        self.outbox.append(
            {
                "id": event_id or "evt_" + uuid.uuid4().hex,
                "object": "event",
                "account": self.args.account,
                "livemode": False,
                "type": event_type,
                "created": int(time.time()),
                "actor": "system",
                "request": None,
                "data": {"object": json.loads(json.dumps(_public(obj)))},
                "_delivered": False,
            }
        )

    def deliver(self) -> None:
        """Sends every undelivered event until the product answers `2xx`."""
        with self.lock:
            pending = [event for event in self.outbox if not event["_delivered"]]
        for event in pending:
            body = json.dumps(_public(event)).encode()
            headers = sign_webhook(self.key, event["id"], int(time.time()), body)
            try:
                response = httpx.post(
                    self.args.product_webhook,
                    content=body,
                    headers={**headers, "content-type": "application/json"},
                    timeout=10,
                )
            except httpx.HTTPError:
                LOG.warning("webhook delivery failed; retrying")
                return
            event["_delivered"] = response.is_success

    # Quotes -----------------------------------------------------------------------------------

    def add_forwarder(
        self,
        chain_id: int,
        address: str,
        salt: bytes,
        *,
        quote: str | None,
        deposit_address: str | None,
    ) -> None:
        self.forwarders[f"{chain_id}:{address.lower()}"] = {
            "id": "fwd_" + uuid.uuid5(NAMESPACE, f"{chain_id}:{address.lower()}").hex,
            "object": "forwarder",
            "livemode": False,
            "chain_id": chain_id,
            "address": address,
            "factory": self.args.factory,
            "salt": "0x" + salt.hex(),
            "treasury": self.treasury,
            "quote": quote,
            "deposit_address": deposit_address,
            "superseded_at": None,
        }

    def create_quote(self, body: dict[str, Any], idempotency_key: str) -> dict[str, Any]:
        quote_id = "qt_" + uuid.uuid5(uuid.NAMESPACE_OID, idempotency_key).hex
        customer = str(body["client_reference_id"])
        amount = int(body["amount"])
        chain_id = int(body["chain_id"])
        token = self.token(chain_id, str(body["asset"]))
        salt = quote_salt(self.args.account, customer, quote_id)
        address = forwarder_address(
            self.args.factory, self.args.implementation, self.treasury, salt
        )
        # The amount at the token's price (cents / 100 / USD per token), rounded up to 4 decimals
        # as the service rounds it (a route's default `quote.amount_decimals`).
        step = 10 ** max(token["decimals"] - 4, 0)
        exact = Fraction(amount * 10 ** token["decimals"]) / (Fraction(token["price"]) * 100)
        atomic = str(-(-exact // step) * step)
        now = int(time.time())
        with self.lock:
            quote = self.quotes.get(quote_id)
            if quote is None:
                quote = {
                    "id": quote_id,
                    "object": "quote",
                    "livemode": False,
                    "client_reference_id": customer,
                    "treasury": self.treasury,
                    "metadata": dict(body.get("metadata") or {}),
                    "amount": amount,
                    "currency": "usd",
                    "chain_id": chain_id,
                    "asset": token["asset"],
                    "amount_atomic": atomic,
                    "exchange_rate": token["price"],
                    "address": address,
                    "payment_uri": f"ethereum:{token['contract']}@{chain_id}/transfer"
                    f"?address={address}&uint256={atomic}",
                    "status": "open",
                    "expires_at": now + 900,
                    "created": now,
                    "deposit": None,
                    "terms": QUOTE_TERMS,
                    "_contract": token["contract"].lower(),
                    "_decimals": token["decimals"],
                }
                self.quotes[quote_id] = quote
                self.add_forwarder(chain_id, address, salt, quote=quote_id, deposit_address=None)
            secret = f"{quote_id}_secret_{secrets.token_hex(24)}"
            self.secrets.setdefault(quote_id, []).append(secret)
            return {**self.quote_view(quote), "client_secret": secret}

    def transfers_at(self, address: str, chain_id: int | None = None) -> list[dict[str, Any]]:
        """Transfers to the address, on one chain or (a deposit address) on every chain."""
        return sorted(
            (
                t
                for t in self.seen.values()
                if t["address"] == address.lower() and chain_id in (None, t["chain_id"])
            ),
            key=lambda t: (t["created"], t["block"], t["log_index"]),
        )

    def payment(self, transfer: dict[str, Any], quote: dict[str, Any] | None) -> dict[str, Any]:
        """A transfer as the merchant's `Payment`: `seen`, then `recorded` as a deposit."""
        recorded = transfer["id"] in self.deposits
        return {
            "status": "recorded" if recorded else "seen",
            "chain_id": transfer["chain_id"],
            "asset": transfer["asset"],
            "tx_hash": transfer["tx_hash"],
            "amount_atomic": transfer["amount_atomic"],
            "confirmations": None
            if recorded
            else self.depth(transfer, self.head(transfer["chain_id"])),
            "estimated_final_at": None if recorded else transfer["created"] + 60,
            "matches_quote": None
            if quote is None
            else transfer["amount_atomic"] == quote["amount_atomic"],
            "deposit": transfer["id"],
        }

    def quote_view(self, quote: dict[str, Any]) -> dict[str, Any]:
        transfers = self.transfers_at(quote["address"], quote["chain_id"])
        shown = next((t for t in transfers if t["id"] == quote["deposit"]), None)
        shown = shown or (transfers[0] if transfers else None)
        payment = None if shown is None else self.payment(shown, quote)
        return {**_public(quote), "payment": payment}

    def client_quote(self, quote: dict[str, Any]) -> dict[str, Any]:
        transfers = self.transfers_at(quote["address"], quote["chain_id"])
        payment_status, confirmations = "none", None
        credited = self.deposits.get(quote["deposit"]) or next(
            (self.deposits[t["id"]] for t in transfers if t["id"] in self.deposits), None
        )
        if credited is not None:
            payment_status = "credited"
        elif transfers:
            payment_status = "seen"
            confirmations = self.depth(transfers[0], self.head(quote["chain_id"]))
        keys = (
            *("id", "object", "status", "amount", "currency", "asset", "chain_id"),
            *("amount_atomic", "address", "payment_uri", "expires_at"),
        )
        view = {k: quote[k] for k in keys}
        view.update(
            livemode=False,
            decimals=quote["_decimals"],
            payment_status=payment_status,
            confirmations=confirmations,
            amount_credited=None if credited is None else credited["amount"],
            typical_credit_seconds=CREDIT_SECONDS,
        )
        return view

    # Deposit addresses ------------------------------------------------------------------------

    def create_deposit_address(self, body: dict[str, Any]) -> dict[str, Any]:
        customer = str(body["client_reference_id"])
        with self.lock:
            address = next(
                (
                    a
                    for a in self.addresses.values()
                    if a["client_reference_id"] == customer and a["status"] == "active"
                ),
                None,
            )
            if address is None:
                salt = deposit_address_salt(
                    self.args.account, livemode=False, client_reference_id=customer, version=1
                )
                at = forwarder_address(
                    self.args.factory, self.args.implementation, self.treasury, salt
                )
                address = {
                    "id": "da_" + uuid.uuid4().hex,
                    "object": "deposit_address",
                    "livemode": False,
                    "client_reference_id": customer,
                    "address": at,
                    "version": 1,
                    "salt": "0x" + salt.hex(),
                    "status": "active",
                    "created": int(time.time()),
                    "retired_at": None,
                    "metadata": {},
                    # The same address on every chain, as the treasury is the same.
                    "networks": [
                        {
                            "chain_id": chain_id,
                            "address": at,
                            "treasury": self.treasury,
                            "assets": [
                                {
                                    "asset": token["asset"],
                                    "contract": token["contract"].lower(),
                                    "decimals": token["decimals"],
                                    "payment_uri": f"ethereum:{token['contract']}@{chain_id}"
                                    f"/transfer?address={at}",
                                }
                                for token in chain["tokens"].values()
                            ],
                        }
                        for chain_id, chain in self.chains.items()
                    ],
                }
                self.addresses[address["id"]] = address
                for chain_id in self.chains:
                    self.add_forwarder(
                        chain_id, at, salt, quote=None, deposit_address=address["id"]
                    )
            # The create request's metadata merges into the address's, as an update would.
            address["metadata"].update(body.get("metadata") or {})
            secret = f"{address['id']}_secret_{secrets.token_hex(24)}"
            self.secrets.setdefault(address["id"], []).append(secret)
            return {**self.deposit_address_view(address), "client_secret": secret}

    def deposit_address_view(self, address: dict[str, Any]) -> dict[str, Any]:
        transfers = self.transfers_at(address["address"])[::-1][:10]
        return {**address, "payments": [self.payment(t, None) for t in transfers]}

    def client_deposit_address(self, address: dict[str, Any]) -> dict[str, Any]:
        payments = []
        for transfer in self.transfers_at(address["address"])[::-1][:10]:
            deposit = self.deposits.get(transfer["id"])
            status = "seen" if deposit is None else deposit["status"]
            head = self.head(transfer["chain_id"])
            payments.append(
                {
                    "status": status,
                    "chain_id": transfer["chain_id"],
                    "asset": transfer["asset"],
                    "decimals": transfer["decimals"],
                    "amount_atomic": transfer["amount_atomic"],
                    "tx_hash": transfer["tx_hash"],
                    "confirmations": self.depth(transfer, head) if deposit is None else None,
                    "created": transfer["created"],
                }
            )
        return {
            "id": address["id"],
            "object": "deposit_address",
            "livemode": False,
            "status": address["status"],
            "address": address["address"],
            "networks": [
                {
                    **{k: network[k] for k in ("chain_id", "address", "assets")},
                    "typical_credit_seconds": CREDIT_SECONDS,
                }
                for network in address["networks"]
            ],
            "payments": payments,
        }

    # Refunds ----------------------------------------------------------------------------------

    def create_refund(self, body: dict[str, Any]) -> dict[str, Any]:
        with self.lock:
            deposit = self.deposits.get(str(body.get("deposit")))
            if deposit is None:
                raise RefusedError(HTTPStatus.BAD_REQUEST, "parameter_invalid", "deposit")
            if not deposit["final"]:
                raise RefusedError(HTTPStatus.BAD_REQUEST, "deposit_not_final")
            if deposit["status"] not in ("credited", "rejected"):
                raise RefusedError(HTTPStatus.BAD_REQUEST, "deposit_not_refundable")
            reserved = sum(
                int(r["amount_atomic"])
                for r in self.refunds.values()
                if r["deposit"] == deposit["id"] and r["status"] in ("pending", "succeeded")
            )
            remainder = int(deposit["amount_atomic"]) - reserved
            amount = int(body.get("amount_atomic") or remainder)
            token = self.chains[deposit["chain_id"]]["tokens"][deposit["asset_contract"]]
            if amount < MIN_REFUND_TOKENS * 10 ** token["decimals"] or remainder <= 0:
                raise RefusedError(HTTPStatus.BAD_REQUEST, "amount_too_small", "amount_atomic")
            if amount > remainder:
                raise RefusedError(HTTPStatus.BAD_REQUEST, "amount_too_large", "amount_atomic")
            refund = {
                "id": "re_" + uuid.uuid4().hex,
                "object": "refund",
                "livemode": False,
                "deposit": deposit["id"],
                "amount_atomic": str(amount),
                "destination_address": str(body["destination_address"]).lower(),
                # The treasury the deposit's address pays, which the refund must come from.
                "treasury": self.forwarders[f"{deposit['chain_id']}:{deposit['address']}"][
                    "treasury"
                ],
                "status": "pending",
                "failure_reason": None,
                "transaction_hash": None,
                "receipt_log_index": None,
                "created": int(time.time()),
                "metadata": dict(body.get("metadata") or {}),
            }
            self.refunds[refund["id"]] = refund
            self.emit("refund.created", refund)
            return refund

    def mark_paid(self, refund_id: str, body: dict[str, Any]) -> dict[str, Any]:
        with self.lock:
            refund = self.refund(refund_id)
            tx_hash = str(body["transaction_hash"]).lower()
            if refund["transaction_hash"] == tx_hash:
                return refund
            if refund["status"] != "pending" or refund["transaction_hash"] is not None:
                raise RefusedError(HTTPStatus.BAD_REQUEST, "refund_unexpected_state")
            index = body.get("receipt_log_index")
            refund.update(transaction_hash=tx_hash, receipt_log_index=index)
            self.emit("refund.updated", refund)
            return refund

    def cancel_refund(self, refund_id: str) -> dict[str, Any]:
        with self.lock:
            refund = self.refund(refund_id)
            if refund["status"] == "canceled":
                return refund
            if refund["status"] != "pending" or refund["transaction_hash"] is not None:
                raise RefusedError(HTTPStatus.BAD_REQUEST, "refund_unexpected_state")
            refund["status"] = "canceled"
            self.emit("refund.updated", refund)
            return refund

    def refund(self, refund_id: str) -> dict[str, Any]:
        refund = self.refunds.get(refund_id)
        if refund is None:
            raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
        return refund

    def end_quote(self, quote_id: str, status: str) -> dict[str, Any]:
        """An unpaid quote's end: `expired` at the end of its window, or `canceled`."""
        with self.lock:
            quote = self.quotes.get(quote_id)
            if quote is None:
                raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
            if quote["status"] != "open":
                raise RefusedError(HTTPStatus.BAD_REQUEST, "quote_not_open")
            quote["status"] = status
            if status == "expired":
                quote["expires_at"] = int(time.time())
            self.emit(f"quote.{status}", self.quote_view(quote))
            return self.quote_view(quote)

    def reverse(self, deposit_id: str) -> dict[str, Any]:
        """The finality watch's outcome for a proven-dropped transaction."""
        with self.lock:
            deposit = self.deposits.get(deposit_id)
            if deposit is None:
                raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
            if deposit["final"] or deposit["status"] == "reversed":
                raise RefusedError(HTTPStatus.BAD_REQUEST, "deposit_final")
            deposit.update(status="reversed", amount_reversed=deposit["amount"] or 0)
            for refund in self.refunds.values():
                if (
                    refund["deposit"] == deposit_id
                    and refund["status"] == "pending"
                    and refund["transaction_hash"] is None
                ):
                    refund["status"] = "canceled"
                    self.emit("refund.updated", refund)
            self.emit("deposit.reversed", deposit)
            return deposit

    # Balance, forwarders, sweeps --------------------------------------------------------------

    def unswept(self, deposit: dict[str, Any]) -> bool:
        return deposit["status"] != "reversed" and not deposit["swept"]

    def balance(self) -> dict[str, Any]:
        """The unswept amounts, per chain and token."""
        amounts: dict[tuple[int, str], dict[str, Any]] = {}
        with self.lock:
            for deposit in self.deposits.values():
                if not self.unswept(deposit):
                    continue
                key = (deposit["chain_id"], deposit["asset_contract"])
                amount = amounts.setdefault(
                    key,
                    {
                        "chain_id": deposit["chain_id"],
                        "token": deposit["asset_contract"],
                        "asset": deposit["asset"],
                        "amount_atomic": 0,
                        "final_amount_atomic": 0,
                    },
                )
                amount["amount_atomic"] += int(deposit["amount_atomic"])
                if deposit["final"]:
                    amount["final_amount_atomic"] += int(deposit["amount_atomic"])
        unswept = [
            {
                **a,
                "amount_atomic": str(a["amount_atomic"]),
                "final_amount_atomic": str(a["final_amount_atomic"]),
            }
            for a in amounts.values()
        ]
        return {"object": "balance", "livemode": False, "unswept": unswept}

    def config(self) -> dict[str, Any]:
        assets = [
            {
                "asset": token["asset"],
                "chain_id": chain_id,
                "confirmations": str(CREDIT_DEPTH),
                "contract": token["contract"].lower(),
                "decimals": token["decimals"],
                "max_deposit_atomic": str(10 ** (6 + token["decimals"])),
                "min_amount": 100,
                "min_deposit_atomic": "0",
                "min_refund_atomic": str(MIN_REFUND_TOKENS * 10 ** token["decimals"]),
                "pricing": token["pricing"],
                "quote_spread_bps": 0,
                "quote_tolerance_bps": 0,
                "quote_amount_decimals": 4,
                "quote_ttl_seconds": 900,
                "typical_credit_seconds": CREDIT_SECONDS,
                "typical_finality_seconds": FINAL_DEPTH,
            }
            for chain_id, chain in self.chains.items()
            for token in chain["tokens"].values()
        ]
        return {
            "object": "config",
            "livemode": False,
            "currency": "usd",
            "assets": assets,
            "max_open_amount_per_account": 1_000_000,
            "max_open_amount_per_customer": 500_000,
            "max_open_quotes": 100,
            "quote_creations_per_customer_per_minute": 10,
        }

    def list_forwarders(self, query: dict[str, str]) -> list[dict[str, Any]]:
        with self.lock:
            forwarders = [
                f
                for f in list(self.forwarders.values())[::-1]
                if query.get("chain_id", str(f["chain_id"])) == str(f["chain_id"])
            ]
            sweepable = query.get("sweepable", "").lower()
            if sweepable:
                forwarders = [
                    f
                    for f in forwarders
                    if any(
                        d["chain_id"] == f["chain_id"]
                        and d["address"] == f["address"].lower()
                        and d["asset_contract"] == sweepable
                        and d["final"]
                        and self.unswept(d)
                        for d in self.deposits.values()
                    )
                ]
            return forwarders

    def attestation(self, nonce: str) -> dict[str, Any]:
        public = self.key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
        report = attestation_report_data(
            bytes.fromhex(nonce), self.args.account, False, [(1, public)]
        )
        return {
            "object": "attestation",
            "account": self.args.account,
            "livemode": False,
            "webhook_keys": [
                {
                    "version": 1,
                    "public_key": "whpk_" + base64.b64encode(public).decode(),
                    "expires_at": None,
                }
            ],
            "report_data": report.hex(),
            "tdx_quote": "00" * 1024,
        }


def evidence() -> dict[str, Any]:
    events = [
        ("app-id", "e2e0000000000000000000000000000000000001"),
        ("compose-hash", "c0" * 32),
        ("os-image-hash", "05" * 32),
    ]
    return {
        "quote": "00" * 1024,
        "report_data": "00" * 64,
        "vm_config": "{}",
        "event_log": json.dumps(
            [{"imr": 3, "event": name, "event_payload": value} for name, value in events]
        ),
    }


def serve(fake: FakeTopup) -> ThreadingHTTPServer:
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            self.dispatch(self.get)

        def do_POST(self) -> None:
            self.dispatch(self.post)

        def do_OPTIONS(self) -> None:
            self.send(HTTPStatus.NO_CONTENT, {}, cors=True)

        def dispatch(self, route: Any) -> None:
            url = urlsplit(self.path)
            query = {k: v[0] for k, v in parse_qs(url.query).items()}
            parts = url.path.strip("/").split("/")
            try:
                route(url.path, parts, query)
            except RefusedError as refused:
                self.error(refused.status, refused.code, refused.param)

        def merchant(self) -> bool:
            return self.headers.get("authorization", "").startswith("Bearer ppay_")

        def get(self, path: str, parts: list[str], query: dict[str, str]) -> None:
            if path == "/evidences/quote.json":
                self.send(HTTPStatus.OK, evidence())
                return
            if parts[:2] == ["v1", "quotes"] and len(parts) == 3:
                quote = fake.quotes.get(parts[2])
                if quote is None:
                    raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
                if not self.merchant():
                    if query.get("client_secret") not in fake.secrets.get(quote["id"], []):
                        raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
                    with fake.lock:
                        self.send(HTTPStatus.OK, fake.client_quote(quote), cors=True)
                    return
                with fake.lock:
                    self.send(HTTPStatus.OK, fake.quote_view(quote))
                return
            if parts[:2] == ["v1", "deposit_addresses"] and len(parts) == 3:
                address = fake.addresses.get(parts[2])
                if address is None:
                    raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
                if not self.merchant():
                    if query.get("client_secret") not in fake.secrets.get(address["id"], []):
                        raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
                    with fake.lock:
                        self.send(HTTPStatus.OK, fake.client_deposit_address(address), cors=True)
                    return
                with fake.lock:
                    self.send(HTTPStatus.OK, fake.deposit_address_view(address))
                return
            if not self.merchant():
                raise RefusedError(HTTPStatus.UNAUTHORIZED, "api_key_missing")
            if path == "/v1/attestation":
                self.send(HTTPStatus.OK, fake.attestation(query["nonce"]))
            elif path == "/v1/deposits":
                with fake.lock:
                    data = [
                        _public(d)
                        for d in sorted(fake.deposits.values(), key=lambda d: -d["created"])
                        if all(
                            query.get(name, d[name]) == d[name]
                            for name in ("quote", "client_reference_id", "deposit_address")
                        )
                    ]
                self.send(HTTPStatus.OK, _page(path, data))
            elif parts[:2] == ["v1", "deposits"] and len(parts) == 3:
                deposit = fake.deposits.get(parts[2])
                if deposit is None:
                    raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")
                self.send(HTTPStatus.OK, _public(deposit))
            elif path == "/v1/refunds":
                with fake.lock:
                    data = [
                        r
                        for r in fake.refunds.values()
                        if query.get("deposit", r["deposit"]) == r["deposit"]
                    ][::-1]
                self.send(HTTPStatus.OK, _page(path, data))
            elif parts[:2] == ["v1", "refunds"] and len(parts) == 3:
                self.send(HTTPStatus.OK, fake.refund(parts[2]))
            elif path == "/v1/config":
                self.send(HTTPStatus.OK, fake.config())
            elif path == "/v1/balance":
                self.send(HTTPStatus.OK, fake.balance())
            elif path == "/v1/forwarders":
                self.send(HTTPStatus.OK, _page(path, fake.list_forwarders(query)))
            elif path == "/v1/sweeps":
                with fake.lock:
                    data = [
                        s
                        for s in fake.sweeps
                        if query.get("token", s["token"]).lower() == s["token"]
                        and query.get("chain_id", str(s["chain_id"])) == str(s["chain_id"])
                        and query.get("forwarder", s["forwarder"]) == s["forwarder"]
                    ]
                self.send(HTTPStatus.OK, _page(path, data))
            else:
                raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")

        def post(self, path: str, parts: list[str], _query: dict[str, str]) -> None:
            length = int(self.headers.get("content-length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
            if (
                parts[:2] in (["v1", "quotes"], ["v1", "deposit_addresses"])
                and len(parts) == 4
                and parts[3] == "transactions"
            ):
                # A hint acknowledges submission only; the stand-in's scanner records payments.
                self.send(
                    HTTPStatus.ACCEPTED,
                    {
                        "object": "transaction_submission",
                        "transaction_hash": body.get("transaction_hash", ""),
                        "status": "received",
                    },
                    cors=True,
                )
                return
            if parts[:2] == ["_test", "deposits"] and parts[3:] == ["reverse"]:
                self.send(HTTPStatus.OK, _public(fake.reverse(parts[2])))
                return
            if parts[:2] == ["_test", "mining"] and parts[2:] in (["pause"], ["resume"]):
                with fake.mining_lock:
                    fake.mining = parts[2] == "resume"
                self.send(HTTPStatus.OK, {"mining": fake.mining})
                return
            if parts[:2] == ["_test", "quotes"] and parts[3:] in (["expire"], ["cancel"]):
                status = "expired" if parts[3] == "expire" else "canceled"
                self.send(HTTPStatus.OK, fake.end_quote(parts[2], status))
                return
            if not self.merchant():
                raise RefusedError(HTTPStatus.UNAUTHORIZED, "api_key_missing")
            key = self.headers.get("idempotency-key", "").strip('"')
            if path == "/v1/quotes":
                self.send(HTTPStatus.OK, fake.create_quote(body, key))
            elif path == "/v1/deposit_addresses":
                self.send(HTTPStatus.OK, fake.create_deposit_address(body))
            elif path == "/v1/refunds":
                with fake.lock:
                    replay = fake.idempotent.get(key) if key else None
                    refund = replay or fake.create_refund(body)
                    if key:
                        fake.idempotent[key] = refund
                self.send(HTTPStatus.OK, refund)
            elif parts[:2] == ["v1", "refunds"] and parts[3:] == ["mark_paid"]:
                self.send(HTTPStatus.OK, fake.mark_paid(parts[2], body))
            elif parts[:2] == ["v1", "refunds"] and parts[3:] == ["cancel"]:
                self.send(HTTPStatus.OK, fake.cancel_refund(parts[2]))
            else:
                raise RefusedError(HTTPStatus.NOT_FOUND, "resource_missing")

        def error(self, status: HTTPStatus, code: str, param: str | None = None) -> None:
            error: dict[str, Any] = {
                "type": "invalid_request_error",
                "code": code,
                "message": code,
                "doc_url": DOCS + code,
            }
            if param is not None:
                error["param"] = param
            self.send(status, {"error": error}, cors=True)

        def send(self, status: HTTPStatus, body: dict[str, Any], *, cors: bool = False) -> None:
            payload = b"" if status == HTTPStatus.NO_CONTENT else json.dumps(body).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("request-id", "req_" + uuid.uuid4().hex)
            if cors:
                self.send_header("access-control-allow-origin", "*")
                self.send_header("access-control-allow-methods", "POST, OPTIONS")
                self.send_header("access-control-allow-headers", "Content-Type")
                self.send_header("access-control-max-age", "600")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, format: str, *args: Any) -> None:
            LOG.debug(format, *args)

    return ThreadingHTTPServer(("127.0.0.1", fake.args.port), Handler)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for name in ("chains", "product-webhook", "webhook-seed", "factory"):
        parser.add_argument(f"--{name}", required=True)
    for name in ("implementation", "account", "treasury"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--port", type=int, required=True)
    logging.basicConfig(level=logging.INFO, format="fake_service %(levelname)s %(message)s")
    logging.getLogger("httpx").setLevel(logging.WARNING)
    fake = FakeTopup(parser.parse_args())
    server = serve(fake)
    threading.Thread(target=fake.watch, daemon=True).start()
    try:
        server.serve_forever()
    finally:
        fake.stop.set()


if __name__ == "__main__":
    main()
