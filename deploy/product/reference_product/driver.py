"""The deposit driver: plays a Phala Cloud user against a running product.

It registers a workspace through the product's account API, gets a quote on one of the product's
chains (the config's first unless `chain_id` names another) and recomputes its address locally,
pays the exact locked amount with the chain's test token, polls until the deposit is
credited, and checks that the product ledger credited the locked amount exactly once and received
the verified `deposit.credited` webhook. Its options drive the abnormal paths instead: a different
amount, a payment after the quote window, another token, and
a refund request for a rejected deposit (deploy/phala.md, "Abnormal paths").
"""

from __future__ import annotations

import getpass
import logging
import os
import time
import uuid
from collections.abc import Callable
from datetime import UTC, datetime
from http import HTTPStatus
from pathlib import Path
from typing import Any
from urllib.parse import quote

import httpx
from eth_account import Account
from web3 import Web3
from web3.contract.contract import ContractFunction
from web3.middleware import SignAndSendRawMiddlewareBuilder

from topup_client.models import Deposit, Quote
from topup_sdk import RequestSigner, SigningAuth
from topup_sdk.addresses import same_address
from topup_sdk.signing import sf_string

from .config import ChainConfig, ProductConfig
from .server import quote_address

LOG = logging.getLogger(__name__)

# The driver pays this long after a quote's `expires_at` to be late by chain time too.
LATE_MARGIN_S = 60
# The test token's `mint` and `transfer`, both `(address to, uint256 amount)`.
TOKEN_ABI = [
    {
        "type": "function",
        "name": name,
        "inputs": [{"name": "to", "type": "address"}, {"name": "amount", "type": "uint256"}],
        "outputs": [],
        "stateMutability": "nonpayable",
    }
    for name in ("mint", "transfer")
]


class Payer:
    """Sends test-token transactions with web3.py, on the chain's RPC.

    On Anvil the payer is `payer`, an unlocked development account. On a testnet it signs with a
    Foundry keystore holding a funded throwaway test key: the account named by `payer_account`
    (`cast wallet import`), or else the keystore file named by `ETH_KEYSTORE`. As with `cast`, the
    keystore password comes from the mode-0600 file named by `ETH_PASSWORD`, or is prompted for;
    no key is ever passed in the environment or on a command line. The test token's `mint` is
    public, so the payer mints what it pays and needs only the testnet's ETH for gas.
    """

    def __init__(self, config: ProductConfig, chain: ChainConfig) -> None:
        self._web3 = Web3(Web3.HTTPProvider(chain.rpc_url))
        if self._web3.eth.chain_id != chain.chain_id:
            raise ValueError(f"the RPC of chain {chain.chain_id} reports another chain")
        keystore = os.environ.get("ETH_KEYSTORE")
        if config.payer_account is not None:
            keystore = str(Path.home() / ".foundry/keystores" / config.payer_account)
        if keystore:
            account = Account.from_key(Account.decrypt(Path(keystore).read_text(), _password()))
            signer = SignAndSendRawMiddlewareBuilder.build(account)
            self._web3.middleware_onion.inject(signer, layer=0)
            self.address = account.address
        elif config.payer is not None:
            self.address = Web3.to_checksum_address(config.payer)
        else:
            raise ValueError("set payer (Anvil), payer_account, or ETH_KEYSTORE")

    def mint_and_transfer(self, token: str, to: str, amount_atomic: int) -> str:
        erc20 = self._web3.eth.contract(Web3.to_checksum_address(token), abi=TOKEN_ABI)
        self._send(erc20.functions.mint(self.address, amount_atomic))
        return self._send(erc20.functions.transfer(Web3.to_checksum_address(to), amount_atomic))

    def _send(self, call: ContractFunction) -> str:
        tx_hash = call.transact({"from": self.address})
        if self._web3.eth.wait_for_transaction_receipt(tx_hash)["status"] != 1:
            raise RuntimeError(f"transaction {tx_hash.to_0x_hex()} reverted")
        return tx_hash.to_0x_hex()


