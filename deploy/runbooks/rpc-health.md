# RPC endpoint health

Use this procedure for an endpoint unavailable for five minutes, dual evidence disagreement,
coverage lag over two hours, or provider quota pressure. [RPC operations](../RPC.md) lists the
fixed cadences and shared quotas.

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
   enlarge budgets. Fresh quote snapshots also have a hard 60/day/price-chain cap; exhaustion
   returns retryable `price_unavailable`, and the UTC day resets that budget.
   `RpcPriceSnapshotBudgetExhausted` fires at `topup_daily_budget_used{name=~"price:.*"} >= 60`,
   including exactly the hard cap. This repository has no Prometheus rule-test harness; when
   importing the rules, verify with the operator's rule evaluator that a sample of 59 does not
   fire and a sample of 60 does. Admitted hint tasks have a hard 80/day/environment cap;
   `RpcHintBudgetExhausted` fires at `topup_daily_budget_used{name="hints"} >= 80`.
   Verify that 79 does not fire and 80 does. Exhausted hints keep their quiet acknowledgement
   and fall back to scanning; they cannot establish negative coverage.
5. On disagreement, preserve decoded evidence and wait. Never pick one source or manually
   advance coverage. A checkpoint conflict or progressed evidence mismatch uses the existing
   [chain freeze gate](chain-frozen.md) and audited lift.
6. Verify readiness restored, coverage advancing at its actual scanned end, lagging addresses
   catching up, and finality and credit resuming. Other chains and the API remain available.

## Metrics refresh failure

Coverage and daily budget gauges are read from durable tables on authenticated metrics scrapes.
An encoding or database failure fails the scrape; inspect database availability and capacity.
Do not replace an unavailable sample with zero or infer negative payment evidence from metrics.

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
Production deposits/day is 46 and staging 20; include both in shared quota checks.

Receipt absence, disagreement, evidence changes and RPC failures instead enter S, which has
its own stock/entry alerts and one-hour age alert. Follow the existing
[deposit incident procedure](deposit-reversed.md). An old `final_at` on a detected S row is not
credit or reversal proof; the watcher must acquire fresh dual terminal evidence created at or
after `first_unresolved_at`, matching `(to, token, from, amount, tx_from, tx_nonce)`. A mismatch
follows S or reversal. Price retries may reuse only the complete versioned proof linked to the
current final marker by `confirmation_terminal_transition_id`, never a pre-S proof. They never
restart normal confirmation quotas. See the [proof recovery rules](deposit-reversed.md#confirmation-delay-and-slow-lane-l).
