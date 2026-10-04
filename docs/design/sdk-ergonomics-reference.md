# SDK contract reference and ledger recipes

Status: Accepted (owner, 2026-10-04); implementation pending

## Resource and transport contract

JS uses camelCase controls/resources; Python snake_case. Both preserve snake_case wire fields:

| Python resource | Methods (JS camelCases these names) |
|---|---|
| quotes | create, retrieve, list, update, cancel |
| deposit_addresses | create, retrieve, list, update, rotate |
| deposits | retrieve, list, update |
| refunds | create, retrieve, list, update, cancel, mark_paid |
| payment_settings | retrieve, update |
| config, balance | retrieve |
| account | retrieve, pause_quotes, resume_quotes, roll_webhook_key |
| treasuries | challenge, create, retrieve, list, cancel, pause, resume |
| api_keys | create, retrieve, list, roll, revoke |
| webhook_endpoints | create, retrieve, list, update, delete, test |
| events | retrieve, list, resend |
| sweeps, forwarders | list |

JS create/update/action methods return `Promise<T>`; signatures are `(params, options?)`, `(id,
params, options?)`, and `(id, params?, options?)`. Parameterless actions use `(id, options?)`;
retrieve uses `(id, params?, options?)`; singleton retrieve uses `(options?)`, update/challenge
`(params, options?)`. Params follow OpenAPI operationId bodies/queries. Python retains keyword
parameters, adding idempotency_key to every POST and per-call request_deadline. Omitted api_keys
permissions creates a secret key; a list creates a restricted key. Signing/sweep builders stay
offline.

Generate aliases QuoteCreateParams/Quote/Deposit from OpenAPI 3.1 with MIT
openapi-typescript==7.13.0; Python keeps openapi-python-client==0.29.1/Makefile. CI regeneration
must produce no diff. Preserve null/absent, metadata clears, expandables, additional fields and open
response enums; decoders ignore unknown fields. Dispatch ignores unknown event types. Atomic amounts
and rates remain decimal strings; Python preserves int64. JS rejects unsafe integers before JSON
rounding using lossless decoding; requests and arithmetic reject unsafe integers too.

### Transport and errors

- Options: `{idempotencyKey?, signal?, requestDeadlineMs?, upgradeTolerance?}`. One UUID per logical POST; freeze
  body/key across retries, validate 255-character header limit, encode path/query input. Explicit
  order keys survive restart; automatic keys cover one invocation. No DELETE retries or unreplayable
  POST bodies.
- Retry network failures, 429, 500/502/503/504, only 409 idempotency_key_in_use. Replayed responses
  end retries, including errors. Exponential jitter: half/full of 0.5/1/2 seconds, subsequent caps 5
  seconds. Retry-After seconds/date is a minimum; if outside remaining deadline, return last error,
  never retry earlier. Per-attempt timeout includes body; deadline includes attempts/sleeps. JS
  cancellation and Python interrupts are terminal. Finite positive timeouts, integer attempts 1..10.
- HTTPS origin only; explicit test HTTP loopback allowed. No authenticated redirects, credentials,
  query, fragment or path prefix. close() aborts JS owned requests; Python closes via context
  manager. Never close injected transport/fetch; use worker threads for Python network calls in
  async routes.
- list is JS AsyncIterable / Python Iterator; existing Python api_keys/treasuries lists stay lists.
  Add listPage/list_page returning `{data, has_more}` with limit/starting_after. Empty continuing
  pages or repeated cursors raise ResponseValidationError; verify quote/address items on every
  page/action.
- Errors: PhalaPayError (Python TopupError), ApiError, TransportError, ConfigurationError,
  ResponseValidationError, AddressMismatchError, AttestationError, SignatureVerificationError,
  LedgerSnapshotError. Keep JS WebhookSignatureError alias. TransportError.code:
  network/timeout/cancelled. ApiError: statusCode, code, message, errorType, param, docUrl,
  requestId, retryAfter (seconds); Python keeps snake_case fields, including error_type; optional
  fields null. Malformed responses raise ResponseValidationError with status/request ID, no raw
  body. Redact secrets in logs/reprs/ causes; handlers expose generic codes, never exception text.
  Wrapping raw httpx errors is breaking.

### Upgrade tolerance amendment (JS and Python implemented)

