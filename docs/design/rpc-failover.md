# RPC load balancing and failover

Status: implemented in v0.7.0; current operations are in [the RPC runbook](../../deploy/RPC.md).

## Decision and scope

Keep the existing Alloy 2.5.0 HTTP client and introduce two independent typed
`RpcGroupClient`s per chain: A for scanning, heads and reconciliation; B for independent
receipt, head and call confirmation. Credit still requires both groups to agree. Use
Tower timeout/retry middleware, governor budgets and the existing member CountingLayer.
Implement two deterministic selectors plus application safety checks; no programmable
policy engine, hedging or response cache.

In 0.6.0, `chain.rpc_providers` accepts more than two ids but runtime uses only positions 0
and 1. Each id has one attested URL and an optional sealed `TOPUP_RPC_<ID>_KEY`.
See [RPC providers](../../deploy/README.md#rpc-providers),
[usage accounting](../../deploy/README.md#measuring-rpc-usage), architecture
[§7](../architecture.md#7-states-and-pump), [§8](../architecture.md#8-chain-valuation-screening),
[§13](../architecture.md#13-reconciliation), and the existing
[config](../../crates/topup/src/config.rs), [routes](../../crates/topup/src/routes.rs),
[key handling](../../crates/topup/src/rpc_provider.rs) and
[EVM client](../../crates/adapters/src/chain/evm/mod.rs).

This adds bounded Rust selection/state logic and governor to the service TCB, with no
extra processes, images or configuration interpreter. It offers HAProxy-style priority
failover and weighted distribution with explicit deadlines, classification and cooldowns;
it does not implement HAProxy's full policy language. Enable Tower 0.5.3's `timeout` and
`retry` features. Pin governor in Cargo.lock (Alloy's optional
[throttle layer](https://docs.rs/alloy-transport/2.5.0/src/alloy_transport/layers/throttle.rs.html)
uses governor 0.10); share governor instances rather than constructing a limiter per call.
Keep Alloy's typed providers/RPC client over `GroupTransport`, backed by a size-bounded
reqwest 0.13.5 adapter, with
[`redirect::Policy::none()`](https://docs.rs/reqwest/0.13.5/reqwest/redirect/struct.Policy.html#method.none).
All 3xx responses fail, including same-host redirects. Disable reqwest internal retries
with `retry(reqwest::retry::never())` so every resubmission passes through admission/counting.

## Configuration

Proposed public schema; omitted routes and the second chain use the same structure.
Member ids remain unique when URL templates match but keys differ.

```yaml
rpc_companies:
  tenderly: { domains: [tenderly.co] }
  alchemy: { domains: [alchemy.com] }
  publicnode: { domains: [publicnode.com] }
rpc_budgets:
  tenderly-account: { requests_per_second: 10, burst: 10 }
  tenderly-public: { requests_per_second: 10, burst: 5 }
  publicnode-account: { requests_per_second: 10, burst: 10 }
  publicnode-public: { requests_per_second: 10, burst: 5 }
  alchemy-account: { requests_per_second: 20, burst: 20 }
  alchemy-key-1: { requests_per_second: 10, burst: 10 }
  alchemy-key-2: { requests_per_second: 10, burst: 10 }
rpc_groups:
  sepolia-a:
    chain_id: 11155111
    members:
      - id: provider-a
        company: tenderly
        account_budget: tenderly-account
        key_budget: tenderly-public
        url: https://sepolia.gateway.tenderly.co
        sealed_key: null
        priority: 0
        weight: 1
      - id: alchemy-sepolia-1
        company: alchemy
        url: https://eth-sepolia.g.alchemy.com/v2/{key}
        sealed_key: TOPUP_RPC_ALCHEMY_SEPOLIA_1_KEY
        account_budget: alchemy-account
        key_budget: alchemy-key-1
        priority: 1
        weight: 2
      - id: alchemy-sepolia-2
        company: alchemy
        url: https://eth-sepolia.g.alchemy.com/v2/{key}
        sealed_key: TOPUP_RPC_ALCHEMY_SEPOLIA_2_KEY
        account_budget: alchemy-account
        key_budget: alchemy-key-2
        priority: 1
        weight: 1
    policy:
      selection: failover # or weighted_round_robin
      total_deadline_ms: 10000
      attempt_timeout_ms: 3000
      max_attempts: 3
      retry_delay_ms: 100
      failures: 3
      cooldown_ms: 30000
      recovery_successes: 2 # bounded worker schedules full probes every 5s
      probe: { attempts: 3, deadline: 30000 } # per-RPC attempts, complete-probe milliseconds
      rpc_error_rules: [] # reviewed provider-specific code/message mappings
  sepolia-b:
    chain_id: 11155111
    members:
      - id: provider-b
        company: publicnode
        account_budget: publicnode-account
        key_budget: publicnode-public
        url: https://ethereum-sepolia-rpc.publicnode.com
        sealed_key: null
        priority: 0
        weight: 1
    policy: { selection: failover } # other fields resolve to the explicit defaults above
chain:
  rpc_groups: { a: sepolia-a, b: sepolia-b } # inside each route's chain section
```

Require exactly the named roles A and B, distinct group ids, 1–8 members per group, one
chain per group and identical references across routes on that chain. Accept the same
URL template with different sealed names; reject duplicate `(normalized URL, sealed name)`
identities, member-id collisions and aliases that hide reuse of the same credential.
Budget ids identify reviewed actual account/key quota scopes; reject conflicting limits
for one id. Weights are bounded positive integers; priorities are ordered integers. Unknown/
unused definitions, invalid bounds and missing mappings fail `topup config check`;
`config show` resolves defaults without secrets. Keyed members require both account and
key budget references; aliases of one credential must use the same key budget. Secret
preflight detects equal keys under different names in memory without logging/persisting them.

**Company independence:** for every chain, A and B must have disjoint reviewed `company`
identities. Hosts, registrable domains (pinned Public Suffix List), CNAMEs and vendor
ownership records are evidence for that identity, not the identity itself. Map aliases,
resellers and custom domains to their actual reviewed provider company; reject unmapped
endpoints, contradictory mappings, or one company split under two ids. Different URLs,
ports, subdomains or keys do not establish independence. The reviewed domain-alias map
uses canonical company ids; the same PSL registrable domain can never belong to two
company labels or appear in both roles. No fallback crosses a group.

## Group client and bounded execution

Topup constructs shared typed A/B clients directly from `rpc_groups`; there is no proxy
URL. Existing EVM readers consume these clients, while a private `MemberClient` exposes
pinned-member probes and window reads. Each operation carries immutable chain/group,
method, numeric block bounds, deadline and attempt budget. Selection and health transitions
are serialized per group; network I/O never holds the selection lock.

`failover` tries eligible members in ascending priority then configured order.
`weighted_round_robin` uses smooth weighted round robin among eligible members; weights
apply to new operations, not individual subrequests of a logs window. A failed operation
tries another eligible member before revisiting the failed one. An empty eligible set
returns `GroupUnavailable`, never repopulates from excluded members. Selection errors
return errors, never reuse a stale candidate list. Both modes skip quarantined members,
capability failures and cooldowns; no latency race or hidden default fallback exists.

Execution order is: total Tower timeout → Tower retry with our typed classification
policy → selection/pinned member operation → shared account and key governor admission
→ per-send timeout → CountingLayer → size-bounded reqwest transport adapter under Alloy.
The thin adapter retains HTTP status, parsed `Retry-After`, redirect status and bounded
body bytes before JSON-RPC decoding; native Alloy non-2xx normalization loses metadata
and cannot implement this table. Reject oversized bodies without logging their contents. Count only when an
admitted request future is actually polled into the HTTP transport, not when enqueued
or cloned for retry. A member attempt includes validation calls and, for logs, every
filter batch. `max_attempts` bounds member attempts including the first; each attempt's
finite send count is derived from the operation's bounded filter list. All sends, admission
waits and retry delays share one total deadline, including outer scanner/reconciler retries
within that operation. Do not stack Alloy fallback/retry layers or reset the deadline
for each RPC. Pass the absolute window deadline through the typed pinned transport and its
nested log splits; budget bounded split trees and factory per-block verification as well as
transfer verification. Subsequent scheduled passes are new operations and keep existing backoff.

Every real send acquires both account and key permits, irrespective of method, group or
chain, including startup/recovery/head checks. Admission checks both limiters together under one shared admission lock immediately
before dispatch. Never reserve one permit while waiting for the other: after every wait,
recheck both; charge both only when both are available. This prevents banked account
permits bursting when keys recover. Keyless members have explicit account budgets and
a synthetic per-member key budget. Unknown 429 pauses the entire configured account
scope by default; only a reviewed rule may narrow it to a key.
Selection skips explicitly paused account/key scopes when another account is eligible.
No quota bypass for recovery and no limiter per method. Budgets are process-local: one
active topup worker owns them; multiple active workers would need a shared admission service
before claiming account-wide enforcement. Restarts refill buckets, so these are short-term
rate/burst budgets, not persistent daily spending limits.

### Error classification

Classify a bounded response by method, HTTP status, RPC code and allowlisted normalized
message patterns before deciding success, retry, cooldown or cursor eligibility. Parse
HTTP-200 RPC errors too. Message matching uses bounded exact/prefix matches, with provider
rules in attested configuration; raw messages/data/URLs never reach logs or clients.
Each `rpc_error_rules` entry names `company`, `methods`, `http_statuses`, `rpc_code`,
`message_prefix`, `class` and optional `budget_scope` (`key`/`account`); reject overlapping
conflicting rules. Permanent request/safety classes cannot be redefined as success/retry.
Precedence: safety/redirect/auth, semantic RPC errors, HTTP transport; a recognizable
request error/revert inside a 5xx remains terminal.

| Method/status/code/message | Class and action |
|---|---|
| Read, 2xx, valid result | Success only after typed validation. Null receipts and empty logs are legitimate only under the window/evidence rules; neither an error nor missing data is converted to `[]`. |
| Any, HTTP 408, DNS/connect/TLS/reset or timeout (including plain timeout) | Transient transport failure; retry another member within bounds; increment consecutive failure count and enter cooldown at the threshold. Cancellation by the caller/total deadline does not mark a member failed. |
| Any, HTTP 429 or recognized quota/rate message | Throttled; honor bounded `Retry-After` within the deadline, pause the affected key/account budget scope, try another independently budgeted member; never immediately hammer the same account. |
| Any, HTTP 500/502/503/504 or RPC `-32603` without a terminal semantic cause | Transient server error; bounded failover and failure cooldown. Other non-2xx statuses not listed below fail terminally unless an attested transient rule matches. |
| Any, HTTP 3xx | Redirect refused; quarantine member until endpoint config is reviewed. Never forward a key to `Location`. |
| Any, HTTP 401/403, wrong chain/genesis | Configuration/security failure; quarantine member, alert and require revalidation after repair. No retry to that member. Other eligible members may serve the operation. |
| Any, RPC `-32600`/`-32602`, or invalid request/params message | Terminal request error; no retry or failure cooldown, no cursor advance. |
| Read, RPC `-32601` or recognized unsupported-method message | Capability failure for this member/method; try another capable member, exclude that capability until revalidated. Never silently skip the required read. |
| `eth_getLogs`, `-32005` with range/result-size/too-many-results message, | Split into smaller numeric subranges on the same fixed member within the original deadline; merge all subranges before committing the original window. No partial progress. |
| Any, `-32005` with rate/credits/quota-exceeded message | Throttled as above; shared budget scope is determined by the reviewed rule. |
| `eth_getLogs`, HTTP 413 | Request body too large; shrink address/topic batches on the same pinned member and retry the entire fixed numeric window. Do not shrink the block range or advance partial progress. |
| Any, unknown `-32005` or other unmapped RPC error/message | Terminal unclassified error; wait/alert, no heuristic unlimited retries. Add a reviewed rule only after identifying its meaning. |
| `eth_call`, estimate or send, recognized execution revert (including send `-32000`/`-32003` with revert message) | Terminal execution failure; do not retry, do not count as transport failure. |
| `eth_sendRawTransaction`, timeout/transport or server error | Submission uncertain; only bounded resubmission of identical signed bytes/hash is allowed. Never rebuild a transaction or change nonce. “Already known” and nonce-too-low remain explicit uncertain submission errors; the existing transaction lifecycle must verify receipt/nonce before treating them as accepted. |
| Read validation: stale head, null required block, malformed/truncated result or mismatched window anchor | Reject attempt, discard result; stale member enters recovery-only selection, malformed response counts as failure. Finalized fork conflict freezes the chain. |

Submission rules override generic retry rules. Unclassified errors never advance credit
or cursors; every table row and overlapping status/message needs a test.

Consecutive successful validated operations reset the transient failure count. Cooldown
expiry permits one probe lease per member, never automatic readmission; two consecutive
successful pinned probes restore eligibility. Probes verify chain/genesis, required route
contracts and heads/anchors at the group's current floor; A additionally probes log
capabilities. Failed probes renew cooldown; auth/redirect/identity quarantine requires
operator repair and revalidation. Each member's recovery probe has isolated tentative head
state: a failed high-head capability probe cannot make the next member stale. Probes use the
same keys, budgets and counters. The explicit `policy.probe` bounds each RPC to three attempts
by default and the complete member probe to 30 seconds, including budget waits and backoff.
Tower retries only transport/timeouts, classified server errors and throttling, on the same
member with exponential backoff from `retry_delay_ms`; 429 admission honors `Retry-After`.
Deterministic capability/identity/code failures, malformed responses and stale evidence never
retry. `recovery_successes` counts complete successful probes, not individual RPC attempts.
Acceptance takes one finalized snapshot for numeric capabilities and the unsplit 2 000-block
address-less A logs test, avoiding a redundant tagged read against a different gateway backend.
B checks a 100-block addressed recent log range. Persisted floors and finalized canonical hashes
are still checked before readmission. Same-height hashes must agree across the snapshot,
persisted anchors and every subsequent block read within a probe. Owner recovery gives each
member a fresh `probe.deadline`, then starts anchor agreement on fresh copies with a separate
shared `total_deadline_ms` bound after both groups finish probing; `probe.deadline` does not
cap anchor operations. A finalized conflict detected during numeric validation freezes the
chain even when snapshot consistency rejects the response before canonical comparison.

## Heads, forks and logs windows

Persist watermarks keyed by `(chain id, stable group id, tag, recovery epoch)`; the chain's
genesis identity is fixed by persisted member validation evidence and checked on startup,
readmission and owner recovery. Each record contains number, hash, parent hash, observed
member id, acceptance time and config digest; retain the current anchors, old recovery
epochs and immutable cursor/window replay evidence. `latest`,
`safe`, `finalized` have separate records. Accepted chain A/B role bindings are immutable;
renaming or swapping a group requires an explicit audited migration, never a config edit. Fetch a real block/header even when the caller
needs only `eth_blockNumber`. Under a serialized compare-and-persist step, reject any
member response below that tag's current high-water mark and feed `StaleHead` back into
selection before another attempt. Persist before publishing; persistence failure publishes
nothing. Never synthesize/clamp a head or treat a lower observation as an advance.

Compare ancestry to saved anchors when advancing (fetch intervening headers or the saved
height's canonical header). A conflicting finalized hash/ancestry freezes credit and both
cursor writers for the chain. Latest/safe ancestry conflicts or same-height hash changes persist a reorg range above the
last finalized anchor for the head scanner; replay progress commits atomically with evidence.
Poll both latest and safe before checking unchanged height; pending replay bypasses that check.
Only advance the contiguous covered prefix of each queued range. Replay inserts confirmed
deposits even below the old confirmed cursor, and caps confirmation progress at the read end.
These changes are reorgs: preserve
the numeric high-water mark, update the canonical branch only through existing reorg
handling and replay the affected interval. A new branch shorter than that numeric mark
waits until it catches up. Receipt plus its judging head and optional consumed nonce are one member-pinned operation.
Before accepting absence, validate that member against the needed height and persisted
floors, and recheck it after the reads. Typed decoding belongs inside attempt success, so
malformed results feed failure/cooldown before any success reset.
Receipt/call evidence stays tied to the requested block/hash and
A/B agreement, not merely a high block number. Concurrent responses are rechecked against
the watermark at acceptance, not just before sending.

A wrong high watermark is not cleared by restart, config edits, member removal or automatic
expiry. Freeze the chain and investigate. An owner-authorized audited recovery verifies a
lower canonical anchor against both independent companies, records old/new anchors and
reason, and opens a new recovery epoch. A numeric-only poisoned floor can enter the same
authorized recovery transaction, which freezes the chain before repairing progress. Old
window reviews and reorg ranges remain immutable audit evidence; the new epoch gets its
own review/replay baseline. Reconcile every affected credited deposit and replay
from the last verified common anchor before unfreezing. Keep original watermarks/evidence;
repair invalid cursors and all derived address progress (`created_block`,
`backfilled_through`, backfilled flags and replay coverage) through an audited rescan
checkpoint. Rebase poisoned address starting points to the last trusted issuance anchor
or conservatively the deployment/route activation floor; clear invalid backfill completion
and queue all affected addresses/windows. Never silently skip history. Preserve originals
in audit evidence and never lower a verified coverage floor. This explicit recovery is the only exception to
monotonicity across epochs; ordinary operation remains monotonic within an epoch.

**Logs completeness boundary:** a synced node can still omit logs; a matching head alone
is not a cryptographic completeness proof. This design prevents routing through a known
lagging/wrong-fork member; finalized replay detects omissions when another member returns
the missing data. A/B receipt agreement still gates all credits.

For scanners, address backfills and reconciler missing-deposit reads:

1. Plan immutable inclusive numeric `[from, to]` windows (at most 2,000 blocks); no `latest`
   tags in log filters. Pin one member and snapshot every token/recipient/factory filter
   batch required for that window. Do not choose a new member for individual batches.
2. Before reading logs, validate that member's appropriate head (`finalized` for finalized
   work, `latest` for head scanning) against the persisted group watermark **and** `to`.
   Fetch the window-end header and verify canonical ancestry/anchors. A head below either
   floor rejects the member; do not send `getLogs` to it or accept its fast empty result.
3. Read typed raw logs from all filter batches once, without hedging/cache. Extend the
   window's single base deadline for planned batches and actual verification work: allow
   four sends per raw log plus bounded overhead, at twice the admission time of the slower
   account/key quota plus the configured per-send timeout allowance. Allocation is bounded by the decoded body sizes and attempt count;
   every send retains its timeout. Buffering the current window is not a response cache.
   Validate returned
   logs, range bounds, block hashes and detectable truncation/result-limit errors. After the reads,
   recheck that member's head/end anchor and the current group floor; detect regressions,
   forks and concurrent watermark advancement before accepting even `[]`.
4. Buffer the entire window. On any failure or member switch, discard it and retry the
   **whole original window**, including head checks and all batches, on another member
   under the work-scaled deadline and original attempt bound. Never merge partial answers across members.
5. Only a typed validated window can be committed. Record deposits/factory events,
   address-backfill progress and the applicable cursor/checkpoint atomically. Errors,
   timeout, cancellation or exhausted candidates leave both scanner and reconciler cursors
   unchanged for that window. Already completed earlier windows need not be repeated.

Persist per-window review coverage, original answering member, bounds/filters, end anchor
and independent replay checkpoint in the same transaction as progress. Every window,
including historical catch-up, remains due for review until re-read with another eligible
member where possible. A rolling finalized tail is additional review, not replacement: old
unreviewed windows never expire when the latest 2,000-block tail moves. Singleton groups
retain pending independent review and replay against their sole member; report that limitation until another independent member is available.
Queue the full affected interval on a reorg, suspect omission, outage recovery or operator
backfill request. Historical jobs start at the last verified anchor/address creation floor,
use the same pinned-window rules and normal deposit uniqueness/confirmation path. Compare
replay coverage and evidence, insert missed deposits idempotently, and freeze/investigate
conflicting finalized evidence. A failed replay remains queued without advancing its own
checkpoint; it cannot be hidden by a forward cursor already beyond the gap.

## Preflight, sealed keys and attestation

Topup and restore-check retain the existing owner-sealed environment interface. Each keyed
member names `TOPUP_RPC_<...>_KEY`; URL templates permit at most one `{key}` as a whole path
segment/query value and forbid userinfo, fragments, host substitutions and inline secrets.
`config check --secrets` rejects missing/empty/stray keys and normalized-name collisions
without printing values. All configured key names must be present even for an offline backup;
network failure is distinct from malformed configuration or absent sealed material.

Pinned-member preflight is an executable path inside the service: resolve that member's
sealed key in memory, construct its ordinary counted HTTP client with redirects disabled,
and invoke `probe_member(id)`, bypassing selection only. It does not bypass budgets,
timeouts, chain checks or redaction. No separate key-isolated proxy prevents these probes.
Offline deployment preflight validates templates/names without keys; owner secret preflight
and in-CVM startup perform keyed checks. Expanded URLs never appear in config, command-line
arguments, persistence or telemetry. Disable raw HTTP/RPC trace output; classify bounded
messages internally and expose only ids/codes/classes. Test secrets echoed by upstreams.

Persist acceptance of the public config digest with each member's verified chain/genesis,
route capabilities, template and sealed-name identity. On **first acceptance or changed
config**, independently probe all members under bounded startup deadlines. Require at least
one freshly verified eligible member in each group; an unreachable backup is recorded as
unverified/cooling and must not block startup. It cannot serve until full validation passes.
Static identity/secrets/independence failures remain fatal; verified wrong-chain members
are quarantined and alerted, never accepted as serving candidates.

On **restart of the same accepted config**, load watermarks, cursors and member validation
records; do not require every backup to pass again. Fresh pinned checks still gate eligibility,
so an offline backup cannot block a healthy primary. If a whole previously accepted group
is down, run API/health in degraded mode and wait with credit/cursor work disabled for that
chain. First acceptance with no verified member in either group fails acceptance. Config
changes cannot reset floors; unchanged members retain evidence, changed/new members need
full probes. Recovery workers retry offline members after startup. Restore-check follows
the same distinction and cannot mark an unverified chain ready for resumed credit.

Retain the existing compose services and digest-pinned topup image. Inline group/company/
policy/budget configuration under content-digest config names; update renderer, sealed-name
allowlist and attested-compose tests for topup and restore-check. The service image, compiled
classification/selection behavior and public config are attested. Keys stay sealed; rotation
within an existing name does not change public config, adding a name requires resealing.
No sidecar, proxy ports or additional live infrastructure is introduced.

## Accounting and operation

Keep admin-signed `/v1/admin/metrics`,
`topup_rpc_calls_total{provider,chain_id,method}`, the existing 16 methods/`other`, process
start timestamp and cost formula. Provider labels are actual member ids; preserve staging's
four existing ids. Place CountingLayer at each real member transport, below admission and
retry, and count failed as well as successful dispatches once, including head validation,
preflight, recovery and replay. No group-level double counting or idle counter eviction.
Move recording into the polled dispatch future if necessary: requests canceled before
admission/dispatch count zero. A dispatched transport attempt may fail before reaching the
remote server; this remains a dispatch estimate, not an exact provider invoice. Separate
logical group-operation, retry and admission-wait counters from physical RPC dispatches.

Add bounded-label group eligibility, member cooldown/quarantine, stale-head/window rejection,
accepted-head/epoch, replay backlog/coverage and budget-wait metrics. Page on zero eligible
A or B for 1 minute, finalized fork conflict, stalled scanner/finality/reconciliation SLOs,
or overdue replay coverage; warn on repeated regressions, cooldowns, quota pressure and
usage growth. Instrumentation failure must not be represented as zero usage.

Runbook: identify chain/group/member and last verified anchors; distinguish outage, quota,
lag, fork and credential/config failures using sanitized metrics and counted pinned probes.
Leave credit pending when either group is unavailable. Repair endpoints via an attested
config PR/upgrade or rotate an existing sealed key; never borrow a company from the other
group. Validate recovery before readmission, drain queued replay/backfill and reconcile
missed deposits without duplicating credits. Wrong-watermark recovery uses the audited epoch
procedure above, not ordinary config rollback. This design authorizes no live operations.

## Tests and 0.7.0 migration

- Unit tests cover config/company aliases, exactly A/B, URL/key rules, duplicate templates
  with distinct credentials, shared budget identity, selector order/weighted fairness under
  concurrency, exclusion/empty pools, deadlines/cancellation, and every classification row
  and provider-rule ambiguity. Test all-method/all-key shared account limits, per-key limits,
  shared chains, budget waits and canceled unsent requests. Check guard persistence/races,
  ancestry/reorgs, wrong-watermark epochs, no implicit floor reset and secret-canary redaction.
- Local integration uses real group clients against deterministic HTTP fixtures: a failing
  member, stale/lagging member and HTTP-200 RPC-error member beside a correct member. The
  lagging member returns fast `[]` while the correct member has a deposit: reject the lagging
  window and assert **neither scanner nor reconciler cursor advances**; allow advancement
  only after a complete validated retry returns the correct answer. Also regress a head
  between precheck and logs, switch after one recipient batch, fail the final batch and
  cancel mid-window. Assert no partial commit and whole-window retry/replay after restart.
- Exercise every error class, both `-32005` meanings, read/send reverts, request errors,
  ambiguous submission, 429 across keys/methods, redirects to another host (target gets no
  request/key), plain-timeout cooldown, probe failure and two-success recovery. Kill all A
  then all B and assert no single-group credit. Compare fixture dispatches to CountingLayer;
  test idle retention and cancellation before/after dispatch. First accept with one failing
  backup, restart the same config with that backup still failing, and change the config:
  healthy members permit startup, unverified backups cannot serve. Test all-down first
  acceptance versus degraded restart, lost persistence, restore and queued historical gaps.
- Acceptance tests additionally cover every status/RPC/Retry-After/redirect/oversized-body
  adapter combination, delayed-key recovery without account-permit bursts, a missed log in
  an old historical window after the rolling tail moves, repair of poisoned address
  progress, and 0.6 height-only migration to a trusted A/B-agreed hash anchor.
- Compose tests cover public config/image/policy/name changes in attestation, unchanged hash
  on key rotation, no secret in rendered output/logs/errors, and startup/restore interfaces.

Staging starts with singleton Tenderly A and PublicNode B on Sepolia and Base Sepolia,
preserving four existing member ids, route versions, sealed-name conventions and cursors.
Singletons introduce the group client without upstream redundancy. This PR includes the
implementation and singleton config migration; rehearse locally before a reviewed staging
release. Add reviewed
independent backup companies later (for example Alchemy only in A, Infura only in B), with
capability probes and shared account/key budgets. Observe a full finality/reconciliation
cycle, replay coverage and usage before promotion. Same-company keys provide quota
resilience; company-outage resilience needs another company.

0.7.0 replaces top-level `rpc_providers` and ordered `chain.rpc_providers` with typed
`rpc_groups` and explicit `{a, b}` references; removes implicit provider defaults; rejects
legacy/unknown fields, including lists of more than two ids. Manual offline migration
maps **exactly two** old ids to singleton groups and requires company review; never discard extra ids.
For a 0.6 height-only cursor, pin its canonical header/hash through agreement of A and B
before treating it as a trusted anchor; disagreement/unavailability blocks chain progress,
and a cursor above agreed heads enters audited recovery. Add acceptance/watermark/replay
persistence through additive migrations and update config,
self-hosting, preflight and restore documentation. Keep RPC keys in topup/restore-check and
keep the metrics API; new member ids add series and validation/replay increase usage.

Rollback only to an image that understands the accepted schema, watermarks and replay
checkpoints; preserve all durable floors/evidence and sealed material. An immediate
pre-credit staging rollback can restore the archived 0.6.0 image/config after verification.
After 0.7.0 accepts new watermarks or processes credit, 0.6.0 is not a safe automatic
rollback target: pause chain work and deploy a compatible corrective release, or use an
owner-reviewed recovery/rescan procedure. Config rollback alone never lowers watermarks.

## Considered: eRPC 0.3.0, rejected

At tag `0.3.0`, commit `a98914408d5e13e848d4baba0ea06d20d58c62e9`,
[empty selection refills registered upstreams](https://github.com/erpc/erpc/blob/0.3.0/erpc/networks.go#L1838-L1866),
[policy errors retain stale candidates](https://github.com/erpc/erpc/blob/0.3.0/internal/policy/slot.go#L256-L269),
and [logs omissions lack intrinsic proof](https://github.com/erpc/erpc/blob/0.3.0/architecture/evm/integrity/checks_getlogs.go#L11-L29).
Its [retry/cooldown semantics](https://github.com/erpc/erpc/blob/0.3.0/upstream/upstream_executor.go),
[method-scoped budgets](https://github.com/erpc/erpc/blob/0.3.0/upstream/ratelimiter_budget.go),
[dispatch counters](https://github.com/erpc/erpc/blob/0.3.0/upstream/upstream.go) and
[idle metric eviction](https://github.com/erpc/erpc/blob/0.3.0/health/tracker.go),
[HTTP redirects](https://github.com/erpc/erpc/blob/0.3.0/clients/http_json_rpc_client.go),
and [cache key scope](https://github.com/erpc/erpc/blob/0.3.0/architecture/evm/json_rpc_cache.go)
do not match these contracts. Key-isolated sidecars also lacked a specified keyed
per-member probe path. Correcting these boundaries would require maintaining proxy changes
on top of its pre-1.0 Go/JS TCB; use the existing in-process transport instead.
