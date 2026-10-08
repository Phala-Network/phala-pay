# Scanner lag

**Trigger:** `topup-fast-scanner-<chain_id>` or `topup-coverage-scanner-<chain_id>` Sentry Crons,
`RpcCoverageLag` (>2 hours), or persistent `RpcAddressBackfillLag`.

**Impact:** fast discovery runs every 300 seconds on read. A separate dual checkpoint check
runs every 600 seconds; full dual log coverage runs every 3,600 seconds, with 19,200-block
catch-up every sixth round (six hours). Admitted checkout hints use their independent instant
processing path. Missing fast discovery delays unhinted credits until coverage discovers the
payment. Failed coverage delays negative decisions: quote expiry/cancel completion and hourly
custody checks require complete coverage. Expiry/cancel has a conditional target of about
70 minutes after qualifying finality, with healthy endpoints and caught-up addresses; backlog
can extend it. A not-ready or frozen chain pauses issuance and credit, while the API and other
chains continue.

## First steps

Inspect the relevant chain's readiness and coverage metrics, the Crons check-ins, and its blocks:

```sh
admin GET /v1/admin/metrics | jq
admin GET /v1/admin/reports/daily | jq '.reconciliation_blocks'
cast block latest --json --rpc-url "$RPC_PROVIDER_A_URL" | jq '(.data // .) | {number,hash}'
cast block finalized --json --rpc-url "$RPC_PROVIDER_A_URL" | jq '(.data // .) | {number,hash}'
cast block finalized --json --rpc-url "$RPC_PROVIDER_B_URL" | jq '(.data // .) | {number,hash}'
```

The fast monitor expects a five-minute interval, two-minute margin and three failures before
alerting. Coverage expects a one-hour interval and two-minute margin. Both have per-chain
slugs; success on another chain cannot hide this chain's failure. Check independent ten-minute
checkpoint progress and finality/refund progress even while hourly coverage is waiting.

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
- Historical backfill: each successful round advances at most 3,000 blocks normally or 19,200
  every six hours, in provider-sized log windows, and processes at most 1,000 lagging addresses.
  New, reissued and restored addresses remain lagging until their own history catches up.
  Let the bounded rounds finish; never bulk-mark them. The target for at most 24 hours of
  equivalent backlog is recovery within 12 hours of continuous healthy operation, subject to
  the work allowances in [RPC recovery targets](../RPC.md#latency-and-recovery-targets).
- Healthy endpoints but missing check-ins: escalate the worker issue. **HUMAN-ONLY:** restart
  the CVM only after preserving Sentry evidence (`deploy/phala cvms restart "$TOPUP_CVM_ID"`).
  Durable checkpoints and coverage survive a restart; interrupted rounds do not advance them.
  Restarting resets the catch-up round counter, so repeated restarts extend recovery.

## Done when

Both per-chain scanner monitors check in at their new periods, independent checkpoints advance,
coverage lag drains below two hours, address backfill completes, and payments appear once.
Resume quotes only when pending inventory and daily work fit the
[pilot limits](../RPC.md#pilot-limits-and-operating-modes). Confirm delayed expiry/cancel decisions
complete only after the address catches up, dual coverage passes its expiry time, and no
in-window deposit remains pending; elapsed wall-clock time alone never releases a reservation.