Add boolean `upgradeTolerance` (JS) / `upgrade_tolerance` (Python) to client configuration and
per-call controls; default **false**, with a per-call value overriding the client. Preserve the
interactive defaults: 15-second attempt timeout, 60-second total deadline, four attempts. Opt-in
lets a backend wait through the measured approximately 160-second CVM outage without making every
interactive request wait five minutes.

For GET and replayable idempotent POST only, a network failure (including connection refusal and
attempt timeout), HTTP 502/503/504, or `503 service_maintenance` activates the upgrade retry
budget: at most **300 seconds from the original request start**, including attempts, response body
reads, and sleeps. This budget replaces the ordinary attempt-count cap for those failures only.
An explicitly configured client or per-call deadline remains a hard limit and takes precedence;
never extend it. Other responses retain the ordinary attempt cap. Keep one serialized body and one
Idempotency-Key across every retry. Never retry DELETE, canceled requests, replayed responses
(`Idempotent-Replayed: true`), redirects, invalid successful responses, or permanent 4xx errors.

Use exponential jitter from 500 ms, with a 10-second cap during upgrade failures (uniform half to
full cap); honor Retry-After seconds or HTTP-date as a minimum, and never sleep/retry beyond the
remaining deadline. Gateway 502/503/504 without Retry-After, including HTML, empty, or malformed
error envelopes, are retryable transport failures in this mode; do not expose their raw bodies.
Ordinary response validation remains unchanged outside this mode. Cancellation/close (JS) and
interrupts (Python) stop waits immediately. Test with a fake clock: maintenance + Retry-After,
connection failure, then HTML 502 for three minutes, followed by success; assert unchanged key/body,
recovery beyond four attempts, five-minute exhaustion, explicit shorter deadline, cancellation,
and no retries of DELETE or replayed execution failures.

Python implements this amendment through the existing merchant transport path, with fake-clock
acceptance tests in `sdk/python/tests/test_upgrade_tolerance.py`.

## Pins validation

| Boundary | Required validation / format |
|---|---|
| Envelope | Canonical unpadded base64url, zero unused bits, valid UTF-8/object, <=16 KiB decoded. |
| Fields | Reject unknown format, duplicate/unknown/missing fields; accept JSON key order. |
| Encoding | Compact JSON; recursive ASCII key sort including textual chain keys, lowercase addresses; keys sorted by version. |
| Account/contracts | acct_ + 32 lowercase hex; nonzero 20-byte hex addresses, case-insensitive input. |
| Treasuries | Nonempty; canonical positive decimal safe chain IDs, no leading zeros. |
| Webhook keys | Nonempty unique positive uint32 versions/public keys; whpk_ + standard base64 of 32 bytes. |
| Identity | Boolean mode; valid sk/rk test/live key length/checksum; mode derives from prefix. |
| Origin | Lowercase host/scheme, remove default port/root slash; use transport origin rules. |

## Address pins and migration

Expose readonly pay.pins/pay.livemode. Missing/mismatched pins fail at construction in both modes;
account/mode must match response fields. Legacy Python constructor overloads keep test warnings;
never add live fallback, and reject mixing pins with old account/forwarder/treasury arguments.
Recompute every open quote and active address network from pinned account/contracts/chain treasury;
compare response treasury/address, reject missing chains. Historical closed/retired objects are not
payable. Treasury changes need replacement pins once active; never trust response treasury values.

## Bound webhooks

Bound constructEvent(body, headers, {tolerance: 300}) / construct_event(body, headers, *,
tolerance=300) uses client keys/account/mode; callers cannot override them. Original bytes/exact
UTF-8 only; case-insensitive headers, duplicate signing headers rejected. Verify Ed25519 v1a, ID
equality, account/mode, envelope and resource shape before returning typed Event/event.deposit;
unknown types retain raw objects. Signature/envelope failures raise SignatureVerificationError;
invalid tolerance is configuration error. Inclusive bilateral 300 seconds; zero exact match. Keep
explicit low-level functions/signatures/errors and legacy bound-call compatibility. No unsigned JSON
or fetched keys. Rotation requires attestation, explicit overlapping pins/deployment/removal;
notices never update trust. Retiring-key notices remain verifiable while that key is pinned.

## Setup flags and permissions

