# Scanner lag

**Trigger:** `topup-fast-scanner-<chain_id>` or `topup-coverage-scanner-<chain_id>` Sentry Crons,
`RpcCoverageLag` (>45 minutes), or persistent `RpcAddressBackfillLag`.

**Impact:** fast discovery runs every 60 seconds on read; dual finalized coverage runs every
600 seconds on read and verify. Missing fast discovery delays credits until coverage discovers
the payment. Failed coverage delays negative decisions: quote expiry/cancel completion and
custody reconciliation wait for complete coverage. A not-ready or frozen chain pauses issuance
and credit, while the API and other chains continue.

## First steps

Inspect the relevant chain's readiness and coverage metrics, the Crons check-ins, and its blocks:

```sh
admin GET /v1/admin/metrics | jq
admin GET /v1/admin/reports/daily | jq '.reconciliation_blocks'
cast block latest --json --rpc-url "$RPC_PROVIDER_A_URL" | jq '(.data // .) | {number,hash}'
cast block finalized --json --rpc-url "$RPC_PROVIDER_A_URL" | jq '(.data // .) | {number,hash}'
cast block finalized --json --rpc-url "$RPC_PROVIDER_B_URL" | jq '(.data // .) | {number,hash}'
```

The fast monitor expects a one-minute interval, two-minute margin and three failures before
alerting. Coverage expects a ten-minute interval and two-minute margin. Both have per-chain
slugs; success on another chain cannot hide this chain's failure.

When lag is material, pause quotes:
`admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["quotes"]}'`.

## Decide

- Endpoint unavailable or throttled: follow [RPC health](rpc-health.md). Three consecutive final
  request failures mark it not-ready; short head probes run at most every 30 seconds to recover.
  Infura 402 waits until UTC midnight. Never select a substitute endpoint in process.
- Evidence disagrees: follow [provider disagreement](provider-disagreement.md). A round cannot
  advance coverage until both sources verify every candidate and the actual scanned end.
- Chain frozen: follow [Chain frozen](chain-frozen.md); a code, checkpoint or verified-evidence
  conflict requires cause repair and an audited lift. Repeated requests do not lift it.
- Historical backfill: each successful round advances by the configured log-window limit and
  processes at most 1,000 lagging addresses. New, reissued and restored addresses remain lagging
  until their own history catches up. Let the bounded rounds finish; never bulk-mark them.
- Healthy endpoints but missing check-ins: escalate the worker issue. **HUMAN-ONLY:** restart
  the CVM only after preserving Sentry evidence (`deploy/phala cvms restart "$TOPUP_CVM_ID"`).
  Durable checkpoints and coverage survive a restart; interrupted rounds do not advance them.

## Done when

Both per-chain scanner monitors check in, coverage lag drains below 45 minutes, address backfill
completes, payments appear once, and quotes are resumed. Confirm delayed expiry/cancel decisions
complete only after dual coverage passes their expiry time.
