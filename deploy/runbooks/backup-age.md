# Backup age

**Trigger:** the `topup-backup` monitor: three `error` check-ins in a row (the WAL-G success marker
is older than 60 seconds), missing/stale base backup, excessive archive backlog or LSN gap, or
missed check-ins.

**Impact:** the service continues, but the recoverable point falls behind the one-minute RPO; a
database failure now would lose more data, on every route.

## First steps

1. During a restore the replacement archives nothing and runs no loop: expected, see
   [RESTORE.md](../RESTORE.md#the-restore-check-variant). A staging drill never affects this
   monitor.
2. List the newest archived segment with the owner's storage credentials:

   ```sh
   aws s3 ls "${WALG_S3_PREFIX%/}/wal_005/" --endpoint-url "$AWS_ENDPOINT" | tail -1
   ```

3. Check object storage health and the sealed credentials' permissions (R2 dashboard).

Inspect these shared-volume markers separately:

- `last-base-backup-unix-seconds`: a verified current-timeline base backup, at most 48 hours old.
  Missing means no restorable base backup has been verified. First-backup failures keep retrying
  with capped backoff; WAL uploads alone cannot make health green.
- `last-backup-unix-seconds`: the uploaded segment's data mtime, not upload completion time.
  Old backlog uploads retain their old age.
- `wal-progress`: observation Unix time, oldest `.ready` backlog age in seconds, and bytes from
  the last archived segment end to the current insert LSN. Health requires observation ≤45 seconds,
  backlog ≤60 seconds, and gap ≤16 MiB. Missing, malformed or future timestamps fail closed.

Check `wal-g backup-list --json` through `walg-cron run base`, compare names with the current
PostgreSQL timeline, and inspect `pg_stat_archiver` and `pg_wal/archive_status`. Object arrival
time alone cannot prove recovery freshness. See [operation deadlines](../RESTORE.md#bounded-backup-and-recovery-operations).

## Decide

- New segments keep arriving but the monitor is stale: the marker is not refreshed, or `topup`
  cannot read it; escalate to Engineering.
- No new segments and storage reachable: archiving or the heartbeat that forces one segment a
  minute has stopped, or the credentials lost write access. Fix the credentials and re-seal them
  ([deploy/README.md, "Sealing the secrets"](../README.md#sealing-the-secrets)); otherwise
  **HUMAN-ONLY:** restart the CVM (`deploy/phala cvms restart "$TOPUP_CVM_ID"`).
- Storage unavailable: a provider outage; escalate and never delete WAL or backups.

## Done when

A current-timeline base backup and WAL data younger than one minute are verified, `topup-backup` checks in `ok`, and a later
[restore drill](../RESTORE.md#staging-restore-drill) passes within RPO and RTO. Never take an
unencrypted backup as a substitute.
