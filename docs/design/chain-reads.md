# Chain reads: final design

Status: implemented in v0.10.0 (unreleased). This describes the adopted pilot design, not a
verification of a live deployment. Supersedes
[the RPC failover design](https://github.com/Phala-Network/phala-pay/blob/5f585cca2073b81fd7d016192940e906fef8c13e/docs/design/rpc-failover.md). Provider capabilities measured with the real keys on
2026-10-07 (§5.1).

Binding requirements: $0 for staging and the production pilot; simplest standard design, exactly
two endpoints per chain, no in-process failover, the custom RPC framework deleted; no downgrade in
money safety (chain money evidence and prices dual-source; negative payment conclusions need
dual-source complete coverage; sanctions use verified local OFAC SDN/manual lists); checkout
credit uses the independent hint fast path; a real
N-1 → N → N-1 → N rollback passes.

## 1. Model

Every chain has two endpoints from independent companies: **read** (Ankr Freemium) and
**verify** (Infura Free). Assumption: at least one is correct and synced.

- **R1 Dual evidence.** A chain fact that moves money or is permanently recorded is accepted only when
  both endpoints derive the same decoded fields independently. Disagreement = wait + alert.
- **R2 Pinning.** State reads use EIP-1898 `{blockHash, requireCanonical: true}`. Current-state
  price pins come from verify (`eth_blockNumber`, then the header at `latest − 2`);
  finalized pins use the checkpoint. "Not found / not canonical" on either side = wait, re-pin.
- **R3 Errors are never results.** A range is covered only if every request for it succeeded.
- **R4 Negative conclusions** ("no payment to address A up to block B") come only from dual
  coverage (§2.2): the chain cursor and the address's own coverage.
- **R5 Positive candidates** may come from one source (fast discovery, hints) and are recorded as
  `detected`; nothing beyond `detected` happens without R1, and a record is marked
  `dual_verified_at` only after R1 on every decision field.

## 2. Read paths

### 2.1 Fast discovery (read only, every 300 s per payment chain)

`eth_getBlockByNumber(latest)`, then one `eth_getLogs` per 1 000 issued addresses over
`(fast cursor, latest]` with `topics: [[Transfer], null, [≤1000 recipients]]` (no `address`;
ERC-20 layout decoded locally; routed tokens only). Transfers at or below the route's confirmation
horizon are inserted as `detected` as today (`Evidence::Confirmed`, key
`(chain, tx, receipt log index)`, `dual_verified_at` NULL); `cursors.confirmed_block` advances.

### 2.2 Finalized dual coverage (both endpoints, hourly per payment chain)

State: chain cursor `c = chain_coverage.through_block`; per address
`addresses.dual_covered_through` (NULL = nothing covered). An address is **caught up** when
`dual_covered_through = c`.

Initialisation: when `topup run` first starts with a chain, before the API can issue an address
on it, `chain_coverage` is set to the agreed checkpoint (§2.5). A restored or upgraded database
without a row starts at `min(created_block) − 1`, so the first range includes `min(created_block)`.

Each round, staggered per chain:

1. Read the published checkpoint `t` from the independent ten-minute loop (§2.5), without
   repeating its RPC checks. Range end `e = min(t, c + L)`, `L` = 3 000 blocks, or 19 200 on
   every sixth round (six-hour catch-up; §5.2 budgets it). Read the header of `e` on both
   endpoints (hash and time must agree).
2. Snapshot address sets after step 1: caught-up set C; lagging set G = at most 1 000 addresses
   with `dual_covered_through IS NULL OR < c`, oldest progress first (new addresses, reissued
   addresses whose `created_block` was lowered by `deposit_addresses.rs` reissue, restored
   addresses). Addresses issued after the snapshot are paid above `t` (payments before issuance
   are out of scope, as today).
3. Query on **each** endpoint, windows ≤ `max_log_blocks`, recipient chunks of 1 000,
   `topics: [[Transfer, ForwarderCreated, Flushed, FlushFailed], null, [chunk]]` (all four index
   the recipient/forwarder at `topics[2]`, `contracts/src/ForwarderFactory.sol`; factory events
   count only from the configured factory): C over `(c, e]`; G from
   `g = min(coalesce(dual_covered_through + 1, created_block))` to `g_end = min(e, g + L − 1)`,
   keeping only logs at or after each address's own start. Any failed request aborts the round.
4. **Union** both candidate sets (never equality, never intersection) and resolve:
   - new candidate → R1 by receipt, transaction and header on both: agreed present → record
     (`detected`, `rejected(unsupported_asset)`, `flushed`, `flush_failures`, `deployed_block`;
     `Evidence::Finalized`, `dual_verified_at` set); agreed absent → drop;
   - existing deposit with `dual_verified_at` set → skip;
   - every deposit of the covered addresses in the covered range with `dual_verified_at` NULL,
     whether or not it is in the union (fast-path rows, N-1 rows) → R1 on every decision field
     including `block_time` and nonce: equal → set `dual_verified_at`; both agree on other
     evidence → correct it while `detected` (existing provisional rule), otherwise freeze the
     chain (`reconciliation_blocks`, `check_name = 'unverified_evidence_mismatch'`); both agree
     the receipt or log is absent → left to the finality watch (the `detected` row keeps holding
     its quote);
   - existing factory-event rows in range (insert-only tables, rare) → always re-verified;
   - any disagreement → no commit, alert.
5. One transaction: records and markers; `chain_coverage` → `(e, hash(e), time(e))`; C's
   `dual_covered_through = e`; G's `dual_covered_through = g_end` for addresses whose start
   ≤ `g_end`; `backfilled = true, backfilled_through = e` only for addresses now caught up;
   pending-view rows ≤ `e` deleted. The N-1 compatibility cursor advances only to the common
   complete boundary across addresses. When it lags `e`, both endpoints supply an extra agreed
   header for that boundary; it never jumps to the checkpoint ahead of coverage.

This replaces the single-source finalized backstop and the reconciler's missing-deposit pass.

### 2.3 Transaction-hash hints (checkout fast path)

```text
POST /v1/quotes/{id}/transactions?client_secret=…             {"transaction_hash":"0x…"}
POST /v1/deposit_addresses/{id}/transactions?client_secret=…  {"transaction_hash":"0x…","chain_id":84532}
```

Authenticated by the object's `client_secret` (the Stripe browser pattern the read routes already
use; it grants nothing beyond this object) or a merchant key with `quotes:write` /
`deposit_addresses:write` (server forwarding). `chain_id` is required for deposit addresses and
must be one of the object's networks; the recipient is always derived server-side. Admitted
requests return `202 {"object":"transaction_submission","transaction_hash":…,"status":"received"}`
(no oracle). Global API overload returns retryable `503 unavailable` before reading the body.
Only the hash and that chain id are used.

