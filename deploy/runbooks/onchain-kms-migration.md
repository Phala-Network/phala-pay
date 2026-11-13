# Migrate an application to Base onchain KMS

Run this for **one environment at a time: staging first**, then production after staging passes.
The owner agrees the window and executes the Safe transactions. This procedure does not itself
authorize a live cutover. Use attested TEE-to-TEE transfer: only signed metadata and encrypted
archives leave the TEEs. Never download or decrypt a plaintext dump on the operator's machine.

A new app changes webhook, client-secret, database and backup keys. Deposit addresses stay the
same. Old quote/deposit-address secrets fail; preserved idempotency responses can still contain
them. Merchants needing fresh secrets must use **fresh idempotency keys**. Read the authoritative
[migration rules](../../docs/architecture.md#kms-application-migration),
[Base prepare/sign/commit procedure](onchain-kms-upgrade.md), [device policy](device-drift.md)
and [restore procedure](../RESTORE.md) first.

## Preconditions and owner actions

- Release vN containing [deploy support (#628)](https://github.com/Phala-Network/phala-pay/pull/628),
  [migration tooling (#629)](https://github.com/Phala-Network/phala-pay/pull/629) and
  [export-to-service rollback (#632)](https://github.com/Phala-Network/phala-pay/pull/632). Download and
  [verify its kit and images.json](../../docs/self-hosting.md#verify-a-release).
- **Freeze the release from prepare through closeout.** Every release changes image digests
  and therefore compose hashes, even if the environment files do not change. Prepare all
  import, service and restore-check approvals with the exact release that commit will use;
  never sign an older release's preparation. A release change requires fresh preparation,
  review and approvals, plus repeating step 1 for the old service before export.
- Adopt that release in the caller: repin workflow SHA and `version` together, expose modes
  `provision`, `upgrade`, `prepare`, `commit`, and forward `transaction_hash`, `gateway_domain`,
  `variant`, `migration_plan` using the exact caller diff in #628. A caller still pinned to
  v0.13.6 cannot perform this migration. Use the reusable workflow's existing
  `environment_dir` input to select the reviewed old or new directory for each run below.
- Reserve the chosen Base-capable node, verify its device/quote and gateway, and keep the reviewed
  OS measurement. No OS upgrade belongs in this window. Arrange one measured standby device;
  `allowAnyDevice=false`, `requireTcbUpToDate=false`. The staging reference product stays on Phala KMS.
- Reserve an empty **new WAL-G prefix** and a separate unique **migration object prefix** on the
  environment's HTTPS S3 endpoint/region. Existing sealed AWS credentials must cover the required
  source backup, transfer and destination backup operations. Owner seals the complete runtime
  env-name set on the new CVM; no secret values go into plans, GitHub artifacts or Safe batches.
- Confirm merchant contacts, pin rollout/restart procedure and fresh deposit-secret fetching.
  Finish any webhook key roll first. Preserve old pins, DNS, CVM id, app id and environment commit.
  Book the signer for preparation and incident response ([response target](onchain-kms-upgrade.md#upgrades-and-rollback)).

All three batches are on **Base (8453)**. Staging and production use treasury Safe
`0x8AB55148944F837040bbB887f8A5005f70FF7700` as DstackApp owner, with a separate app per
environment.
Verify each target app, device, hash and Safe nonce before signing.

| Owner action | Prepare/sign | Execute | Calls |
|---|---|---|---|
| Safe batch #1 | T-1, from Deploy `prepare` | T-1, before provision commit | Base KmsAuth `deployAndRegisterApp(Safe,false,false,deviceId,importHash)`; record the receipt's new DstackApp in `dstack-app` |
| Safe batch #2 | T-1, after new app is known | T-1 | New DstackApp `addComposeHash(restoreCheckHash)`, `addComposeHash(serviceHash)` without migration overlay, optional `addDevice(standbyDeviceId)`, and revocation of unused import hashes |
| Safe batch #3 | **Pre-sign at T-1**, Safe nonce ordered after #2 | **T0, immediately after imported-app verification, or on rollback** | New DstackApp `removeComposeHash(importHash)` (the initial provision/overlay hash); retain the no-overlay service and restore-check approvals |
| Runtime env and deploy state | T-1 | T-1/T0 | Seal env locally; select the correct `TOPUP_CVM_ID` and environment directory per run; never run concurrent deploys |
| DNS, merchant pins and route controls | Agree before T-1 | T0 | Owner switches TXT/CNAME; each merchant verifies/re-pins; admin resumes only after final acceptance |

Batch #3 needs no signature during downtime, but someone must execute its pre-signed transaction.
Do not insert another Safe transaction at its reserved nonce. If any approved bytes change,
prepare/review/sign again; do not substitute an unreviewed hash. Tokens last 14 days; commit
re-prepares and must reproduce the approved hash ([token rules](onchain-kms-upgrade.md#token-lifetime-and-hash-format)).

## T-7d to T-1: prepare both applications

The [staging Base environment](../environments/phala-network/staging/topup-okms/README.md)
records the completed rehearsal and its public import/export plans. A new attempt needs fresh
prefixes and reviewed T-1 evidence; do not reuse the completed rehearsal's plans.

Use the release's `deploy/` scripts below, `images.json`, and the
[admin helper](README.md#environment). Keep public evidence in a private, task-scoped directory
`EVIDENCE`. Set `OLD_ENV`, `NEW_ENV`, `OLD_CVM_ID`, `NEW_CVM_ID`, `OLD_APP_ID`, `NEW_APP_ID`,
`SAFE`, `GATEWAY` (new node's `gateway.dstack-base-<node>.phala.network`) and `DOMAIN` from
reviewed records. For Base on `prod5`, use `gateway.dstack-base-prod5.phala.network`, **not**
the old Phala KMS gateway `gateway.dstack-pha-prod5.phala.network`. The baseline render in
`provision.json` and every `gateway_domain` input must use the reviewed Base gateway.
App ids used in migration JSON are lowercase hex **without** `0x`; DstackApp contract arguments
use the `0x` address. `BASE_URL` is always the environment's configured public origin.
`OLD_ENV`/`NEW_ENV` and migration-plan inputs are committed paths relative to the caller's repo.
`TOPUP_CVM_ID` below means the **GitHub Environment variable**, not a shell override; freeze
other deploys while it is unset or switched. Set `PHALA_NODE` to the reviewed physical provision
node name (for example, `prod5`), starting with a letter and containing only letters, digits and
hyphens. Remove the legacy `PHALA_NODE_ID` variable; a nonempty value fails workflow validation.

Run the snippets in Bash with failure stopping enabled; define the
[evidence helper](#key-evidence) before its first use:

```sh
set -euo pipefail
umask 077
mkdir -p "$EVIDENCE"
```

1. **T-7d: release and old service (mandatory).** Deploy `mode: upgrade`, `variant: service`,
   `environment_dir: OLD_ENV`, with `TOPUP_CVM_ID=OLD_CVM_ID` and the old `kms: phala` settings.
   Verify the vN service, ingress and backup. Save its exact rendered artifact as
   `EVIDENCE/old-service.yml`, and its gateway as `OLD_GATEWAY`. Record its verified full
   app-compose hash and OS hash as `OLD_COMPOSE_HASH` and `OLD_OS_HASH` for checks and rollback.
   The old service must be on the **same exact release as the export run**, including any
   release adopted after this step. Skipping this fails at `Capture verified service Info
   before export`, before fencing; a stable app-derived export key does not satisfy the
   service-render check. In staging, run [43205920404](https://github.com/Phala-Network/phala-pay/actions/runs/43205920404)
   failed here; upgrading the old service in [43206317742](https://github.com/Phala-Network/phala-pay/actions/runs/43206317742)
   allowed export [43206975626](https://github.com/Phala-Network/phala-pay/actions/runs/43206975626) to pass.
2. **T-1: fetch the old export key.** Challenge the admin endpoint with a fresh nonce; verify
   it with [Key evidence](#key-evidence) before using its public key:

   ```sh
   NONCE=$(openssl rand -hex 32)
   admin GET "/v1/admin/migration/export-key?nonce=$NONCE" >"$EVIDENCE/export-key.json"
   verify_key_evidence export "$EVIDENCE/export-key.json" "$OLD_APP_ID" \
     "$OLD_COMPOSE_HASH" "$OLD_OS_HASH" "$NONCE"
   ```

3. **T-1: review the new environment.** Commit `NEW_ENV/topup.yaml` and `compose.yaml`, retaining
   routes, admin/maintenance public keys, public origin and ingress domain. Change WAL-G's
   `WALG_S3_PREFIX` on both postgres and backup to the new empty prefix. Commit `kms` containing
   `base`, `dstack-owner`, `os-image-hash`, a complete `provision.json` with a pinned physical
   node name (`node: "prod5"` for the prepared staging environment, never `node_id`/`teepod_id`) and
   reviewed full `compose_file` baseline (optionally `app-compose.json`), following
   [initial provisioning](onchain-kms-upgrade.md#initial-provisioning). Review all backend
   defaults, including the stored string `manifest_version: "3"` and both gateway flags.
   Initial prepare omits the deprecated `tproxy_enabled` alias only in the creation payload;
   its restored value must still match the baseline's full local hash. Never accept unexplained
   normalization. `dstack-app` is committed after batch #1. Prepare alone creates no CVM;
   live `CreateTappRequest`/Safe-deployer acceptance is checked only after batch #1.
   Create and review the import plan (the `object` is a prefix, not an archive filename):

   ```sh
   # MIGRATION_OBJECT is a new s3://bucket/unique-prefix for this attempt.
   jq -n --arg source "$OLD_APP_ID" \
     --arg key "$(jq -er .public_key "$EVIDENCE/export-key.json")" \
     --arg object "$MIGRATION_OBJECT" \
     '{migration_import:{source_app_id:$source,export_public_key:$key,object:$object}}' \
     >"$NEW_ENV/migration-import.json"
   ```

4. **T-1, no more than 24h before T0: provision.** Save the old `TOPUP_CVM_ID`, temporarily unset
   it, and dispatch Deploy `prepare`, `variant: migration-import`,
   `migration_plan: NEW_ENV/migration-import.json`, `environment_dir: NEW_ENV`,
   `gateway_domain: GATEWAY`. Check its locally recomputed hash/device summary; execute batch #1.
   Commit the receipt-derived `dstack-app`, then dispatch `commit` with the same inputs and the
   batch #1 transaction hash. Record `NEW_CVM_ID`, `IMPORT_HASH` and `NEW_DEVICE_ID` from the
   artifact; save its rendered compose as `EVIDENCE/import.yml` and set `TOPUP_CVM_ID` to the new CVM.
   Verify the provision response, stored request and on-chain owner/hash/device/proxy policy
   as in the upgrade runbook. **Current workflow limitation:** `POST /cvms` can succeed while
   the initial commit run subsequently fails its wait/readback. An unsealed Base import cannot
   boot: `postgres-walg` refuses without S3 credentials. Do not wait for an unsealed attestation
   or retry initial provisioning after creation succeeded. Keep the created CVM id and artifact,
   let that run finish, then seal immediately in step 5. This is creation, not acceptance.

   The migration-import render **includes `dstack-ingress` and `topup`**; they start only after
   `import-verify` succeeds. Before DNS switch the public origin is the **old service**.
   Deploy therefore skips the import's `PUBLIC_URL` maintenance lease, origin probes, DNS
   output and TLS checks. Preserve that isolation when retrying an existing import CVM.
   Existing-CVM commit can apply/start an approved compose, but its running/attestation waits
   cannot accept an import still waiting for the manifest either. Full acceptance is at T0.
5. **T-1: seal locally, then start.** From the **operator machine**, prepare `.env.ENV` with
   the complete secret set and pull the pinned image as directed by this runbook. Use Phala
   CLI **1.2.7** for Base sealing: CLI 1.1.22 dropped `kms_info.k256_pubkey` and failed with
   `KMS k256_pubkey is required`. Use the supported `-e` path, with the full plan-aware preflight:

   ```sh
   deploy/preflight.sh --env .env.ENV --compose "$EVIDENCE/import.yml" \
     --environment-dir "$NEW_ENV" --migration-import "$NEW_ENV/migration-import.json" --offline
   deploy/phala envs update "$NEW_CVM_ID" -e .env.ENV
   ```

   `envs update` leaves a stopped CVM stopped. After sealing has settled, inspect the state;
   start only when `in_progress=false` and `status=stopped`:

   ```sh
   deploy/phala cvms get "$NEW_CVM_ID" --json >"$EVIDENCE/sealed-cvm.json"
   jq -e '.in_progress == false and .status == "stopped"' "$EVIDENCE/sealed-cvm.json" >/dev/null
   deploy/phala cvms start "$NEW_CVM_ID" --json
   ```

   If the CVM is already running/starting, do not issue a second start; inspect its current
   operation. The import publishes `import-info.json` and `recipient.json` and waits; ingress,
   API, migrations and backup stay gated. While waiting for the manifest, Cloud reports
   `starting`, `is_online=false`, and its attestation endpoint has no compose hash. That state
   does not prevent verifying the recipient's own quote in step 6.
   Default manifest wait is **172800 seconds (48h)** from import invocation; allow enough margin
   for the window. Do not run ordinary service acceptance against the old origin yet.
6. **T-1: verify recipient and render export.** Fetch only public metadata using locally loaded
   AWS credentials; use the new provisioning artifact's hash and committed OS hash. This is
   the **pre-export recipient quote gate**; complete it before export. Cloud's full
   `verify-attestation` check must wait until import completes and Cloud reports `running`:

   ```sh
   aws --endpoint-url "$AWS_ENDPOINT" s3 cp \
     "${MIGRATION_OBJECT%/}/recipient.json" "$EVIDENCE/recipient.json"
   aws --endpoint-url "$AWS_ENDPOINT" s3 cp \
     "${MIGRATION_OBJECT%/}/import-info.json" "$EVIDENCE/import-info.json"
   NEW_OS_HASH=$(cat "$NEW_ENV/os-image-hash")
   verify_key_evidence import "$EVIDENCE/recipient.json" "$NEW_APP_ID" \
     "$IMPORT_HASH" "$NEW_OS_HASH"
   jq -n --arg recipient "$(jq -er .public_key "$EVIDENCE/recipient.json")" \
     --arg object "$MIGRATION_OBJECT" \
     '{migration_export:{recipient:$recipient,object:$object}}' >"$OLD_ENV/migration-export.json"
   deploy/render.sh --export --migration-export "$OLD_ENV/migration-export.json" \
     --images images.json "$OLD_ENV" >"$EVIDENCE/export.yml"
   ```

   Review/commit the public export plan and exact render; no API, ingress, heartbeat or migrator
   may remain. The recipient key is app-derived and stable; an import restart changes its nonce,
   so re-verify current evidence and re-run export, without re-rendering for a nonce-only change.
   Save both the recipient document and verified quote result. `import-info.json` is untrusted
   transport saved for the later full OS/vm_config check; fetching it alone proves no acceptance.
7. **T-1: approve closeout and restore.** Render the final service and restore-check with known
   new app/node; `RESTORE_URL` is `https://<new-app-id>-8081.dstack-base-<node>.phala.network`
   (app id without `0x`), using the Base gateway domain rather than `dstack-pha-<node>`:

   ```sh
   deploy/render.sh --images images.json --gateway-domain "$GATEWAY" "$NEW_ENV" \
     >"$EVIDENCE/service.yml"
   deploy/render.sh --restore-check --images images.json --origin "$RESTORE_URL" "$NEW_ENV" \
     >"$EVIDENCE/restore-check.yml"
   for variant in service restore-check; do
     jq --rawfile compose "$EVIDENCE/$variant.yml" \
       --rawfile pre_launch deploy/phala-cloud-pre-launch.sh \
       '.compose_file | .docker_compose_file=$compose | .pre_launch_script=$pre_launch' \
       "$NEW_ENV/provision.json" >"$EVIDENCE/$variant.app-compose.json"
     hash=$(node deploy/tools/onchain-kms.mjs hash "$EVIDENCE/$variant.app-compose.json")
     node deploy/tools/onchain-kms.mjs batch "$EVIDENCE/approve-$variant.json" \
       "$SAFE" "$NEW_APP_ID" addComposeHash "$hash"
   done
   node deploy/tools/onchain-kms.mjs batch "$EVIDENCE/approve-standby.json" \
     "$SAFE" "$NEW_APP_ID" addDevice "$STANDBY_DEVICE_ID"
   node deploy/tools/onchain-kms.mjs batch "$EVIDENCE/remove-import.json" \
     "$SAFE" "$NEW_APP_ID" removeComposeHash "$IMPORT_HASH"
   ```

   Use the same reviewed baseline as provisioning; if `app-compose.json` exists it must agree.
   These are **full app-compose** hashes (sorted compact JSON SHA-256), never hashes of YAML.
   Keep inherited service port policy/allowed env names for restore-check. Combine the approval
   calls into Safe Transaction Builder batch #2 (include standby if needed), execute it, then
   pre-sign removal batch #3 at the next reserved Safe nonce. Record the no-overlay service hash
   as `SERVICE_HASH` and the restore-check hash alongside it.
   One **MultiSendCallOnly** transaction may combine #2's approvals with removal of unused
   import hashes from abandoned preparations. Review the MultiSend deployment/code and every
   inner call. Keep the current `IMPORT_HASH` approved until imported-app verification; its
   final revocation remains batch #3. Do not include it in the pre-T0 cleanup.
8. **T-1: DNS and merchants.** Lower TXT/CNAME TTL to 60 seconds early enough for the previous
   TTL to expire. Record old values; check any Let's Encrypt CAA account restriction. Notify each
   merchant of downtime, old-secret expiry, fresh idempotency keys, unchanged addresses and the
   required pin rollout. Confirm who can change DNS and execute #3 throughout the window.
   Complete the [Base gateway routing precheck](#base-gateway-routing-precheck) before T0.

### Base gateway routing precheck

The November 12, 2026 staging cutover needed a **Phala front-proxy fix** for custom domains on
Base gateways. Correct TXT/CNAME records alone did not prove routing: the shared front sent
custom-domain SNI to the old Phala gateway, while the Base instance passthrough worked.

Before production T0, run these probes against an already-running Base staging/control service
on the chosen gateway. Set `PROBE_DOMAIN` to its configured custom domain, `PROBE_GATEWAY` to
its `gateway.dstack-base-<node>.phala.network`, and `PROBE_INSTANCE_ID` to its verified instance.
Both probes must return that service's **ingress certificate**; save the certificate and
fingerprint. A reset or `no peer certificate available` blocks cutover:

```sh
PROBE_BASE_DOMAIN=${PROBE_GATEWAY#gateway.}
openssl s_client -connect "$PROBE_GATEWAY:443" -servername "$PROBE_DOMAIN" \
  -showcerts </dev/null >"$EVIDENCE/base-custom-sni.txt" 2>&1
openssl x509 -in "$EVIDENCE/base-custom-sni.txt" -noout -subject -issuer -fingerprint -sha256
openssl s_client -connect "$PROBE_GATEWAY:443" \
  -servername "$PROBE_INSTANCE_ID-443s.$PROBE_BASE_DOMAIN" \
  -showcerts </dev/null >"$EVIDENCE/base-instance-passthrough.txt" 2>&1
openssl x509 -in "$EVIDENCE/base-instance-passthrough.txt" -noout -subject -issuer -fingerprint -sha256
```

The new import's ingress is gated until import verification, so a pre-T0 probe of that instance
cannot pass. Before DNS switch, the migration's public origin still reaches the old service;
its certificate/health is not evidence for the new Base app. Repeat the passthrough probe with
the **new verified instance** after import completes, then the custom-domain probe after DNS
switch. A tls-alpn placeholder proves ingress reachability only; final acceptance still requires
the issued domain certificate and [ingress evidence](../README.md#attestation-ingress-and-egress).

## T0: fence, transfer, verify and close out

Record every transition in US Pacific time with its workflow run/commit. In the November 12
staging rehearsal, **fence → import complete was approximately 11 minutes**, and **closeout was
approximately 5 minutes**, excluding the external Phala gateway outage. These measurements
include an old-service release correction; they are not a production downtime guarantee.

| When | Action and gate |
|---|---|
| At least the maximum configured quote TTL before T0 | On **every route**, admin-pause `quotes`. Wait the full maximum TTL, not a fixed one-hour assumption. Take pre-T0 merchant deposit/quote listings and admin daily/restore snapshots; keep these customer records private. |
| T0 | Admin-pause `settlement` and `refunds` on every route; preserve pre-existing incident pauses. For **each live merchant**, check `GET /v1/webhook_endpoints`: every endpoint has `pending_deliveries=0`, `oldest_pending_at=null`; include RedPill with its restricted key and the smoke account. Check undelivered events too ([outbox](outbox-backlog.md)). Resolve any backlog before fencing; daily credited counters alone do not prove drain. |
| T0, fence | Set `TOPUP_CVM_ID=OLD_CVM_ID`; select `OLD_ENV`, `mode: upgrade`, `variant: export`, `migration_plan: OLD_ENV/migration-export.json`. Deploy takes the instance maintenance pause before replacing the compose. Its verified **export attestation** is the fencing evidence: no topup/ingress/writer, no health check or successful maintenance resume. Do not proceed without that artifact. Maintenance is an in-memory lease; persistent route pauses travel in the dump. |
| Transfer | Export runs final WAL switch/base backup to the **old prefix**, holds one read-only snapshot for counts and `pg_dump`, encrypts to the pinned recipient and signs/uploads the manifest last. Import checks signature/source/recipient/prefix/current nonce/ciphertext hash, restores, then `import-verify` compares signed table counts, latest migration and heartbeat and commits the completion marker. A failed gate keeps topup down. New migrations/API/heartbeat/backup can start only after that gate. |
| Import complete, before DNS | Wait for Cloud `running` and the approved import hash, then run the **full migration-import attestation check below** with saved `import-info.json`. Record the verified new instance id. Probe its Base `<instance>-443s.<base-domain>` passthrough; ingress/topup now run after `import-verify`. |
| DNS switch, routes still paused | Point `_dstack-app-address.DOMAIN` TXT to `<new verified instance_id>:443`; change `DOMAIN` CNAME to the new `GATEWAY` if it differs, DNS only. Use the new attestation's **instance id**, not app id or CVM UUID. Wait for DNS/cache convergence, TLS certificate and `/healthz` 200. The public origin now serves the new app with all routes paused: no new quotes or credits. |
| Verify imported app | Verify custom-domain SNI and ingress evidence after DNS switch. Healthy startup under the fully attested dependency chain is evidence the signed snapshot checks and completion marker succeeded; there is no admin row-count/marker endpoint, production DB access or guest log surface. Record the public signed manifest from S3 and its snapshot counts. Check admin daily/restore/deposit reads against saved state; do not lift carried-over holds. |
| **Closeout, before resume** | **Immediately execute pre-signed batch #3**; confirm receipt success and `allowedComposeHashes(importHash)=false`. Set `TOPUP_CVM_ID=NEW_CVM_ID`, select `NEW_ENV`; Deploy `mode: commit`, `variant: service`, empty `migration_plan`, `transaction_hash: already-registered`. It reproduces the pre-approved no-overlay hash without a prepare PATCH, pauses at the now reachable public origin, re-verifies policy, applies the ordinary Cloud update and verifies the stored full compose hash/health/attestation/ingress. Both revocation and no-overlay deployment must finish in this T0 window; never leave the import path approved until T+7d. |
| Final acceptance and pins | Verify the **final service** compose hash/app id/OS/Base KMS and ingress evidence. Each merchant verifies its nonce-bound attestation against this final deployment ([merchant procedure](../../docs/integration.md#53-pin-your-accounts-webhook-keys)). The reference product re-pins from `/v1/attestation` on restart. RedPill and other merchants using static `PHALA_PAY_PINS` must replace those pins before resume; duplicate key versions cannot be pinned side by side. Confirm every merchant's restart/rollout completed. |
| Resume and stop | Admin-resume `settlement`, then `refunds`, then `quotes` on every route whose pause this migration owns. **First resume of writes is the rollback boundary.** Confirm merchant reads/payment/credit acceptance below, then `deploy/phala cvms stop "$OLD_CVM_ID"`. Keep the old CVM/storage and old prefix **at least 30 days**; never run another writer against either prefix. |
| Within 7 days | Run the [new-app restore drill](../RESTORE.md) using the pre-approved restore-check hash/device and new backup prefix. Record its results before widening onboarding. |

After import completes, run the full check deferred from T-1 step 6, before final import-hash
revocation. This proves OS/vm_config, compose/port/env policy and Base provider in addition to
the earlier recipient quote:

```sh
deploy/phala-cvm.sh wait --compose-hash "$IMPORT_HASH" "$NEW_CVM_ID" >"$EVIDENCE/import-running.json"
deploy/phala-cvm.sh attestation "$NEW_CVM_ID" "$IMPORT_HASH" >"$EVIDENCE/import-attestation.json"
deploy/verify-attestation.sh "$EVIDENCE/import-attestation.json" "$EVIDENCE/import-info.json" \
  "$NEW_APP_ID" "$EVIDENCE/import.yml" migration-import "$NEW_OS_HASH" "$NEW_ENV"
deploy/phala-cvm.sh instance-id "$EVIDENCE/import-attestation.json"
```

Route commands below use the signed `admin` helper; repeat for each route and record returned
`paused_scopes`. For resume, use the same path with `/resume` and only the scopes this cutover paused.
The Deploy workflow owns the separate [instance maintenance lease](instance-maintenance.md).
**All route pauses must be in place before export:** Deploy does not inspect database route
state, so a successful export deployment does not prove those business pauses were set.

```sh
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["quotes"]}'
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["settlement","refunds"]}'
admin GET /v1/admin/reports/daily
admin GET /v1/admin/restore
```

### Staging timeline: November 12, 2026 (PST)

| Time | Observed transition |
|---|---|
| 02:02 | Quotes paused; waited the configured one-hour maximum TTL. |
| 03:04 | T0: settlement/refunds paused; export started. |
| 03:06–03:11 | First export failed the old-service Info check; old service upgraded to the frozen v0.13.11 release. |
| 03:15 | Export run [43206975626](https://github.com/Phala-Network/phala-pay/actions/runs/43206975626) completed; import completed and started the gated service. |
| 03:17 | DNS switched to the new instance and `gateway.dstack-base-prod5.phala.network`. |
| 03:20 | Full migration-import attestation verified. |
| Until approximately 05:56 | Phala-side custom-domain gateway outage; certificate issuance at 05:56 PST (13:56 UTC) later established recovery. |
| 16:27–16:32 | Closeout [43337188731](https://github.com/Phala-Network/phala-pay/actions/runs/43337188731) deployed the approved final service. |
| 16:35 | Reference product restarted/re-pinned; resumed settlement, refunds, then quotes. |

The operator confirmed recovery and began closeout later that afternoon. The full paused
interval includes both the gateway outage and that delay; do not count it as migration runtime.

## Retry and rollback

- Each import invocation has a fresh random 32-byte nonce held in memory. **Import restart before
  completion requires re-running the entire export chain** (backup, snapshot, dump, encryption,
  upload) against current `recipient.json`. An old-nonce export is rejected before fetching or
  decrypting its dump; import keeps waiting with its current nonce, and the overall 48h deadline
  still applies. Wrong signatures, recipients and ciphertext hashes are fatal. If the deadline
  expires, restart the new CVM with `deploy/phala cvms restart "$NEW_CVM_ID"`, re-verify its
  fresh recipient evidence, then re-run the entire old export chain. A same-compose Deploy can
  be a no-op: it is not an exporter retry. Restart the **fenced exporter CVM** to re-run its
  one-shot chain, and verify its export attestation again.
- **Any retry after the old app resumed writes MUST use a fresh migration prefix.** Any retry
  of an abandoned cutover must revoke **every abandoned import hash**, verify each revocation,
  and use a fresh migration prefix and new import hash with reviewed plans/approvals and a new
  removal batch. A pre-signed #3 for an old hash cannot close out its replacement. This is
  separate from re-running export for the current waiting import's nonce. Do not reuse a
  partially restored destination: without a valid completion marker it fails the fresh-database
  gate; return to reviewed fresh provisioning.
- A completed `topup_migration.imported` marker makes reboot idempotent: verified matching
  signed completion evidence skips transfer/restore even after service writes. Empty/forged or
  mismatched markers fail closed. Original row counts are not rechecked after completion; never
  delete a marker or re-restore a running destination.
- **Before any write resume:** execute batch #3 if still pending, verify import-hash revocation
  as below, then stop the new CVM and use the rollback procedure. Executing #3 also consumes its
  reserved Safe nonce and unblocks the treasury Safe queue. Stopping a CVM alone does not close
  its import key-release path. The export only read its data; keep the new CVM stopped.
- **After write resume:** do not switch DNS back to the stale database. Fence the current writer;
  plan a reverse logical migration (new → old, reviewed import/export configs and approvals) or
  use [RESTORE.md](../RESTORE.md) and [reconciliation](restore.md) for writes/credits since cutover.
  Preserve immutable webhook ids and merchant deduplication throughout.

### Close an abandoned import path

Execute the pre-signed batch #3 if it has not executed. Set `REMOVAL_TX` to its executed
transaction hash, verify receipt success, then check the new app's owner, approved final service
hash, device and proxy pins. Read the import allowlist entry explicitly and require **false**;
`verify-onchain-app.sh` alone requires an approved hash, so its failure on an import hash would
not prove revocation. Record these results before declaring rollback complete, even if the
no-overlay service never deployed:

```sh
node deploy/tools/onchain-kms.mjs receipt "$REMOVAL_TX"
deploy/verify-onchain-app.sh "$NEW_APP_ID" "$SAFE" "$SERVICE_HASH" "$NEW_DEVICE_ID" \
  >"$EVIDENCE/abandoned-app-policy.json"
node --input-type=module - "$NEW_APP_ID" "$IMPORT_HASH" <<'JS' >"$EVIDENCE/import-revoked.json"
import {abi, address, client, hex} from './deploy/tools/onchain-kms.mjs';
const [app, hash] = process.argv.slice(2);
const chain = await client();
const allowed = await chain.readContract({address: address(app), abi,
  functionName: 'allowedComposeHashes', args: [hex(hash, 32)]});
if (allowed !== false) throw new Error('abandoned import hash is still approved');
console.log(JSON.stringify({app: address(app), import_hash: hex(hash, 32), allowed}));
JS
deploy/phala cvms stop "$NEW_CVM_ID"
```

Repeat for every abandoned import hash/its removal transaction. A retry starts only after all
those hashes are revoked, with a fresh migration prefix and a new approved import hash.

### Roll back before resume through Deploy

First complete [import-path revocation](#close-an-abandoned-import-path) and stop the new CVM;
keep both apps' routes paused. Restore the **old Phala KMS CVM** through the
[export-to-service Deploy path](../README.md#planned-upgrade-admission-and-downtime):

1. Re-select `TOPUP_CVM_ID=OLD_CVM_ID` and `environment_dir: OLD_ENV`. Use the **same release
   `version` and the same `ENVIRONMENT_DIR` commit as the export run**, including its reviewed
   export plan and OS pin. Otherwise source-export verification fails before mutation.
2. Revert TXT to the old verified instance id and CNAME to `OLD_GATEWAY` before the workflow's
   service health/TLS checks. The old exporter has no ingress until service is restored.
3. Dispatch Deploy with `mode: upgrade`, `variant: service`, and
   `migration_plan: OLD_ENV/migration-export.json` (the **export** plan) and
   `source_info_artifact: EXPORT_DEPLOY_ARTIFACT_ID` (the export run's deployment artifact ID).
   Forward this optional input and grant `actions: read` when adopting the release. Deploy verifies the
   current fenced export attestation before skipping maintenance pause/resume, then restores
   service with the normal target attestation, health and TLS checks. Record the source-export
   verification and target acceptance artifacts. Database route pauses remain; there is
   **no automatic route resume**.
4. Require the restored service's exact reviewed compose, old app/OS identity and ingress
   evidence. Check admin state and have merchants verify/restore their old pins, then manually
   resume only migration-owned route pauses. Record this acceptance and import-hash revocation
   evidence before declaring rollback complete; keep the new CVM stopped.

### Fallback when the Deploy workflow is unavailable

After import-path revocation and stopping the new CVM, the existing Phala `compose-update`
helper can restore the saved reviewed vN service render, retaining sealed env, app/KMS identity
and pgdata. Use this only when the workflow is unavailable; verification failures require
investigation. This fallback applies to the old Phala KMS CVM; keep routes paused throughout.

```sh
policy=$(jq -cn -L deploy --arg variant service 'include "compose-policy"; port_policy($variant)')
deploy/phala-cvm.sh compose-update "$OLD_CVM_ID" "$EVIDENCE/old-service.yml" \
  deploy/phala-cloud-pre-launch.sh "$policy" "$EVIDENCE/rollback.json"
rollback_hash=$(jq -er .compose_hash "$EVIDENCE/rollback.json")
test "${rollback_hash#0x}" = "${OLD_COMPOSE_HASH#0x}"
deploy/phala-cvm.sh wait --compose-hash "$rollback_hash" "$OLD_CVM_ID" >"$EVIDENCE/old-cvm.json"
deploy/phala-cvm.sh attestation "$OLD_CVM_ID" "$rollback_hash" >"$EVIDENCE/old-attestation.json"
curl -fsS "https://$DOMAIN/v1/attestation/info" >"$EVIDENCE/old-info.json"
deploy/verify-attestation.sh "$EVIDENCE/old-attestation.json" "$EVIDENCE/old-info.json" \
  "$OLD_APP_ID" "$EVIDENCE/old-service.yml" service "$OLD_OS_HASH"
```

Stop on any failure or hash mismatch. Revert TXT to the old verified instance id and CNAME to
`OLD_GATEWAY`; wait for TLS and `/healthz`, then verify ingress with
`deploy/verify-ingress-evidence.sh "$DOMAIN" "$OLD_APP_ID"`. Check admin state, have merchants
verify/restore their old pins, and resume only migration-owned route pauses. Record this full
acceptance and the import-hash revocation evidence before declaring rollback complete.
Re-select the old CVM/directory in deploy state.

## Key evidence

Define this helper before preparation. It runs the official pinned dstack verifier and recomputes
the key/nonce commitment from [the protocol](../../docs/architecture.md#kms-application-migration).
Expected app/compose/OS values must come from independently verified deployment evidence, never
from the downloaded key document. Import's canonical age recipient is also checked by the exporter.

```sh
verify_key_evidence() {
  local kind=$1 file=$2 app=$3 compose=$4 os=$5 nonce=${6:-}
  jq '{quote:null,attestation:.tdx_quote}' "$file" |
    deploy/dstack-verifier.sh >"$file.verified.json" || return 1
  python3 - "$kind" "$file" "$app" "$compose" "$os" "$nonce" <<'PY'
import hashlib
import json
import sys

kind, path, app, compose, os_hash, challenge = sys.argv[1:]
with open(path) as source:
    evidence = json.load(source)
with open(path + ".verified.json") as source:
    verified = json.load(source)
nonce = bytes.fromhex(evidence["nonce"])
if kind == "export":
    key = bytes.fromhex(evidence["public_key"])
    if not 1 <= len(nonce) <= 32 or len(key) != 32 or nonce != bytes.fromhex(challenge):
        raise SystemExit("export key/challenge mismatch")
    payload = b"phala-pay/migration/export-key/v1\0" + bytes([len(nonce)]) + nonce + key
elif kind == "import":
    if len(nonce) != 32:
        raise SystemExit("invalid import nonce")
    payload = b"phala-pay/migration/import/v1\0" + nonce + evidence["public_key"].encode("ascii")
else:
    raise SystemExit("unknown evidence kind")
digest = hashlib.sha256(payload).hexdigest()
details = verified["details"]
info = details["app_info"]
expected_app = app.removeprefix("0x").lower()
expected_compose = compose.removeprefix("0x").lower()
if not (
    verified["is_valid"] is True and details["tcb_status"] == "UpToDate"
    and info["app_id"] == expected_app and evidence["app_id"] == expected_app
    and info["compose_hash"] == expected_compose and evidence["compose_hash"] == expected_compose
    and info["os_image_hash"] == os_hash
    and evidence["report_data"] == digest and details["report_data"] == digest + "0" * 64
):
    raise SystemExit("key evidence verification failed")
print("verified", kind, "key evidence")
PY
}
```

## Acceptance and evidence

Staging must exercise the whole window before scheduling production.

Follow the [Base upgrade approval rule](onchain-kms-upgrade.md#upgrades-and-rollback): review a
prepare artifact before Safe execution, and never execute a Safe approval during a prepare or
commit run for that app. An already-approved hash yields a read-only preparation summary with
`already_approved: true` and no compose batch; approve any missing device before commit. Cloud's
prepare-only header can apply already-approved updates, so only commit sends that update,
after the pause. If an unchanged CVM is stopped, recovery re-verifies the stored full hash and
on-chain policy before starting it without a maintenance lease; full acceptance still runs.
Unexpected 2xx during prepare requires stopping, inspecting stored compose/attestation and
running commit to verify before continuing the migration.

Record:

- Release/kit checks, environment/plan commits, old/new CVM and app ids, node/device/OS/provider
  evidence, full compose hashes, all three Safe batch files/nonces/signatures/receipts and final
  import-hash revocation. Prove an unapproved hash cannot boot; exercise a normal Base
  prepare/Safe/commit upgrade and N-1 `already-registered` rollback.
- Old nonce-bound export key, current recipient evidence/nonce, signed manifest/ciphertext hash,
  export fencing attestation and final old-prefix backup. Import healthy-start evidence proves
  internal restored row counts/latest migration/heartbeat checks; compare saved merchant
  deposit/quote listings and admin reads after resume. Idempotency/history/holds remain intact.
  Exercise forged-manifest rejection and incomplete-import restart/re-export in staging.
- Final attestation and ingress/certificate evidence, DNS before/after, per-merchant outbox
  drain/pin rollout confirmations and fresh-secret behavior. One end-to-end payment credited
  **once** with the new signature, plus a payment to a pre-migration fixed address during the
  window found/credited after resume. Check scanner catch-up and webhook-id deduplication.
- New-prefix `base_…`, fresh `wal_005` and healthy `topup-backup` check-in; completed-marker reboot,
  T0 closeout/old-CVM stop and the new-app restore drill within seven days. Restrict customer
  snapshots and storage evidence; retain no local plaintext dump or secret-bearing env files.

**Confirm for each target:** Safe accepted as `deployer_address` (stop and return to the owner if
rejected; no improvised EOA fallback), exact provision normalization, target-node Base availability,
import/closeout/restore behavior, merchant secret caching/pin rollout, DNS/gateway/certificate
delay and measured downtime including the closeout deployment. Staging's recorded results are
not verified production claims. Record production timestamps against the rehearsal measurements.
