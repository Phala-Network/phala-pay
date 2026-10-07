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

Fast discovery runs every 60 seconds. Dual finalized coverage runs every ten minutes, at most
3,000 blocks normally and 19,200 every sixth round. Address history is backfilled separately in
chunks of at most 1,000 addresses. A candidate is provisional until both endpoints agree on
receipt status, transaction, inclusion, log position and contents, block time, sender and nonce.
Scheduled custody checks each chain/token route on the first reconciliation tick and every
hour thereafter. An in-memory last-run timestamp and a separate hourly wake-up keep custody
due even when the checkpoint stalls or the normal reconciliation interval does not divide an
hour. Both endpoints read at the same canonical hash. Manual `reconcile`, post-restore checks
and process restarts run custody immediately.

### Worst-case pilot budget

The approved §5.2 figure of **1,344,960 Infura credits/day is a worst-case figure**, before the
×1.1 retry allowance. The following replaces its custody term and lowers the operational pilot
caps to include every routed chain/token pair; it does not change the approved design document.

Four payment chains (Sepolia, Base Sepolia, Ethereum and Base), each with PHA, USDC and USDT,
mean **12 custody routes** across staging and production. Each route can have balances on all
1,000 historical addresses. The 200-address multicall batch needs five calls per endpoint per
hour: `12 × 24 × ceil(1000 / 200) = 1,440` Ankr calls and `1,440 × 80 = 115,200` Infura
credits/day. Empty ledgers use no balance RPC, but the bound assumes every route is full.

The original fixed Ankr term includes `4 × 144 = 576` single-source custody calls. Replace,
rather than add to, that term. Infura's original term has no custody reads:

```text
Fixed Ankr:   16,128 - 576 + 1,440 = 16,992 calls/day
Fixed Infura: 771,840 + 115,200    = 887,040 credits/day
```

Keep the 1,000-address cap and reserve the original PR 3 hint bound `H = 150` per environment.
Reduce the combined deposit pilot bound to `D = 100` per day (stop adding merchants at a
seven-day average above 80), and enforce `Q = 60` fresh quote snapshots per price chain per
environment per UTC day. There are four price-chain/environment instances. Staging's two PHA
routes share one TWAP pool, policy and snapshot; the sampler coalesces them at each tick,
so five-minute sampling adds `86,400 / 300 = 288` snapshots/day. Production in this pilot has
no additional periodic TWAP sampler; adding a pool or sampler requires a new budget review.

```text
Ankr:   16,992 + (288 + 4 × 60) + 15 × 100 + 2 × 12 × 150 = 22,620
        22,620 × 1.1 = 24,882 calls/day ≤ 25,000
Infura: 887,040 + 240 × (288 + 4 × 60) + 1,440 × 100 + 2 × 640 × 150 = 1,349,760
        1,349,760 × 1.1 = 1,484,736 credits/day ≤ 1,500,000
```

Reducing only the quote cap cannot fit Ankr: even `Q = 0` with `D = H = 150` costs
`16,992 + 288 + 2,250 + 3,600 = 23,130`, or 25,443 calls after retries. Both reduced caps
are necessary for this allocation. The retry allowance covers bounded snapshot retries and
ordinary endpoint retries. Additional routes, manual reconciliation runs, repeated restarts or
provider preflights need quota headroom or a paid-provider upgrade before adding that load.
Alerts report unexpected usage; they do not authorize exceeding either stop line.

An address or unverified-id snapshot change abandons all cached evidence and retries once
immediately with a full fresh RPC round. A second change waits for the next tick. Coverage
commits only after every request succeeds, and every boundary and pending cleanup ends
at the scanned block, even when the checkpoint is farther ahead.

Observe `topup_rpc_endpoint_ready`, `topup_rpc_errors_total`, `topup_coverage_lag_seconds`,
`topup_addresses_lagging`, and `topup_daily_budget_used`. Import [rpc-alerts.yaml](rpc-alerts.yaml).
Both environments share provider quotas: stop adding load at Ankr's 25,000 calls/day seven-day
run rate or Infura's 50% daily credit usage. Compare provider dashboards with the recording rules.
Keep at most 1,000 issued addresses per chain and stop adding merchants when the seven-day
average exceeds 80 deposits/day (the combined pilot bound is 100).

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
The real `deploy/local/rollback-drill.sh` gate covers N-1 → N → N-1 → N. PR 3 extends its named
hint extension point; PR 2 implements no transaction-submission endpoint or task.
