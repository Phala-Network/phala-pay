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
All pilot caps remain unchanged; a later increase needs a separately reviewed budget
([launch procedure](runbooks/production-launch.md#18-rpc-accounts-and-production-allocation)).

`topup.yaml` also requires the top-level positive integer `max_attached_pending_refunds`:
production sets it to 2 and staging to 1. It atomically limits attached-pending refunds across
all accounts and modes in that environment; one new attachment per rolling 24 hours remains
a fixed admission limit. Missing, zero or negative values fail configuration validation.

CI resolves the latest stable release, currently v0.10.0, as N-1 for the next release's rollback
rehearsal. v0.10.0 supports `rpc`, `max_attached_pending_refunds`, and the current R/N admission
limits. Retain and use its verified configuration, kit and image digests when rolling back from
a later compatible release; keep operational inventory and load controls in force.

v0.10.0 is production's first release, so this launch has no earlier production rollback target.
v0.9.2 cannot parse the current configuration, lacks the environment-wide R/N admission limits,
and screens with the stale oracle. Never deploy it to production. See
[launch rollback](runbooks/production-launch.md#12-rollback) and
[rollback compatibility](README.md#rollback-compatibility).

```sh
topup config check --secrets /etc/topup/topup.yaml
topup rpc check --config /etc/topup/topup.yaml
```

`rpc check` independently validates both endpoints with typed identity, finalized transaction,
receipt and log reads, the real coverage queries with 1,000 recipients, canonical state calls,
and state at
twice the observed finalized lag. Contract identity and the preflight boundary pin both
endpoints at the lower finalized height and require matching header hash and timestamp.
Receipt-log comparison ignores only optional `blockTimestamp` metadata. Deploy uses the
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

Read-only discovery runs every five minutes on every payment chain. Independent dual checkpoint
checks run every ten minutes and publish agreed advances for finality, reversal and refunds.
Full dual log coverage runs hourly: at most 3,000 blocks normally and 19,200 every sixth round,
now every six hours. Address history is backfilled separately in chunks of at most 1,000 addresses.
Observation-only price chains retain a separate 60-second contract-recovery loop; it skips
RPC when both endpoints' contracts are ready and does not perform payment discovery or coverage.
Candidates remain provisional until both endpoints agree on receipt, transaction, inclusion,
log contents and position, block time, sender and nonce.

Custody checks every chain/token route on the first reconciliation tick and hourly thereafter,
at the same canonical hash on both endpoints, no later than complete coverage. A separate
hourly wake-up keeps custody due when the checkpoint stalls. Manual `reconcile`, post-restore
checks and restarts run custody immediately and consume the extra-operation reserve.

### Pilot limits and operating modes

The approved parallel mode runs normal staging tests, including refunds, alongside production.
Both environments use Ankr read; production uses BlockPI verify, staging uses Infura verify.

| Limit | Production | Staging | Enforcement |
|---|---:|---:|---|
| Deposits handled/day, D | 46 | 20 | Operational |
| Concurrent attached pending refunds, R | 2 | 1 | Atomic attachment admission |
| New refund attachments/rolling 24 h, N | 1 | 1 | Atomic attachment admission |
| Unresolved finality stock/environment, S | 1 | 1 | Operational; pause affected-chain quotes and escalate above S |
| New unresolved entries/environment/rolling 24 h | 1 | 1 | Operational; same stock stop rule |
| Current slow-confirmation stock, L/environment | 1 | 1 | Operational; pause affected-chain quotes above the limit |
| New slow-confirmation entries/environment/rolling 24 h | 1 | 1 | Operational; same L capacity stop rule |
| Hint tasks/environment/UTC day, H | 80 | 80 | Atomic daily budget |
| Fresh quote snapshots/price chain/environment/UTC day, Q | 60 | 60 | Atomic daily budget |
| Factory receipt-verification instances/day | 60 | 20 | Operational |
| Safe challenge/proof sets/day | 10 | 2 | Operational |
| Extra RPC calls/endpoint/day | 150 | 150 | Operational reserve |
| Historical issued addresses/payment chain | 1,000 | 1,000 | Permanent issuance cap |

D includes recovery work, not just payments made that day. Factory counts include historical
re-verification, not only distinct transactions or newly committed events. Safe counts include
failed challenge/proof attempts. Refund admission is environment-wide across accounts and payment
chains; reattaching the same transaction is idempotent. An attached refund retains its reservation
and continues verification until finalized evidence resolves it; never cancel it to make room.

Production-only operation uses the same production caps and leaves additional provider headroom.
During recovery or maintenance, count preflights, restarts, manual reconciliation, restore checks
and extra range requests against the 150-call reserve on each endpoint in each environment.
Pause new tests and onboarding while backlog consumes operational allowances. New routes, pools, samplers or larger
caps require a reviewed budget before adding load.

### Worst-case pilot budget

Stop lines are **25,000 Ankr calls/day** (75% of 1,000,000 monthly calls divided by 30),
**1,500,000 staging-only Infura credits/day** (50% of the daily quota), and
**1,250,000 production-only BlockPI RU/day** (75% of 50M monthly RU divided by 30).
Charge one Ankr call per method, 80 Infura credits per non-log method and 255 per `eth_getLogs`.
BlockPI weights are below. Assume every payment chain runs all day, one caught-up recipient chunk plus one lagging chunk, and full custody vectors on all
1,000 historical addresses. No idle savings are used.

An hour adds about 300 blocks on Ethereum/Sepolia or 1,800 on Base/Base Sepolia, within the
configured 3,000-block log window. Coverage uses one address-less Transfer query per
1,000-recipient chunk and one factory-address query with three event signatures, filtered
locally to the tracked forwarders. This avoids Ankr's multi-topic alternatives limit.
At `max_log_blocks: 3000`, a normal range needs two requests per recipient set; 19,200 blocks
need fourteen. BlockPI uses `max_log_blocks: 1000` beneath its measured 1,024-block cap: budget
three windows for the full 3,000-block normal allowance and twenty for catch-up, even when the
chain is caught up and the actual normal range is shorter. An Infura
10,000-result limit error aborts the round: the current adapter does not automatically split
on that error. Extra requests must fit the reserve or
be budgeted separately.

For each payment chain with C custody routes, the shared Ankr and staging-only Infura costs are:

```text
Fast discovery:      288 × (1 head + 1 logs) = 576 Ankr
Checkpoint headers: 144 × 2 = 288 Ankr; 144 × 3 × 80 = 34,560 Infura
Coverage headers:   24 × 2 = 48 calls per endpoint
Coverage logs:      20 × 2 × 2 + 4 × 2 × 2 × ceil(19,200 / 3,000) = 192 per endpoint
Hourly custody:     C × 24 × ceil(1,000 / 200) = 120C per endpoint

Ankr:   576 + 288 + 48 + 192 + 120C = 1,104 + 120C
Infura: (432 + 48)×80 + 192×255 + 120C×80 = 87,360 + 9,600C
```

Checkpoint checks include both previous-checkpoint headers, both finalized heads and verify's
header at read's finalized height. Coverage uses the published checkpoint without repeating
those checks. Its header term includes the extra common-boundary read when that boundary lags.

BlockPI's [EVM RU table](https://docs.blockpi.io/pricing/request-unit-ru/evm-ru-table), checked
2026-10-13, charges `eth_chainId` 5; `eth_blockNumber` 10; `eth_getCode` and
`eth_getTransactionReceipt` 15; both block-header methods and `eth_call` 20;
`eth_getTransactionByHash` 35; `eth_getLogs` 50 RU. The
[request-unit policy](https://docs.blockpi.io/pricing/request-unit-ru) adds log response-size
charges above 200 KB. The budget assumes no such surcharge; reconcile counters with the
account dashboard and charge any excess to the reserve before adding load.

```text
BlockPI coverage logs/payment chain: 20×2×2×ceil(3,000/1,000)
                                  + 4×2×2×ceil(19,200/1,000) = 560
BlockPI fixed RU/payment chain: (432 + 48)×20 + 560×50 + 120C×20 = 37,600 + 2,400C
```

| Payment chain | Custody routes | Fixed Ankr calls/day | Fixed verify units/day |
|---|---:|---:|---:|
| Staging Sepolia | 3 | 1,464 | 116,160 Infura credits |
| Staging Base Sepolia | 3 | 1,464 | 116,160 Infura credits |
| Production Ethereum | 2 | 1,344 | 42,400 BlockPI RU |
| Production Base | 1 | 1,224 | 40,000 BlockPI RU |
| **Per account** | **9** | **5,496 shared** | **232,320 Infura; 82,400 BlockPI** |

Variable allowances before retries (the Infura column applies only to staging):

| Work | Ankr calls | Infura credits |
|---|---:|---:|
| Deposit: cold discovery 3, confirmation and first finality verification 13, coverage completion/re-verification 6, price snapshots 2 | 24 | 2,000 |
| Unresolved finality recheck, including at most one replacement candidate | 4 | 320 |
| L stock/turnover: 86 head methods per environment/day | 86 | 6,880 |
| Hint task | 12 | 640 |
| Price snapshot, including verify's head and pin header | 1 | 240 |
| Factory receipt verification | 3 | 240 |
| Safe challenge/proof set | 3 | 160 |
| Extra operation, conservatively priced as logs | 1 | 255 |

BlockPI variable allowances use the same pilot work limits, priced by its RU table. A complete
receipt/header/transaction set costs 70 RU; an unresolved replacement check costs at most
15 + 70 = 85; a refund check costs 15 + 35 + 2×20 = 90. Price snapshots cost 3×20 = 60
on verify. For the existing 2,000-credit deposit allowance and 640-credit hint allowance,
conservatively charge all non-log methods at the highest used rate, 35 RU: **875/deposit**
and **280/hint**. These are upper bounds, not a blended average. Each environment has 86 S
rechecks and 86 L head methods. Factory verification costs 70 and a two-call Safe proof 40 RU.
Extra operations reserve 50 RU each. Lower actual method weights do not authorize raising caps.

Confirmation first probes the required head independently on both endpoints. Once both reach
the requirement, the same claim reads full evidence once per endpoint: receipt, canonical
header and transaction, at most three methods. Recheck the actual receipt's inclusion and
applicable confirmation policy before crediting. Waiting for heads does not repeatedly read
receipts. Normal logical bounds per endpoint are:

| Mode | Head probes | Full evidence methods | First later finality verification | Total |
|---|---:|---:|---:|---:|
| Depth | 6 | 3 | 4 | 13 |
| Safe | 3 | 3 | 4 | 10 |
| Finalized | 7 | 3 | 0 | 10 |

Finalized confirmation records finality without a duplicate first watcher check. Depth sets a
fixed estimated-depth anchor `a` and probes immediately, then at `a + 4, 12, 28, 60, 124 s`.
Safe probes immediately and every 384 s, at most three probes in a 768 s window; Finalized
allows seven in a 2,304 s window. Missed slots are skipped, deadlines do not extend, and
normal/L together permit only one full confirmation-evidence read per endpoint. See the
[confirmation schedule](../docs/design/chain-reads.md#confirmation-waits-and-slow-lane-l).

Sanctions screening uses local OFAC/manual lists, with no RPC term; address derivation is local.
Staging's two PHA routes share one TWAP pool and five-minute sampler: `86,400 / 300 = 288`
snapshots/day. Quotes have four price-chain/environment instances, so `288 + 4Q = 528` snapshots.
Production has no periodic TWAP sampler in this pilot.

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

`ChainReader::finality_evidence` uses the stored checkpoint and calls `receipt_transfer`.
A missing receipt costs one `eth_getTransactionReceipt` per endpoint (1 Ankr call,
80 staging Infura credits or 15 production BlockPI RU).
A present ordinary EVM receipt also requires its block header and transaction: three methods
per endpoint (3 Ankr calls, 240 staging Infura credits or 70 production BlockPI RU), even if
the evidence disagrees and remains unresolved.

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
header and transaction. D includes four first-finality methods in its worst-case Depth
allocation: 24 Ankr calls plus 2,000 staging Infura credits or 875 conservative production
BlockPI RU per deposit in total. Charge four Ankr calls, 320 staging Infura credits or
85 production BlockPI RU per subsequent recheck, before ×1.1:

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

The refund receipt reader uses `eth_getTransactionReceipt`, the persisted-checkpoint header and
the receipt-height header independently on both endpoints: at most three methods per endpoint.
Missing receipts use one method; included receipts above the checkpoint use two; finalized
receipts use three. The checkpoint is loaded from the DB; there is no finalized-tag RPC.
The worker's pending/missing branches additionally call `eth_getTransactionByHash` on both
endpoints until an agreed sender/nonce is persisted. Asymmetric evidence can therefore cost
four methods per endpoint per check; a never-seen transaction costs two.

Checks run every 60 s for the first 30 min after attachment, every ten minutes until 24 h, then
hourly indefinitely. The Infura figures below apply only to staging; production uses the
90-RU BlockPI bound per check above. Before transport retries, the first half-open refund-day
has at most
`30 + 141 = 171` checks: `171×4 = 684` Ankr / 54,720 Infura. A full ten-minute-phase day costs
`144×4 = 576` / 46,080; an hourly-phase day costs `24×4 = 96` / 7,680. A never-seen transaction
uses `171×2 = 342` / 27,360 on its first day and `24×2 = 48` / 3,840 per hourly-phase day.
Reserve first-day cost for N new attachments plus later-day cost for all R pending attachments,
including carry-over. Combined N=2, R=3. Refunds reserve all three allowed transport attempts;
non-refund work uses the approved ten-percent allowance:

```text
Shared Ankr non-refund:
5,496 + 528 + 24×(46+20) + 2×12×80 + 3×80 + 3×12 + 300 + 688 + 172 = 10,964
Shared Ankr refunds: 3×(684×2 + 96×3) = 4,968
Shared Ankr total:   ceil(10,964×1.1 + 4,968) = 17,029 calls/day

Staging-only Infura non-refund:
232,320 + 240×408 + 2,000×20 + 640×80 + 240×20 + 160×2
+ 255×150 + 320×86 + 80×86 = 499,210
Staging-only Infura refunds: 3×(54,720×1 + 7,680×1) = 187,200
Staging-only Infura total:   499,210×1.1 + 187,200 = 736,331 credits/day

Production-only BlockPI non-refund:
82,400 + 60×120 + 875×46 + 280×80 + 70×60 + 40×10
+ 50×150 + 85×86 + 20×86 = 173,380
Production-only BlockPI refunds: 3×(171×90×1 + 24×90×2) = 59,130
Production-only BlockPI total:   173,380×1.1 + 59,130 = 249,848 RU/day
```

Staging's two mainnet price chains do not run payment discovery, checkpoints, coverage or
custody. They contribute quote snapshots and the PHA sampler: staging uses 288 + 2Q = 408
snapshots/day; production uses 2Q = 120. All of staging's verify traffic stays on Infura.
BlockPI's two chain keys share one account quota; they do not each receive 50M RU.

| Provider account | Roles | Modeled daily usage | Stop line/day | Headroom below stop | D permitted with ≥10% headroom |
|---|---|---:|---:|---:|---|
| Ankr | Production + staging read | 17,029 calls | 25,000 calls | 31.88% | Production ≤253 with staging D=20 |
| Infura | Staging verify only | 736,331 credits | 1,500,000 credits | 50.91% | Staging ≤298; no production D constraint |
| BlockPI | Production verify only | 249,848 RU | 1,250,000 RU | 80.01% | Production ≤955 under conservative method weights |

Holding every other allowance fixed, the equations before rounding are:

```text
Ankr(D production, staging 20) = 15,814 + 26.4D;  ≤ 0.9×25,000       → D≤253
Infura(D staging)             = 692,331 + 2,200D; ≤ 0.9×1,500,000    → D≤298
BlockPI(D production)         = 205,573 + 962.5D; ≤ 0.9×1,250,000    → D≤955
```

The shared Ankr account is the production capacity bound. BlockPI's D ceiling uses conservative
35-RU non-log bounds for deposits and hints; actual billing may be lower.
**Configured production D=46, staging D=20 and all other caps remain unchanged.** Increasing
any cap is a separate reviewed PR. The launch gate is the production Ankr/BlockPI allocation
and complete sealed mappings, with passing preflight before
[production launch §2](runbooks/production-launch.md#2-provision-the-topup-cvm).
Staging continues using Ankr + Infura.

Non-refund reads use ×1.1; refunds reserve all three transport attempts. Logical method counts
have hard bounds, while each method can make up to three physical sends. These totals do not
guarantee ten-percent headroom if all non-refund requests exhaust retries. Approved caveat, verbatim:

> 延续已批准的计费口径：非退款 ×1.1，退款预留全部三次 transport attempts。逻辑方法数有硬上界；每方法的物理发送上界仍为三倍。不能把上述总额描述成“所有非退款请求都耗尽重试时仍保留 10%”。

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
credit without waiting for the published checkpoint. Discovery/hint evidence does not substitute
for that read. A failed or anomalous full read enters S and never restarts the normal window.

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

D, factory and Safe limits have no code enforcement. Maintain daily tallies per environment of
deposits handled, factory verification instances and Safe attempts, including recovery work.
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
`topup_coverage_lag_seconds`, `topup_addresses_lagging` and `topup_daily_budget_used`.
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
| Custody discrepancy | ≤ about 130 min after finality: checkpoint ≤10 min + coverage ≤60 min + hourly custody ≤60 min |

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

Catch-up capacity per caught-up or lagging recipient chunk:

```text
Per six hours: 5×3,000 + 19,200 = 34,200 blocks
Per day:      20×3,000 + 4×19,200 = 136,800 blocks
Net Ethereum/Sepolia recovery: 136,800 − 7,200  = 129,600 blocks/day
Net Base/Base Sepolia recovery: 136,800 − 43,200 = 93,600 blocks/day
Base net recovery in 12 h: 2×(34,200 − 6×1,800) = 46,800 blocks
```

Target recovery of at most 24 h of equivalent backlog within **12 h of continuous healthy
operation** after both endpoints are ready, to the current published checkpoint. Base's
46,800-block net capacity covers a 43,200-block day. Recovery work must fit D/factory allowances
and extra requests must fit the reserve. Restarts reset the in-process round counter; repeated
restarts, failures or larger backlog require a revised estimate. The two-hour coverage-lag
warning is an early incident signal, not a replacement for the twelve-hour recovery target.

A changed address/unverified-id snapshot abandons cached evidence and retries once with a fresh
RPC round; a second change waits for the next tick. Coverage commits only after every request
succeeds. Coverage and pending cleanup end at the scanned block even when the checkpoint is
ahead; the compatibility cursor uses the common complete boundary across addresses. Never
replace coverage-gated release with wall-clock expiry.

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