Task (in process; deduplicated by `(chain, tx, object)`; insertion dedupes by receipt position):

1. Claim one unit of today's hint budget (§2.3 caps); none left → stop (scanning covers it).
2. Poll the receipt on read (1, 2, 4, then every 4 s on Ethereum chains; every 1 s on Base).
3. Poll confirmation heads independently; only once both have reached the route's depth, refresh
   receipts on both endpoints, allowing for receipt lag or a confirmation-time reorg. Complete
   receipt, transaction and header evidence independently and compare every decision field (§2.4).
4. Quiet stop unless successful evidence contains a routed transfer paying the object's address.
   Insert through the scanner's insert function (`Evidence::Confirmed`, `dual_verified_at` set).
   The normal pipeline follows.
5. The whole task, head polls included, is bounded by **12 read calls, 8 verify calls and a
   deadline** (90 s Ethereum chains, 30 s Base chains); hitting either ends the task (scanning
   covers it).

Caps: per object 3/min and 10/day (`api/rate_limit.rs`); no per-IP limit because TCP ingress shares
one peer across clients. The shared API concurrency gate admits at most 256 requests; hint workers
run at most 4 tasks in flight. **80 tasks per environment per UTC day**, enforced by an atomic
`daily_budgets` row (`UPDATE … SET used = used + 1 WHERE day = $today AND name = 'hints' AND
used < 80`), because the governor buckets refill. Endpoint not ready → park in a bounded
in-memory queue (256 entries, 15 min TTL); overflow dropped.

