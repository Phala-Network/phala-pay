# RPC endpoint health

Use this procedure for an endpoint unavailable for five minutes, dual evidence disagreement,
coverage lag over 45 minutes, or provider quota pressure. [RPC operations](../RPC.md) lists the
fixed cadences and shared quotas.

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
   fire and a sample of 60 does.
5. On disagreement, preserve decoded evidence and wait. Never pick one source or manually
   advance coverage. A checkpoint conflict or progressed evidence mismatch uses the existing
   [chain freeze gate](chain-frozen.md) and audited lift.
6. Verify readiness restored, coverage advancing at its actual scanned end, lagging addresses
   catching up, and finality and credit resuming. Other chains and the API remain available.

## Metrics refresh failure

Coverage and daily budget gauges are read from durable tables on authenticated metrics scrapes.
An encoding or database failure fails the scrape; inspect database availability and capacity.
Do not replace an unavailable sample with zero or infer negative payment evidence from metrics.
