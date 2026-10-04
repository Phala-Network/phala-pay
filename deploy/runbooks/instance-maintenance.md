# Instance maintenance stuck after an upgrade

Start here when mutations return `503 service_maintenance` with `Retry-After: 5` after a deploy
failed. Reads keep working while the process is up. Gateway/network failures during the CVM
restart are a different symptom: resume cannot reach a stopped instance. This runbook changes
only the instance `mutations` pause, never business incident pauses.

## Inspect

Use the [runbook environment and admin helper](README.md#environment), with the running
instance's public origin and verification key id. Inspection uses the operator's `admin_key`,
held outside GitHub Actions. A maintenance-only key cannot inspect this or any other admin read
route: it receives audited `403 permission_denied`:

```sh
admin GET /v1/admin/instance/pause | jq
```

The response names `owner` (workflow run id and attempt, or `startup` before the first healthy
probe), `paused_scopes`, and display deadline Unix `expires_at`. Compare it with the run's
`maintenance.json` and `unavailability.json`. An expired lease reports empty effective scopes.
Leases cannot exceed 900 seconds; expiry uses the process's monotonic clock and needs no runner
or expiry worker. Audit records persist, while the maintenance lease does not survive process
exit. The replacement starts paused and automatically opens admission at its first successful
`/healthz` check. That check never clears an explicit deployment pause on the old process. If the
API is unreachable, wait for CVM recovery. Do not repeatedly renew the lease.

## Manual clear

Verify that the intended version is healthy, or that the failed upgrade left the old version
serving. Check the deployment record and `/healthz` before resuming. Use the owner returned by
inspection, not a stale run's id:

```sh
admin POST /v1/admin/instance/resume \
  '{"owner":"<owner from inspection>","reason":"<ticket>: deployment failed; serving version verified healthy"}'
admin GET /v1/admin/instance/pause | jq
```

The full admin key still starts/clears maintenance. To clear with a separate maintenance key,
use the same `admin` helper with `ADMIN_KEY_FILE=maintenance.pem` and
`ADMIN_KEY_ID=maintenance/<Environment>-v1` for the **POST resume only**, then restore the admin
credential for inspection. Take the owner from the operator's inspection or the deployment's
`maintenance.json`. The workflow uses `TOPUP_MAINTENANCE_PRIVATE_KEY_PEM` and
`TOPUP_MAINTENANCE_KEY_ID`; its public key must appear in the running config's `maintenance_keys`.
A missing/rotated key produces `401`; a valid maintenance key on any other admin route produces
audited `403`. Check key id/public key and [rotation ordering](../README.md#planned-upgrade-admission-and-downtime),
and use the offline operator key for recovery. Never substitute the full admin key in CI.

The resume is audited and returns empty `paused_scopes`. A mismatched owner cannot lift an active
lease. A canceled runner or failed clear cannot freeze the service permanently: expiry restores normal
admission, and process exit discards its lease. Persistent 503 after the deadline with a different error code needs that error's
runbook (for example [restore reconciliation](restore.md) for `service_restoring`).

## Done when

The instance pause reports empty scopes and a merchant mutation is admitted again. Existing
account/customer/route pauses remain in force. Preserve the deployment's measured outage, even
when the upgrade failed; an incomplete window means recovery was not observed, not zero downtime.