SDK: `@phala/pay` gains `submitTransaction(...)`; `payWithWallet` (`sdk/js/src/wallet.ts`) calls
it when `writeContract` resolves, so `@phala/pay-react` Checkout needs no change;
`@phala/pay-server` gains `quotes.submitTransaction` and `depositAddresses.submitTransaction`.
Documented in `docs/integration.md` §1 and the reference.

A hint record is a dual-verified positive fact at the route's confirmation: it can only hold a
quote open or refuse a cancel, never produce a negative conclusion. Later fast discovery and
coverage meet the same key (`ON CONFLICT DO NOTHING`; marked rows are skipped by §2.2).

### 2.4 Money evidence (both endpoints)

- **Confirm**: each endpoint independently reads receipt, transaction, header and its own head;
  equal fields required: chain, tx hash, receipt status, block number and hash, receipt log
  index, token, from, to, amount, block time, sender nonce (OP-stack `0x7e`: receipt `from` and
  `depositNonce`), confirmation reached on each. Read's detection values are never passed to
  verify (removes `KnownTransfer` reuse in `evm/mod.rs` `receipt_lookup` and
  `steps/confirm.rs` `confirmed_evidence`). Success sets `dual_verified_at`.
- **Sanctions**: verified local OFAC SDN snapshots and audited manual supplements at decision
  time, with no RPC call. An active-list hit denies even when stale or another list read fails;
  a negative answer clears only with a fresh snapshot and successful list reads. Evidence
  records snapshot provenance and screening time. Refund destinations use the same rules;
  hourly publication checks and destination re-screening are independent of scan cadence.
- **Prices**: one Multicall3 `aggregate3` per snapshot per endpoint at one pin: all Chainlink
  feeds of the chain (`latestRoundData`, `decimals`), Base sequencer uptime, or the Uniswap V2
  pair state with `getCurrentBlockTimestamp`. Bytes must match; existing freshness, heartbeat,
  peg, liquidity, continuity and jump checks unchanged. Quote reuse stays 12 s; confirm always
  takes a fresh snapshot. Fresh quote snapshots are capped (§5.2) by a `daily_budgets` row per
  `(price chain)`; when exhausted, quotes needing that chain fail closed with a retryable
  `price_unavailable`. Staging PHA TWAP sampling: `uniswap_v2.rs` `SAMPLE_INTERVAL_S` 60 → 300
  (staging-only, production-ineligible route; continuity and jump limits re-derived for 300 s).
- **Refunds** (`refunds.rs`): independent dual receipt, transaction and header evidence, completed
  only under the agreed checkpoint. Attachment checks run every 60 s for 30 min, every 10 min
  until 24 h, then hourly; a never-seen transaction remains pending with its reservation.
  Atomic environment-wide concurrent and rolling-24-hour attachment caps are in §5.2.
- **Treasury EIP-1271** (`treasuries/proof.rs`): dual calls at the same canonical pin.
- **Flush records**: only from §2.2 step 4.

### 2.5 Checkpoint, reorgs, reversal

An independent ten-minute loop per payment chain checks and publishes
`chain_checkpoints(number, hash)`. It advances when read's `finalized` hash at
`number` equals verify's header at `number`, `number ≤` verify's `finalized`, and the previous
checkpoint height still has the stored hash on both. A conflict inserts a chain-scope
`reconciliation_blocks` row (`'finalized_checkpoint_conflict'`); the existing freeze gate halts
the chain until the audited admin lift. Finality, reversal and refund consumers use these
published advances independently of hourly log coverage. Unresolved deposits back off from
when they first became due for finality: every 60 s for ten minutes, every ten minutes until six hours,
then hourly. The one-hour pending-after-reorg alert remains; verification and reservations
continue. The operational stock allowance is S=1 per environment, with at most one new stuck
deposit per environment in each 24-hour budget window. Quote pause and escalation apply above
either limit (§5.2); resolving stock does not reset the arrival tally.

