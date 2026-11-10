# Base onchain KMS: prepare, sign, commit

This flow is opt-in. Existing `kms: phala` deployments use `provision` / `upgrade` as before.
Production and staging are still Phala KMS in the committed directories. Never switch an
existing CVM's KMS in place: a new Base app has new derived keys.

## Release adoption

`deploy-phala.yml` remains pinned to v0.13.6, whose reusable workflow has no Base inputs. The
release-adoption PR must repin both its workflow SHA and `version` to the release containing
this change, extend its mode choices to `[provision, upgrade, prepare, commit]`, add optional
string dispatch inputs `transaction_hash` and `gateway_domain`, and forward both in `with`.
Do that together; passing new inputs to the old pinned workflow fails GitHub validation.

## Reviewed inputs

Commit `kms` (`base`), `dstack-app` (DstackApp address), `dstack-owner` (expected Base Safe),
`os-image-hash`, a full reviewed `app-compose.json` baseline and the ordinary environment settings.
The baseline may instead be the committed `provision.json`'s `compose_file`. Every non-render
field and the exact key set must match that baseline in Cloud and attestation. `init_script`
must be absent or `[]`; no root init commands are accepted. Allowed env names, flags, runner,
manifest, features, storage, provider and any other fields are pinned. Only docker-compose and
pre-launch bytes are replaced by the reviewed render and kit script. A change to another field
requires a reviewed baseline change and explicit operation; never copy unreviewed Cloud fields
into a baseline to make a check pass. Staging and production use treasury Safe
`0x8AB55148944F837040bbB887f8A5005f70FF7700` as DstackApp owner, with a separate app per
environment. The reference product and template stay on Phala KMS. The Base KmsAuth address and
both implementation pins are in `deploy/onchain-kms.json`. Review upstream source and chain evidence before changing
those pins. No private signing key belongs in GitHub or these files.

## Initial provisioning

Commit a complete `provision.json` request in the new environment directory. It holds the full
initial `compose_file` (including explicit backend defaults, allowed env **names**, KMS provider
pin, port policy and pre-launch script), `kms: "BASE"`, CVM name, instance type, OS image and a
pinned physical node name in `node` (for example `"prod5"`, from `phala nodes ls --json`).
Catalog numeric ids (`node_id` and `teepod_id`) are no longer accepted for initial provision.
Set `prefer_dev: false`. It must contain no secret values or
`encrypted_env`. Set `init_script` to `[]` or omit it; include the supported secure flags and
exact allowed env-name set. Any backend-injected init command, extra key or changed flag causes
refusal before a batch is produced. Backend defaults must be reviewed in the baseline. Resolve the selected node's gateway before preparing.

The baseline describes the **stored/attested** compose. Pin `manifest_version` to the string
`"3"` for the current backend; the current OpenAPI permits strings and integers, whereas the
pinned SDK's compose parser still expects a number. The raw API path sends the reviewed type
unchanged. Include both `gateway_enabled: true` and `tproxy_enabled: true` in that baseline.
For the creation payload only, `provision-prepare` omits `tproxy_enabled`, following the pinned
SDK's [gateway compatibility handling](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.1.22/js/src/actions/cvms/provision_cvm.ts#L280).
Cloud rejects the deprecated alias on create and restores it in the final compose. Local hashing
still includes it: this explicit request projection does not relax any returned hash or baseline
check. Do not strip it from stored compose or upgrade requests. No other normalization is accepted.

With `TOPUP_CVM_ID` unset, dispatch Deploy `prepare` with `gateway_domain` set to that node's
`gateway.<base-domain>`. The workflow renders the compose and sets its reviewed pre-launch and
port policy in the request, runs preflight, and calls `provision-prepare`. It recomputes the hash
locally and refuses any backend mutation. If it refuses, stop and investigate the backend mutation against the reviewed baseline and
supported schema. Never copy an unexplained stored field into the baseline or approve its hash.

Import the factory batch into Safe Transaction Builder on **Base (8453)**. Review the Safe,
factory address, pinned device and compose hash. It calls
`deployAndRegisterApp(Safe,false,false,deviceId,composeHash)`: upgrades remain enabled, hardware
pinning remains enabled, `requireTcbUpToDate` stays false. Deploy still requires UpToDate quotes.
Execute it outside GitHub. Record the DstackApp address from the KmsAuth-emitted
`AppDeployedViaFactory` event in `dstack-app` through a reviewed change.

