# Operations runbooks

Each runbook starts from a Sentry alert or Crons monitor ([deploy/README.md, "Sentry"](../README.md#sentry))
and works only through the surfaces a production CVM offers: it has no SSH, no logs, and no
database access. Run `make runbook-check` after editing a runbook: it checks every `topup` command
against the CLI and every API path against `crates/topup/openapi.json` and
`crates/topup/openapi.admin.json`.

## Surfaces

| Surface | What it shows or does |
|---|---|
| Sentry | the issue: its `alert` tag and grouping tags (`route`, `state`, `check`, `chain_id`, `scope`, `id`), a `runbook` link, and the log line's fields (for example `deposit_id`, or a finding's `subjects`, `expected`, `observed`); at most one event per issue every 10 minutes. Crons monitors for every loop; an Uptime monitor on `/healthz` |
| Daily report, admin-signed `GET /v1/admin/reports/daily` | per route: `deposits_by_state`, `age_in_state_max_seconds`, `refunds_by_status`, `credited_undelivered` and `credited_undelivered_max_age_seconds` (credited deposits whose `deposit.credited` is not delivered yet), `unflushed_balance_atomic` (what forwarders still hold for merchants to sweep), `open_rate_lock_exposure_atomic`, and `rejected_holds_atomic`; globally `exposure_minor`, the last reconciliation round's `failed_checks`, the active `reconciliation_blocks` (`block_key`, `scope`, `check`, `reason`), and `failing_webhook_endpoints`: every enabled endpoint of any account whose oldest undelivered event is older than `failing_for_hours` (default 24; `?failing_for_hours=` 1 to 720), with its `account`, `livemode`, `url`, `pending_deliveries`, `oldest_pending_at`, and `last_attempt_status` ([outbox backlog](outbox-backlog.md)) |
| Deposit view, admin-signed `GET /v1/admin/deposits/{id}` (`dep_…`) | the deposit as its account sees it, with `admin`: the processing `state`, route, transition timeline (each step's evidence), and webhook `events` (`id`, `type`, `delivered_at`). The merchant finds deposits with its own `GET /v1/deposits?tx_hash=…` or `?client_reference_id=…` |
| Attestation, `GET /v1/attestation?nonce=`, with an account's API key | that account's webhook keys in the key's mode ([verification](../README.md#attestation-ingress-and-egress)) |
| Chain | `cast` reads through both RPC providers: balances, nonces, receipts, `addressOf`, the factory's `Flushed` and `FlushFailed` logs |
| Admin actions, admin-signed | route and account `pause`/`resume` of the scopes `quotes`, `settlement`, `refunds`; a treasury's crediting `pause`/`resume`; a customer's `quotes` pause; deposit `nudge` (`dep_…`); account creation and update (`charges_enabled` for live access, `restricted`, `max_unfinalized_credit`, `contact`) and recovery keys ([deploy/README.md, "Account credentials"](../README.md#account-credentials)); reconciliation block `lift`; after a restore, the freeze's status and the reconciliation under `/v1/admin/restore` ([Reconciliation after a restore](restore.md)). Merchants manage their webhook endpoints and resend their events themselves |
| Phala Cloud, **HUMAN-ONLY** with the Environment's `PHALA_CLOUD_API_KEY` | `deploy/phala cvms restart "$TOPUP_CVM_ID"` (or `stop`), in the kit's directory (`deploy/phala` runs its locked CLI; `npm ci --prefix deploy/tools --ignore-scripts` once): the whole CVM, every container; state is in the database, so loops resume from it |

Database rows the API does not expose (reconciliation findings, `flushed` and `flush_failures`,
delivery attempts,
events not about a deposit, audit) and log lines other than the errors and alerts Sentry receives
are not observable in production. A restore-check instance ([RESTORE.md](../RESTORE.md)) serves
the same read API on a copy restored from backup and reports row counts and a full reconciliation
round's findings on its `/healthz`.

## Environment

Load values from the attested configuration and the admin's key store, never from chat or a
ticket. `BASE_URL` must be the service's `public_origin` (the Environment's `topup.yaml`; Phala's
instance: `https://pay-api.phala.com`, staging `https://pay-api-staging.phala.com`), or signatures
fail with `401`; `ADMIN_KEY_ID` is its `admin_key.id`, `admin/<Environment>-v1` unless rotated
([deploy/README.md, "Attested settings"](../README.md#attested-settings)).

```sh
export BASE_URL="https://pay-api-staging.phala.com"   # the Environment's public_origin
# The affected route, one of deploy/phala.md's "Staging routes" on staging; Base Sepolia's are
# CHAIN_ID=84532 with providers base-sepolia-a/-b.
export ROUTE=phala-cloud-sepolia-pha-usd CHAIN_ID=11155111
export RPC_PROVIDER_A_URL=https://provider-a.example RPC_PROVIDER_B_URL=https://provider-b.example
export FACTORY=0x... IMPLEMENTATION=0x... TOKEN=0x... TREASURY=0x...
export ADMIN_KEY_FILE=admin.pem ADMIN_KEY_ID=admin/production-v1
# admin METHOD PATH [JSON BODY]: signs the exact body (single-use, valid five minutes) and sends it.
admin() {
  printf '%s' "${3:-}" > /tmp/topup-admin-body
  mapfile -t headers < <(deploy/runbooks/sign-admin-request.sh "$1" "$BASE_URL$2" \
    /tmp/topup-admin-body "$ADMIN_KEY_FILE" "$ADMIN_KEY_ID")
  curl --fail-with-body -sS -X "$1" -H 'content-type: application/json' \
    -H "${headers[0]}" -H "${headers[1]}" -H "${headers[2]}" \
    --data-binary @/tmp/topup-admin-body "$BASE_URL$2"
}
admin GET /v1/admin/reports/daily | jq
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["settlement"]}'
```

A pause answers `200` with the route's `paused_scopes`; `resume` takes the same body. The service
sends no transactions, so no pause stops a sweep: anyone can call the factory's `flush`, and a
forwarder only ever pays its own treasury. Route config (providers, caps, thresholds) is attested:
changing it is a route PR and Deploy `upgrade` ([deploy/README.md, "Deploy"](../README.md#deploy)).

## Alert and symptom index

| Alert, monitor, or symptom | Runbook |
|---|---|
| `TopupRpcGroupUnavailable`, `TopupRpcChainFrozen`, `TopupRpcMemberQuarantined`, `TopupRpcMemberCooldown`, `TopupRpcQuotaPressure`, `TopupRpcUnclassifiedError`, `TopupRpcAnchorUnavailable`, `TopupRpcRecoveryUnavailable`, `TopupRpcMetricsRefreshFailed` | [RPC health](rpc-health.md) |
| `TopupReconciliationMismatch` (`check:address_derivation` or `check:custody_balance`), `400 chain_frozen` | [Chain frozen](chain-frozen.md) |
| `TopupReconciliationMismatch` (other `check`), `topup-reconciler` | [Reconciliation mismatch](reconciliation-mismatch.md) |
| `TopupDepositStateAgeExceeded` (`state:detected` or `state:confirmed`) | [Provider disagreement](provider-disagreement.md), then [Price outage](price-outage.md) |
| `TopupLockExposureNearCap`, `400 exposure_cap_exceeded` | [Lock exposure near cap](lock-exposure-near-cap.md) |
| `TopupLockExpiryFailing`, `topup-lock-expiry` | [Lock expiry worker failure](lock-expiry-worker-failure.md) |
| `topup-scanner-<chain_id>` | [Scanner lag](scanner-lag.md) |
| `TopupHeartbeatStale`, `TopupTreasuryProgressAge`, `TopupRefundProgressAge`, `TopupCertificateExpiry`, `TopupCertificateProbeFailed`, `TopupBusinessProbeFailed` | [Business health](business-health.md) |
| `topup-backup` | [Backup age](backup-age.md) |
| `TopupOutboxBacklog`, `TopupOutboxStalled`, `TopupOutboxInternalFailure`, `topup-outbox-test`, `topup-outbox-live`, `outbox delivery claim failed` or `outbox delivery failed`, daily report `credited_undelivered` or `failing_webhook_endpoints`, a merchant reports missing webhooks or credits | [Outbox backlog](outbox-backlog.md) |
| `TopupUnsupportedInflows`, rejected funds at the treasury | [Rejected funds at treasury](rejected-funds-at-treasury.md) |
| `TopupTreasurySanctioned`, a treasury on a sanctions list | [Treasury change, "Sanctioned treasury"](treasury-change.md#sanctioned-treasury) |
| `TopupDeliveredCreditSanctioned`, a credit delivered before a restore whose sender is now listed | [Reconciliation after a restore, "Sanctioned delivered credit"](restore.md#sanctioned-delivered-credit) |
| `TopupDepositReversed`, `TopupDepositPendingAfterReorg`, `topup-finality-watch` | [Deposit reversed or pending after a reorg](deposit-reversed.md) |
| Merchant reports a secret key exposed or lost, or requests it did not make | [API key compromise and key recovery](api-key-compromise.md) |
| Unswept credited deposits, a `FlushFailed` target | Not a platform alert: the merchant sweeps with its own wallet, and a target whose transfer failed (a token or treasury refusing it) is the merchant's to resolve ([deploy/README.md, "Sweeping"](../README.md#sweeping)) |
| Database loss, restore drill | [RESTORE.md](../RESTORE.md) |
| After a restore: merchants get `503 service_restoring`, `GET /v1/admin/restore` shows `frozen` | [Reconciliation after a restore](restore.md) |
| A merchant's refund stays `pending` or `failed` | Not a platform action: the merchant pays refunds from the treasury of the deposit's address and attaches the transaction with `POST /v1/refunds/{id}/mark_paid`; a `failed` refund's `failure_reason` says why ([integration guide, §3](../../docs/integration.md#3-refunds)) |
| A merchant's treasury change, or a pending `treasury.created` it did not request | [Treasury change](treasury-change.md) |
| A treasury reported compromised: hold its payments uncredited | [Treasury crediting pause](treasury-credit-pause.md) |
| Removing a route version or a chain's last route | [Route or chain retirement](route-retirement.md) |
| On a USDT route: a nonzero fee (`basisPointsRate`, `maximumFee`) or a `Params` event, a `FlushFailed` with an empty `reason`, a forwarder or treasury on Tether's blacklist | [USDT fee switch and blacklist](usdt-issuer-controls.md) |
| Payment sent on another EVM chain | [Wrong-network deposit](wrong-network-deposit.md) |
| Any customer-impacting incident | [Incident communication](incident-communication.md) |

## Exercise status

Operators record exercises for their own instance. Run `make runbook-check` to validate the
commands against the current CLI and OpenAPI; local payment scenarios are in the
[sandbox](../sandbox/README.md#scenarios). Contact verification, Safe operations, and publication
require human exercises.

Phala's historical exercise results and limitations are in the evidence for
[the local runbooks](https://github.com/Phala-Network/phala-pay/pull/59) and
[the full CVM restore drill](https://github.com/Phala-Network/phala-pay/pull/248).

## Synthetic alert validation

Sentry is the only channel. Before rollout, use the **staging** DSN loaded from the operator's
secret store and run each command below from the candidate image. These commands only emit
synthetic events (`environment:staging`, `component:synthetic`); they do not change business data.
They use the same tracing layer, event throttle, sanitization, fingerprints and runbook mapping
as runtime alerts. Exit success means the SDK queued/flushed the event, **not** that Sentry
accepted or notified it. Record each resulting Sentry issue URL and notification receipt in the
staging deployment evidence; do not mark an exercise passed without both. No staging exercise
was performed by the implementation PR.

| Runtime alert | Synthetic staging command | Detector verification |
|---|---|---|
| `TopupOutboxBacklog` | `topup alert-test --alert TopupOutboxBacklog` | test `business_thresholds_emit_sentry_events_at_boundaries`: count ≥ 1,000 or oldest ≥ 24 h, even when unclaimed |
| `TopupOutboxStalled` | `topup alert-test --alert TopupOutboxStalled --severity critical` | fake-clock test `retries_do_not_hide_stalls_and_success_or_idle_resets_clock`: backlog with no success for 15 min; success and idle reset; startup gets 15 min grace |
| `TopupOutboxInternalFailure` | `topup alert-test --alert TopupOutboxInternalFailure --severity critical` | unavailable signer test `signer_failure_reaches_sentry_without_key_or_payload`; proxy connection test `unreachable_proxy_alerts_without_penalizing_endpoint` |
| `TopupHeartbeatStale` | `topup alert-test --alert TopupHeartbeatStale --severity critical` | same boundary test: missing heartbeat or age ≥ 180 s, independent of WAL activity |
| `TopupTreasuryProgressAge` | `topup alert-test --alert TopupTreasuryProgressAge` | same boundary test: unapplied effective treasury age ≥ 1 h |
| `TopupRefundProgressAge` | `topup alert-test --alert TopupRefundProgressAge` | same boundary test: pending attached refund age ≥ 1 h since `paid_at` |
| `TopupCertificateExpiry` warning | `topup alert-test --alert TopupCertificateExpiry` | parsed DER with a fake clock at 14 days; test `certificate_expiry_probe_emits_warning_and_critical_events` |
| `TopupCertificateExpiry` critical | `topup alert-test --alert TopupCertificateExpiry --severity critical` | same probe test at three days; no event at 15 days |
| `TopupCertificateProbeFailed` | `topup alert-test --alert TopupCertificateProbeFailed --severity critical` | invalid DER rejected; TLS/network failures raise this alert |
| `TopupBusinessProbeFailed` | `topup alert-test --alert TopupBusinessProbeFailed --severity critical` | failed/timed-out database scan |

Production preflight integration (batch 2 owns deployment scripts): add the following command
with the deployment's `SENTRY_DSN` already present in its environment. Missing/empty/malformed
DSNs fail without printing their value; runtime still fails fast on malformed DSNs.

```sh
topup config check --require-sentry topup.yaml
```

If the existing preflight command uses `--secrets`, add `--require-sentry` to that invocation.
For batch 2's Rust disk probes, call
`topup::observability::emit_alert("TopupDiskPressure", "database", "critical", used_bytes, limit_bytes)`
with numeric `i64` observations. This emits an alert-tagged WARN through the normal Sentry path.
Keep names and component/severity dimensions stable, never include credentials, URLs or user
input. Add the disk alert's synthetic exercise to this table when that probe is implemented.
