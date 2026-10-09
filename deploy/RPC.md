# Chain RPC operations

Each configured chain has an Ankr read endpoint and an independent Infura verify endpoint.
[Chain reads](../docs/design/chain-reads.md) specifies the evidence, cadences and budget.
No endpoint failover, provider pool, acceptance state or RPC recovery command remains.

## Configuration and preflight

Configure `rpc` entries with `chain_id`, `read` and `verify`. Each endpoint requires a unique
`id`, HTTPS `url` containing one whole-segment or query-value `{key}`, explicit `sealed_key`,
and positive `max_log_blocks`. Read and verify hosts must differ. Configure observation-only
Ethereum and Base price chains too. Staging and the pilot use the same owner-managed credentials:
`TOPUP_RPC_ANKR_KEY` and `TOPUP_RPC_INFURA_KEY`.

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
Seal the complete set of secrets on every update, including unchanged names; a partial update
can unset credentials required by another chain or the price sources.

## Outages, lag and quotas

An endpoint failure pauses its chain's evidence or the routes observing that price chain.
The API, `/healthz` and other chains continue. Never resolve a disagreement by choosing one
answer. Restore the endpoint and let the ordinary bounded loops retry with a fresh canonical pin.
Alloy's single retry layer retries HTTP 429/503 and JSON-RPC throughput errors; Infura HTTP 402
halts that endpoint until the next UTC midnight (5:00 PM PDT, 4:00 PM PST).

Fast discovery runs every 300 seconds. Checkpoints advance independently every 600 seconds.
Dual finalized coverage reads the published checkpoint every hour, at most
3,000 blocks normally and 19,200 every sixth round (six hours). Observation-chain contract recovery keeps its separate
60-second interval. Address history is backfilled separately in
chunks of at most 1,000 addresses. A candidate is provisional until both endpoints agree on
receipt status, transaction, inclusion, log position and contents, block time, sender and nonce.
Scheduled custody checks each chain/token route on the first reconciliation tick and every
hour thereafter. An in-memory last-run timestamp and a separate hourly wake-up keep custody
due even when the checkpoint stalls or the normal reconciliation interval does not divide an
hour. Both endpoints read at the same canonical hash. Manual `reconcile`, post-restore checks
and process restarts run custody immediately.

### Worst-case pilot budget

Confirmation waits read only the required tag's head, once per endpoint. Both must qualify before
one full receipt set (receipt, canonical header, transaction: at most three methods per endpoint)
is reserved. Normal confirmation and slow lane L share that one receipt allowance. Receipt
absence, disagreement, changed actual evidence, or RPC failure enters unresolved lane S.
Persisted checkpoint/hash conflicts freeze immediately. Height differences alone are ordinary lag.

| Mode | Normal head probes per endpoint | Fixed wake-up positions | Normal plus first terminal verification |
|---|---:|---|---:|
| Depth | 6 | First immediately, then a+4/12/28/60/124 seconds | 13 methods |
| Safe | 3 | First immediately, then every 384 seconds | 10 methods |
| Finalized | 7 | First immediately, then every 384 seconds | 10 methods |

For Depth, B=discovered block+depth-1. For each endpoint estimate e=t0 when its head meets B,
otherwise e=head timestamp+(B-head height)×chain block seconds (Ethereum 12, Base 2).
The fixed anchor a=max(t0,min(e_read,e_verify)); estimates schedule checks and never prove depth.
Safe/Finalized deadlines are t0+768/t0+2,304 seconds. Restart skips missed positions, consumes
at most one current position and cannot restart the window or refund a reserved read.
Healthy Depth confirmation normally completes about four seconds after estimated depth, plus
processing; a lagging endpoint can wait up to the current interval (last normal interval 64 seconds).

Only valid height lag exhausting the normal window enters L, setting immutable `first_slow_at`.
It does not enter S. L first rechecks after 60 seconds, then uses the shared backoff:
60 seconds before ten minutes, ten minutes until six hours, hourly thereafter. Each check is
one tagged head method per endpoint. As soon as both qualify, the same claim reads its sole
receipt set and confirms/values **without waiting for the published checkpoint**. Recovery latency
is bounded by the current 60-second, ten-minute, or hourly interval plus RPC/processing time.
The first half-open 24 hours has at most 62 L head rechecks; an hourly-phase rolling day has 24.
Depth plus the first L day uses at most 71 methods/endpoint (75 with first terminal verification),
Safe 68 (72 with terminal verification), Finalized 72. There is no finite lifetime bound for a
permanently lagging deposit.

L stock and rolling 24-hour entries each have an operational allocation of one per environment,
summed across chains. DB gauges `topup_confirmation_slow` and
`topup_confirmation_slow_entries_24h` survive restart and include resolved entries in the entry
window. Non-empty L for ten minutes warns; either sum exceeding one stops new quote/payment
load. Continue verifying existing funds and retain reservations. See [recovery](runbooks/rpc-health.md).
S remains separately allocated: one current unresolved deposit and one rolling entry per environment.
Its immutable anchor is `first_unresolved_at`; it shares the same segmented backoff. Each S check
uses at most four methods per endpoint, including a single service-known replacement when needed.
The watcher owns S even when an earlier `final_at` exists; only fresh terminal proof resolves it.
Positive provisional reappearance stays S. Terminal dual evidence is passed to valuation within
the same lease, without a receipt reread. Terminal price retries reuse full persisted proof;
nonterminal price failures retain watcher ownership. The existing one-hour pending alert remains.