Prompt for omitted --api-base/--account/--webhook-url, repeatable --treasury CHAIN:ADDRESS and
--asset CHAIN:ASSET, --app-id/--compose-hash/--factory/--implementation. Hidden-prompt admin key; no
secret arguments. Default test; --live matches key. Outputs: --env-file .env, --state-dir
.phala-pay/setup. Identify first key via api_keys.list. Attestation nonce is 32 fresh random bytes.
Runtime grants: quotes.write only; opt-in account.read for config, deposit_addresses.write for
persistent addresses, deposits.read/events.read for reconciliation. No admin grants. Settings
reconciliation sends the complete document within catalog bounds. Enabled events are exactly
["deposit.credited", "deposit.rejected", "deposit.refunded", "deposit.reversed"]; security notices
are automatic. The main record defines signing, verification, readiness and mutation ordering.

## Setup persistence and recovery

Journal: versioned/account/origin/mode-bound, locked 0700 directory/0600 files, ignored with
env/admin files. Record intent/body hash/idempotency key before mutation; reconcile ids after
interruption. Reject symlinks, preserve unrelated env lines, confirm credential replacement. Print
paths/redacted ids/tasks only, never secrets/bodies/env blocks/raw verifier errors. Lost key
response replays id without secret: roll it with surviving admin credential, persist, retire lost
key. Expired records require reconciliation; no admin left means operator recovery. --resume uses
private credential or hidden re-entry. Completed rerun creates nothing; exit 0 ready, 2 human task,
1 failure. Test delivery after receiver starts; it proves signature, not payment. Simulator setup
remains separate.

## Pure helper validation

Snapshot: id/livemode/client_reference_id/currency/status/amount/amount_refunded/amount_reversed,
JSON-serializable readonly mapping/TypedDict. No IO/mutation/cache/floats. Net is amount minus
cumulative deductions for credited/reversed, zero otherwise. Merge pending < credited/rejected <
reversed, max deductions; reject conflicting identity/customer/currency/mode/valuation, credited vs
rejected, unknown status, invalid/negative/unsafe integers and deductions above amount. Null can
become valued once; credited requires amount. Unvalued snapshots require zero deductions; valued
reversal deducts all; refund and reversal cannot coexist. Unknown future statuses never credit.

## Copied transaction recipes

Copy one module next to the merchant entrypoint. apply accepts only verified client Events and
ignores unknown event types before accessing event.deposit, including future deposit.* types. These
are merchant-owned SQLite recipes, not SDK SQL APIs: ownership, schema changes and production
locking stay in your app. They are dedicated to one pinned account/test mode and accept only the
fixed example customer/USD. Production checks customer/ order ownership from merchant records, uses
unique (account, mode, deposit), inserts-and-locks per deposit, and commits snapshot plus balance
atomically. If applied contribution is stored separately, verify it matches prior snapshot. Metadata
and event-id deduplication never authorize fulfillment. Do not hold transactions across API calls.
Connection/validation/merge/commit failure rolls back: 5xx.

- Node: createLedger(pay) returns `{apply(event): void, close(): void}`; apply commits or throws.
  Install better-sqlite3; immediate transactions serialize first insert. Close on shutdown.
- Python: create_ledger(pay) returns an object with apply(event) -> None; each call closes its
  connection in finally. Run in a worker thread; BEGIN IMMEDIATE serializes deliveries.
- Both: replace fixed customer check with merchant authorization, keep parameterized SQL. PostgreSQL
  needs unique insert + SELECT FOR UPDATE; do not copy SQLite locking.

### Node ledger.mjs (49 formatted physical lines)