Dispatch `commit` with the executed transaction hash and the same gateway input. The workflow
recomputes the request, matches the receipt's factory and Safe, verifies owner/compose/device and
both implementations, then calls `POST /cvms`. It waits unsealed and verifies the attestation,
including Base provider and committed app id. Set `TOPUP_CVM_ID` from the deployment record.
Seal the runtime values from the owner's machine, configure DNS from the verified instance id,
and run the ordinary Base upgrade acceptance flow below. Backend acceptance of a Safe deployer,
initial full-compose normalization and target-node Base availability require staging rehearsal.

The shared debugging entry points are:

```sh
deploy/phala-cvm.sh provision-prepare request.json prepare.json "$ENVIRONMENT_DIR"
deploy/phala-cvm.sh provision-commit "$TRANSACTION_HASH" prepare.json "$ENVIRONMENT_DIR"
```

## Upgrades and rollback

1. Commit the new release/configuration, keeping the existing app and node. Dispatch `prepare`.
   Preflight and preparation do not pause admission. Download the batch, device batch and public
   summary artifact. Verify the locally computed compose hash and review the exact rendered
   compose. Base requires `kms_enabled`, `key_provider: kms` and the CA identity from
   `KmsAuth.kmsInfo().caPubkey` in `key_provider_id`.
2. Import and execute the compose batch on the expected Safe on Base. Execute the device batch
   if that device is not already approved; `addDevice` is idempotent. Pre-approve one standby
   device using its measured PPID hash, while keeping `allowAnyDevice=false`.
3. Dispatch `commit` with the transaction hash and the same reviewed release/configuration.
   Commit re-prepares deterministically, verifies the hash is approved, checks device/owner and
   both proxy pins **before pausing**. A transaction receipt check establishes success only;
   separate state reads establish the expected owner, approved hash/device and proxy pins. It then uses the existing pause → commit → wait → verify →
   resume path. Read the attestation and ingress evidence before declaring acceptance.
4. Keep N-1 approved. Rollback selects its release/configuration, prepares it, and commits with
   `transaction_hash: already-registered`. State checks still run; no Safe signature is needed
   for an approved hash and device. Never run another writer against the same backup prefix.
5. After acceptance, remove hashes older than N-1 with a separately reviewed Safe batch.
   `node deploy/tools/onchain-kms.mjs batch remove.json "$SAFE" "$APP" removeComposeHash "$OLD_HASH"`
   writes that batch offline. Never remove the running hash or the rollback hash.

A Safe signer is required for every new hash and device. The production Safe is 1/1: arrange
signer availability before a release, with an operator response target of one hour during an
agreed maintenance window. Outside that window agree a response time before preparing. Admin
incident pauses remain available without a Safe signature.

## Migration compose variants

After the migration-tooling release lands, Deploy also accepts `variant: export` or
`variant: migration-import` and a reviewed relative `migration_plan` JSON path. The caller
release-adoption PR must forward these optional inputs together with the new Base inputs.
An export is an upgrade of an existing CVM: preflight checks its exact render/plan and images,
then the normal maintenance pause precedes replacement. Its **verified attestation**, proving
that exact export compose with no topup service, is the fencing evidence in the run artifact.
There is no service health/ingress check or successful maintenance resume on the exporter. A
failure before fencing still attempts the normal lease cleanup.

Export keeps the old service's allowed env names (AWS, Sentry and RPC keys) without re-sealing;
its compose may reference only its declared AWS names. That retained-name exception applies
only to export. Migration import keeps the service ingress and ordinary acceptance checks.
The PR 2 render/policy/tooling changes are required before either variant can deploy.

## Token lifetime and hash format

Prepare tokens/cache entries last **14 days**. Preparing again invalidates the previous token.
The workflow keeps token-bearing JSON private in its temporary directory, never in logs or
artifacts; commit re-prepares from the reviewed inputs to obtain a fresh token. A delayed signature
is safe only if the new prepare reproduces the signed hash. If any setting, env name, node or
backend default changes, stop and review a new batch. Sealed env values are excluded from the
hash; their allowed names are included. Changing allowed names needs a separately reviewed
prepare operation, followed by sealing; the ordinary upgrade helper preserves the current set.

