# Chain reads: final design

Status: accepted 2026-10-07; not yet implemented. Supersedes
[the RPC failover design](https://github.com/Phala-Network/phala-pay/blob/5f585cca2073b81fd7d016192940e906fef8c13e/docs/design/rpc-failover.md). Provider capabilities measured with the real keys on
2026-10-07 (§5.1).

Binding requirements: $0 for staging and the production pilot; simplest standard design, exactly
two endpoints per chain, no in-process failover, the custom RPC framework deleted; no downgrade in
money safety (every money decision, prices included, dual-source; every negative conclusion needs
dual-source complete coverage); checkout credit speed does not regress; a real
N-1 → N → N-1 → N rollback passes.

## 1. Model

Every chain has two endpoints from independent companies: **read** (Ankr Freemium) and
**verify** (Infura Free). Assumption: at least one is correct and synced.

- **R1 Dual evidence.** A fact that moves money or is permanently recorded is accepted only when
  both endpoints derive the same decoded fields independently. Disagreement = wait + alert.
- **R2 Pinning.** State reads use EIP-1898 `{blockHash, requireCanonical: true}`. Current-state
  pins (prices, sanctions) come from verify (`eth_blockNumber`, then the header at `latest − 2`);
  finalized pins use the checkpoint. "Not found / not canonical" on either side = wait, re-pin.
- **R3 Errors are never results.** A range is covered only if every request for it succeeded.
- **R4 Negative conclusions** ("no payment to address A up to block B") come only from dual
  coverage (§2.2): the chain cursor and the address's own coverage.
- **R5 Positive candidates** may come from one source (fast discovery, hints) and are recorded as
  `detected`; nothing beyond `detected` happens without R1, and a record is marked
  `dual_verified_at` only after R1 on every decision field.

## 2. Read paths

### 2.1 Fast discovery (read only, every 60 s per chain)

`eth_getBlockByNumber(latest)`, then one `eth_getLogs` per 1 000 issued addresses over
`(fast cursor, latest]` with `topics: [[Transfer], null, [≤1000 recipients]]` (no `address`;
ERC-20 layout decoded locally; routed tokens only). Transfers at or below the route's confirmation
horizon are inserted as `detected` as today (`Evidence::Confirmed`, key
`(chain, tx, receipt log index)`, `dual_verified_at` NULL); `cursors.confirmed_block` advances.

### 2.2 Finalized dual coverage (both endpoints, every 10 min per chain)

State: chain cursor `c = chain_coverage.through_block`; per address
`addresses.dual_covered_through` (NULL = nothing covered). An address is **caught up** when
`dual_covered_through = c`.

Initialisation: when `topup run` first starts with a chain, before the API can issue an address
on it, `chain_coverage` is set to the agreed checkpoint (§2.5). A restored or upgraded database
without a row starts at `min(created_block) − 1`, so the first range includes `min(created_block)`.

Each round, staggered per chain:

1. Advance the checkpoint to `t`. Range end `e = min(t, c + L)`, `L` = 3 000 blocks, or 19 200 on
   every sixth round (hourly catch-up; §5.2 budgets it). Read the header of `e` on both
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
5. One transaction: records and markers; `chain_coverage` → `(e, hash(e), time(e))`;
   `cursors.scanned_block/time` → the same `e` (N-1 compatibility); C's
   `dual_covered_through = e`; G's `dual_covered_through = g_end` for addresses whose start
   ≤ `g_end`; `backfilled = true, backfilled_through = e` only for addresses now caught up;
   pending-view rows ≤ `e` deleted.

This replaces the single-source finalized backstop and the reconciler's missing-deposit pass.

### 2.3 Transaction-hash hints (checkout fast path)

```text
POST /v1/quotes/{id}/transactions?client_secret=…             {"transaction_hash":"0x…"}
POST /v1/deposit_addresses/{id}/transactions?client_secret=…  {"transaction_hash":"0x…","chain_id":84532}
```

Authenticated by the object's `client_secret` (the Stripe browser pattern the read routes already
use; it grants nothing beyond this object) or a merchant key with `quotes:write` /
`deposit_addresses:write` (server forwarding). `chain_id` is required for deposit addresses and
must be one of the object's networks; the recipient is always derived server-side. Response is
always `202 {"object":"transaction_submission","transaction_hash":…,"status":"received"}`
(no oracle). Only the hash and that chain id are used.