```javascript
import Database from "better-sqlite3";
import { balanceDelta } from "@phala/pay/server";

const depositEvents = new Set([
  "deposit.credited",
  "deposit.rejected",
  "deposit.refunded",
  "deposit.reversed",
]);

export function createLedger(pay) {
  if (pay.livemode) throw new Error("This recipe is for a local test merchant");
  const db = new Database(`merchant-${pay.pins.account}-test.sqlite`);
  db.exec(`
    CREATE TABLE IF NOT EXISTS credits (id TEXT PRIMARY KEY, snapshot TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS balances (customer TEXT PRIMARY KEY, amount INTEGER NOT NULL);
  `);
  const previous = db.prepare("SELECT snapshot FROM credits WHERE id = ?");
  const save = db.prepare(`
    INSERT INTO credits VALUES (?, ?)
    ON CONFLICT(id) DO UPDATE SET snapshot = excluded.snapshot
  `);
  const credit = db.prepare(`
    INSERT INTO balances VALUES (?, ?)
    ON CONFLICT(customer) DO UPDATE SET amount = amount + excluded.amount
  `);
  const transaction = db.transaction((deposit) => {
    if (
      deposit.livemode !== pay.livemode ||
      deposit.client_reference_id !== "team-42" ||
      deposit.currency !== "usd"
    ) {
      throw new Error("Unknown customer, currency or mode");
    }
    const row = previous.get(deposit.id);
    const change = balanceDelta(row ? JSON.parse(row.snapshot) : null, deposit);
    save.run(deposit.id, JSON.stringify(change.snapshot));
    credit.run(deposit.client_reference_id, change.delta);
  });
  return {
    apply(event) {
      if (!depositEvents.has(event.type)) return;
      transaction.immediate(event.deposit);
    },
    close() {
      db.close();
    },
  };
}
```

### Python ledger.py (55 formatted physical lines)

```python
import json
import sqlite3

from phala_pay import balance_delta


DEPOSIT_EVENTS = {"deposit.credited", "deposit.rejected", "deposit.refunded", "deposit.reversed"}


def create_ledger(pay):
    if pay.livemode:
        raise ValueError("This recipe is for a local test merchant")
    database = f"merchant-{pay.pins.account}-test.sqlite"
    db = sqlite3.connect(database)
    try:
        db.executescript("""
            CREATE TABLE IF NOT EXISTS credits (id TEXT PRIMARY KEY, snapshot TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS balances (customer TEXT PRIMARY KEY, amount INT NOT NULL);
        """)
    finally:
        db.close()

    class Ledger:
        def apply(self, event):
            if event.type not in DEPOSIT_EVENTS:
                return
            deposit = event.deposit
            if (
                deposit.livemode != pay.livemode
                or deposit.client_reference_id != "team-42"
                or deposit.currency != "usd"
            ):
                raise ValueError("Unknown customer, currency or mode")
            connection = sqlite3.connect(database)
            try:
                with connection:
                    connection.execute("BEGIN IMMEDIATE")
                    row = connection.execute(
                        "SELECT snapshot FROM credits WHERE id = ?", (deposit.id,)
                    ).fetchone()
                    change = balance_delta(json.loads(row[0]) if row else None, deposit)
                    connection.execute(
                        """INSERT INTO credits VALUES (?, ?)
                        ON CONFLICT(id) DO UPDATE SET snapshot = excluded.snapshot""",
                        (deposit.id, json.dumps(change.snapshot)),
                    )
                    connection.execute(
                        """INSERT INTO balances VALUES (?, ?)
                        ON CONFLICT(customer) DO UPDATE SET amount = amount + excluded.amount""",
                        (deposit.client_reference_id, change.delta),
                    )
            finally:
                connection.close()

    return Ledger()
```

## Recipe verification and line counts

Query balances.amount for team-42 with SQLite CLI/database console after webhook commit: 2500.
Browser success is not evidence. Shared fixtures: duplicate/replayed/concurrent first insert,
partial refunds, refund-first, reversal-first, pending/rejected/unvalued reversals, replacements,
unknown statuses, conflicting identities/valuations and rollback. Failed commits leave snapshot/
balance unchanged and produce 5xx. Merchant production adapters run the same fixtures.

Count physical lines INCLUDING blanks/comments/imports/lifecycle after Prettier printWidth=100 and
Ruff's repository line-length=100; no semicolon packing or one-line Python suites. Core includes
quote, browser, verified webhook, recipe invocation, errors/lifecycle; separately count recipes.
Auth/forms/production adaptation and commands are outside both. Baseline is published source:

- Today Python: sdk/examples/fastapi_app.py excluding module docstring (206 formatted physical
  lines), plus integration.md Quickstart React block (10). Includes ledger: 216 total.
- Proposed: Node 49 backend + 15 browser + 49 recipe = 113; Python 45 + 15 + 55 = 115.
- Node today has no full merchant client/example; complete merchant LOC is unknown, not fabricated.
- Eight setup API operations: key roll, old-key revoke, restricted-key creation, attestation,
  webhook registration, treasury challenge/submission, settings. Verifier/signature are additional.
