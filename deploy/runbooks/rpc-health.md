# RPC endpoint health

Use this procedure for an endpoint unavailable for five minutes, dual evidence disagreement,
coverage lag over two hours, or provider quota pressure. [RPC operations](../RPC.md) lists the
fixed cadences and per-account quotas: shared Ankr read, staging Infura verify and production
BlockPI verify. Stop adding load at 25,000 Ankr calls/day (seven-day run rate), 1,500,000 Infura
credits/day or 1,250,000 BlockPI RU/day (seven-day run rate). Compare BlockPI with its account
dashboard too: log responses above 200 KB incur surcharges that method counters cannot measure.

The service evaluates readiness/backfill/coverage and S/L/H/Q in its existing business monitor;
[production launch §11](production-launch.md#11-pilot-caps-monitoring-and-stop-actions) lists the
`TopupRpc*` alerts and their thresholds. Sentry owns notification delivery. The `Rpc*` names in
[rpc-alerts.yaml](../rpc-alerts.yaml) are the optional Prometheus equivalents. Rolling provider
quotas still require a time-series collector or daily provider-dashboard checks.

## Hot set and issuance budgets

`TopupRpcHotAddressBudget` / `RpcHotAddressBudget` fires when
`topup_hot_addresses{chain_id}` exceeds `topup_hot_addresses_budget{chain_id}` (2,000).
The DB counts distinct addresses quoted in the last 24 hours or with a deposit in the last
seven days (chain block time), using the configured windows, in every state. Counts are per
address and per chain. Quote snapshot Q
cannot bound this set because quotes reuse snapshots. Staging's historical address cap is 1,000.

Pause new quotes on the affected chain or reduce discovery windows through an attested config
change. Keep `quote_window_hours × 3600` at least every route's maximum quote TTL; config
validation rejects a smaller window. Cold addresses remain under hourly dual coverage and hints.
Keep existing funded work running. Resume growth once the hot set fits its budget or a reviewed
paid-provider budget replaces this one. Do not truncate discovery recipients to hide the alert.

`TopupRpcIssuanceBudget` / `RpcIssuanceBudget` fires when
`topup_address_issuances_24h{chain_id}` exceeds its `_budget` gauge: 50 per production chain,
10 in staging. This rolling count includes all quote-owned and deposit-address-owned physical
forwarders across accounts/modes, including retired addresses. RedPill currently admits at most
20 new crypto organizations/day; Phala Cloud is not live. Token reuse on an existing network
adds no issuance; new networks, treasury changes and rotations do.

Pause new issuance before that allowance is spent. An alert above it requires reconciling
opening jobs and RPC counters, debiting the excess/retries to the 150-call extra or monthly
recovery reserve, and waiting for rolling usage to fit before resuming. Continue opening proof
capture and all funded verification. These are operational allowances with alerts, not atomic
admission limits. D is separately capped at 280 production/10 staging per UTC day, including
recovery work. Check the [shared-account budget](../RPC.md#worst-case-pilot-budget).

## Confirmation provider lag

Compare each endpoint's corresponding latest, safe or finalized head with the deposit's
applicable requirement; unequal heights alone are not conflicting evidence. Depth normally
confirms about four seconds after expected depth, with lag bounded by the current normal
probe interval (up to 64 seconds). Safe/Finalized target one 384-second epoch interval.
If the fixed normal window expires only because of height lag, use
[slow lane L](../RPC.md#slow-confirmation-lane-l), not S. L probes heads every 60 seconds until
ten minutes, every ten minutes until six hours, then hourly, anchored to `first_slow_at`.
Both heads meeting the requirement permits the one full dual evidence read and credit in that
claim without waiting for the checkpoint.

L remaining non-empty for ten minutes raises the provider-lag alert. The DB-derived gauges
`topup_confirmation_slow{chain_id}` and `topup_confirmation_slow_entries_24h{chain_id}` measure
stock and exact rolling-24-hour first entries. Sum chains separately per environment. Either
sum above one raises the independent L capacity alert: pause new quotes on all routes of the
affected chain and coordinate merchant intake. Continue existing funded verification, retain
reservations and records, and count entries even after they leave L. Do not clear timestamps,
replay missed slots or restart normal/evidence allowances.

A failed or anomalous full read, changed evidence or an RPC failure that prevents proving
height-only lag enters S; checkpoint hash conflicts still freeze the chain. Follow
[confirmation/finality recovery](deposit-reversed.md#confirmation-delay-and-slow-lane-l).
Resume new quotes only after the provider incident is reviewed and both L limits, S and daily
work fit the [pilot caps](../RPC.md#pilot-limits-and-operating-modes). L does not count toward S
and has its own capacity stop action.

## First steps

1. Check `topup_rpc_endpoint_ready` by provider and chain and sanitized error classes in
   `topup_rpc_errors_total`. Compare the provider dashboard's status and quota with both
   environments' billed call counters. Keep keys and complete request URLs out of incident logs.
2. Run `topup rpc check --config /etc/topup/topup.yaml` through the deployment's compose env
   path. It validates both endpoints independently, including canonical state and complete logs.
3. For a credential failure, submit the complete sealed secret set, preserving all unchanged
   secrets. Run compose-path preflight before an owner-authorized upgrade.
4. HTTP 402 waits until UTC midnight. Do not create extra free accounts, switch endpoints or
   enlarge budgets. Fresh quote snapshots have a configured daily cap per price chain
   (production 600, staging 30, default 60); exhaustion returns retryable `price_unavailable`,
   and the UTC day resets that budget.
   `RpcPriceSnapshotBudgetExhausted` and `RpcHintBudgetExhausted` compare
   `topup_daily_budget_used{name}` against the matching `topup_daily_budget_limit{name}`,
   including all deployment scrape labels, and fire at exactly the configured cap. Hint tasks
   use production 300, staging 20, default 80 per environment per UTC day. When importing the rules,
   verify with the operator's evaluator that cap−1 does not fire and cap does for both environments.
   Older binaries lack the limit gauge; retain their original rules for rollback. Exhausted hints
   keep their quiet acknowledgement and fall back to scanning; they cannot establish negative coverage.
5. On disagreement, preserve decoded evidence and wait. Never pick one source or manually
   advance coverage. A checkpoint conflict or progressed evidence mismatch uses the existing
   [chain freeze gate](chain-frozen.md) and audited lift.
6. Verify readiness restored, coverage advancing at its actual scanned end, lagging addresses
   catching up, and finality and credit resuming. Other chains and the API remain available.

## Metrics refresh failure

Coverage and daily budget gauges are read from durable tables on authenticated metrics scrapes.
An encoding or database failure fails the scrape; inspect database availability and capacity.
Do not replace an unavailable sample with zero or infer negative payment evidence from metrics.
Before raising pilot volume, add suitable indexes for the deposits `first_unresolved_at` and
`first_slow_at` stock/rolling-entry scans, which currently lack timestamp indexes, and verify
query plans against representative retained history. The internal-alert change adds no migration.

## Confirmation slow lane

`RpcConfirmationProviderLag` means height-only confirmation lag has kept L non-empty for ten
minutes. Read `topup_confirmation_slow{chain_id}` and the DB-derived
`topup_confirmation_slow_entries_24h{chain_id}`. Ordinary differing heights are not S anomalies.
L uses one head method per endpoint on the 60-second/ten-minute/hourly schedule. When both
qualify, it confirms and values within the same lease without waiting for checkpoint.

If `sum(topup_confirmation_slow) > 1` or `sum(topup_confirmation_slow_entries_24h) > 1`, pause
new quotes on affected chains using the existing route pause procedure and coordinate with
merchants to stop new payment load. Each environment has one stock and one rolling-entry
allocation. Continue verifying received funds, keep reservations, and retain every excess row.
Do not reset history/counters or speed polling up. Resolved entries stay in the rolling window
until 24 hours after first entry. Resume new load only after stock and rolling entries both
return to at most one, both endpoints recover, and existing payments resume confirmation.
Production deposits/day is 280 and staging 10; include both in shared quota checks.

Receipt absence, disagreement, evidence changes and RPC failures instead enter S, which has
its own stock/entry alerts and one-hour age alert. Follow the existing
[deposit incident procedure](deposit-reversed.md). An old `final_at` on a detected S row is not
credit or reversal proof; the watcher must acquire fresh dual terminal evidence created at or
after `first_unresolved_at`, matching `(to, token, from, amount, tx_from, tx_nonce)`. A mismatch
follows S or reversal. Price retries may reuse only the complete versioned proof linked to the
current final marker by `confirmation_terminal_transition_id`, never a pre-S proof. They never
restart normal confirmation quotas. See the [proof recovery rules](deposit-reversed.md#confirmation-delay-and-slow-lane-l).

## Incomplete custody coverage

`TopupCustodyCoverageIncomplete` means a custody sweep excluded caught-up forwarders because
one or more deposits at the canonical pin remain non-final and non-reversed. The event reports
`excluded_forwarders` once per chain and route, without per-forwarder alert groups. The default
age bound is 24 h (`topup run --custody-unsettled-alert-after-s`, default `86400`), measured from
the unsettled deposit's durable creation time. An already-old deposit alerts on the first sweep
that excludes it. Sweeps run every six hours by default (`--custody-interval-s`), so an age-bound
crossing may take one further sweep interval to alert.

1. Inspect the alerts API or Sentry event's chain, route and count. Check the authenticated daily
   report and RPC readiness/coverage metrics for that chain, then follow
   [provider disagreement](provider-disagreement.md) or [deposit finality](deposit-reversed.md).
2. Restore both endpoints' availability and independent evidence, and let the finality watcher
   settle or reverse the deposits. Do not set final markers manually, omit forwarders, or infer
   that a partial custody sweep proved a clean ledger. Eligible forwarders continue to be read.
3. Run the existing manual reconciliation procedure from [reconciliation mismatch](reconciliation-mismatch.md)
   after finality recovers, or wait for the next scheduled sweep. Confirm the alert no longer
   recurs. This event-only alert remains in the alerts API until an operator resolves it through
   the [alerts procedure](README.md#manage-alerts).

## Custody opening unavailable

`TopupCustodyOpeningUnavailable` means an issued forwarder lacks an independently verified
opening balance at its inclusive discovery start minus one block. Alerts group only by chain and
route and report `excluded_forwarders`; `reason` is a stable category, without provider URLs.
Custody fails/incompletes for those forwarders and continues comparing eligible ones. This does
not by itself freeze the whole chain or prove a discrepancy. A genuine post-issuance mismatch
still freezes the chain. The normal custody detection target requires opening evidence.

New issuance starts one block after the current agreed checkpoint, so the background capture
normally reads a recent pin within a minute. Existing addresses, later-added tokens and restored
addresses with an earlier discovery boundary may require older state. Both providers must serve
that exact historical pin; a free endpoint returning recent state does not establish this.
If the token or Multicall3 was not deployed at that pin, capture also remains incomplete and
needs Engineering review; an absent or undecodable balance result is never assumed to be zero.

1. Inspect the alert's chain, route, count and the reconciler's failed custody check in the daily
   report. Restore both providers' availability and historical state support for the original
   pin, using the [RPC health checks](#first-steps). If a pin remains unavailable, escalate to
   Engineering for independent providers that retain that state; leave custody incomplete.
2. Allow the durable jobs to retry (six-hour failure backoff), or run `topup reconcile --config FILE` with the
   deployment's normal config and database environment after provider repair for an immediate retry.
   Jobs sharing a pin are batched; repeatedly failing historical jobs do not precede new jobs.
   Never insert or edit opening amounts, change discovery start to avoid a failure, use a
   current balance, or treat a missing opening as zero. Binary rollback preserves the proofs.
3. Confirm the custody check completes and the alert stops recurring. Resolve this event-only
   alert through the [alerts procedure](README.md#manage-alerts). A later-added route includes
   already recorded unsupported transfers; it must not establish an activation-time baseline.
