# Chain RPC operations

Each configured chain has an Ankr read endpoint and an independent Infura verify endpoint.
[Chain reads](../docs/design/chain-reads.md) specifies the evidence and read paths.
No endpoint failover, provider pool, acceptance state or RPC recovery command remains.

## Configuration and preflight

Configure `rpc` entries with `chain_id`, `read` and `verify`. Each endpoint requires a unique
`id`, HTTPS `url` containing one whole-segment or query-value `{key}`, explicit `sealed_key`,
and positive `max_log_blocks`. Read and verify hosts must differ. Configure observation-only
Ethereum and Base price chains too. Both environments share `TOPUP_RPC_ANKR_KEY` and
`TOPUP_RPC_INFURA_KEY`, including for staging's mainnet price observations.

`topup.yaml` also requires the top-level positive integer `max_attached_pending_refunds`:
production sets it to 2 and staging to 1. It atomically limits attached-pending refunds across
all accounts and modes in that environment; one new attachment per rolling 24 hours remains
a fixed admission limit. Missing, zero or negative values fail configuration validation.

For an N-1 rollback, use the previous release's verified configuration without this field;
its strict schema rejects it. The current N-1, v0.9.2, has **no environment-wide attached-pending
or rolling attachment-count cap**. Its built-in checks reserve pending and succeeded refund
amounts against each deposit and reject a new refund above the remaining refundable amount.
Its worker claims one due refund at a time (`LIMIT 1`), which does not cap attached inventory.
See the
[v0.9.2 refund API](https://github.com/Phala-Network/phala-pay/blob/v0.9.2/crates/topup/src/api/repository.rs#L603)
and [worker](https://github.com/Phala-Network/phala-pay/blob/v0.9.2/crates/topup/src/refunds.rs#L699).
The current R/N admission limits are not inherited by that release; retain operational
inventory and load controls during rollback.

```sh
topup config check --secrets /etc/topup/topup.yaml
topup rpc check --config /etc/topup/topup.yaml
```

`rpc check` independently validates both endpoints with typed identity, finalized transaction,
receipt and log reads, a 1,000-recipient coverage filter, canonical state calls, and state at
twice the observed finalized lag. Deploy uses the actual candidate compose environment:

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
Both environments use Ankr read and Infura verify for payments and prices.

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

Stop lines are **25,000 Ankr calls/day** (75% of 1,000,000 monthly calls divided by 30) and
**1,500,000 Infura credits/day** (50% of the daily quota). Charge one Ankr call per method,
80 Infura credits per non-log method and 255 per `eth_getLogs`. Assume every payment chain runs
all day, one caught-up recipient chunk plus one lagging chunk, and full custody vectors on all
1,000 historical addresses. No idle savings are used.

An hour adds about 300 blocks on Ethereum/Sepolia or 1,800 on Base/Base Sepolia, within the
configured 3,000-block log window. A normal range needs one request per recipient chunk;
19,200 blocks need seven. An Infura 10,000-result limit error aborts the round: the current
adapter does not automatically split on that error. Extra requests must fit the reserve or
be budgeted separately.

For each payment chain with C custody routes:

```text
Fast discovery:      288 × (1 head + 1 logs) = 576 Ankr
Checkpoint headers: 144 × 2 = 288 Ankr; 144 × 3 × 80 = 34,560 Infura
Coverage headers:   24 × 2 = 48 calls per endpoint
Coverage logs:      20 × 2 × 1 + 4 × 2 × ceil(19,200 / 3,000) = 96 per endpoint
Hourly custody:     C × 24 × ceil(1,000 / 200) = 120C per endpoint

Ankr:   576 + 288 + 48 + 96 + 120C = 1,008 + 120C
Infura: (432 + 48)×80 + 96×255 + 120C×80 = 62,880 + 9,600C
```

Checkpoint checks include both previous-checkpoint headers, both finalized heads and verify's
header at read's finalized height. Coverage uses the published checkpoint without repeating
those checks. Its header term includes the extra common-boundary read when that boundary lags.

| Payment chain | Custody routes | Fixed Ankr calls/day | Fixed Infura credits/day |
|---|---:|---:|---:|
| Staging Sepolia | 3 | 1,368 | 91,680 |
| Staging Base Sepolia | 3 | 1,368 | 91,680 |
| Production Ethereum | 2 | 1,248 | 82,080 |
| Production Base | 1 | 1,128 | 72,480 |
| **Combined** | **9** | **5,112** | **337,920** |

Variable allowances before retries:

| Work | Ankr calls | Infura credits |
|---|---:|---:|
| Deposit: cold discovery 3, confirmation and first finality verification 13, coverage completion/re-verification 6, price snapshots 2 | 24 | 2,000 |
| Unresolved finality recheck, including at most one replacement candidate | 4 | 320 |
| Combined L stock/turnover: 172 head methods per endpoint/day | 172 | 13,760 |
| Hint task | 12 | 640 |
| Price snapshot, including verify's head and pin header | 1 | 240 |
| Factory receipt verification | 3 | 240 |
| Safe challenge/proof set | 3 | 160 |
| Extra operation, conservatively priced as logs | 1 | 255 |

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
A missing receipt costs one `eth_getTransactionReceipt` per endpoint (1 Ankr / 80 Infura).
A present ordinary EVM receipt also requires its block header and transaction: three methods
per endpoint (3 / 240), even if the evidence disagrees and remains unresolved.

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
allocation: 24 Ankr / 2,000 Infura per deposit in total.
Charge four Ankr calls / 320 Infura credits per subsequent recheck, before ×1.1:

| Stock age | Rechecks/deposit/endpoint/day | Ankr calls/deposit/day | Infura credits/deposit/day |
|---|---:|---:|---:|
| First unresolved-day | 10 + 34 + 18 = 62 | 62×4 = 248 | 62×320 = 19,840 |
| Every later day | 24 | 24×4 = 96 | 24×320 = 7,680 |

The daily stock/turnover bound per endpoint is:

```text
Rechecks: 2 environments × (1 new × 62 + 1 carried × 24) = 172
Ankr:     172 × 4   = 688 calls
Infura:   172 × 320 = 55,040 credits
```

This includes all allowed replacement-candidate reads and stock turnover; neither is charged
again to the extra-operation reserve. Manual anomaly investigation consumes that reserve.
More-than-one-candidate cases retain their stock slot and verification; the operator preserves
the timeline and canonical evidence and resolves the anomaly through reviewed, audited action.

L separately budgets first-day head probes for one new entry plus hourly probes for one carried
entry per environment. Its full evidence read is already in D's three-method allocation:

```text
L head methods: 2 environments × (1 new × 62 + 1 carried × 24) = 172 per endpoint
Ankr:          172 calls
Infura:        172 × 80 = 13,760 credits
```

The refund receipt reader uses `eth_getTransactionReceipt`, the persisted-checkpoint header and
the receipt-height header independently on both endpoints: at most three methods per endpoint.
Missing receipts use one method; included receipts above the checkpoint use two; finalized
receipts use three. The checkpoint is loaded from the DB; there is no finalized-tag RPC.
The worker's pending/missing branches additionally call `eth_getTransactionByHash` on both
endpoints until an agreed sender/nonce is persisted. Asymmetric evidence can therefore cost
four methods per endpoint per check; a never-seen transaction costs two.

Checks run every 60 s for the first 30 min after attachment, every ten minutes until 24 h, then
hourly indefinitely. Before transport retries, the first half-open refund-day has at most
`30 + 141 = 171` checks: `171×4 = 684` Ankr / 54,720 Infura. A full ten-minute-phase day costs
`144×4 = 576` / 46,080; an hourly-phase day costs `24×4 = 96` / 7,680. A never-seen transaction
uses `171×2 = 342` / 27,360 on its first day and `24×2 = 48` / 3,840 per hourly-phase day.
Reserve first-day cost for N new attachments plus later-day cost for all R pending attachments,
including carry-over. Combined N=2, R=3. Refunds reserve all three allowed transport attempts;
non-refund work uses the approved ten-percent allowance:

```text
Non-refund Ankr:
5,112 + 528 + 24×66 + 2×12×80 + 3×80 + 3×12 + 300
+ 688 + 172 = 10,580

Non-refund Infura:
337,920 + 240×528 + 2,000×66 + 2×640×80
+ 240×80 + 160×12 + 255×300 + 55,040 + 13,760 = 865,460

Refund Ankr:   3 × (684×2 + 96×3)       = 4,968
Refund Infura: 3 × (54,720×2 + 7,680×3) = 397,440

Combined Ankr:   10,580×1.1 + 4,968    = 16,606 calls/day
Combined Infura: 865,460×1.1 + 397,440 = 1,349,446 credits/day

From the previous allocation:
Ankr:   10,424 − 20×80 + 24×66 + 172 = 10,580 before retries
Infura: 854,100 − 1,680×80 + 2,000×66 + 13,760 = 865,460 before retries

Headroom Ankr:   (25,000 − 16,606) / 25,000       = 33.58%
Headroom Infura: (1,500,000 − 1,349,446) / 1,500,000 = 10.04%
```

The modeled allowance is **16,606 Ankr calls/day** and **1,349,446 Infura credits/day**, with
**33.58% Ankr / 10.04% Infura** headroom below the stop lines. Production D=46 and staging D=20
give 66 deposits/day combined; all other existing caps remain unchanged. Production D=47 would
cost 1,351,646 Infura credits/day and leave 9.89%, so 46 is the largest production D that fits
the approved headroom with this allocation.

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
dashboards. Stop adding load at Ankr's 25,000/day seven-day run rate or Infura's 1,500,000 daily
credits. Investigate retry/range growth before it consumes headroom. Alerts do not authorize
exceeding stop lines or adding another free key.

At hard caps, the admission/budget paths refuse new refund attachments, hint tasks, price
snapshots or address issuance. Hints fall back to scanning; quotes needing exhausted price
budgets return retryable `price_unavailable`. Infura 402 halts verify until UTC midnight;
Ankr exhaustion makes read unavailable until its quota resets. Preserve pending work and let
bounded loops recover; never advance cursors or release reservations manually.

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