Task (in process; deduplicated by `(chain, tx, object)`; insertion dedupes by receipt position):

1. Claim one unit of today's hint budget (§2.3 caps); none left → stop (scanning covers it).
2. Poll the receipt on read (1, 2, 4, then every 4 s on Ethereum chains; every 1 s on Base).
3. Quiet stop unless `status = 1` and an ERC-20 `Transfer` of a routed token of that chain pays
   the object's address on it.
4. Read receipt, transaction and header on verify; compare every decision field (§2.4); poll
   both heads until the route's confirmation is reached; insert through the scanner's insert
   function (`Evidence::Confirmed`, `dual_verified_at` set). The normal pipeline follows.
5. The whole task, head polls included, is bounded by **12 read calls, 8 verify calls and a
   deadline** (90 s Ethereum chains, 30 s Base chains); hitting either ends the task (scanning
   covers it).

Caps: per object 3/min and 10/day, per source IP 20/min (keyed limiters of `api/rate_limit.rs`);
at most 4 tasks in flight; **150 tasks per environment per UTC day**, enforced by an atomic
`daily_budgets` row (`UPDATE … SET used = used + 1 WHERE day = $today AND name = 'hints' AND
used < 150`), because the governor buckets refill. Endpoint not ready → park in a bounded
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
- **Sanctions**: `isSanctioned(from)` on both at one pinned block S (R2); the step checks
  `S.number ≥ payment block` explicitly, else waits. List version = at screening time; evidence
  records S. Refund destinations: same.
- **Prices**: one Multicall3 `aggregate3` per snapshot per endpoint at one pin: all Chainlink
  feeds of the chain (`latestRoundData`, `decimals`), Base sequencer uptime, or the Uniswap V2
  pair state with `getCurrentBlockTimestamp`. Bytes must match; existing freshness, heartbeat,
  peg, liquidity, continuity and jump checks unchanged. Quote reuse stays 12 s; confirm always
  takes a fresh snapshot. Fresh quote snapshots are capped (§5.2) by a `daily_budgets` row per
  `(price chain)`; when exhausted, quotes needing that chain fail closed with a retryable
  `price_unavailable`. Staging PHA TWAP sampling: `uniswap_v2.rs` `SAMPLE_INTERVAL_S` 60 → 300
  (staging-only, production-ineligible route; continuity and jump limits re-derived for 300 s).
- **Refunds** (`refunds.rs`) and **treasury EIP-1271** (`treasuries/proof.rs`): dual as today,
  pinned to the checkpoint hash.
- **Flush records**: only from §2.2 step 4.

### 2.5 Checkpoint, reorgs, reversal

Per route chain, `chain_checkpoints(number, hash)` advances when read's `finalized` hash at
`number` equals verify's header at `number`, `number ≤` verify's `finalized`, and the previous
checkpoint height still has the stored hash on both. A conflict inserts a chain-scope
`reconciliation_blocks` row (`'finalized_checkpoint_conflict'`); the existing freeze gate halts
the chain until the audited admin lift.

Reversal requires positive evidence on both at or below the checkpoint: (a) the deposit's
receipt without its transfer at its position (successor recorded as today), or (b) a directly
evident replacement: a transaction already known to the service with the same sender and nonce
and another hash, finalized and agreed on both. Otherwise wait and alert
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
- **Custody balance** (reconciler): at `B = min(checkpoint, chain_coverage.through_block)`, only
  for forwarders whose address is caught up, pinned to B's hash; a mismatch is re-read on verify
  before freezing.