Reversal requires positive evidence on both at or below the checkpoint: (a) the deposit's
receipt without its transfer at its position (successor recorded as today), or (b) a directly
evident replacement: exactly one transaction already known to the service with the same chain,
sender and nonce and another hash, finalized and agreed on both. When both original receipts
are missing, read only that one candidate (K=1). With multiple candidates, read none, raise an
anomaly alert and keep the deposit unresolved in S for operator resolution; make no reversal.
Otherwise wait and alert
(`TopupDepositPendingAfterReorg`, new `TopupDepositReversalUnproven`). No nonce search (EIP-7702
authorizations also increment nonces). Refunds' nonce-based "dropped" verdict is removed.

A reversal that releases a quote (`finality/mod.rs` quote reopen, today
`CASE WHEN … expires_at > now()`) always restores `status = 'open'`,
`exposure_reserved = true`, `consumed_by = NULL`, `closed_at = NULL` and emits nothing; the
coverage-driven flow (§2.6) then expires or cancels it.

### 2.6 Negative decisions

- **Expiry and reservation release** (`locks/mod.rs` `expire_once`): require the quote address
  caught up (`dual_covered_through = chain_coverage.through_block`) and
  `chain_coverage.through_time > expires_at`, plus today's "no in-window `detected` deposit".
  Final status `cancelled` + `quote.canceled` when `cancel_requested_at` is set, else `expired` +
  `quote.expired`.
- **Cancel** (`POST /v1/quotes/{id}/cancel`): accepted while the quote is open, before
  `expires_at`, with no deposit row (positive check, as today); sets
  `cancel_requested_at = now()`, `expires_at = now()`, audit row; returns the quote still `open`.
  An in-window payment found later consumes it normally. API docs and SDK types gain
  `cancel_requested_at`.
- **Custody balance** (reconciler): on startup and hourly, at
  `B = min(checkpoint, chain_coverage.through_block)`, only for caught-up forwarders. Both endpoints
  independently read the full balance vector at B's canonical hash before accepting equality or
  freezing on mismatch. An hourly wake-up keeps custody due when checkpoint progress stalls.

## 3. Endpoints, retries, readiness, deploy

- Per endpoint: Alloy HTTP provider, redirects disabled, request timeout, CountingLayer,
  redaction, and one `RetryBackoffLayer::new_with_policy` that retries HTTP 429/503 and JSON-RPC
  throughput errors with backoff and **never retries Infura HTTP 402** (daily credit limit);
  402 marks verify not-ready until 00:00 UTC (5:00 PM PDT, 4:00 PM PST). No `FallbackLayer`; callers' loops
  are the only outer retry.
- A failing endpoint makes only its chain (or the price feature of routes observing it)
  not-ready (`topup_rpc_endpoint_ready`); `/healthz`, the API and other chains keep running;
  never a crash loop.
- Startup self-test = `topup rpc check`, both endpoints of every chain, real typed calls:
  `eth_chainId`; `latest`/`finalized` headers; typed transaction and receipt from the finalized
  block; `eth_getLogs({blockHash})` returning a log of that receipt; the coverage query shape
  (1 000 recipients, `max_log_blocks`); EIP-1898 `requireCanonical` calls (token `decimals`,
  factory and Multicall3 code, price feeds) at the checkpoint; state at
  2 × the observed finalized lag.
- Deploy preflight runs that check through the real compose env path
  (`docker compose --env-file <candidate sealed env> -f <rendered compose> run --rm --no-deps
  topup topup rpc check …`, also for `restore-check`), replacing `docker run -e` in
  `deploy/preflight.sh`. A configured `sealed_key` missing from the sealed env or the compose
  mapping blocks the deploy. Sealing always submits the complete secret set.

## 4. Config schema

```yaml
rpc:                                    # one entry per chain used by a route or a price source
  - chain_id: 11155111                  # u64, unique
    read:                               # required
      id: ankr-sepolia                  # [a-z0-9-]{1,40}, unique; metric label
      url: "https://rpc.ankr.com/eth_sepolia/{key}"   # https; {key} = one whole path segment or query value
      sealed_key: TOPUP_RPC_ANKR_KEY    # TOPUP_RPC_[A-Z0-9_]+_KEY
      max_log_blocks: 3000              # u32 ≥ 1 (measured)
    verify:                             # required; host ≠ read host
      id: infura-sepolia
      url: "https://sepolia.infura.io/v3/{key}"
      sealed_key: TOPUP_RPC_INFURA_KEY
      max_log_blocks: 3000
```