Staging and production run concurrently on shared free quotas. Keep production **46 deposits/day**,
staging **20/day**, H=80 hints/environment/day and Q=60 fresh quote snapshots/price-chain/environment/day.
D is an operational allocation, not a new code cap. Maintain twelve hourly custody routes, 1,000
historical addresses/chain, and the existing factory, Safe and price allocations. Refund concurrent
attached-pending stock is configured as production R=2 and staging R=1; each environment's rolling
24-hour new attachment cap N=1 is constant. Idempotent attachment retries do not consume N.

| Per deposit | Ankr calls | Infura credits |
|---|---:|---:|
| Cold discovery | 3 | 0 |
| Confirmation and first terminal verification | 13 | 1,040 |
| Coverage completion/reverification | 6 | 480 |
| Price snapshot allocation | 2 | 480 |
| Total | 24 | 2,000 |

Two environments reserve L separately: 2×(62 new+24 carried)=172 head methods/endpoint/day,
172 Ankr calls and 13,760 Infura credits. S retains its separate 688 calls/55,040 credits.

```text
Non-refund Ankr:   10,580 calls/day
Non-refund Infura: 865,460 credits/day
Combined Ankr:    10,580 × 1.1 + 4,968 = 16,606 calls/day
Combined Infura:  865,460 × 1.1 + 397,440 = 1,349,446 credits/day
Headroom:         Ankr 33.58%; Infura 10.04%
```

The approved budget applies ×1.1 to non-refund traffic and reserves all three transport attempts
for refunds. Logical methods have hard bounds; each method has at most three physical attempts.
This total does not cover every non-refund method exhausting all retries while preserving 10%
headroom. Production D=47 would leave only 9.89% Infura headroom; 46 is the maximum integer here.
Refund checks read both endpoints: `eth_getTransactionReceipt`, the persisted-checkpoint
`eth_getBlockByNumber`, and receipt-height `eth_getBlockByNumber` (at most three/endpoint).
Missing receipts use one method; included but not finalized receipts use two; finalized receipts
use three. The checkpoint height is read from the DB; refund checks issue no transaction or
finalized-tag RPC. Cadence is 60 seconds for 30 minutes, ten minutes until 24 hours, then hourly
indefinitely. Per-refund phase-day ceilings at three logical methods/check (excluding transport
retries): first half-open day 171 checks=513 Ankr calls/41,040 Infura credits; ten-minute phase
144 checks=432/34,560 per full day; hourly phase 24 checks=72/5,760. The approved combined
refund reserve is conservative and includes all three transport attempts for both environments'
permitted stock and inflow.

Manual reconciliation, repeated restarts, preflights and additional chains/routes need budget
headroom or a reviewed paid-provider allocation. Alerts do not authorize exceeding stop lines.

An address or unverified-id snapshot change abandons all cached evidence and retries once
immediately with a full fresh RPC round. A second change waits for the next tick. Coverage
commits only after every request succeeds, and every boundary and pending cleanup ends
at the scanned block, even when the checkpoint is farther ahead.

Observe `topup_rpc_endpoint_ready`, `topup_rpc_errors_total`, `topup_coverage_lag_seconds`,
`topup_addresses_lagging`, and `topup_daily_budget_used`. Import [rpc-alerts.yaml](rpc-alerts.yaml).
Both environments share provider quotas: stop adding load at Ankr's 25,000 calls/day seven-day
run rate or Infura's 50% daily credit usage. Compare provider dashboards with the recording rules.
Keep at most 1,000 issued addresses per chain. Stop adding payment load before either environment
exceeds its deposit allocation or either L/S operational capacity rule.

Each fresh quote snapshot consumes one unit of its environment's UTC price-chain budget, capped
at 60. Quotes return retryable `price_unavailable` when exhausted. A cached snapshot is usable
for at most twelve seconds; confirmation always fetches fresh evidence. A snapshot costs one
Ankr call and 240 Infura credits, including verify's head and pin header.

Staging PHA samples every 300 seconds, with explicit route limits `max_sample_age_s: 900`
(three intervals, tolerating two misses) and `max_sample_jump_bps: 1100`
(500 × sqrt(300/60), rounded). Defaults remain 180 seconds and 500 bps for 60-second sampling.
Persisted TWAP hashes are proved in the same multicall; a baseline outside the EVM BLOCKHASH
history fails closed rather than using unproved history.

## Finality conflicts and rollback

A changed stored checkpoint hash or a progressed, unverified deposit contradicting agreed
canonical evidence freezes the chain through `reconciliation_blocks`. Use the existing audited
[chain freeze procedure](runbooks/chain-frozen.md); do not edit checkpoint hashes or remove records.
A missing receipt does not prove replacement. Reversal needs a finalized receipt without the
transfer, or a service-known same-sender/same-nonce transaction with another hash agreed by both.
No account nonce query is proof; EIP-7702 authorizations can increment it.

The migration is expand-only. N keeps N-1's cursor and address compatibility fields, writes no
legacy RPC table, and refuses to start if N-1 left frozen, awaiting-anchor or recovery-pending
state. Resolve that state with N-1 before upgrading. Preserve N-1's verified image and config.
The local published-image rollback drill covers N-1 → N → N-1 → N, preserving money state and
new confirmation counters, deadlines and entry history. N-1 data compatibility does not promise
the new RPC bounds: rollback must use the old binary's operating allocation.
