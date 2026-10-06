# phala-pay

Phala Pay for Python (3.12+), in the shape of Stripe's SDK: create a
quote, hand its `client_secret` to the browser checkout (`@phala/pay`), and fulfil from
the signed `deposit.credited` webhook.

## Install

```sh
uv add phala-pay        # or: pip install phala-pay
```

`phala-pay` shares the service's version: pin the one equal to your operator's service version
([compatibility](../../docs/integration.md#compatibility)). `phala-pay[eoa]` adds the signer an EOA treasury's proof needs (`pay.treasuries.set_eoa`).

## Quickstart

The operator creates your account and hands your contact its first secret key, `ppay_sk_test_…`;
roll it on receipt, keep it offline for administration, and create a restricted key
(`ppay_rk_…`) for your servers.

Create a quote for the signed-in account and return its client secret to the browser:

```python
import os

from phala_pay import PhalaPay

pay = PhalaPay.from_env()  # PHALA_PAY_API_KEY and PHALA_PAY_PINS

quote = pay.quotes.create(
    client_reference_id="team-42",  # your id for the customer; credits are addressed to it
    amount=2500,  # US cents
    chain_id=11155111,
    asset="pha",
    idempotency_key=order_id,  # for 24 hours the same key replays this quote and client secret
)
# The page renders <Checkout clientSecret expectedAddress apiBase /> (@phala/pay).
return {"checkout": pay.checkout_params(quote)}
```

Fulfil from the webhook, once per deposit, and answer `2xx` after the credit is committed:

```python
from phala_pay import SignatureVerificationError

try:
    event = pay.webhooks.construct_event(raw_body, request.headers)
except SignatureVerificationError:
    return Response(status_code=400)

if event.type in {"deposit.credited", "deposit.refunded", "deposit.reversed", "deposit.rejected"}:
    # Credit, refund, and reversal alike: per deposit, serially, merge the snapshot and move the
    # balance to `amount - amount_refunded - amount_reversed` (0 unless credited or reversed).
    apply_deposit(event.deposit)
```

Events arrive in any order; the snapshot's cumulative `amount_refunded` and `amount_reversed`
make the result independent of it (docs/integration.md §2.3; `apply_deposit` in
`sdk/examples/fastapi_app.py`).

`WEBHOOK_PUBLIC_KEY` is your account's webhook key in the mode, `whpk_…`, pinned from `GET
/v1/attestation` (docs/integration.md §5.3); pass a list of keys while a rotation overlaps.
`construct_event` fails closed: it checks the Standard Webhooks signature, the timestamp (five
minutes' tolerance), that the body's id is the `webhook-id`, and that the event's `account` and
`livemode` are the expected ones;
`event.data.object` is the `Deposit` (a `Quote` for `quote.*`, a `Refund` for `refund.*`) as it
was when the event happened: it is rendered with the change and never re-rendered, so read the
object again for its current state. `*.updated` events carry `event.data.previous_attributes`, and
an event your own request caused names it in `event.request` (`id`, `idempotency_key`).
`sdk/examples/fastapi_app.py` is a complete FastAPI backend with both routes.

## Reference

| Call | API |
|---|---|
| `pay.account.retrieve()` / `.pause_quotes()` / `.resume_quotes()` / `.roll_webhook_key(expires_in=)` | `GET /v1/account`, `POST /v1/account/pause\|resume`, `POST /v1/account/webhook_keys/roll` |
| `pay.payment_settings.retrieve()` / `.update(chains=, quote_creations_per_customer_per_minute=)` | `GET\|POST /v1/payment_settings`: what the account accepts and on what terms; a new account accepts nothing |
| `pay.quotes.create(client_reference_id=, amount=, chain_id=, asset=, idempotency_key=, metadata=)` | `POST /v1/quotes` |
| `pay.quotes.retrieve(id)` / `.list(client_reference_id=, status=)` / `.cancel(id)` | `GET /v1/quotes[/{id}]`, `POST /v1/quotes/{id}/cancel` |
| `pay.quotes.update(id, metadata=)` | `POST /v1/quotes/{id}` |
| `pay.deposit_addresses.create(client_reference_id=, metadata=)` | `POST /v1/deposit_addresses`: the customer's active address, one for every supported token and network (`networks`), its recent `payments`, and a `client_secret` for `<DepositAddress>` |
| `pay.deposit_addresses.retrieve(id)` / `.list(client_reference_id=, status=)` / `.rotate(id)` / `.update(id, metadata=)` | `GET /v1/deposit_addresses[/{id}]`, `POST /v1/deposit_addresses/{id}/rotate`, `POST /v1/deposit_addresses/{id}` |
| `pay.deposits.list(client_reference_id=, quote=, deposit_address=, status=, tx_hash=, created_gt=, created_gte=, created_lt=, created_lte=)` | `GET /v1/deposits`, every page; `status` is `pending`, `credited`, `rejected`, or `reversed` |
| `pay.deposits.retrieve(id)` / `.update(id, metadata=)` | `GET /v1/deposits/{id}`, `POST /v1/deposits/{id}` |
| `pay.refunds.create(deposit=, destination_address=, amount_atomic=, metadata=)` / `.mark_paid(id, transaction_hash=, receipt_log_index=)` / `.cancel(id)` / `.retrieve(id)` / `.update(id, metadata=)` | `POST /v1/refunds`, `POST /v1/refunds/{id}/mark_paid`, `POST /v1/refunds/{id}/cancel`, `GET /v1/refunds/{id}`, `POST /v1/refunds/{id}` |
| `pay.refunds.list(deposit=, status=)` | `GET /v1/refunds`, every page |
| `pay.config.retrieve()` | `GET /v1/config` |
| `pay.balance.retrieve()` / `pay.sweeps.list(chain_id=, forwarder=, token=)` / `pay.forwarders.list(chain_id=, sweepable=)` | `GET /v1/balance`, `GET /v1/sweeps`, `GET /v1/forwarders` |
| `pay.treasuries.challenge(chain_id=, address=)` / `.create(chain_id=, message=, signature=)` / `.set_eoa(chain_id=, address=, private_key=)` / `.list()` / `.retrieve(id)` / `.cancel(id)` / `.pause(id)` / `.resume(id)` | `POST /v1/treasuries/challenge`, `GET\|POST /v1/treasuries`, `POST /v1/treasuries/{id}/cancel\|pause\|resume` |
| `pay.api_keys.create(name=, permissions=)` / `.list()` / `.retrieve(id)` / `.roll(id, expires_in=)` / `.revoke(id)` | `/v1/api_keys` |
| `pay.webhook_endpoints.create(url=, enabled_events=)` / `.list()` / `.retrieve(id)` / `.update(id, …)` / `.delete(id)` / `.test(id)` | `/v1/webhook_endpoints` |
| `pay.events.list(type=, types=, delivery_success=, created_gt=, …)` / `.retrieve(id)` / `.resend(id, webhook_endpoint=)` | `/v1/events` |
| `pay.export_account(directory)` | every list, written as JSON files |
| `pay.webhooks.construct_event(payload, headers)` (pins bind the keys, account, and mode; `phala_pay.Webhook` accepts explicit keys, account, and mode without a client) | verifies a webhook delivery |

Every request sends the API key as `Authorization: Bearer …`: a restricted key, `ppay_rk_…`, for
production servers (`pay.api_keys.create(permissions=[...])`, which never manages keys,
treasuries, webhook endpoints, webhook keys, or account settings), or a secret key, `ppay_sk_…`,
kept offline for administration. Transport errors, `429`
(after its `Retry-After`), `5xx`, and `409 idempotency_key_in_use` are retried with backoff,
reusing one `Idempotency-Key` per `POST`; a response the service saved for the key, even a `500`,
comes back marked `Idempotent-Replayed` and is raised as it is, since the request already ran.
Failures raise `ApiError` with the service's stable `code`, `error_type`, `param`, `doc_url`,
`request_id` (the response's `Request-Id`), and `retry_after`. Business-state failures, such as
`deposit_not_final` or `quote_unexpected_state`, are `400`; only `idempotency_key_in_use` is
`409`. Status arguments have `Literal` hints, and `EventType` names every event (`phala_pay.QuoteStatus`,
`DepositStatus`, `RefundStatus`, `TreasuryStatus`, `EventType`, …); the generated models keep
statuses as `str`, so a value added later still parses.
`forwarder=(factory, implementation)`, pinned from the attested deployment, `treasuries={chain_id:
treasury}`, your own treasury per chain as you proved it, and `account=` (`acct_…`) are the pins
every open quote and every network of an active deposit address is recomputed from, never the
response's `treasury`: a compromised service could return an attacker's treasury with its valid
address. An address you cannot derive, or one naming another treasury than your pinned one of its
chain, raises `AddressMismatchError`. A live key requires all three pins and fails closed without
them; in test mode `account` is read once from `GET /v1/account`, and an unpinned treasury falls
back to the response's with an `UnpinnedTreasuryWarning`. `topup_sdk.deposit_address(factory, implementation, treasury, account=, livemode=,
client_reference_id=, version=)` recomputes any deposit address offline; pass the treasury of the
network, since a chain whose treasury differs has its own address.

**Sweeping.** Funds stay in the forwarders until you sweep them, with your own wallet or Safe:

```python
from topup_sdk import flush_transactions, safe_batch, write_safe_batch

forwarders = list(pay.forwarders.list(chain_id=1, sweepable=PHA))  # never a sanctioned one
calls = flush_transactions(forwarders, PHA)  # offline: one factory.flush per treasury
write_safe_batch("sweep.json", safe_batch(1, TREASURY_SAFE, calls))  # for the Transaction Builder
```

`flush_transaction(factory, treasury, salts, token)` encodes one call offline from exported
forwarders (`pay.export_account` writes them all), so funds stay sweepable without the service;
`safe_batch` writes the Safe Transaction Builder's `BatchFile` JSON with its checksum.

**Treasuries.** An EOA proves itself with `pay.treasuries.set_eoa(chain_id=, address=,
private_key=)` (install `phala-pay[eoa]`); a Safe's owners sign the challenge's `message` as a Safe
message with the Safe{Core} SDK and submit it with `pay.treasuries.create` (docs/integration.md
§1.6, "Safe treasuries").

For metadata limits, merge semantics, and sensitive-data guidance, see
[Metadata](../../docs/integration.md#14-metadata).

Lower-level modules: `topup_sdk` (webhook and admin request signatures, address derivation,
attestation, `TopupClient`) and `topup_client` (generated from `crates/topup/openapi.json`; do not
edit). `uv run topup-sdk send-test-event --url … --seed-file test.seed --client-reference-id …` sends a
signed test event, a duplicate, and a forged copy to a webhook receiver whose test instance pins
that seed's public key. See `docs/integration.md` for the integration guide and the versioning
and deprecation policy, `deploy/product/reference_product` for a complete product, and
`deploy/sandbox/README.md` for the sandbox.

## Development

```sh
make sync    # install the locked environment
make check   # ruff, mypy --strict, pytest, and the regeneration no-op check
```

`phala-pay` is released with the service: the `v<version>` tag's Release workflow publishes it to
PyPI (environment `pypi`) with trusted publishing
([CONTRIBUTING.md, "Releasing"](../../CONTRIBUTING.md#releasing)). Its changes are in the top-level
[CHANGELOG.md](https://github.com/Phala-Network/phala-pay/blob/main/CHANGELOG.md), under "Python
SDK"; [its releases before v0.5.0](https://github.com/Phala-Network/phala-pay/blob/main/sdk/python/CHANGELOG.md),
versioned on their own, stay in `sdk/python`.

### Pins-based client

```python
from phala_pay import PhalaPay

pay = PhalaPay.from_env()  # PHALA_PAY_API_KEY and PHALA_PAY_PINS
quote = pay.quotes.create(
    client_reference_id="order-1", amount=2500, chain_id=11155111, asset="pha"
)
checkout = pay.checkout_params(quote)
event = pay.webhooks.construct_event(raw_body, request.headers)
```

Network resource methods accept `request_deadline` in seconds and `upgrade_tolerance`; every POST
also accepts `idempotency_key`. `list()` returns an iterator, except the existing `api_keys.list()` and
`treasuries.list()` which return lists. Every list resource has `list_page(limit=100,
starting_after=None, request_deadline=None)` returning `{"data": [...], "has_more": bool}`.
Empty continuing pages and repeated cursors raise `ResponseValidationError`.

Timeouts include response bodies; the logical deadline includes attempts and sleeps. Retries use
one frozen POST body/key, half-to-full exponential jitter (0.5, 1, 2 seconds, capped at 5), and
Retry-After as a minimum. Redirects and DELETE requests are never retried or followed. Raw httpx
failures are wrapped in `TransportError` with `network` or `timeout`; Python interrupts propagate.
Malformed success responses raise `ResponseValidationError` with status and request ID. Error and
object diagnostics redact API keys and checkout client secrets. Context cleanup closes the client
pool and leaves injected transports open.

For backend work that can wait through a planned upgrade, opt in at construction or per call:

```python
pay = PhalaPay(api_key, pins=pins, upgrade_tolerance=True)
quote = pay.quotes.create(
    client_reference_id="order-42",
    amount=2500,
    chain_id=11155111,
    asset="pha",
    idempotency_key="order-42",
)
quote = pay.quotes.retrieve(quote.id, upgrade_tolerance=False)  # Interactive override
```

The default is `False`: 15 seconds per attempt, four attempts, and 60 seconds total. For GETs and
replayable idempotent POSTs, `upgrade_tolerance=True` lets maintenance (`503 service_maintenance`),
network failures/attempt timeouts, and gateway 502/503/504 retry beyond four attempts for at most
300 seconds from the original request start, including attempts, body reads, and sleeps. Upgrade
backoff uses half-to-full exponential jitter from 0.5 seconds with a 10-second cap; Retry-After
seconds or HTTP-date is a minimum. HTML, empty, and malformed gateway errors become `TransportError`
without exposing raw bodies. Other responses keep the ordinary attempt cap and validation.

An explicit constructor or per-call `request_deadline` remains a hard limit, even an explicitly
supplied 60 seconds; omission keeps the interactive 60-second default eligible for extension.
Per-call `upgrade_tolerance=True` or `False` overrides the client. POST body and key stay fixed;
DELETE, interrupts (`KeyboardInterrupt`), replayed errors, redirects, and permanent failures end
retries. Keep the application's request budget long enough, or run this work asynchronously in a
worker thread. On exhaustion, preserve the order's explicit key for a later retry.

Pins require a canonical origin and valid API key checksum. A normalized override must match pins;
HTTP is limited to test loopback. `PhalaPay` requires pins and never discovers trust from the
service. Use `PhalaPay(api_key, pins=pins)` or `PhalaPay.from_env()` with `PHALA_PAY_API_KEY` and
`PHALA_PAY_PINS`.