Same for `84532` (`rpc.ankr.com/base_sepolia/{key}`, `base-sepolia.infura.io`), `1`
(`rpc.ankr.com/eth/{key}`, `mainnet.infura.io`), `8453` (`rpc.ankr.com/base/{key}`,
`base-mainnet.infura.io`) in both environments (staging uses `1` and `8453` for prices only).
Cadences and hard admission caps are code constants. Deposit, factory and Safe limits are
operational caps with monitoring and stop actions in §5.2. Removed: `rpc_companies`,
`rpc_budgets`, `rpc_groups`, route `chain.rpc_groups`, price `rpc_group`/`rpc_group_b`,
`asset.backstop`, `--head-poll-interval-s`, `--finalized-poll-interval-s`,
`TOPUP_RPC_PROBE_DEBUG`. `topup config check` rejects missing or unknown fields, duplicate ids,
same-host pairs and bad templates.

## 5. Providers, measurements, budget

### 5.1 Verified limits and measurements

| | Ankr Freemium (read) | Infura Free (verify) |
|---|---|---|
| Quota | 200 M credits/month at 200 per EVM call = 1 M calls/month | 3 M credits/day; 80 per call, 255 per `eth_getLogs`; batch items billed each |
| Throughput | ≈1 800 req/min guaranteed | 500 credits/s (HTTP 429) |
| Exhausted | response not documented → generic errors → not-ready | HTTP 402, halted for the rest of the UTC day |
| Keys | 1 personal token → shared by both environments | API key limit 1 → shared |
| Terms | Freemium "to try out premium features"; no production prohibition found (full ToS unverified) | Consensys terms forbid use "intended to avoid … usage limits or quotas"; no free-tier production prohibition found |

Sources: ankr.com/docs/rpc-service/service-plans; infura.io/pricing;
docs.infura.io/how-to/avoid-rate-limiting; docs.metamask.io credit-cost table; metamask.io/terms-of-use.

Keyed measurement, 2026-10-07 ~9:20 AM PDT, from an operator host, both providers on Sepolia,
Base Sepolia, Ethereum and Base, all passed: `eth_chainId`; `finalized`; token-scoped
recipient-topic logs over 1 000 blocks; address-less recipient logs over 100 blocks; EIP-1898
`eth_call` with `blockHash` + `requireCanonical` at finalized; state at finalized − 200 (Ethereum
chains) and − 1 300 (Base chains); a 1 000-recipient OR-list (returned the real Sepolia deposit);
3 000-block logs; full transaction fields (`accessList`, `yParity`). Infura serves Base finalized
state, so there is no third provider. Not yet measured: the staging CVM's egress, Ankr's
exhaustion response, address-less logs over 3 000 blocks (self-test covers it).

### 5.2 Worst-case daily budget (staging + production combined)

