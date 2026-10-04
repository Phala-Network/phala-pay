# Design: SDK integration ergonomics

Status: **Proposed**. JS/Python implementation contract, documentation only; Go is out of scope ([security parity](go-sdk.md)).

## 1. Purpose and precedents

Configure trust once; create verified quotes and apply signed snapshots without handwritten HTTP/crypto.
[Integration](../integration.md) and [OpenAPI](../../crates/topup/openapi.json) govern semantics. References inspected:

| Reference | Adopt / avoid |
|---|---|
| [stripe-node](https://github.com/stripe/stripe-node#readme), [stripe-python](https://github.com/stripe/stripe-python#readme) | Instance resources, generated types, options/errors, pagination and idempotent retries. |
| [Stripe.js/Elements](https://docs.stripe.com/payments/accept-a-payment?platform=web&ui=elements) | Backend creation then browser client-secret handoff. |
| [Coinbase Commerce Node](https://github.com/coinbase/coinbase-commerce-node#readme) | Key/base/timeout configuration; avoid global client and shared-secret signatures. |
| [BTCPay Node](https://github.com/btcpayserver/node-btcpay#readme), [Greenfield](https://docs.btcpayserver.org/Development/GreenFieldExample/) | One-time setup then client recreation/scoped API keys; avoid legacy pairing and secret-printing examples. |

Preserve Phala treasury proof, attestation, and Ed25519 semantics across these ergonomic patterns.

## 2. Server clients and transport

JS: `new PhalaPay({apiBase, apiKey, pins})` from `@phala/pay/server`, with `pins: string | Pins`;
`PhalaPay.fromEnv(env = process.env)` performs no network requests or dotenv loading. For example,
`pay.quotes.create(params, {idempotencyKey: "order-42", signal})` returns `Promise<Quote>`.

Python:
`PhalaPay(api_base, api_key, *, pins: str | Pins, timeout=15.0, max_attempts=4, request_deadline=60.0, transport=None)`
and `PhalaPay.from_env(env: Mapping[str, str] | None=None)`. JS constructor adds `timeoutMs=15000`, `maxAttempts=4`,
`requestDeadlineMs=60000`, and injectable `fetch`. Python stays synchronous with `close()`/context-manager ownership;
use a worker thread from async routes. JS AbortSignal cancels; Python interrupts are not retried. JS `close(): Promise<void>` aborts owned requests; never closes injected fetch.

JS controls/resources camelCase: depositAddresses, paymentSettings, apiKeys, webhookEndpoints;
Python snake_case. Both retain snake_case wire/object fields and these Python resource methods:

| Resource (Python spelling) | Methods (JS camelCases method names) |
|---|---|
| `quotes` | `create`, `retrieve`, `list`, `update`, `cancel` |
| `deposit_addresses` | `create`, `retrieve`, `list`, `update`, `rotate` |
| `deposits` | `retrieve`, `list`, `update` |
| `refunds` | `create`, `retrieve`, `list`, `update`, `cancel`, `mark_paid` |
| `payment_settings` | `retrieve`, `update` |
| `config`, `balance` | `retrieve` |
| `account` | `retrieve`, `pause_quotes`, `resume_quotes`, `roll_webhook_key` |
| `treasuries` | `challenge`, `create`, `retrieve`, `list`, `cancel`, `pause`, `resume` |
| `api_keys` | `create`, `retrieve`, `list`, `roll`, `revoke` |
| `webhook_endpoints` | `create`, `retrieve`, `list`, `update`, `delete`, `test` |
| `events` | `retrieve`, `list`, `resend` |
| `sweeps`, `forwarders` | `list` |

JS: `create(params, options?): Promise<T>`, `retrieve(id, params?, options?): Promise<T>`,
`update(id, params, options?): Promise<T>`; actions `(id, params?, options?)` or `(id, options?)` when parameterless.
Singleton retrieve `(options?)`, update/challenge `(params, options?)`, account actions `(params?, options?)`. Params
map to operationId's OpenAPI body/query. Python retains keyword signatures, adding `idempotency_key` to every POST and
per-call `request_deadline`. In both SDKs, api_keys.create without permissions creates a secret key; a list creates a
restricted key. Offline treasury signing and sweep/Safe builders stay separate; JS need not take private keys.

`list(params?, options?): AsyncIterable<T>` in JS, existing Python `Iterator[T]` (existing `api_keys.list`
/`treasuries.list` remain materialized lists). Add `listPage` /`list_page` everywhere, returning `{data, has_more}` with
`limit` /`starting_after` query inputs. Iterators follow the last id, stop only at `has_more=false`, and raise
`ResponseValidationError` on empty continuing pages or repeated cursors. Quote/address verification also runs on every
page item and action result.

Generate internal paths/components from committed OpenAPI 3.1 with pinned MIT
[openapi-typescript 7.13.0](https://github.com/openapi-ts/openapi-typescript#readme); Python keeps
openapi-python-client==0.29.1/Makefile. Public QuoteCreateParams/Quote/Deposit alias generated types.
CI requires regeneration no-diff; test null/absent, metadata clears, expandables and open enums.
Atomic amounts/rates stay strings; Python preserves int64. JS uses numbers, lossless decoding
rejects unsafe integers before rounding (ResponseValidationError); reject unsafe inputs too.

RequestOptions is `{idempotencyKey?: string, signal?: AbortSignal, requestDeadlineMs?: number}`. Generate one UUID per
logical POST, freeze serialized body and key across attempts, validate the 255-character header limit, and encode
IDs/query values rather than interpolating raw input. Explicit order keys survive process restarts; automatic keys only
protect one invocation. Retry transport failures, 429, 500/502/503/504, and only `409 idempotency_key_in_use`; never
every 409. `Idempotent-Replayed: true` ends retries even for a saved error. Default four total attempts, exponential
jitter in `[0.5, 1, 2]` seconds (each delay randomized between half and full), capped at 5 seconds. A valid non-negative
Retry-After seconds/date is a minimum wait; if it exceeds the remaining 60-second total budget, return the last error,
without retrying earlier. Timeout covers body reads; deadline covers all attempts and sleeps; cancellation interrupts
both without retry. No retries of DELETE; POSTs without replayable bodies are rejected locally. Validate finite positive
timeouts and integer attempts in 1..10. Never redirect authenticated requests; HTTPS origins only, except explicit
test-mode HTTP loopback. Reject URL credentials, query, fragment, and path prefixes.

JS exports PhalaPayError, ApiError, TransportError, ConfigurationError, ResponseValidationError, AddressMismatchError,
AttestationError, SignatureVerificationError and LedgerSnapshotError; Python mirrors these under TopupError. Transport
wrapping replaces raw httpx errors (breaking); TransportError.code is network, timeout or cancelled. ApiError fields: statusCode, code, message, errorType, param, docUrl,
requestId, retryAfter; Python keeps existing snake_case fields (error_type). Optional fields are null, retryAfter
seconds. Malformed/non-JSON responses raise ResponseValidationError with status/request ID, never raw body. Preserve
diagnostic causes; redact credentials/client secrets in logs/reprs. Handlers expose generic codes, never exception text.
Keep WebhookSignatureError as JS alias. No global mutable client.

`@phala/pay` stays keyless. `/server` browser exports fail at build time; merchant constructor rejects non-Node
execution before reading keys/network. Browser bundles contain no merchant/env/key code or Node dependencies. Preserve
non-browser WebCrypto/offline helpers through `@phala/pay/server/helpers`. Go retains its context/error/retry/security
contract; no Go code.

## 3. One pins value and bound webhooks

`PHALA_PAY_PINS=ppay_pins_v1.<base64url>`: canonical unpadded RFC 4648 URL-safe base64 of UTF-8 JSON (reject nonzero unused bits). The following decoded
example is schematic (setup emits real addresses and keys):

```json
{
  "api_base": "https://api.phala-pay.example", "account": "acct_...", "livemode": false,
  "factory": "0x...", "implementation": "0x...",
  "treasuries": {"11155111": "0x..."},
  "webhook_keys": [{"version": 1, "public_key": "whpk_..."}]
}
```

Export `parsePins(value: string): Pins` / `parse_pins(value: str) -> Pins`, and `encodePins(pins): string` /
`encode_pins(pins) -> str`. Frozen values; encoding recursively sorts ASCII object keys, uses compact JSON, lowercase
addresses, and ascending webhook-key versions; serialize textual chain keys in ASCII order, not JS property enumeration order. Parsing accepts any key order, rejects duplicate/unknown/missing fields,
invalid UTF-8/base64, non-object JSON, unknown format versions, and decoded payloads over 16 KiB. Account is `acct_`
plus 32 lowercase hex digits; addresses are nonzero 20-byte hex, accepted case-insensitively. Treasury keys are
canonical positive decimal safe-integer chain IDs (no leading zeros), at least one chain. Webhook keys are nonempty,
unique positive uint32 versions and unique `whpk_` values whose standard base64 decodes to exactly 32 bytes; boolean
mode only. Cross-language golden encoding and rejection vectors are mandatory. Pins are public configuration, not a
signed trust certificate: replacing the env value replaces trust, so protect deployment configuration.

Both env constructors require exactly `PHALA_PAY_API_BASE`, `PHALA_PAY_API_KEY`, `PHALA_PAY_PINS`; ignore unrelated
env vars, never merge legacy trust vars. They do not read files. Parse only `ppay_{sk|rk}_{test|live}_` key forms with
the existing length/checksum rules; derive mode from the validated prefix, never an `expected_livemode` env override.
Normalize origins (lowercase host/scheme, remove default port/root slash); pins origin and mode must match the client; configured account/mode must match responses wherever present. Missing pins
fail at construction in either mode; expose readonly `pay.pins` and `pay.livemode`. Explicit legacy Python constructors remain during migration, with
their existing test-only warning fallback; live mode never gains a fallback. Combining `pins` with old
account/forwarder/treasury arguments is `ConfigurationError`.

Include webhook public keys in pins: one copy/paste and atomic account/mode/address/key deployment beats an
independently drifting fourth variable. Never fetch keys when verifying a delivery. Every open quote and active
deposit-address network is recomputed from pinned account, contracts, and that chain's treasury; compare returned
treasury and address, reject missing chains. Preserve existing salt algorithms and vectors. Closed/retired objects are
readable history, not payable; checkout handoff requires an open verified quote. Treasury changes deliberately require
new pins once active; do not silently allow old and new treasuries or trust response values.

Bound methods: `await pay.webhooks.constructEvent(body, headers, {tolerance: 300})` and
`pay.webhooks.construct_event(body, headers, *, tolerance=300)`. Body is original bytes (JS Uint8Array or exact UTF-8
string); headers are case-insensitive Headers/Node map or Python Mapping; reject duplicate signing headers. Use pinned keys/account and derived mode; callers cannot
override them. Return typed Event with `event.deposit` on deposit events; wrong resource access raises a type error.
Unknown event types retain raw objects. Reject bad signatures, header/body ID mismatch, other account/mode, and
malformed signed envelopes before fulfillment. Signature/envelope failures on this bound path raise
`SignatureVerificationError`; invalid tolerance is a local configuration error. Preserve Ed25519 v1a and the inclusive
bilateral 300-second window; zero means exact timestamp match. Keep explicit `constructEvent`/`Webhook.construct_event` signatures/errors; legacy Python clients retain their old webhook calling form. No unsigned JSON input. Rotation requires verified attestation,
explicit replacement pins with both keys, deployment during overlap, then explicit old-key removal; signed notices never
auto-update pins. Late retiring-key notices remain verifiable while that key is explicitly pinned.

## 4. Guided setup

Primary command: `npx @phala/pay@X.Y.Z setup` (shorthand `npx @phala/pay setup`). One Node tool serves either backend
language and avoids duplicating sensitive setup logic. Add the package bin; defer a Python CLI alias. Match X.Y.Z to the
operator until §8 is decided. Inputs: `--api-base`, `--account`, `--webhook-url`, repeatable
`--treasury CHAIN:ADDRESS`, `--asset CHAIN:ASSET`, `--app-id`, `--compose-hash`, `--factory`, `--implementation`,
`--env-file .env`, `--state-dir .phala-pay/setup`, and `--resume`. Default test mode; `--live` is explicit and must
match the hidden-prompt admin key. Prompt for omitted non-secret inputs; no secret CLI arguments.

1. Verify operator identity/contracts against an independently verified release manifest/config;
   GET config is not contract provenance. Hidden-prompt first secret key, identify id with
   api_keys.list, roll with 3600-second overlap; persist replacement before revoking the original.
2. Fresh 32-byte nonce, digest-pinned official dstack verifier: require UpToDate TCB, expected
   app ID/compose hash, quote report data/padding, then nonce/account/mode/key binding. Binding
   alone is insufficient. Missing verifier/evidence stops both modes; simulator setup is separate.
3. Export exact SIWE text, import signature with account/chain/address summary; never wallet keys.
   EOA personal_sign and deployed Safe EIP-1271 follow integration §1.6; Safe owners collect
   Safe-message threshold signatures. Print human tasks/expiry (10 minutes EOA, 24 hours Safe);
   expired challenges need fresh signatures. Pending 48-hour live changes stop readiness.
4. Once active, reconcile complete payment settings within catalog bounds and webhook endpoint
   with `enabled_events=["deposit.credited", "deposit.rejected", "deposit.refunded", "deposit.reversed"]`; security notices are automatic. Reuse exact URL/event-set match; ambiguous
   endpoints/divergent settings require operator choice. Never roll webhook keys on rerun.
5. Runtime restricted key defaults to quotes.write only. Feature selections add account.read
   (config), deposit_addresses.write, deposits.read/events.read (reconciliation); no admin grants.
   Persist returned secret immediately. Pins contain verified evidence and chosen active treasuries.
   Atomically write three-variable .env block (0600); offline admin key goes to separate file/vault.

State journal is versioned, account/origin/mode-bound, mode 0600 under a 0700 directory, ignored by Git along with
env/admin files. Record intents/body hashes/stable idempotency keys before mutations and reconcile resource ids after
interruption; lock out simultaneous setup processes. Never print API/client secrets, HTTP bodies, env blocks, wallet
material, or verifier raw errors to terminal/logs. Reject symlink outputs, preserve unrelated env lines, and report only
paths, redacted IDs, and human tasks. Never overwrite an existing managed credential without confirmation. A lost
key-create/roll response replays an id without secret: recover by rolling that key with the still-valid admin
credential, persist it, then retire the lost key. Expired idempotency records require list/reconciliation, not blind
recreation. If no admin key survives, stop for operator recovery. Repeating completed setup does not mint keys/endpoints
or rewrite settings. Exit 0 when configured; 2 for pending human action; 1 for failure. Resume uses the same command
with --resume, using privately persisted admin credentials (or hidden re-entry). Endpoint testing waits for a running receiver; it proves delivery/signature, not a paid deposit.

## 5. Checkout handoff and non-React UI

`pay.checkoutParams(quote): CheckoutParams` and `quote.checkout_params() -> dict[str, str]` return the shared browser-exported CheckoutParams type:
`{clientSecret, expectedAddress, apiBase}` (camelCase JSON in both). Python's handwritten Quote wrapper delegates
generated fields and retains client/base verification context. JS accepts this client's checked Quote only; both recheck
pins/open status and require create/replay client_secret, else ResponseValidationError. Retrieval cannot recover a
secret. Backend returns `{checkout: ...}` to the order's authenticated browser; never exposes pins/API keys.

```tsx
"use client";
import { useState } from "react";
import { Checkout } from "@phala/pay/react";
import type { CheckoutParams } from "@phala/pay";
import "@phala/pay/styles.css";
export function CheckoutPage() {
  const [checkout, setCheckout] = useState<CheckoutParams | null>(null);
  const [error, setError] = useState("");
  async function start() {
    try {
      const response = await fetch("/topups", { method: "POST" });
      if (!response.ok) throw new Error("Payment unavailable");
      setCheckout((await response.json()).checkout);
    } catch { setError("Payment unavailable. Please retry."); }
  }
  return checkout ? <Checkout {...checkout} /> :
    <><button onClick={start}>Pay with crypto</button><p role="alert">{error}</p></>;
}
```

Current individual props continue working: spreading the object is the same API. Browser public fetches must still
compare expectedAddress, including wallet/QR/manual paths, and fail closed. Do not persist/log client secrets or fulfill
from onSuccess. Keep API calls under the merchant's authenticated same-origin backend; no server credential in browser
configuration.

Recommend existing `createCheckout({...checkout})` with vanilla/Vue/Svelte examples for subscribe, render, wallet
errors, cancel and teardown. It already owns polling/verification. `<phala-checkout>` adds shadow-DOM styling,
accessibility, SSR/registration, secret attributes, events and cleanup. Defer the element to a separate proposal after
demonstrated demand; no new element here.

## 6. Pure accounting helpers and ledger contract

Export `depositNetAmount(deposit): number` / `deposit_net_amount(deposit) -> int` and
`balanceDelta(previous: LedgerSnapshot | null, current: Deposit): BalanceChange` /
`balance_delta(previous: LedgerSnapshot | None, current: Deposit) -> BalanceChange`. Snapshot is a JSON-serializable readonly mapping (Python TypedDict) of
`{id, livemode, client_reference_id, currency, status, amount, amount_refunded, amount_reversed}`; amount is null until valuation. Change is `{snapshot, contribution, delta}`; Python exposes attributes with these names. No IO,
mutation, event-id cache, or floats.

Net is amount minus cumulative refunds/reversals for credited/reversed, zero otherwise. Merge pending <
credited/rejected < reversed; larger cumulative deductions win. Credited vs rejected conflict,
identity/customer/currency/mode conflict, changed non-null valuation, unknown status, negative/invalid integers or
deductions above valuation raise `LedgerSnapshotError`, never clamp. Null amount can become valued once; never
accept null credited amount. Unvalued pending/rejected/reversed snapshots net to zero only with zero deductions;
a valued reversed snapshot must reverse the full amount. Reject simultaneous refunds and reversal. JS validates safe-integer arithmetic, Python exact integers; raw future statuses stay
readable but must not credit. Metadata is not an authority to select the customer or order.

Ledger key is `(account, livemode, deposit.id)`; uniquely insert if missing, lock it, merge, add delta to the
customer's currency balance, and save snapshot/contribution in ONE transaction. Recompute prior contribution from stored
snapshot; require it equal stored applied value if stored separately. Commit before 2xx; failures return 5xx. Event-ID
deduplication alone is insufficient. Verify customer/order ownership against merchant records, parameterize queries, and
serialize concurrent first insert. Replaced deposits have distinct ids. Refund/reversal-first, duplicates, replays,
concurrent and reordered snapshots must converge, with no credit for pending/rejected.

## 7. Proposed quickstarts and acceptance

Target **N = 100 nonblank merchant code lines per backend**, including ledger/error paths and shared
frontend; installs/setup excluded. These complete local apps fix customer/amount/asset; production
must authenticate/rate-limit orders. Database is account/test-mode scoped; startup rejects live.

Install Express/better-sqlite3 plus exact-version @phala/pay, or FastAPI/uvicorn plus exact-version
phala-pay. Run §4's setup once, sign the treasury challenge, load .env through the process manager
and start the receiver at its configured reachable test URL. POST /topups returns checkout; pay the
displayed test-token amount with a funded wallet. GET /balance shows 2500 after verified commit.

Node (`merchant.mjs`, `node merchant.mjs`; Express raw route precedes any JSON middleware):

```javascript
import express from "express";
import Database from "better-sqlite3";
import { PhalaPay, balanceDelta, SignatureVerificationError } from "@phala/pay/server";
const pay = PhalaPay.fromEnv();
if (pay.livemode) throw new Error("This example requires test mode");
const db = new Database(`merchant-${pay.pins.account}-test.sqlite`);
db.exec(`CREATE TABLE IF NOT EXISTS credits(id TEXT PRIMARY KEY, snapshot TEXT NOT NULL); CREATE TABLE IF NOT EXISTS balances(customer TEXT PRIMARY KEY, amount INTEGER NOT NULL);`);
const apply = db.transaction((deposit) => {
  if (deposit.client_reference_id !== "team-42" || deposit.currency !== "usd") throw new Error("Unknown customer");
  const row = db.prepare("SELECT snapshot FROM credits WHERE id=?").get(deposit.id);
  const change = balanceDelta(row ? JSON.parse(row.snapshot) : null, deposit);
  db.prepare("INSERT INTO credits VALUES(?,?) ON CONFLICT(id) DO UPDATE SET snapshot=excluded.snapshot").run(deposit.id, JSON.stringify(change.snapshot));
  db.prepare("INSERT INTO balances VALUES(?,?) ON CONFLICT(customer) DO UPDATE SET amount=amount+excluded.amount").run("team-42", change.delta);
});
const app = express();
app.post("/webhooks/phala-pay", express.raw({ type: "application/json", limit: "1mb" }), async (req, res) => {
  let event;
  try { event = await pay.webhooks.constructEvent(req.body, req.headers); }
  catch (e) { return res.sendStatus(e instanceof SignatureVerificationError ? 400 : 500); }
  try { if (event.type.startsWith("deposit.")) apply.immediate(event.deposit); }
  catch { return res.sendStatus(500); }
  return res.sendStatus(200);
});
app.post("/topups", async (_req, res) => {
  try {
    const quote = await pay.quotes.create({ client_reference_id: "team-42", amount: 2500, chain_id: 11155111, asset: "pha" }, { idempotencyKey: "local-test-order-1" });
    return res.json({ checkout: pay.checkoutParams(quote) });
  } catch { return res.status(503).json({ code: "payment_unavailable" }); }
});
app.get("/balance", (_req, res) => {
  try { return res.json(db.prepare("SELECT amount FROM balances WHERE customer=?").get("team-42") ?? { amount: 0 }); }
  catch { return res.sendStatus(500); }
});
const server = app.listen(3000);
server.on("error", () => { db.close(); process.exitCode = 1; });
process.on("SIGTERM", () => server.close(() => {
  void pay.close().catch(() => { process.exitCode = 1; }).finally(() => db.close()); }));
```

Python (`merchant.py`, `uvicorn merchant:app --port 3000`):

```python
import sqlite3, json
from contextlib import asynccontextmanager
from fastapi import FastAPI, Request, Response, HTTPException
from phala_pay import PhalaPay, balance_delta, SignatureVerificationError
pay = PhalaPay.from_env()
if pay.livemode: raise RuntimeError("This example requires test mode")
@asynccontextmanager
async def lifespan(app):
    try: yield
    finally: pay.close()
app = FastAPI(lifespan=lifespan)
database = f"merchant-{pay.pins.account}-test.sqlite"
with sqlite3.connect(database) as db:
    db.executescript("""CREATE TABLE IF NOT EXISTS credits(id TEXT PRIMARY KEY, snapshot TEXT NOT NULL); CREATE TABLE IF NOT EXISTS balances(customer TEXT PRIMARY KEY, amount INTEGER NOT NULL);""")
def apply(deposit):
    if deposit.client_reference_id != "team-42" or deposit.currency != "usd":
        raise ValueError("Unknown customer")
    db = sqlite3.connect(database)
    try:
        with db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute("SELECT snapshot FROM credits WHERE id=?", (deposit.id,)).fetchone()
            change = balance_delta(json.loads(row[0]) if row else None, deposit)
            db.execute("INSERT INTO credits VALUES(?,?) ON CONFLICT(id) DO UPDATE SET snapshot=excluded.snapshot", (deposit.id, json.dumps(change.snapshot)))
            db.execute("INSERT INTO balances VALUES(?,?) ON CONFLICT(customer) DO UPDATE SET amount=amount+excluded.amount", ("team-42", change.delta))
    finally: db.close()
@app.post("/topups")
def topup():
    try:
        quote = pay.quotes.create(client_reference_id="team-42", amount=2500, chain_id=11155111, asset="pha", idempotency_key="local-test-order-1")
        return {"checkout": quote.checkout_params()}
    except Exception: raise HTTPException(503, detail={"code": "payment_unavailable"}) from None
@app.post("/webhooks/phala-pay")
async def webhook(request: Request):
    from starlette.concurrency import run_in_threadpool
    try:
        event = pay.webhooks.construct_event(await request.body(), request.headers)
    except SignatureVerificationError: return Response(status_code=400)
    if event.type.startswith("deposit."): await run_in_threadpool(apply, event.deposit)  # failure -> 500
    return Response(status_code=200)
@app.get("/balance")
def balance():
    db = sqlite3.connect(database)
    try:
        row = db.execute("SELECT amount FROM balances WHERE customer=?", ("team-42",)).fetchone()
        return {"amount": row[0] if row else 0}
    finally: db.close()
```

Next.js Node route (`app/api/webhooks/phala-pay/route.ts`), importing the Node example's transactional ledger:

```typescript
import { pay, apply } from "@/lib/pay-ledger";
export const runtime = "nodejs";
export async function POST(request: Request) {
  let event;
  try { event = await pay.webhooks.constructEvent(new Uint8Array(await request.arrayBuffer()), request.headers); }
  catch { return new Response(null, { status: 400 }); }
  try { if (event.type.startsWith("deposit.")) apply.immediate(event.deposit); }
  catch { return new Response(null, { status: 500 }); }
  return new Response(null, { status: 200 });
}
```

## 8. Compatibility: Owner decision

The owner chose exact SDK/service version equality and no backward compatibility at 0.5.0; §5.9 of the integration guide
remains binding. This proposal does not choose a replacement.

| Option | Benefit | Cost / owner commitment |
|---|---|---|
| Status quo, matching X.Y.Z | Smallest implementation and test matrix | Every operator upgrade requires coordinated merchant SDK upgrades; 0.x minor breaks have no window. |
| Stripe-style dated API versions | Merchants pin request and webhook shapes independently | Persist account/endpoint defaults, per-request version header, historical serializers/errors, upgrade tooling and multi-version tests; define how security fixes apply across versions. |
| Additive-only stable `/v1` from 1.0 | Fewer merchant upgrades without historical date transforms | Freeze semantics including errors/webhooks; test old SDKs, retain deprecated behavior for a proposed 12-month window, and use `/v2` for removals. Owner must approve window/support floor. |

**Owner decision list:** compatibility option; if dated, defaults/retention and webhook migration; if stable,
deprecation duration and supported SDK floor. No apiVersion option or compatibility promise ships until decided. Pins
format version is independent of service/API version.

## 9. Phased delivery and release gates

1. **Contract/vectors PR:** approve record and common pins/address/webhook/ledger fixtures;
   lock generator config. Both agents consume the same fixtures; no Go code.
2. **Parallel JS/Python PRs:** JS generated resources/transport; Python pins/env/bound webhooks/
   Quote wrapper; both errors/retries/pages/checkout/ledger. Parity and no-diff gate both releases.
   New methods are additive. Python transport wrapping, retry budget, server browser rejection
   and relocated non-Node helpers break: one coordinated **0.x minor**, migration notes, equal
   service/SDK versions. Do not release either agent's incomplete contract independently.
3. **Setup PR after clients:** JS bin/journal, cross-language env/golden tests; exercise lost key
   responses, duplicates, expiry/Safe pending, attestation failure and no-secret output.
4. **Examples/docs PR after setup:** both full quickstarts, Next.js/non-React guides; keep props and
   explicit webhook functions. Removing legacy constructors requires a later announced minor.
   Coordinate edits after [PR #318](https://github.com/Phala-Network/phala-pay/pull/318) and
   checkout-density land.

Acceptance: each quickstart reaches a paid test quote and one verified 2500 credit in service/Anvil,
one setup command plus human signature, <=100 merchant code lines. Duplicate/concurrent/reordered,
refund-first and reversal-first events converge; never acknowledge before commit. Missing/wrong live
pins, account/mode/treasury/address substitution, unpinned key rotation and bad envelopes fail closed.
Test deadline bounds, browser rejection/verification, secret-free logs and interrupted/idempotent
setup. Run SDK checks/docs CI; this record implements no proposed APIs.