The hash is SHA-256 over the **exact** sorted compact UTF-8 app-compose JSON, including port policy,
pre-launch script and public/KMS flags. The production v0.13.6 fixture reproduces its attested
`ed7dc915…` hash byte for byte. The pinned SDK's public `getComposeHash` matches it; an older
internal SDK helper uses four-space JSON and yields a different hash. No approval uses that
older helper. `attested-compose.sh` continues hashing the attested raw bytes.

The implementation follows the pinned CLI 1.1.22's
[prepare handling](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.1.22/cli/src/commands/deploy/handler.ts#L944)
and [commit handling](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.1.22/cli/src/commands/deploy/handler.ts#L1223),
with [SDK PATCH](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.1.22/js/src/actions/cvms/patch_cvm.ts#L159)
and [SDK commit](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.1.22/js/src/actions/cvms/commit_cvm_update.ts#L67):
`PATCH /cvms/{id}` with `X-Prepare-Only: true`, then `POST /cvms/{id}/commit-update` with
`token`, `compose_hash`, `transaction_hash`. Initial provision uses `POST /cvms/provision`, then
`POST /cvms` with receipt-derived app/contract and Safe deployer addresses. These are in the
[reviewed OpenAPI](https://github.com/Phala-Network/phala-docs/blob/c098ff18da4cb66a4691afa0979e210313a1b6c6/openapi.json), operationIds
`update_cvm_endpoint_api_v1_cvms__cvm_id__patch`,
`commit_cvm_update_api_v1_cvms__cvm_id__commit_update_post`,
and `handle_create_cvm_api_v1_cvms_post`. The current create endpoint documents a same-path
token flow and explicitly retains the legacy `POST /cvms/provision` → `POST /cvms` pair during
its deprecation window; this kit uses that existing pair. Its prepare response now includes an
optional token, which the legacy `app_id` + `compose_hash` commit does not use.
There is no documented `/compose_file/commit`.

At phala-docs commit `c098ff18da4cb66a4691afa0979e210313a1b6c6`, that create operation states:

> The legacy pair (POST /cvms/provision then POST /cvms with app_id + compose_hash) keeps working during the deprecation window.

Upstream gives **no deprecation end date** in that OpenAPI description. Continued support for
this pair is an **open production-cutover risk**. Before production cutover, verify that it
still works; follow up with a reviewed migration to `handle_create_cvm_api_v1_cvms_post`'s
same-path prepare/token-commit flow. That migration is outside this schema-fix change.

The initial `POST /cvms` payload still matches `CreateTappRequest`: receipt-derived `app_id`,
`compose_hash`, `contract_address`, Safe `deployer_address`, reviewed `env_keys` and optional
`kms_id`. Live commit acceptance remains a staging gate after Safe batch #1; prepare creates no CVM.

## OS changes and governance alerts

Base compose commit refuses an implicit OS change. Review the new OS measurements and KmsAuth
OS allowlist, prepare the explicit OS `image` change with the documented unified PATCH token
flow, and verify the resulting exact compose hash before approval. A pure OS-image field change
may retain its compose hash; the OS measurement pin is still a separate acceptance check. This
special operation requires operator rehearsal; do not fall back to the Phala-only OS helper.

Verify contracts scans the real Base KmsAuth and configured apps every day using a keyless RPC.
ComposeHashAdded/Removed, DeviceAdded/Removed, AllowAnyDeviceSet, UpgradesDisabled,
RequireTcbUpToDateSet, owner and implementation events fail the job for review, including expected
changes. KmsAuth monitoring also includes GatewayAppIdSet, OS-image, KMS-MR, KMS-device,
KMS-info and app-implementation events. It omits AppRegistered: the KmsAuth is shared, so every
tenant's new app emits it; the configured apps' own events are monitored directly. Each topic is tested against recorded
bytecode of the pinned deployed implementation; those implementations do not emit the newer
upstream PolicyChanged event.
The report includes transaction hashes and raw topics/data. Its 45,000-block window overlaps
normal daily runs; for an outage longer than that, run the monitor from the last reviewed block:

```sh
node deploy/tools/onchain-kms.mjs monitor "$LAST_REVIEWED_BLOCK" "$APP"
```

Check both implementation slots and review every governance event before updating pins. Phala's
KMS governance can still approve OS/KMS builds or upgrade KmsAuth; our Safe controls only our
DstackApp. See [device drift](device-drift.md) for a device approval incident.