def _password() -> str:
    path = os.environ.get("ETH_PASSWORD")
    if path is None:
        return getpass.getpass("keystore password: ")
    return Path(path).read_text(encoding="utf-8").rstrip()


class ProductApiError(Exception):
    def __init__(self, status: int, body: str) -> None:
        super().__init__(f"product answered {status}: {body[:200]}")
        self.status = status


class ProductApi:
    """The deposit driver's client for the product's account API, signed with the driver key."""

    def __init__(self, public_url: str, signer: RequestSigner) -> None:
        self._base = public_url.rstrip("/")
        self._http = httpx.Client(auth=SigningAuth(signer), timeout=60)

    def __enter__(self) -> ProductApi:
        return self

    def __exit__(self, *_: object) -> None:
        self._http.close()

    def register(self, team: str) -> None:
        self._call("POST", "/accounts", {"account_id": team})

    def quote(self, team: str, amount_minor: int, chain_id: int, asset: str) -> Quote:
        body = {"amount_minor": amount_minor, "chain_id": chain_id, "asset": asset}
        return Quote.from_dict(self._call("POST", f"/accounts/{quote(team)}/quotes", body))

    def account(self, team: str) -> dict[str, Any]:
        return self._call("GET", f"/accounts/{quote(team)}")

    def submit_transaction(self, team: str, quote_id: str, tx_hash: str) -> dict[str, Any]:
        return self._call(
            "POST",
            f"/accounts/{quote(team)}/quotes/{quote(quote_id)}/transactions",
            {"transaction_hash": tx_hash},
        )

    def restore_records(self, *, since: int | None = None) -> dict[str, Any]:
        """The product's records for a service restore (`GET /accounts/restore-records`)."""
        query = "" if since is None else f"?since={since}"
        return self._call("GET", "/accounts/restore-records" + query)

    def refund(
        self, team: str, deposit_id: str, destination_address: str, amount_atomic: str
    ) -> dict[str, Any]:
        body = {"destination_address": destination_address, "amount_atomic": amount_atomic}
        return self._call(
            "POST", f"/accounts/{quote(team)}/deposits/{quote(deposit_id)}/refunds", body
        )

    def _call(self, method: str, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
        # The product requires a signed Idempotency-Key on every POST, so a replay creates nothing.
        headers = {"Idempotency-Key": sf_string(str(uuid.uuid4()))} if method == "POST" else {}
        response = self._http.request(method, self._base + path, json=body, headers=headers)
        if response.status_code != HTTPStatus.OK:
            raise ProductApiError(response.status_code, response.text)
        value = response.json()
        if not isinstance(value, dict):
            raise ProductApiError(response.status_code, "not a JSON object")
        return value


def run_deposit(
    config: ProductConfig,
    driver: RequestSigner,
    *,
    amount_minor: int,
    min_atomic: int = 0,
    until: str = "credited",
    timeout: float = 1800,
    pay_bps: int = 10_000,
    pay_after_expiry: bool = False,
    token: str | None = None,
    refund_to: str | None = None,
    chain_id: int | None = None,
) -> None:
    """Registers a workspace through the product, pays one quote, and checks the outcome.

    By default it pays the exact amount of a fresh quote and expects exactly the quoted credit
    at the quoted price. `pay_bps` pays that fraction of the quote instead, and
    `pay_after_expiry` pays it after the quote's window; those deposits must be credited at spot.
    `token` pays another token. `chain_id` is the configured chain to pay on, the first by
    default; the quote is in that chain's first test token. `until` is `credited` or `swept` for
    a credit, `rejected` for a rejection, or `refunded`: a rejection, then a refund to
    `refund_to` for the whole deposit, which the operator pays from the refund's treasury and
    attaches with `POST /v1/refunds/{id}/mark_paid` while this waits for the `deposit.refunded`
    webhook.
    """
    chain = config.chain(chain_id)
    payer = Payer(config, chain)
    with ProductApi(config.public_url, driver) as api:
        team = f"team-{uuid.uuid4().hex[:12]}"
        api.register(team)
        LOG.info("registered workspace %s", team)

        quote = api.quote(team, amount_minor, chain.chain_id, chain.test_token.symbol.lower())
        # Pay only an address recomputed here from the account, workspace, quote id, and the
        # chain's treasury.
        if not same_address(quote_address(config, team, quote.id, chain.chain_id), quote.address):
            raise RuntimeError("quote address does not match the driver's own computation")
        address = quote.address
        amount_atomic = int(quote.amount_atomic) * pay_bps // 10_000
        LOG.info(
            "quote %s: pay %s atomic to %s before %s for %s cents (%s)",
            quote.id,
            quote.amount_atomic,
            quote.address,
            datetime.fromtimestamp(quote.expires_at, UTC).isoformat(),
            quote.amount,
            quote.payment_uri,
        )
        if amount_atomic < min_atomic:
            needed = -(-amount_minor * min_atomic // amount_atomic)
            raise RuntimeError(
                f"the payment would be {amount_atomic} atomic, below --min-atomic {min_atomic}; "
                f"nothing was paid; rerun with --amount-minor of at least {needed}"
            )
        if pay_after_expiry:
            wait_s = quote.expires_at + LATE_MARGIN_S - time.time()
            LOG.info("waiting %.0fs to pay after the quote window", max(wait_s, 0))
            time.sleep(max(wait_s, 0))

        tx_hash = payer.mint_and_transfer(token or chain.test_token.address, address, amount_atomic)
        try:
            api.submit_transaction(team, quote.id, tx_hash)
            LOG.info("transaction hint received for %s", tx_hash)
        except ProductApiError as error:
            LOG.warning("transaction hint unavailable: %s; scanner discovery will continue", error)
        LOG.info("paid %s atomic in %s from %s", amount_atomic, tx_hash, payer.address)
        if until in {"rejected", "refunded"}:
            _check_rejection(api, team, address, refund_to, timeout)
            return
        at_quote_price = pay_bps == 10_000 and not pay_after_expiry
        states = {"credited", "swept"} if until == "credited" else {"swept"}
        expired_ref = quote.id if pay_after_expiry else None
        deposit, view = _wait_for_account(
            api, team, timeout, lambda view: _credited(view, address, states, expired_ref)
        )
        credited = _event(view, "deposit.credited", id=deposit.id)
        if credited["price_source"] != ("quote" if at_quote_price else "spot"):
            raise RuntimeError(f"deposit was valued at the {credited['price_source']} price")
        if credited["client_reference_id"] != team or credited["quote"] != quote.id:
            raise RuntimeError(f"deposit.credited names another account or quote: {credited}")
        expected = quote.amount if at_quote_price else deposit.amount
        if credited["amount"] != expected:
            raise RuntimeError("credited amount differs from the expected credit")
        if deposit.quote != quote.id:
            raise RuntimeError("deposit does not reference its quote")
        credits = [(c["provider_order_id"], c["amount_minor"]) for c in view["credits"]]
        if credits != [(deposit.id, credited["amount"])]:
            raise RuntimeError(f"unexpected product ledger credits: {credits}")
        LOG.info(
            "deposit %s is %s: credited %s cents at the %s price (quoted %s); "
            "the ledger holds one credit",
            deposit.id,
            deposit.status,
            credited["amount"],
            credited["price_source"],
            quote.amount,
        )


def _check_rejection(
    api: ProductApi, team: str, address: str, refund_to: str | None, timeout: float
) -> None:
    """Waits for the deposit's rejection; with `refund_to`, requests and awaits its refund."""
    deposit, view = _wait_for_account(api, team, timeout, lambda view: _rejected(view, address))
    rejected = _event(view, "deposit.rejected", id=deposit.id)
    if view["credits"]:
        raise RuntimeError(f"a rejected deposit was credited: {view['credits']}")
    LOG.info(
        "deposit %s is rejected (%s); nothing was credited",
        deposit.id,
        rejected["rejection_reason"],
    )
    if refund_to is None:
        return
    # A deposit is rejected at the route's confirmations but refunded only once final.
    LOG.info("deposit %s: waiting for finality to request its refund", deposit.id)
    deposit, _ = _wait_for_account(api, team, timeout, lambda view: _final(view, address))
    refund = api.refund(team, deposit.id, refund_to, deposit.amount_atomic)
    LOG.info(
        "refund %s is %s: pay %s atomic from %s to %s, then attach the transaction with "
        "POST /v1/refunds/%s/mark_paid",
        refund["id"],
        refund["status"],
        refund["amount_atomic"],
        refund["treasury"],
        refund["destination_address"],
        refund["id"],
    )
    _, view = _wait_for_account(
        api,
        team,
        timeout,
        lambda view: (
            (deposit, view) if _find_event(view, "deposit.refunded", id=deposit.id) else None
        ),
    )
    refunded = _event(view, "deposit.refunded", id=deposit.id)
    if refunded["amount_refunded_atomic"] != refund["amount_atomic"]:
        raise RuntimeError(f"deposit.refunded differs from the request: {refunded}")
    LOG.info("refund %s succeeded; deposit %s is refunded", refund["id"], deposit.id)


def _deposit_at(view: dict[str, Any], address: str) -> Deposit | None:
    for item in view["deposits"]:
        deposit = Deposit.from_dict(item)
        if same_address(deposit.address, address):
            return deposit
    return None


def _find_event(view: dict[str, Any], event_type: str, **fields: str) -> dict[str, Any] | None:
    """The `data.object` of the first `event_type` webhook whose object has `fields`."""
    for event in view["events"]:
        found = event["data"].get("object") or {}
        if event["type"] == event_type and all(found.get(k) == v for k, v in fields.items()):
            return dict(found)
    return None


def _event(view: dict[str, Any], event_type: str, **fields: str) -> dict[str, Any]:
    event = _find_event(view, event_type, **fields)
    if event is None:
        raise RuntimeError(f"no {event_type} webhook for {fields}")
    return event


def _credited(
    view: dict[str, Any], address: str, states: set[str], expired_lock_ref: str | None
) -> tuple[Deposit, dict[str, Any]] | None:
    """Ready once the deposit is in `states` with its `deposit.credited` webhook (and
    `quote.expired` for a late payment) recorded."""
    deposit = _deposit_at(view, address)
    if deposit is None or ("swept" if deposit.swept else deposit.status) not in states:
        return None
    if _find_event(view, "deposit.credited", id=deposit.id) is None:
        return None
    if expired_lock_ref is not None and (
        _find_event(view, "quote.expired", id=expired_lock_ref) is None
    ):
        return None
    return deposit, view


def _rejected(view: dict[str, Any], address: str) -> tuple[Deposit, dict[str, Any]] | None:
    deposit = _deposit_at(view, address)
    if deposit is not None and deposit.status == "credited":
        raise RuntimeError(f"deposit {deposit.id} is {deposit.status}, not rejected")
    if deposit is None or deposit.status != "rejected":
        return None
    if _find_event(view, "deposit.rejected", id=deposit.id) is None:
        return None
    return deposit, view


def _final(view: dict[str, Any], address: str) -> tuple[Deposit, dict[str, Any]] | None:
    deposit = _deposit_at(view, address)
    return (deposit, view) if deposit is not None and deposit.final else None


def _wait_for_account(
    api: ProductApi,
    team: str,
    timeout: float,
    ready: Callable[[dict[str, Any]], tuple[Deposit, dict[str, Any]] | None],
) -> tuple[Deposit, dict[str, Any]]:
    """Polls the product's view of the workspace until `ready` returns a result."""
    deadline = time.monotonic() + timeout
    states: dict[str, str] = {}
    while time.monotonic() < deadline:
        try:
            view = api.account(team)
        except (httpx.HTTPError, ProductApiError) as error:
            if isinstance(error, ProductApiError) and error.status < 500:
                raise
            LOG.warning("product unavailable: %s", error)
            time.sleep(5)
            continue
        for item in view["deposits"]:
            if states.get(item["id"]) != item["status"]:
                LOG.info("deposit %s is %s", item["id"], item["status"])
                states[item["id"]] = item["status"]
        result = ready(view)
        if result is not None:
            return result
        time.sleep(5)
    raise TimeoutError(f"workspace {team} did not reach the expected state in {timeout:.0f}s")
