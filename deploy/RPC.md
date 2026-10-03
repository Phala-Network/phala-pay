# RPC groups runbook (0.7.0)

The attested topup image contains the group clients; the inline `topup.yaml` contains every
member, reviewed company identity, quota scope and bounded policy. Keys exist only in the sealed
`TOPUP_RPC_*_KEY` variables explicitly named by members. Declare those variables for both topup
and restore-check in the environment's compose overlay. No RPC sidecar or cache is deployed.

## Configuration and acceptance

Staging uses Tenderly/Sentio A on both chains, PublicNode/ethPandaOps B on Sepolia, and
PublicNode alone for B on Base Sepolia (see
[staging routes](phala.md#staging-routes)). Keep the
existing member ids so usage series continue. Add credentials or backups through a reviewed PR;
review company ownership independently of DNS names, including resellers and aliases. Companies
in A must never appear in B. Same templates with distinct credentials are permitted in one group.
Give keyless endpoints explicit synthetic key budgets, shared across their methods, and share
account scopes across chains/credentials belonging to the same paid account. Never run two
active replicas with the same budgets: admission is process-local, under the existing lease model.

Run the pinned image's `topup config check --secrets FILE`, then `topup rpc check --config FILE`
with the sealed environment. The latter uses each member's actual key and returns only validated
member ids as a JSON array on stdout. Logs and failure summaries go to stderr and name each
failed member, probe and sanitized error class; the Deploy preflight preserves that summary.
Set `TOPUP_RPC_PROBE_DEBUG=1` to trace sanitized RPC method/tag attempts and decoded head
heights; this never enables raw transport logs, URLs, credentials or upstream response bodies.
A failed backup does not block first acceptance when another member in each group
passes. Runtime persists the public configuration digest and per-member chain/genesis evidence.
On the same digest, restart may be degraded; a returning member needs complete probes and the
configured number of successes before readmission. A new digest still requires one serving
member in each group. Neither path permits credit using only A or B.

`policy.probe: { attempts: 3, deadline: 30000 }` explicitly configures the default acceptance
and readmission bounds. Attempts includes the first send of each RPC; deadline is milliseconds
for the complete member probe. Transport/timeouts, classified server errors and throttling
retry through Tower, with exponential backoff from `retry_delay_ms` and quota admission honoring
`Retry-After`. Wrong chain/genesis, missing or mismatched code, unsupported capabilities,
malformed replies and stale heads fail immediately. `recovery_successes` (default 2) requires
consecutive complete successful probes before readmission; retries are not recovery successes.
Owner recovery also gives each member a fresh `probe.deadline`; after both groups finish,
anchor agreement starts with fresh copies and its own shared `total_deadline_ms` bound.
Same-height snapshot, persisted-anchor and later-read hash conflicts fail immediately.
A must serve address-less Transfer logs over an unsplit 2 000-block window; B must serve a
100-block addressed recent log range. All probes check canonical Multicall3 and route contracts.

## Outages, lag and quotas

Read `topup_rpc_group_eligible_members` and the per-member health/quarantine gauges from the
existing admin-signed metrics endpoint. Import [rpc-alerts.yaml](rpc-alerts.yaml) into the
operator's Prometheus rules. Existing `topup_rpc_calls_total` counts every real member send,
including retries, preflight, head checks and replay; selection and denied admissions cost zero
sends. Compare rates by account across all member ids and methods, rather than one method alone.

A whole group outage pauses evidence and credit. Failover never fills an empty candidate pool.
Quota-paused members are skipped when a separately budgeted account is healthy.
Cooldown expiry schedules probes; it does not admit a member. A lagging member cannot answer a
window below its end or the group's persisted high-water mark, even with a fast empty result.
Investigate auth/redirect quarantines before restarting with a reviewed config. All redirects,
including same-host redirects, are refused. Unknown HTTP429 pauses the entire account; narrow
only reviewed error rules to a key scope. HTTP408 is transient timeout. HTTP413 splits request
address/topic arrays while preserving the numeric block window. Unrecognized limits wait for
classification review; they never become successful empty results.

## Historical review and gaps

Each committed window retains its selectors, hash anchor and answering member in
`rpc_window_reviews`. Review does not expire when the rolling tail moves. The finalized scanner
re-reads pending coverage with a different member where available, recording missed deposits
idempotently. Singleton groups replay with their sole member after restart while retaining pending independent
review coverage and alerting on the limitation. Nonfinal branch changes queue durable head-scan
replay ranges, including same-height latest/safe changes. The production poll keeps replaying
pending ranges even while head height is unchanged. Only the contiguous range actually read
advances replay coverage; confirmation progress never skips beyond that window.
`topup_rpc_reorg_pending_ranges` alerts if replay remains stalled.
After an outage the committed cursor/backfill progress resumes the complete uncommitted window;
a member switch discards the partial answer. Add a verified independent member to drain old
coverage, and use reconciliation to check custody and ledger effects. Review before removing a
member whose history still needs independent checks.

## Wrong-watermark recovery

A finalized hash conflict freezes the chain persistently. A numeric-only poisoned floor can
enter the same owner recovery command; its audited transaction freezes the chain before repair. Never lower a database watermark by
hand or roll back configuration to bypass it. Preserve the database and investigate the agreed
A/B genesis, finalized headers and affected credited deposits first. A 0.6 height-only cursor
gets its first hash only after A/B agreement at that numeric height; one above agreed heads
requires this recovery procedure.

With owner approval, stop topup and every writer, retain a backup, and use the new pinned image
with the owner `DATABASE_URL`/password file and sealed RPC keys. These commands acquire the
exclusive lease-owner lock and refuse while runtime writers hold it:

```sh
topup rpc recover --config /etc/topup/topup.yaml --chain 11155111 --block 123456 \
  --actor reviewed-operator --reason 'incident reference and reviewed A/B anchor'
topup rpc resume --config /etc/topup/topup.yaml --chain 11155111 --max-windows 16
```

Recover verifies A/B agreement, opens an audited epoch with its own window/replay baseline and preserves old watermarks, cursors and
address progress. It repairs `created_block` conservatively to genesis, clears `backfilled` and
`backfilled_through`, clears pending display progress and its cursor timestamp, resets the
confirmed/reconciliation cursors and queues historical review;
the chain stays frozen. Resume replays bounded complete windows, with atomic owner-only writes
and retained progress. Repeat it while it reports incomplete replay. Every credited deposit's
stored branch, finalized inclusion and receipt transfer (token, sender, recipient and amount)
are checked against both groups; any mismatch refuses unfreeze and requires an
explicit ledger reconciliation. No automatic compensating credit is invented. Only successful
replay, receipt and factory-ledger branch checks atomically save both new cursor anchors and
clear the recovery freeze. New quotes wait for a fresh scanner timestamp after restart. Run regular reconciliation and inspect
findings before restarting topup. Reconciliation freezes are separate and remain in force.

## Migration and rollback

Accepted chain role/group ids cannot be renamed or swapped through configuration; preserve them
through upgrades and rollback.

Before upgrading staging, snapshot the database, stop writers, migrate the schema and validate
the singleton config with the new image. Keep route versions, factories, assets and existing
member ids. Keep sealed keys under their explicit existing names. Verify first acceptance,
A/B cursor anchoring, head progression, usage and pending-review metrics in the stopped/local
rehearsal, then use the normal reviewed deployment process.

Before 0.7.0 writes, rollback restores the old image and old config together. After it accepts
watermarks or processes deposits, stop writers and restore/reconcile a reviewed snapshot or
complete an owner-reviewed rescan; 0.6.0 cannot enforce the new persisted safety state. Do not run
migration down against a live service. Config rollback alone never lowers watermarks or repairs
address floors. All commands in this runbook are operator procedures, not automatic live actions.