[RPC operations](../../deploy/RPC.md#worst-case-pilot-budget) is the authoritative budget:
five-minute read discovery, ten-minute dual checkpoints, hourly dual coverage, six-hour
19,200-block catch-up, all nine hourly custody routes, prices, deposits, hints, factory proofs,
Safe proofs, attached refunds, unresolved finality stock and extra operations. It assumes every
payment chain runs all day, one caught-up recipient chunk plus one lagging chunk, and all 1,000
historical addresses.

The approved [parallel operating mode and limits](../../deploy/RPC.md#pilot-limits-and-operating-modes)
run staging tests, including refunds, alongside production on the shared free quotas. D, factory
and Safe caps have no code enforcement; operators must tally work and stop new load at the limits.
Refund attachments have atomic concurrent and rolling-24-hour admission caps. Hint and price
daily budgets and the permanent address cap are hard limits.

With K=1, each unresolved recheck costs at most `max(3, 1 + 3×K) = 4` methods per endpoint.
S=1 per environment and at most S arrivals per environment per 24-hour budget window give
`2×(62 + 24) = 172` daily rechecks, including first-day arrivals and carried stock: 688 Ankr
calls / 55,040 Infura credits before retries. With ×1.1 non-refund work, ×3 refund attempts and
the extra reserve, totals are 16,347 Ankr calls/day and 1,329,910 Infura credits/day, leaving
34.61% / 11.34% headroom below the stop lines. All other caps remain unchanged. The complete
modeled upper bound includes replacement reads and stock turnover under the operating limits
and retry assumptions; it is not a code-enforced quota guarantee. Follow the linked monitoring
and stop procedures before adding load.

### 5.3 Latency and recovery targets

[RPC latency and recovery targets](../../deploy/RPC.md#latency-and-recovery-targets) define the
healthy-operation SLOs and backlog exceptions:

| Path | Scheduling target |
|---|---|
| Manual transfer | Discovery in 0–5 min, then confirmation and processing |
| Admitted checkout hint | Instant processing path; seconds at route confirmation |
| Quote expiry / cancel / unpaid reservation release | ≤ about 70 min after qualifying finality |
| Known-deposit reversal / checkpoint conflict | ≤10 min, plus watch/RPC processing |
| Custody discrepancy | ≤ about 130 min after finality |
| Recovery from ≤24 h equivalent backlog | ≤12 h continuous healthy operation, within work allowances |

Hint readiness, task budgets and confirmations still apply. Already unresolved deposits follow
the age-dependent recheck cadence; later-day evidence can wait up to an hour for the next check.
The ten-minute reversal target concerns newly due deposits. Conflicts visible only in full logs
may wait for hourly coverage. Payment windows and negative-evidence gates are unchanged:
in-window qualifying payments retain quote terms; late, partial and persistent-address payments
use fresh processing-time prices and screening uses the verified local lists at decision time.

## 6. DB changes and N-1

One expand-only migration, compatibility floor = N-1's maximum migration:

```sql
CREATE TABLE chain_checkpoints (
  chain_id bigint PRIMARY KEY CHECK (chain_id > 0),
  block_number bigint NOT NULL CHECK (block_number >= 0),
  block_hash text NOT NULL, block_time timestamptz NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE chain_coverage (
  chain_id bigint PRIMARY KEY CHECK (chain_id > 0),
  through_block bigint NOT NULL CHECK (through_block >= 0),
  through_hash text NOT NULL, through_time timestamptz NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE daily_budgets (
  day date NOT NULL, name text NOT NULL, used integer NOT NULL DEFAULT 0 CHECK (used >= 0),
  PRIMARY KEY (day, name));
ALTER TABLE addresses ADD COLUMN dual_covered_through bigint CHECK (dual_covered_through >= 0);
ALTER TABLE deposits  ADD COLUMN dual_verified_at timestamptz;
ALTER TABLE quotes    ADD COLUMN cancel_requested_at timestamptz;
GRANT SELECT, INSERT, UPDATE ON chain_checkpoints, chain_coverage, daily_budgets TO topup_app;
```

Cursor updates are monotonic (`WHERE … < $new`). New tables go into the privileges test and
restore's table set; a restore without them restarts coverage at `min(created_block) − 1` with
every `dual_covered_through` NULL.

N-1 safety:

- N-1 never reads the new tables or columns. Rows N-1 inserts get NULL markers, so N re-covers
  their addresses and re-verifies their deposits. A cancel-requested quote is `open` with a past
  `expires_at`; N-1 expires it as `expired` (no money effect).
- N writes no `rpc_*` table and keeps `cursors.scanned_block/time` = coverage end,
  `confirmed_block` = fast cursor, `backfilled`/`backfilled_through` only for caught-up addresses,
  deposits, `flushed`, `flush_failures` with today's semantics; N-1's own sweeps, reconciliation
  cursors, watermarks, reviews and reorg ranges are untouched and stay valid for it.
- N refuses to start while any configured chain has `rpc_chain_state.frozen`, `awaiting_anchor`
  or `recovery_pending` (resolve with N-1 first).
- Proven by `deploy/local/rollback-drill.sh` (CI gate in `deploy-rollback.yml`), Anvil with two
  hostnames: real N-1 image → payments, credit, reconcile → N (migrate) → payments with and without
  hints, credit, finality, flush, a cancel, an expiry, a deposit-address **reissue with a lowered
  `created_block` and a historical payment to it**, a coverage round killed mid-way → N-1 → new
  payment credited, the historical payment recorded once, no duplicate events, finality, sweep,
  reconciler clean → N again: the reissued address is backfilled dual-source from its
  `created_block`, N-1-era deposits get `dual_verified_at`, same assertions.

## 7. Deleted and kept

Deleted:

- `crates/adapters/src/chain/evm/group/` (`mod.rs`, `tests.rs`, `budget.rs`, `rules.rs`,
  `transport.rs`, `metrics.rs`), `chain/evm/window.rs`; `EvmClient::from_group*`, `group()`,
  `independent_review_available`, `read_window`, member pinning, `KnownTransfer` reuse.
- `crates/topup/src/rpc_groups.rs` (and `psl`); `rpc_runtime.rs` probes, acceptance, anchors,
  recovery, availability monitor; `db/rpc.rs` except the `rpc_chain_state` start check and
  `sweep_epoch`; CLI `topup rpc recover`/`resume`.
- `scanner/mod.rs` single-source backstop, review loop and address sweeps; `scanner/head.rs`
  phase-locked head loop and reorg-range replay; `reconciler/mod.rs` `missing_deposits*`;
  `pricing/chainlink.rs` `confirm_block`/`agreed_block`; refund `finalized_nonce` "dropped" path;
  nonce-based reversal and the wall-clock quote expiry in `finality/mod.rs`.
- Tests `crates/topup/tests/{rpc_groups,rpc_entrypoints,rpc_query_plans}.rs` and group tests.
- Metrics: every `topup_rpc_*` except `topup_rpc_calls_total` and `topup_rpc_calls_since_seconds`.
- Config and flags in §4; `docs/design/rpc-failover.md` (replaced by `docs/design/chain-reads.md`).

Kept: `cursors`, deposit identity and insert rules, confirm/screen/credit/finality pipeline,
refund and treasury dual verification, other reconciler checks, `reconciliation_blocks` and its
lift, pending view, URL template and sealed-key rules, redaction, CountingLayer,
`api/rate_limit.rs`, governor.

Added: `ChainRpc {read, verify}`, coverage rounds, checkpoint, hint endpoints and task, daily
budgets; metrics `topup_rpc_errors_total{provider,chain_id,method,class}`,
`topup_rpc_endpoint_ready`, `topup_coverage_lag_seconds{chain_id}` (now − `through_time`),
`topup_addresses_lagging{chain_id}`, `topup_hint_total{result}`, `topup_daily_budget_used{name}`;
alerts: endpoint not ready 5 min, any disagreement, coverage lag > 2 h, reversal unproven,
quota run-rate (recording rules over `topup_rpc_calls_total` × provider cost tables, both
environments summed).

Docs: architecture §0, §2 rule 7, §7, §8, §9 (cancel, snapshot cap), §13;
`docs/configuration.md`; `docs/integration.md` (hints, cancel, latency); `deploy/RPC.md`;
`deploy/rpc-alerts.yaml`; `deploy/phala.md`; environment `topup.yaml`/`compose.yaml`;
`deploy/preflight.sh`, `deploy/preflight-rpc.sh`, `deploy/local/*`, `deploy/tests/*`.

## 8. PR sequence

1. **docs(design): chain reads** — this document as `docs/design/chain-reads.md`.
2. **feat(rpc)!: read/verify endpoints, dual coverage, checkpoint** — §1–§4, §6, §7 server side,
   migration, rollback drill, compose-path preflight, env configs, docs, alerts, CHANGELOG
   `### Breaking (operators)` (config schema, cancel semantics, quote snapshot cap).
3. **feat(api,sdk): transaction-hash hints** — endpoints, task, caps, `@phala/pay`,
   `@phala/pay-react`, `@phala/pay-server`, `docs/integration.md`. Released together with PR 2.
4. **Staging adoption** (operator-authorised): seal the complete secret set with
   `TOPUP_RPC_ANKR_KEY` and `TOPUP_RPC_INFURA_KEY`, compose-path preflight, Deploy upgrade, §9.
5. **Production pilot config**: same providers and keys, after staging acceptance.

## 9. Acceptance

Automated:

- Each R1 field forged on either endpoint (including `block_time` and nonce) → no credit;
  agreement → credit and `dual_verified_at`.
- Coverage: one endpoint omits a log → union records it; any request error → no commit;
  disagreement → no commit; coverage, markers and pending cleanup end at the scanned boundary,
  never the checkpoint ahead of it; the compat cursor uses the common complete boundary, with
  dual header evidence if it lags. First range includes `min(created_block)`; empty chain
  initialised before issuance.
- Unverified existing deposit in range (fast-path or N-1 row with forged `block_time`) →
  re-verified; mismatch after `detected` freezes the chain; marked rows are skipped.
- Reissued address with lowered `created_block` → dual backfill from it; only completed addresses
  marked; its quotes cannot expire until it is caught up.
- Expiry and cancel completion only after the address is caught up and coverage passes
  `expires_at`; reversal reopens the quote and reservation, then coverage closes it.
- Hints: deposit-address `chain_id` outside its networks → quiet 202; unmined, unrelated, other
  address, reverted → quiet, no record; duplicates and scanner-seen → one deposit; task call cap
  and deadline end the task; 81st task of a UTC day refused by `daily_budgets`; not-ready parks;
  shared API overload returns retryable 503 without reading the hint body.
- Quote snapshot cap → `price_unavailable` (retryable); never a stale price beyond 12 s.
- Retry policy: 429 retried; Infura 402 not retried, not-ready until 00:00 UTC; other chains
  and `/healthz` unaffected. Config check and compose-path preflight reject missing keys.
- Rollback drill N-1 → N → N-1 → N passes (§6).

Staging (48 h, real USDC/USDT/PHA payments on Sepolia and Base Sepolia, checkout and manual):

- Checkout hints use the independent fast path and credit in seconds at route confirmation;
  unhinted transfers are discovered in 0–5 min plus confirmation/processing. Verify the §5.3
  release, reversal and custody targets with healthy endpoints and caught-up addresses.
- Coverage lag ≤ 2 h in healthy steady state; no lagging addresses older than one day; zero disagreements;
  reconciler clean; every deposit final and `dual_verified_at` set.
- Ankr and Infura usage below both stop lines under §5.2 caps and retry assumptions; provider
  dashboards agree with the recording rules and operational tallies include recovery work.
- Fault drill: wrong verify key → that chain not-ready, credit pauses, service up; restored →
  resumes. Recover ≤24 h equivalent backlog within the twelve-hour target in §5.3.

## 10. Operator accounts and keys

| Account | Key | Sealed name | Environments |
|---|---|---|---|
| Ankr (Freemium, one account) | the single personal token | `TOPUP_RPC_ANKR_KEY` | staging and production (shared quota) |
| Infura / MetaMask Developer (Free, one account) | the single API key; Ethereum, Sepolia, Base, Base Sepolia enabled; usage emails on | `TOPUP_RPC_INFURA_KEY` | staging and production (shared quota) |

No keyless endpoints. Do not open extra free accounts to raise quota.

## 11. Rejected alternatives

- Current A/B group framework: safety kept by R1–R5; scheduling, budgets, probes, watermarks and
  reviews deleted.
- Paid providers (Alchemy PAYG, Infura Developer): violate the $0 requirement.
- Alchemy free (10-block logs): unnecessary; Infura serves Base finalized state (measured).
- PublicNode (no state beyond ~128 blocks on mainnet/Base, no address-less logs), dRPC free
  (public-node pool, ~100-block range, batch ≤ 3), Chainstack Developer (100 blocks, no archive),
  QuickNode (1-month trial), 1RPC (relay with random upstreams and caching), Tenderly (Node RPC
  not in the free plan).
- Third source, quorum or in-process failover: excluded by the two-endpoint rule.
- Lower price-snapshot cadence or longer quote reuse: would weaken quote freshness; a hard daily
  snapshot cap with fail-closed quotes keeps the 12 s bound.
- Helios, own node, indexers/streams/webhooks, hot/cold address tiers: not needed within the
  pilot limits.
- Nonce binary search for reversals: not proof under EIP-7702.
