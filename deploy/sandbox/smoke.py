"""Sandbox smoke check: the calls a product makes against a Phala Pay service, with their checks.

These are the calls a product such as Phala Cloud makes, and the checks it adds to each:

1. pin the account's webhook keys from attestation evidence bound to a fresh nonce, fetched with
   the account's API key;
2. create a quote (the account is created with it); the client recomputes its fixed customer address
   from the pinned forwarder;
3. list the account's deposits;
4. receive webhooks (Standard Webhooks) and fulfill each `deposit.credited` once.

The handler of step 4 goes behind the product's webhook URL. It stops where the SDK stops: the
product commits the credit, keyed by `credit.fulfillment_key` under a unique index, before
answering `2xx`. deploy/product/reference_product is a complete product that does all of it.

Run steps 1-3 against the sandbox (deploy/sandbox/README.md):

    uv run --locked --project sdk/python python deploy/sandbox/smoke.py \\
        --config sandbox.json
"""

from __future__ import annotations

import argparse
import json
import secrets
import uuid
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, fields
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

from topup_client.models import Quote
from topup_sdk import (
    CreditedDeposit,
    TopupClient,
    WebhookEvent,
    verify_attestation_binding,
    verify_webhook,
)


@dataclass(frozen=True)
class Integration:
    """The product's settings; the sandbox config (deploy/sandbox/README.md) carries them all."""

    service_url: str
    account: str
    api_key_file: str
    chain_id: int
    factory: str
    implementation: str
    treasury: str
    token_symbol: str

    @classmethod
    def load(cls, path: str | Path) -> Integration:
        values = json.loads(Path(path).read_text(encoding="utf-8"))
        # The sandbox config lists the product's chains; this example integrates on the first,
        # with its first test token.
        chain = values["chains"][0]
        values |= {
            "chain_id": chain["chain_id"],
            "treasury": chain["treasury"],
            "token_symbol": chain["test_tokens"][0]["symbol"],
        }
        return cls(**{field.name: values[field.name] for field in fields(cls)})

    def livemode(self) -> bool:
        """The mode of the product's API key, and so of its webhooks."""
        key = Path(self.api_key_file).read_text(encoding="ascii")
        return key.startswith(("ppay_sk_live_", "ppay_rk_live_"))

    def client(self) -> TopupClient:
        """A client of the product's account (`account`, `acct_…`) with its API key; with
        the forwarder pinned, it recomputes every open quote's address before returning it."""
        return TopupClient(
            self.service_url,
            Path(self.api_key_file).read_text(encoding="ascii").strip(),
            account=self.account,
            forwarder=(self.factory, self.implementation),
            treasuries={self.chain_id: self.treasury},
        )


# 1. The webhook keys ----------------------------------------------------------------------------


def pin_webhook_keys(config: Integration, client: TopupClient) -> list[Ed25519PublicKey]:
    """Returns the account's webhook keys in the API key's mode, current first, from attestation
    evidence bound to a fresh nonce.

    The report data binds the nonce, the account, the mode, and the keys. In production, also
    verify the TDX quote with the dstack verifier (deploy/dstack-verifier.sh), then pin the keys
    in configuration.
    """
    nonce = secrets.token_bytes(32)
    evidence = client.attestation(nonce)
    return verify_attestation_binding(
        evidence,
        nonce,
        expected_account=config.account,
        expected_livemode=config.livemode(),
    )


# 2-3. Quotes and deposits --------------------------------------------------------------------


def quote(config: Integration, client: TopupClient, account: str, amount_minor: int) -> Quote:
    """Quotes `amount_minor` cents; the account is created with its first quote.

    The client raises `AddressMismatchError` before returning an address it did not derive from
    the pinned forwarder, the product slug, the account, and the quote id.
    """
    return client.create_quote(
        account, amount_minor, chain_id=config.chain_id, asset=config.token_symbol.lower()
    )


# 4. Webhooks and fulfillment ---------------------------------------------------------------------


def receive_webhook(
    config: Integration,
    webhook_keys: Sequence[Ed25519PublicKey],
    headers: Mapping[str, str],
    body: bytes,
    fulfill: Callable[[CreditedDeposit], None],
) -> WebhookEvent:
    """Verifies a delivery, fail-closed, as the account's event in the key's mode, and fulfills
    it when it is `deposit.credited`.

    `SignatureError` means answer `400`. `fulfill` must credit `credit.amount` cents to
    `credit.client_reference_id` at most once per `credit.fulfillment_key` (the `dep_` id),
    committing before this returns, and treat a repeat as done; answer `2xx` only after it
    returns. Every
    other event type is informational: notify the user and refresh history.
    """
    event = verify_webhook(
        headers,
        body,
        webhook_keys,
        expected_account=config.account,
        expected_livemode=config.livemode(),
    )
    if event.type == "deposit.credited":
        fulfill(CreditedDeposit.from_event(event))
    return event


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", required=True, help="the sandbox config JSON")
    parser.add_argument("--amount-minor", type=int, default=2500, help="quote amount in cents")
    args = parser.parse_args()
    config = Integration.load(args.config)
    with config.client() as client:
        keys = pin_webhook_keys(config, client)
        print(f"pinned {len(keys)} webhook key(s) of {config.account} from attestation")
        account = f"example-{uuid.uuid4().hex[:12]}"
        lock = quote(config, client, account, args.amount_minor)
        print(
            f"quote {lock.id}: pay {lock.amount_atomic} atomic to {lock.address} before "
            f"Unix time {lock.expires_at} for {lock.amount} cents ({lock.payment_uri})"
        )
        deposits = [
            (item.id, item.status) for item in client.list_deposits(client_reference_id=account)
        ]
        print(f"deposits: {deposits}")
    print("smoke: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
