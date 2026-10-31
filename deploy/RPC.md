# Chain RPC operations

Each configured chain has an Ankr read endpoint and an independent verify endpoint: BlockPI in
production, Infura in staging.
[Chain reads](../docs/design/chain-reads.md) specifies the evidence and read paths.
No endpoint failover, provider pool, acceptance state or RPC recovery command remains.
[RPC providers](RPC-PROVIDERS.md) compares providers' fee models, free tiers and compatibility.

## Configuration and preflight

Configure `rpc` entries with `chain_id`, `read` and `verify`. Each endpoint requires a unique
`id`, HTTPS `url` containing one whole-segment or query-value `{key}`, explicit `sealed_key`,
and positive `max_log_blocks`. Read and verify hosts must differ. Configure observation-only
Ethereum and Base price chains too. Both environments share `TOPUP_RPC_ANKR_KEY`. Staging keeps
`TOPUP_RPC_INFURA_KEY` for its payment chains and mainnet price observations. Production uses
`TOPUP_RPC_BLOCKPI_ETHEREUM_KEY` and `TOPUP_RPC_BLOCKPI_BASE_KEY` from one BlockPI free account,
with archive mode off and a separate key for each mainnet. Production verifies with BlockPI
before provisioning; staging stays on Ankr + Infura. No dRPC adoption is required.
The environment-wide caps below use the reviewed steady-state and recovery budget
([launch procedure](runbooks/production-launch.md#18-rpc-accounts-and-production-allocation)).

`topup.yaml` requires positive `max_attached_pending_refunds` and
`max_refund_attachments_per_day`: production uses 10/10 and staging 1/1. The rolling 24-hour
attachment allowance includes resolved refunds. Optional
`max_attached_pending_refunds_per_account` defaults to 1 across modes; production sets 3.
Each `rpc` entry requires `max_issued_addresses`: production uses 12,000/payment chain and
staging 1,000. Address and daily attachment limits accept integers from 1 to 1,000,000 and
appear in `topup config show`; invalid or missing values fail validation. Retirement and expiry
do not free historical capacity. Capacity increases need a reviewed budget and attested upgrade.
Older rollback targets require their retained configuration and compatible inventory limits.

CI resolves the latest stable release other than the current ref as N-1 for rollback rehearsal.
The rollback floor is v0.13.0; going below it requires [restore and reconciliation](RESTORE.md).
Use the target's retained verified configuration, kit and image digests, and keep operational
inventory and load controls in force. See
[launch rollback](runbooks/production-launch.md#12-rollback) and
[rollback compatibility](README.md#rollback-compatibility).

```sh
topup config check --secrets /etc/topup/topup.yaml
topup rpc check --config /etc/topup/topup.yaml
```

`rpc check` independently validates both endpoints with typed identity, finalized transaction,
receipt and log reads, the real coverage queries with 1,000 recipients, canonical state calls,
and a full 1,000-call custody `aggregate3` on **each** endpoint, at both the agreed finalized hash
and state at
twice the observed finalized lag. Contract identity and the preflight boundary pin both
endpoints at the lower finalized height and require matching header hash and timestamp.
Each payment token uses the production `balanceOf` reader with 1,000 distinct synthetic
recipients; observation-only chains use Multicall3 native balance views. Provider rejection,
a reverted subcall, malformed output or fewer than 1,000 results fails preflight closed.
Custody batches up to 1,000 calls per aggregate; smaller-provider limits require changing
providers, not silently falling back to smaller batches.
The receipt/log probe searches up to eight finalized blocks for a logged transaction. If none
is found, it passes without a sample; all other endpoint checks remain required. A sample's
receipt and log responses must agree, ignoring only optional `blockTimestamp` metadata. Deploy uses the
actual candidate compose environment:

```sh
docker compose --env-file candidate.env -f rendered-compose.yaml run --rm --no-deps \
  topup topup rpc check --config /etc/topup/topup.yaml
```

The restore-check variant retains the topup service and uses the same command override.
A missing configured key in either the candidate env or service mapping blocks preflight.
Seal the complete secret set on every update; a partial update can unset unchanged credentials.

## Outages, lag and quotas

An endpoint failure pauses its chain's evidence or routes observing that price chain.
The API, `/healthz` and other chains continue. Never resolve disagreement by choosing one
answer. Restore the endpoint and let bounded loops retry with fresh canonical pins.
Alloy's single retry layer retries HTTP 429/503 and JSON-RPC throughput errors; Infura HTTP 402
halts that endpoint until UTC midnight (5:00 PM PDT, 4:00 PM PST).

Read-only discovery runs every five minutes over each payment chain's hot addresses: any quote
created in the last 24 hours, or any deposit with chain block time in the last seven days.
Configure those defaults through `discovery.quote_window_hours` and
`discovery.deposit_window_days`. The quote window must cover every route's maximum permitted
quote TTL; validation rejects a shorter window. All quote/deposit states qualify; no activity cache is used.
Cold addresses still receive hourly dual coverage, and hints work for every address. An unhinted
cold payment can take about 70 minutes with healthy, caught-up coverage.
Independent dual checkpoint
checks run every ten minutes and publish agreed advances for finality, reversal and refunds.
Full dual log coverage runs hourly: at most 3,000 blocks normally and 19,200 whenever the
stored cursor is more than 3,000 blocks behind the checkpoint. Lagging address history uses
its own backlog to select the same catch-up bound, even with a caught-up cursor. Restarting
preserves both choices. Address history is backfilled separately in chunks of at most 1,000 addresses.
Observation-only price chains retain a separate 60-second contract-recovery loop; it skips
RPC when both endpoints' contracts are ready and does not perform payment discovery or coverage.
Candidates remain provisional until both endpoints agree on receipt, transaction, inclusion,
log contents and position, block time, sender and nonce.

Custody sweeps every chain/token route on the first reconciliation tick and every six hours
thereafter (`topup run --custody-interval-s`, default `21600`). Each sweep reads every caught-up
forwarder whose deposits at the pin are final or reversed, including idle and swept-to-zero
forwarders, in 1,000-call Multicall3 aggregates on both endpoints at the same canonical hash,
no later than complete coverage. An independent timer keeps custody due when the checkpoint
stalls. Manual `reconcile`, post-restore checks and restarts run custody immediately and consume the extra-operation reserve.

### Pilot limits and operating modes

The approved parallel mode runs normal staging tests, including refunds, alongside production.
Both environments use Ankr read; production uses BlockPI verify, staging uses Infura verify.

| Limit | Production | Staging | Enforcement |
|---|---:|---:|---|
| Deposits handled/day, D | 280 | 10 | Operational |
| Concurrent attached pending refunds, R | 10 | 1 | Atomic attachment admission |
| New refund attachments/rolling 24 h, N | 10 | 1 | Atomic admission (`max_refund_attachments_per_day`) |
| Unresolved finality stock/environment, S | 1 | 1 | Operational; pause affected-chain quotes and escalate above S |
| New unresolved entries/environment/rolling 24 h | 1 | 1 | Operational; same stock stop rule |
| Current slow-confirmation stock, L/environment | 1 | 1 | Operational; pause affected-chain quotes above the limit |
| New slow-confirmation entries/environment/rolling 24 h | 1 | 1 | Operational; same L capacity stop rule |
| Hint tasks/environment/UTC day, H | 300 | 20 | Atomic daily budget (`hints_per_day`) |
| Fresh quote snapshots/price chain/environment/UTC day, Q | 600 | 30 | Atomic daily budget (`quote_snapshots_per_day`) |
| Factory receipt-fallback instances/day | 150 | 20 | Operational |
| Safe challenge/proof sets/day | 10 | 2 | Operational |
| Extra RPC calls/endpoint/day | 150 | 150 | Operational reserve |
| New address issuances/chain/rolling 24 h, I | 50 | 10 | Operational; DB-derived alert above the allowance |
| Hot discovery addresses/chain | 2,000 | ≤1,000 (address cap) | Operational; alert above 2,000 |
| Recovery methods/account/month | 9,000 | 9,000 | Monthly reserve; shared once on Ankr |
| Historical issued addresses/payment chain | 12,000 | 1,000 | Atomic admission (`rpc[].max_issued_addresses`) |

D includes recovery work, not just payments made that day. Factory counts include historical
re-verification, not only distinct transactions or newly committed events. Safe counts include
failed challenge/proof attempts. Refund admission is environment-wide across accounts and payment
chains; reattaching the same transaction is idempotent. An attached refund retains its reservation
and continues verification until finalized evidence resolves it; never cancel it to make room.

Production-only operation uses the same production caps and leaves additional provider headroom.
During recovery or maintenance, count preflights, restarts, manual reconciliation, restore checks
and extra range requests against the 150-call reserve on each endpoint in each environment.
Opening captures have their own allowance, separate from those 150 calls: charge the unbatched
cost of 50 issuances/production chain/day and 10/staging chain/day. RedPill currently admits at
most 20 new crypto organizations/day; Phala Cloud is not live. Count all physical forwarders,
including quote addresses, rotations and additional networks, rather than treating one
organization as one issuance. Pause new issuance before the allowance is spent. Legacy
captures, new-token captures and opening retries consume the extra/recovery reserve.
Pause new tests and onboarding while backlog consumes operational allowances. New routes, pools, samplers or larger
caps require a reviewed budget before adding load.

### Worst-case pilot budget

Stop lines are **25,000 Ankr calls/day** (75% of 1,000,000 monthly calls divided by 30),
**1,500,000 staging-only Infura credits/day** (50% of the daily quota), and
**1,250,000 production-only BlockPI RU/day** (75% of 50M monthly RU divided by 30).
Charge one Ankr call per method, 80 Infura credits per non-log method and 255 per `eth_getLogs`.
BlockPI charges `eth_chainId` 5; `eth_blockNumber` 10; `eth_getCode`/receipt 15;
headers/`eth_call` 20; transaction 35; logs 50 RU
([RU table](https://docs.blockpi.io/pricing/request-unit-ru/evm-ru-table)).
[Log response surcharges](https://docs.blockpi.io/pricing/request-unit-ru) above 200 KB are
excluded; reconcile billed usage and debit the reserve before adding load.

This is a steady-state model with an explicit monthly recovery allowance, not a daily full
catch-up model. All four payment chains run continuously. Let `k = ceil(N/1,000)` for historical
addresses and `h = ceil(N_hot/1,000)`. Production uses `k=12, h≤2` on each chain; staging uses
`k=h=1`. The hot set is distinct addresses quoted in the last 24 hours or with a deposit in the
last seven days, across every state. It is not bounded by snapshot Q:
quotes can reuse snapshots. `TopupRpcHotAddressBudget` alerts above 2,000/chain. Lower the
activity windows through a reviewed config change (quote window must still cover maximum TTL)
or pause new quotes; continue existing funded work and dual coverage. See
[hot-set and issuance response](runbooks/rpc-health.md#hot-set-and-issuance-budgets).

Coverage's address work uses set-based SQL: one stored factory-history lookup and one guarded
bulk progress update for the entire covered set, plus bounded snapshot/deposit reads. Query
round trips do not grow per address; positive deposit/factory evidence can still add queries
per candidate. Indexed address maps avoid repeatedly scanning the full set for each log or
common-boundary calculation. An unrelated address insertion/reissue during RPC does not discard
covered evidence. Covered address/active-unverified changes still retry with fresh reads.
The commit lock recomputes the common compatibility boundary across all current addresses;
an unprefetched boundary receives no timestamp until a later dual header read. The affected
uncovered address cannot supply expiry or cancellation evidence before coverage catches up.

For ranges r, the per-endpoint coverage log formula remains
`sum_r(ceil(blocks_r / max_log_blocks) × (ceil(addresses_r / 1,000) + 1 factory query))`.
These database changes reduce round trips while preserving required dual RPC.
The 50,000-address PostgreSQL regressions compare statement counts against 1,000 addresses and
bound each healthy synthetic coverage/custody round to 30 seconds. They validate local scaling,
not provider latency or a live capacity increase. Custody retains its existing set-based SQL.
The local PostgreSQL 18 debug-build check measured 23 coverage statements at both 1,000 and
50,000 addresses (about 0.27 s and 6.78 s), including the durable-proof lookup, and 11 custody
statements (about 0.08 s and 0.67 s).
Fixture creation is excluded; external RPC is scripted, so these are database-scale evidence,
not production latency measurements.

The fixed daily formulas per payment chain and endpoint are:

```text
Fast read only:     288 × (1 head + h recipient-log queries)
Checkpoint:        144 × (2 headers + up to 1 ahead-provider pin) ≤ 432
Coverage:          24 × (k recipient-log queries + 1 factory query) on Ankr/Infura
BlockPI coverage:  24 × (k+1) on Ethereum; 48 × (k+1) on Base
Six-hour custody:  4 × C × ceil(N/1,000) aggregates, including expected-zero forwarders
Opening captures:  2 × C × I methods (header + aggregate, unbatched bound)
```

An hourly Ethereum window is about 300 blocks and Base about 1,800. Both fit Ankr/Infura's
3,000-block limit; BlockPI splits Base at its configured 1,000-block limit. This is the F3
`24·(k+1)` steady-state coverage model, with provider splitting retained. New issuance starts
at checkpoint+1 with coverage through the checkpoint, so no daily lagging chunk is charged.
Coverage reaching the stored checkpoint reuses its timestamp/header. Intermediate boundaries,
restored/reissued address backfill and common-boundary timestamp fills use the recovery reserve.
Equal finalized heads need only two checkpoint headers/endpoint; the table reserves the third
on every round so sustained unequal heads do not spend the recovery allowance.

Not-ready endpoint recovery costs at most `86,400 / 300 = 288` head probes/day per endpoint.
Its five-minute alert threshold is unchanged; recovery can take up to the next probe plus
request time.

Catch-up is adaptive at 19,200 blocks for either the main cursor or a lagging range, retaining
per-provider splitting. Recovery reserves **9,000 logical methods/provider account/month**
(300/day averaged over 30 days), priced at 255 credits for Infura and 50 RU for BlockPI. Ankr's
reserve is shared once by both environments. It covers extra range queries, intermediate coverage-boundary
reads, five-minute not-ready probes, and first-finality fallbacks. A 24-hour backlog adds about
`3·(k+1)` Ethereum and `15·(k+1)` Base log queries on 3,000-block providers, once, rather than
four catch-up rounds every day. Including staging, that is about 270 Ankr queries before range
boundary overhead. Nine thousand also covers a day of five-minute recovery probes on all six
Ankr endpoints (1,728 methods) with room for those ranges. It does not authorize an indefinite
outage/restart loop. Maintain a monthly reserve ledger and pause new load if the remaining
reserve cannot cover the next operation; provider stop lines remain independently binding.
The separate daily 150-call/endpoint/environment allowance covers planned preflights, manual
reconciliation and operator recovery (refund candidates reserve all three transport sends).

Custody reads the full vector in 1,000-call aggregates every six hours. Immutable opening
balances at `created_block-1` are captured asynchronously in same-pin batches of at most 1,000;
new issuance at checkpoint `c+1` uses recent pin `c`. Charge the full unbatched bound above:
production `2·(2+1)·50=300` calls/endpoint/day and staging `2·(3+1)·10=80`.
The worker claims up to 1,000 jobs/route/minute; shared pins can reduce the actual cost but do
not raise the allowance. Missing historical/opening proof fails custody closed for the affected
forwarder and alerts `TopupCustodyOpeningUnavailable`. Never substitute latest state.

| Payment chain | C | h | Fixed Ankr calls/day | Fixed verify units/day |
|---|---:|---:|---:|---:|
| Staging Sepolia | 3 | 1 | 1,068 | 47,760 Infura credits |
| Staging Base Sepolia | 1 | 1 | 1,060 | 47,120 Infura credits |
| Production Ethereum | 2 | 2 | 1,704 | 26,160 BlockPI RU |
| Production Base | 1 | 2 | 1,656 | 40,800 BlockPI RU |
| **Total (openings separate)** | **7** | | **5,488 shared** | **94,880 Infura; 66,960 BlockPI** |

Variable allowances before retries (Infura serves only staging):

| Work | Ankr calls | Infura credits | BlockPI RU |
|---|---:|---:|---:|
| Deposit: 3 cold discovery + 9 Depth confirmation + 1 first ancestry check + 2 fresh valuation snapshots | 15 | 1,520 (19 methods) | 665 (19×35 upper bound) |
| Unresolved finality recheck, including at most one replacement candidate | 4 | 320 | 85 |
| L stock/turnover: 86 head methods/environment/day | 86 | 6,880 | 1,720 |
| Hint task | 12 | 640 | 280 |
| Fresh quote/sampler snapshot | 1 | 240 | 60 |
| Factory receipt fallback (identical matching dual logs add zero) | 3 | 240 | 70 |
| Safe challenge/proof set | 3 | 160 | 40 |
| Extra/recovery method, conservatively priced as logs | 1 | 255 | 50 |

Two valuation snapshots are reserved per D independently of Q (verify uses three methods per
snapshot). H is charged separately in full, even when its valid proof reduces pump work.
Normal cold confirmation remains the upper path: durable exact dual proofs skip completion;
shared per-endpoint immutable caches remove repeated timestamp/origin/receipt-position work.
No cache crosses endpoints or replaces a fresh canonical header. The first ancestry check
uses one fresh numbered inclusion header/endpoint; mismatch/error falls back to a full read,
at most four extra methods charged to recovery. S/L rechecks retain their separate stock and
turnover allocations below. Unexpected cache churn/repeated unverified completion also consumes
recovery; stop new intake if observed counters exceed the model.

Confirmation first probes the required head independently on both endpoints. Once both reach
the requirement, the same claim reads full evidence once per endpoint: receipt, canonical
header and transaction, at most three methods. Recheck the actual receipt's inclusion and
applicable confirmation policy before crediting. Waiting for heads does not repeatedly read
receipts. Normal logical bounds per endpoint are:

| Mode | Head probes | Full evidence methods | First later ancestry check | Normal total |
|---|---:|---:|---:|---:|
| Depth | 6 | 3 | 1 | 10 |
| Safe | 3 | 3 | 1 | 7 |
| Finalized | 7 | 3 | 0 | 10 |

Finalized confirmation records finality without a duplicate first watcher check. Depth sets a
fixed estimated-depth anchor `a` and probes immediately, then at `a + 4, 12, 28, 60, 124 s`.
Safe probes immediately and every 384 s, at most three probes in a 768 s window; Finalized
allows seven in a 2,304 s window. Missed slots are skipped, deadlines do not extend, and
normal/L together permit only one full confirmation-evidence read per endpoint. See the
[confirmation schedule](../docs/design/chain-reads.md#confirmation-waits-and-slow-lane-l).

### Immutable facts and proof reuse

Successful paths use these per-endpoint logical method counts; transport retries and
recovery work retain their separate reserves:

| Work | Methods per endpoint | Infura credits | BlockPI RU |
|---|---:|---:|---:|
| Complete a new raw transfer, cold immutable caches | 3 | 240 | 70 |
| Complete a raw transfer whose immutable facts are already cached | 0 | 0 | 0 |
| Complete an exact `(tx, block hash, block log index)` with active durable dual proof | 0 | 0 | 0 |
| Confirm a valid persisted hint proof, after current identity/policy checks | 1 fresh numbered header | 80 | 20 |
| First finality check of a dual-verified deposit at/below the checkpoint | 1 fresh numbered header | 80 | 20 |
| Verify identical dual factory logs matching stored insert-only facts | 0 extra | 0 | 0 |
| Factory receipt fallback, cold caches | 3 | 240 | 70 |

Log queries and their block-window splitting still run on both coverage endpoints; omitting
completion never establishes new coverage or uses a single-source absence. Fast discovery
uses the same exact durable-proof exclusions. New or changed block hashes or log indexes
are completed normally. Only active, dual-verified deposit identities qualify.

Each endpoint's `EvmClient` shares bounded caches across its fast, coverage, hint, pump and
finality readers: timestamp by block hash, signed transaction origin by transaction hash, and
receipt positions/OP-deposit origin by `(transaction hash, block hash)`. Independently constructed
clients never share facts. Full receipt reads still fetch the receipt and its current numbered
canonical header, even with warm caches; a cached signed origin can save its transaction call.
Header-by-number, canonical checks, confirmation heads and finalized regression guards are
never shared as cached facts.

Hints persist their independently decoded receipts and captured heads separately from terminal
proof. The pump prefers terminal evidence, then validates hint identity, current confirmation
policy and fresh matching dual inclusion headers. Depth/Safe evidence skips head/receipt
reconfirmation and never establishes `final_at`: persisted finalized horizons are zero and the
pump explicitly suppresses hint-sourced finality. Finalized-policy hints store no handoff proof;
the pump uses its normal fresh dual probe, boundary-conflict check and full receipt path. Identity,
inclusion or unresolved-state changes clear stored hint evidence in PostgreSQL; malformed,
reorged or otherwise unusable proof falls back to the ordinary full dual read.

The first finality ancestry shortcut applies only to dual-verified, not-yet-final deposits
outside unresolved stock S. Header errors or mismatches use full receipt evidence, including
replacement checks when needed; S/L schedules and their existing bounds remain unchanged.
An attempted ancestry check adds one header before fallback: at most `1 + 4 = 5` methods
for that first check, versus one on success. An invalid hint can similarly spend one header
before ordinary confirmation. Charge these extra methods to the monthly recovery reserve;
the normal deposit allowance includes a successful ancestry check. Subsequent S rechecks
remain bounded at four.
Factory omissions, conflicting logs or a mismatch with stored facts likewise require fresh
dual receipts. Stored insert-only contradictions still freeze the chain.

### Remaining variable allowances

Sanctions screening uses local OFAC/manual lists, with no RPC term; address derivation is local.
Staging's one PHA route (Sepolia) retains its five-minute TWAP sampler: 288 snapshots/day.
Quotes have two price-chain instances per environment: `288+2·30+2·600=1,548` snapshots/day.
Production has no periodic TWAP sampler. Observation-only staging mainnets perform no payment
discovery, checkpoints, coverage or custody; all staging verify traffic stays on Infura.

A deposit enters S at its first unresolved check, persisted as `first_unresolved_at`. This
includes a `detected` deposit whose transfer both endpoints agree is absent during confirmation,
without waiting for the checkpoint or the one-hour alert. The pump and finality watcher share
one persisted backoff anchored to that timestamp: every 60 s until ten minutes, every ten
minutes until six hours, then hourly, with exactly one reader per due time. The separate
one-hour `TopupDepositPendingAfterReorg` alert remains. S=1 bounds the sum across all payment
chains in each environment, two combined. At most S new unresolved entries may occur per
environment in any rolling 24 hours; resolved entries still count in that window. Budget
first-day cost for S arrivals plus later-day cost for S carried stock each day. This
stock/turnover allowance is separate from D and its initial finality allocation. Height lag
alone stays in normal confirmation or L and does not enter S. Missing transfers, conflicting
or changed evidence, and RPC failures that cannot establish height-only lag enter S.

For unresolved work, a missing receipt costs one method/endpoint; a present cold ordinary EVM
receipt needs receipt, canonical header and transaction (three methods). Cache savings never
remove the fresh receipt/header checks.
When both original receipts are missing, replacement lookup considers service-known candidates
with the same chain, sender and nonce but a different hash. **Exactly one candidate** permits
independent evidence reads for that candidate, at most three methods per endpoint. **More than
one candidate** permits no candidate RPC reads: raise an anomaly alert, make no reversal, and
keep the deposit unresolved in S for operator resolution. Zero candidates also makes no
candidate RPC reads. Never choose among multiple candidates, infer replacement from an account nonce, or
release reservations to bypass this gate. Follow the
[finality recovery runbook](runbooks/deposit-reversed.md#replacement-candidate-anomaly).

The branches are mutually exclusive, so the complete per-endpoint recheck bound is
`max(3, 1 + 3×K) = 4` methods at K=1: one original receipt plus the candidate's receipt,
header and transaction. Initial first-finality fallback uses the recovery allowance; later S
rechecks cost four Ankr calls, 320 Infura credits or 85 BlockPI RU before ×1.1.

| Stock age | Rechecks/deposit/endpoint/day | Ankr calls/deposit/day | Staging Infura credits/deposit/day |
|---|---:|---:|---:|
| First unresolved-day | 10 + 34 + 18 = 62 | 62×4 = 248 | 62×320 = 19,840 |
| Every later day | 24 | 24×4 = 96 | 24×320 = 7,680 |

The shared-read stock/turnover bound across both environments is (Infura serves only the
staging half; production BlockPI uses the RU weights above):

```text
Rechecks: 2 environments × (1 new × 62 + 1 carried × 24) = 172
Ankr:     172 × 4   = 688 calls
Infura:   86 × 320 = 27,520 staging credits
BlockPI:  86 × 85 = 7,310 production RU
```

This includes all allowed replacement-candidate reads and stock turnover; neither is charged
again to the extra-operation reserve. Manual anomaly investigation consumes that reserve.
More-than-one-candidate cases retain their stock slot and verification; the operator preserves
the timeline and canonical evidence and resolves the anomaly through reviewed, audited action.

L separately budgets first-day head probes for one new entry plus hourly probes for one carried
entry per environment. Its full evidence read is already in D's three-method allocation:

```text
L head methods/role: 2 environments × (1 new × 62 + 1 carried × 24) = 172
Ankr:          172 calls
Infura:        86 × 80 = 6,880 staging credits
BlockPI:       86 × 20 = 1,720 production RU
```

Refund verification uses three durable phases:

- **A, inclusion:** `eth_getTransactionReceipt` on read at 0, 1, 2, 5, 10 minutes, then
  every ten minutes through two hours, then hourly. Verify probes hourly from hour one so a
  lagging read cannot stall inclusion. After 24 hours of absence both probe every six hours.
  While the original receipt is absent, capture its sender/nonce on both endpoints at most
  three times, at 0, 10 minutes and one hour, stopping when both agree. These attempts are
  durable across restarts and preserve pending transactions before nodes can evict them.
- **B, checkpoint:** after observing inclusion at block b, no RPC until the finality watch's
  shared `FinalizedHeads` publication reaches b. The checkpoint loop already verifies its
  stored hash and freezes conflicts; refunds do not reread the checkpoint header.
- **C, evidence:** receipt and canonical header(b) independently on each endpoint, plus original
  transaction origin once. Identical finalized receipts at/below the checkpoint are required.
  Reorg-moved receipts return to A/B. Unresolved/disagreeing evidence retries after ten minutes,
  then hourly, then every six hours after 24 hours. No absence or nonce inference cancels or
  fails an attachment.

A typical already-included refund uses seven logical calls total: one read hint plus three
methods on each endpoint. Persistent absence on the half-open first day uses 37 read receipts
and 23 verify receipts (60 combined), plus at most three origin methods per endpoint
(66 combined calls when all three attempts remain unknown); unresolved finalized evidence with a cached origin uses
52 read / 51 verify calls. Allow **80 logical calls per endpoint on day one**, including
inclusion probes followed by unresolved evidence. Ordinary verification attempts the original
origin once in phase C if still unknown, in addition to the three bounded phase A attempts;
a merchant-supplied replacement proof can retry an unavailable original origin.
After day one, absence uses at most four receipt calls per endpoint/day, and unresolved evidence
uses eight receipt/header calls per endpoint/day; reserve **12 per carried attachment/day**.
A positive replacement proof is an operator-controlled recovery operation charged separately to
the extra-operation reserve: both origins must equal the original's sender/nonce, captured by
dual agreement or recovered locally from `original_signed_transaction` whose keccak256 matches
the attached hash. The signed-original fallback uses Alloy's EIP-2718 decoder/signature recovery
(legacy, EIP-2930/1559/4844/7702) and adds zero RPC methods. Raw bytes are not retained,
and both replacement receipts must be identical and finalized at/below the checkpoint. Missing
or disagreeing evidence keeps the reservation. One candidate is stored at a time, with at most
**eight durable recovery passes**, using the attachment's A/C backoff; checkpoint waiting
spends no pass and makes no calls. A pass adds at most one receipt hint, three replacement
receipt/header/origin methods, and one retry of an unavailable original origin per endpoint:
**at most 40 additional logical methods per endpoint/epoch (120 transport attempts at ×3)**.
At exhaustion automatic candidate reads stop. Before exhaustion or 24 hours, identical re-POSTs
are idempotent no-ops. After both, an explicit POST can renew the same hash or supply another;
the stored epoch timestamp allows at most one renewal per rolling 24 hours per attachment.
The worker never renews it. Conservatively reserve **80 additional logical methods per endpoint
in a 24-hour window overlapping the old and renewed epochs (240 transport attempts)**; each
renewal adds at most 40. Charge this to the extra-operation reserve before submitting the
initial or renewed candidate; original verification and its reservation continue.

At Infura's 80-credit method ceiling, refunds reserve 6,400 credits/new attachment and 960
per carried day; BlockPI conservatively uses 2,800 RU and 420 RU. Count N new plus all R pending
(including carried stock) each day. Combined N=11, R=11. Refunds reserve all three transport
attempts; non-refund work uses ×1.1. The exact daily equations are:

```text
Shared Ankr non-refund:
5,488 fixed + 1,548 prices + 15×(280+10) D + 12×(300+20) H
+ 3×(150+20) factory + 3×12 Safe + 300 extra + 380 openings
+ 688 S + 172 L + 300 monthly-recovery average = 17,612
Refunds: 3×(80×11 + 12×11) = 3,036
Total: ceil(17,612×1.1 + 3,036) = 22,410 calls/day

Staging-only Infura non-refund:
94,880 fixed + 240×348 prices + 1,520×10 D + 640×20 H
+ 240×20 factory + 160×2 Safe + 255×150 extra + 80×80 openings
+ 320×86 S + 80×86 L + 255×300 recovery = 367,070
Refunds: 3×(6,400×1 + 960×1) = 22,080
Total: 367,070×1.1 + 22,080 = 425,857 credits/day

Production-only BlockPI non-refund:
66,960 fixed + 60×1,200 prices + 665×280 D + 280×300 H
+ 70×150 factory + 40×10 Safe + 50×150 extra + 20×300 openings
+ 85×86 S + 20×86 L + 50×300 recovery = 457,590
Refunds: 3×(2,800×10 + 420×10) = 96,600
Total: 457,590×1.1 + 96,600 = 599,949 RU/day
```

Staging's share of shared Ankr is `ceil(3,592×1.1+276)=4,228` calls/day before the shared
monthly reserve: fixed 2,128; prices 348; D 150; H 240; factory 60; Safe 6; extra 150; openings
80; S 344; L 86. The shared recovery allowance adds 330 after ×1.1. Production and staging do
not receive separate Ankr quotas; BlockPI's two chain keys also share one account.

| Provider account | Modeled daily usage | Stop line/day | Usage of stop line | Headroom |
|---|---:|---:|---:|---:|
| Ankr (production + staging read) | 22,410 calls | 25,000 calls | **89.64%** | **10.36%** |
| Infura (staging verify) | 425,857 credits | 1,500,000 credits | **28.39%** | **71.61%** |
| BlockPI (production verify) | 599,949 RU | 1,250,000 RU | **48.00%** | **52.00%** |

At all other stated allowances, shared Ankr is `17,789.2 + 16.5·D_production`:
D=300 would cost 22,740/day (90.96%), missing ten-percent headroom. The mathematical ceiling
is D=285 (22,492, 89.97%); the operational cap rounds down to **D=280**.
**Production: D280/H300/Q600, refunds N10/R10, per-account R3, 12,000 addresses/chain.**
**Staging: D10/H20/Q30, refunds N1/R1, 1,000 addresses/chain.** Factory remains 150/20 and Safe
10/2. These are modeled allowances, not measured seven-day run rates. Watch the hot set,
issuance, monthly recovery ledger and provider dashboards; stop lines bind even within the
workload caps. See the [dated capacity check](RPC-PROVIDERS.md#capacity-check-2026-10-30).

Non-refund reads use ×1.1; refunds reserve all three transport attempts. Logical method counts
have hard bounds, while each method can make up to three physical sends. These totals do not
guarantee ten-percent headroom if all non-refund requests exhaust retries. Approved caveat, verbatim:

> 延续已批准的计费口径：非退款 ×1.1，退款预留全部三次 transport attempts。逻辑方法数有硬上界；每方法的物理发送上界仍为三倍。不能把上述总额描述成“所有非退款请求都耗尽重试时仍保留 10%”。

### Ignored transfer spam

Fast discovery uses the route token contracts as the `eth_getLogs` address filter and checks
amounts before completing a log. Unsupported tokens, zero amounts and transfers below a
conservative route floor bypass receipt, origin, timestamp and pricing RPC. Finalized hourly
coverage still reads **all** token contracts on both endpoints and requires exact agreement on
ignored raw logs before advancing its atomic per-address progress. Ordinary deposits retain
full dual receipt evidence and fresh price validation. The stablecoin floor also accounts for
maximum quote tolerance; see [merchant behavior](../docs/integration.md).
Transaction hints apply the same floor and historical ignored-group checks before header/origin
completion; explicit hint polling still consumes its existing bounded H allowance.

Ignored logs add **zero per-log completion or price methods** to either provider. They still
increase `eth_getLogs` response bytes and database work; BlockPI's response-size surcharge and
provider result-size limits remain relevant. The `16.5 × D` marginal Ankr model above applies to
normal deposited payments, not ignored spam; this change does not raise D/H/Q or other caps.

Coverage keeps immutable exact amount/count aggregates per address/token/block, including fake
tokens that might be routed later. Custody includes these totals from `created_block` through its
pin in addition to the immutable opening proof. Reissue/restore replay never promotes already
ignored groups to deposits or counts their amounts twice. The financial seven-year retention
policy applies to totals: never delete accounting still needed by custody without an approved
archival baseline. Distinct fake tokens × watched addresses × blocks can still grow this table.
Diagnostic raw samples are capped at 20 per address/token/UTC observation day; samples and daily
counters older than 30 days are removed in bounded batches (up to 1,000 each coverage round).
Counters use first observation time, so historical backfill counts on the day it is read.
This is a declared restore-only schema boundary: **no rollback to 0.13.4; restore required**.
`COMPATIBILITY_FLOOR=20261111000001` makes v0.13.4's required Compose migration step reject the
unknown migration before service startup. A directly launched old binary has no startup schema
gate. Its custody query cannot include ignored totals; bypassing the deployment gate would
freeze that chain on a nonzero routed dust mismatch and stop crediting.
Never bypass the gate or force-unfreeze such a mismatch. Before upgrade, preserve and verify a
pre-upgrade backup, image/config and merchant delivered-event exports, and exercise the
[restore procedure](RESTORE.md). Recovery restores the pre-upgrade database with its matching
image/config, reconciles post-backup delivered events and reissues addresses as required, then
runs the documented operator checks/unfreeze. Prefer a forward fix using the accounting-aware
binary. The down migration preserves evidence; it does not make an old binary compatible.

`TopupIgnoredTransferVolume` warns when any address on a chain reaches 100 ignored events in a
UTC observation day. It groups only by chain, with a count field, and resolves when that day's
maximum falls below the threshold. See the [response runbook](runbooks/rpc-health.md#ignored-transfer-volume).

### Slow confirmation lane L

L receives a deposit only when its normal confirmation window ran out solely because a required
head was behind. Unequal head heights alone are not evidence disagreement. L is separate from
S: height-only waiting does not set `first_unresolved_at` or consume S capacity. A missing
transfer, conflicting or changed receipt evidence, or an RPC failure that prevents establishing
height-only lag goes to S; a stored-checkpoint hash conflict freezes the chain.

L uses the persisted `first_slow_at` age: 60 s until ten minutes, ten minutes until six hours,
then hourly. The entry probe was already charged in the normal window; the first L probe is
60 s later. Each due time reads only the corresponding head once per endpoint. As soon as both
reach the requirement, read the one allowed full evidence set and continue confirmation and
credit without waiting for the published checkpoint. A valid Depth/Safe hint proof may serve confirmation after a fresh dual inclusion-header check;
it never establishes final_at. Finalized policy keeps the full read. A failed or anomalous full read enters S and never restarts the normal window.

Current L stock and rolling-24-hour first entries are each limited to one, summed across all
payment chains per environment. `first_slow_at` is set once and retained; entries still count
after leaving L. The DB-derived gauges are `topup_confirmation_slow{chain_id}` and
`topup_confirmation_slow_entries_24h{chain_id}`. Current stock excludes deposits that entered S,
confirmed or terminated. Scope selectors to one environment before summing:

- L remaining non-empty for ten minutes triggers the provider-lag alert.
- `sum(topup_confirmation_slow) > 1` or `sum(topup_confirmation_slow_entries_24h) > 1`
  triggers the independent L capacity alert. Pause new quotes on all routes of the affected
  chain and coordinate merchant intake. Existing funded work continues verification; keep its
  reservations and excess records.

Do not treat L as S or use that separation to allow unbounded load. L limits are operational;
resume new quotes only when L inventory, its rolling entries, S and the daily work all fit.
Normal, L and S share the persisted chain-read due time and atomic lease claim. Consume the
probe/evidence allowance and advance due time before RPC; failed or crashed reads do not refund
it. RPC runs outside the transaction and chain lock; evidence commits check the lease token
and record version. One logical reader owns each due time. Restarts, missed ticks, pump/watcher
handoffs and price retries never reopen the normal window or evidence allowance. A deposit that
has not entered S may reuse its complete persisted terminal proof for valuation/price retries.
Entering S invalidates pre-entry proof: the watcher must acquire fresh dual terminal evidence
created at or after `first_unresolved_at`, even if an old `final_at` exists. The transfer identity
(`to`, `token`, `from`, `amount`, `tx_from`, `tx_nonce`) must match the deposit; otherwise follow
the S or reversal path. The exact terminal transition establishing the current final marker is
linked by `confirmation_terminal_transition_id`; only its versioned, complete dual proof may
then serve price-only retries. Provisional reappearance stays in S on the watcher schedule.
Non-final evidence retains watcher eligibility for finality verification.
N-1 data compatibility preserves history but does not make its binary obey these new budgets.

### Monitoring and stop actions

D, factory and Safe limits remain operational rather than admission gates. The service persists
UTC-day RPC attempts by configured endpoint and method, including batch items and retries,
and derives D from newly inserted deposit rows (including rejected deposits). Reprocessing a
row does not count it again; a backfilled transfer counts on its observation day, not block day.
The once-per-minute business monitor generates `TopupRpcQuotaPressure` at ceil(80% of each
configured `quota_alerts.rpc[].stop_line`), critical at the stop line, and
`TopupDepositDailyLimit` at `quota_alerts.deposits_per_day`. These are managed through the
existing admin alerts API; no collector or new delivery setup is required.

Ankr's local allocations are **20,442 production + 4,558 staging = 25,000 calls/day**. Staging
gets its modeled 4,228 plus 330 for shared monthly recovery; production gets the remainder.
The 80% warning boundaries are 16,354 and 3,647 calls respectively. These allocations do not
create separate provider quotas. BlockPI is 1,250,000 RU/day and Infura 1,500,000 credits/day,
warning at 1,000,000 RU and 1,200,000 credits. Configure exact endpoint ids, including staging's
mainnet price endpoints; a provider label is never inferred from a URL.

RPC packets synchronously update in-memory counters without database I/O. A background worker
flushes one batched upsert every 30 seconds on a separate two-connection pool, with a one-second
deadline and one final shutdown flush. Monotonic epoch counters make retries idempotent even
after a lost write acknowledgement. Failed flushes retain pending counts for retry:
`topup_rpc_accounting_failures_total` increments and `TopupRpcAccountingFailed` warns that the
day's persisted usage may be incomplete. RPC never waits for accounting persistence. With a
healthy database, a crash can lose the last flush interval; outages extend that window.
External CLI/preflight calls, another service and BlockPI response-size surcharges can also
undercount billed usage. The provider dashboard remains
authoritative for combined monthly usage. Counters start at upgrade; reconcile the first day's
pre-upgrade calls manually. Restarts preserve committed daily tallies and UTC rollover starts a
new day. A read-only restore process does not write these monitoring tables.

Older binaries reject `quota_alerts`; use the target release's retained configuration and kit
for rollback. The drill strips this key when the target configuration lacks it. Disabled quota
configuration is omitted from resolved/attested output for compatibility.

Maintain manual daily tallies of factory verification instances and Safe attempts, including
recovery work.
Reconcile reports, events/audit records and application evidence with billed RPC counters;
distinct factory transaction counts alone omit re-verification. Budget each planned test/proof
batch before starting it. If a tally cannot be established, stop scheduling that load until
the operator reconciles the evidence.

Monitor stock from the first unresolved check, including a `detected` deposit whose transfer
both endpoints agree is absent during confirmation, before the checkpoint. The pump and watcher
share `first_unresolved_at` and the persisted backoff, with exactly one reader per due time.
Sum all payment chains per environment, independently of the one-hour age alert. The service exposes
`topup_finality_unresolved` (gauge, per chain) and
`topup_finality_unresolved_entries_24h` (DB-derived gauge, per chain). The latter counts
deposits whose persisted `first_unresolved_at` is within the last 24 hours, including resolved
ones. Configure three separate alerts:

| Alert | Condition within one environment |
|---|---|
| Current unresolved stock | `sum(topup_finality_unresolved) > 1` |
| New unresolved entries in rolling 24 h | `sum(topup_finality_unresolved_entries_24h) > 1` |
| Unresolved age | Existing one-hour `TopupDepositPendingAfterReorg` alert |

Evaluate the stock and entry expressions separately for each environment, summing across its
chains. When a Prometheus receives both environments, restrict the selectors using the
deployment's scrape labels before summing. Use the DB-derived gauge directly for the exact
rolling count. Resolved deposits remain counted until their `first_unresolved_at` leaves the
24-hour window; rechecks do not count as new entries.

**Above either S=1 current stock or one new entry in rolling 24 h, pause new quotes on all
routes of the affected chain and escalate.** Do not wait for the one-hour age alert. Multiple
replacement candidates raise the anomaly alert and remain in the stock count. Verification,
existing credit and reservations continue;
never manufacture a finality/reversal verdict or release exposure to reduce the tally.

At an operational limit, stop merchant onboarding and new payment work; stop new staging
payments, factory/proof batches and nonessential manual RPC operations as applicable.
Use audited account/route `quotes` pauses from the
[operator controls](runbooks/README.md#environment) when needed to hold new issuance, and
coordinate merchants' payment intake. Persistent addresses can still receive transfers, so a
pause alone cannot enforce D. Keep observing and resolving funded work.
Keep existing reservations and evidence requirements intact. If existing funded work would
exceed the allocation, escalate for a reviewed quota/load plan instead of skipping verification.
Resume new work only when its next budget window and pending inventory fit. Extra reserve
does not authorize raising D, factory or Safe caps.

Observe `topup_rpc_calls_total`, `topup_rpc_endpoint_ready`, `topup_rpc_errors_total`,
`topup_coverage_lag_seconds`, `topup_addresses_lagging`, `topup_daily_budget_used` and
`topup_daily_budget_limit`. Exhaustion alerts compare usage with the configured H/Q limits,
matching all deployment scrape labels as well as the budget name.
Import [rpc-alerts.yaml](rpc-alerts.yaml) and compare both environments' counters with provider
dashboards. Stop adding load at shared Ankr's 25,000/day seven-day run rate, staging-only
Infura's 1,500,000 credits/day, or production-only BlockPI's 1,250,000 RU/day seven-day run rate.
BlockPI's counter estimate uses the RU table (unknown methods cost 35) and excludes payload
surcharges; its account dashboard is authoritative. Its free rate limits are 20 requests/s
and 400 RU/s per endpoint; at 50 RU/log, at most eight log requests/s fit. Run preflights
serially, count bursts against the reserve, and investigate retry/range growth and 429s before
adding load. Alerts do not authorize exceeding stop lines or adding another free key.

At hard caps, the admission/budget paths refuse new refund attachments, hint tasks, price
snapshots or address issuance. Hints fall back to scanning; quotes needing exhausted price
budgets return retryable `price_unavailable`. Infura 402 halts verify until UTC midnight;
Ankr exhaustion makes read unavailable until its quota resets. BlockPI exhaustion or persistent
throughput errors pause production verification until its account recovers. Preserve pending
work and let bounded loops recover; never advance cursors or release reservations manually.

### Latency and recovery targets

These are scheduling targets for healthy endpoints, a caught-up chain and work within pilot
allowances; RPC/worker processing time is additional. Outages, backlog and address backfill
can extend them. Time measured from inclusion also includes chain confirmation/finality delay.

| Path | Target |
|---|---|
| Manual transfer without a hint | Discovery in 0–5 min, average about 2.5 min, then confirmation and processing |
| Checkout with an admitted hint | Instant processing path: credit in seconds at route confirmation, independent of scan ticks |
| Depth confirmation | About 4 s after expected depth when healthy; under lag, up to the current probe interval (maximum 64 s in the normal window), then L |
| Safe / Finalized confirmation | One 384 s epoch interval after the corresponding head satisfies the requirement, within the normal window |
| L confirmation after heads recover | Current L interval: 60 s / ten minutes / one hour by age, then RPC and processing; no checkpoint wait |
| Quote expiry, cancel completion and unpaid reservation release | ≤ about 70 min after finality: checkpoint ≤10 min + coverage ≤60 min + expiry worker ≤5 s |
| Known-deposit reversal and checkpoint-conflict freeze | ≤10 min checkpoint scheduling delay, then watch/RPC processing |
| Custody discrepancy | ≤ about 430 min (7 h 10 min) after finality, plus sweep runtime: checkpoint ≤10 min + coverage ≤60 min + custody ≤360 min at the default six-hour cadence; healthy endpoints, verified openings and settled, caught-up forwarders |

Hints require ready endpoints, available hint budget and the route's confirmation requirement.
A conflict visible only in full logs may wait for hourly coverage; the ten-minute target concerns
checkpoint hash conflicts and newly due deposit evidence. Already unresolved deposits follow
their age-dependent recheck schedule; later-day evidence can wait up to an hour for its next
check, in addition to checkpoint and RPC processing. Refund completion keeps ten-minute
checkpoints plus its existing age-dependent verification interval. Hints and fast cursors never
establish negative evidence.

Payment windows stay unchanged. In-window qualifying payments retain quote terms when found
later. Late, partial and persistent-address payments use fresh processing-time prices, so delay
can change spot credit and price-policy checks. Screening uses verified local lists at decision
time; its hourly refresh and re-screening remain independent.

Catch-up capacity per forward or lagging recipient chunk while its own backlog exceeds 3,000:

```text
Per successful round: 19,200 blocks
Per six hours:       6 × 19,200 = 115,200 blocks
Per day:             24 × 19,200 = 460,800 blocks
Net Ethereum/Sepolia recovery: 460,800 − 7,200  = 453,600 blocks/day
Net Base/Base Sepolia recovery: 460,800 − 43,200 = 417,600 blocks/day
```

A 24-hour backlog needs about one Ethereum round or three Base rounds, plus blocks arriving
during recovery, rather than waiting for every sixth round. The existing target of at most
24 h of equivalent backlog within **12 h of continuous healthy operation** remains conditional
on D/factory allowances and the extra-operation reserve. Log RPCs for B backlog blocks cost
`ceil(B / max_log_blocks) × (recipient chunks + factory ranges)` per endpoint, with additional
calls for lagging-address history and per-log evidence. Do not charge the maximum catch-up
window as daily steady state or assume recovery fits the reserve without measuring it.
Restarts retain the stored lag and do not reset catch-up scheduling. The two-hour coverage-lag
warning remains an early incident signal.

Changes to covered address rows or covered unverified-deposit identities discard evidence and
retry once; unrelated address insertions retain the covered rows' proof. Coverage commits only
after every request succeeds. The locked commit recomputes the common complete boundary across
all addresses; if a lowered boundary lacks a prefetched dual timestamp, compatibility time is
NULL until a later round fills it. Expiry and unpaid reservation release wait for that time.
Coverage/cleanup end at the scanned boundary even when the checkpoint is ahead. Never replace
coverage-gated release with wall-clock expiry.

Staging PHA samples every 300 seconds, with `max_sample_age_s: 900` and
`max_sample_jump_bps: 1100`. Defaults remain 180 seconds and 500 bps for 60-second sampling.
Persisted TWAP hashes are proved in the same multicall; a baseline outside EVM BLOCKHASH
history fails closed rather than using unproved history.

## Finality conflicts and rollback

A changed checkpoint hash or a progressed, unverified deposit contradicting agreed canonical
evidence freezes the chain through `reconciliation_blocks`. Use the audited
[chain freeze procedure](runbooks/chain-frozen.md); do not edit hashes or remove records.
A missing receipt is not replacement proof. Reversal needs a finalized receipt without the
transfer, or exactly one service-known same-chain/same-sender/same-nonce replacement candidate
agreed finalized by both providers. Multiple candidates trigger an anomaly alert with no candidate
reads or reversal; the deposit stays unresolved in S for operator resolution.
No account nonce query is proof; EIP-7702 authorizations can increment it.

The migration is expand-only. N keeps N-1's cursor/address compatibility fields and writes no
legacy RPC table. N-1's frozen/anchor/recovery state must be resolved before starting N.
Preserve its verified image and config. The published-image rollback drill covers N-1 → N → N-1 → N,
preserving money state, confirmation counters, deadlines and entry history. Data compatibility
does not make the old binary obey the new RPC bounds; use its own operating allocation.
A production rollback pauses all screening-dependent processing until N's verified screening
is restored; database compatibility does not authorize settlement. See
[sanctions rollback](runbooks/sanctions-list.md#n-1-rollback).