## 3. Endpoints, retries, readiness, deploy

- Per endpoint: Alloy HTTP provider, redirects disabled, request timeout, CountingLayer,
  redaction, and one `RetryBackoffLayer::new_with_policy` that retries HTTP 429/503 and JSON-RPC
  throughput errors with backoff and **never retries Infura HTTP 402** (daily credit limit);
  402 marks verify not-ready until 00:00 UTC (5:00 PM PDT). No `FallbackLayer`; callers' loops
  are the only outer retry.
- A failing endpoint makes only its chain (or the price feature of routes observing it)
  not-ready (`topup_rpc_endpoint_ready`); `/healthz`, the API and other chains keep running;
  never a crash loop.
- Startup self-test = `topup rpc check`, both endpoints of every chain, real typed calls:
  `eth_chainId`; `latest`/`finalized` headers; typed transaction and receipt from the finalized
  block; `eth_getLogs({blockHash})` returning a log of that receipt; the coverage query shape
  (1 000 recipients, `max_log_blocks`); EIP-1898 `requireCanonical` calls (token `decimals`,
  sanctions oracle, factory and Multicall3 code, price feeds) at the checkpoint; state at
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
Cadences, caps and budgets of §2 and §5.2 are code constants. Removed: `rpc_companies`,
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

Stop lines: Ankr 75 % of monthly/30 = 0.75 × 1 000 000 / 30 = **25 000 calls/day**; Infura 50 % =
**1 500 000 credits/day** (a 402 halts both environments). Retries: ×1.1 on everything, so the
pre-retry budgets are **22 727** and **1 363 636**.

Per route chain, worst day (k = 1 caught-up chunk, one lagging chunk, every round behind):

- Fast discovery: 1 440 × (1 head + 1 logs) = 2 880 Ankr.
- Coverage logs per endpoint: 120 normal rounds × 2 chunks × 1 window + 24 hourly rounds × 2
  chunks × ⌈19 200 / 3 000⌉ = 240 + 336 = 576 calls.
- Coverage headers: Ankr 3 per round (finalized, previous checkpoint, `e`) = 432; Infura 4 per
  round (adds the header at `t`) = 576 calls.
- Custody: 144 Ankr.
- Ankr 2 880 + 576 + 432 + 144 = **4 032**; Infura 576 × 255 + 576 × 80 = **192 960**.
- Four route chains: Ankr **16 128**, Infura **771 840**.

Variable costs:

- Deposit (Base worst): Ankr detect 3 + confirm 4 × 1.25 + sanctions 1 + finality 1 + prices 2 +
  coverage re-verify 3 = **15**; Infura confirm 400 + sanctions 240 + finality 80 + prices 480 +
  re-verify 240 = **1 440**.
- Hint task: ≤ 12 Ankr, ≤ 8 × 80 = 640 Infura.
- Price snapshot: 1 Ankr, 240 Infura (`eth_blockNumber` + pin header + multicall).
  Snapshots/day = staging TWAP 288 + fresh quote snapshots, capped at Q per price instance
  (4 instances: staging Ethereum, staging Base, production Ethereum, production Base).

Constraints with D deposits/day (both environments), H hint tasks/day per environment, Q:

```text
Ankr:   16 128 + (288 + 4Q)      + 15 D + 2 × 12 H  ≤ 22 727
Infura: 771 840 + 240 (288 + 4Q) + 1 440 D + 2 × 640 H ≤ 1 363 636
```

Choosing Q = 100, D = 150, H = 150:

- Ankr: 16 128 + 688 + 2 250 + 3 600 = **22 666 ≤ 22 727** → ×1.1 = 24 933 ≤ 25 000.
- Infura: 771 840 + 165 120 + 216 000 + 192 000 = **1 344 960 ≤ 1 363 636** → ×1.1 = 1 479 456 ≤ 1 500 000.

The binding term is the quote rate; a lower price cadence was rejected because it would lengthen
quote staleness beyond today's 12 s. Typical days (no catch-up, no lagging chunk, few quotes)
use about half of these figures.

**Pilot limits (all hard or stop-adding-load):**

- ≤ 1 000 issued addresses per chain (one caught-up chunk) — stop issuing on that chain beyond it;
- ≤ 150 deposits/day across both environments — stop adding merchants when the 7-day average
  exceeds 120;
- ≤ 150 hint tasks per environment per UTC day — hard (`daily_budgets`);
- ≤ 100 fresh quote price snapshots per price chain per environment per UTC day — hard;
- backfill one lagging chunk (≤ 1 000 addresses) per chain at a time — by construction;
- stop adding load whenever Ankr's 7-day run-rate exceeds 25 000/day or Infura's daily use
  exceeds 50 % (Infura emails at 75/85/100 %).

At a quota: Ankr exhausted → read not-ready on every chain until the monthly reset; Infura 402 →
verify not-ready until 00:00 UTC. Hints park, quotes needing prices fail closed, coverage and
credit pause. Never wrong data.

### 5.3 Credit latency after inclusion (estimates)

| Path | Ethereum (depth 2) p50 / p95 | Base (depth 3) p50 / p95 |
|---|---|---|
| Today (block-time head loop) | ~15 s / ~25 s | ~6 s / ~8 s |
| Checkout with hint | ~16 s / ~28 s | ~6 s / ~9 s |
| Manual transfer, 60 s scan only | ~45 s / ~75 s | ~37 s / ~65 s |
| Missed by fast path, found by coverage | finality + ≤ 10 min | finality + ≤ 10 min |
| Quote expiry / cancel completion | finality + ≤ 10 min after `expires_at` | same |

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
alerts: endpoint not ready 5 min, any disagreement, coverage lag > 45 min, reversal unproven,
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
  disagreement → no commit; cursor, compat cursor, markers and pending cleanup all end at `e`,
  never `t`; first range includes `min(created_block)`; empty chain initialised before issuance.
- Unverified existing deposit in range (fast-path or N-1 row with forged `block_time`) →
  re-verified; mismatch after `detected` freezes the chain; marked rows are skipped.
- Reissued address with lowered `created_block` → dual backfill from it; only completed addresses
  marked; its quotes cannot expire until it is caught up.
- Expiry and cancel completion only after the address is caught up and coverage passes
  `expires_at`; reversal reopens the quote and reservation, then coverage closes it.
- Hints: deposit-address `chain_id` outside its networks → quiet 202; unmined, unrelated, other
  address, reverted → quiet, no record; duplicates and scanner-seen → one deposit; task call cap
  and deadline end the task; 151st task of a UTC day refused by `daily_budgets`; not-ready parks.
- Quote snapshot cap → `price_unavailable` (retryable); never a stale price beyond 12 s.
- Retry policy: 429 retried; Infura 402 not retried, not-ready until 00:00 UTC; other chains
  and `/healthz` unaffected. Config check and compose-path preflight reject missing keys.
- Rollback drill N-1 → N → N-1 → N passes (§6).

Staging (48 h, real USDC/USDT/PHA payments on Sepolia and Base Sepolia, checkout and manual):

- Checkout credit after inclusion: Sepolia p50 ≤ 20 s, p95 ≤ 30 s; Base Sepolia p50 ≤ 8 s,
  p95 ≤ 12 s. Manual: Sepolia p50 ≤ 50 s, p95 ≤ 80 s; Base Sepolia p50 ≤ 40 s, p95 ≤ 70 s.
- Coverage lag ≤ 45 min; no lagging addresses older than one day; zero disagreements;
  reconciler clean; every deposit final and `dual_verified_at` set.
- Ankr and Infura usage within ±30 % of the typical case and below both stop lines; provider
  dashboards agree with the recording rules.
- Fault drill: wrong verify key → that chain not-ready, credit pauses, service up; restored →
  resumes.

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
