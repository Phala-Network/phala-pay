# Prepared production Base KMS environment

This directory prepares a **new production Base app**, selected explicitly with Deploy's
`environment_dir`. It has not been provisioned or deployed. The original
[`../topup`](../topup/compose.yaml) directory retains the production Phala KMS configuration
for the source service and rollback. Follow the [migration runbook](../../../../runbooks/onchain-kms-migration.md)
for live steps and approval gates. Production and staging use separate DstackApps owned by
treasury Safe `0x8AB55148944F837040bbB887f8A5005f70FF7700` on Base (8453).

## Reviewed values and sources

The read-only production snapshot was taken on November 13, 2026 (PST), using the locked
Phala Cloud CLI 1.2.7 and the `phala` workspace profile in `~/.phala-cloud/credentials.json`.
No credentials or sealed values are recorded here. The source CVM UUID was read from the
repository's production `TOPUP_CVM_ID` Environment variable. The live metadata was read with
`deploy/phala cvms get "$OLD_CVM_ID" --profile phala --json`, its app-compose with
`deploy/phala api "/cvms/$OLD_CVM_ID/compose_file" --profile phala`, and its quote with
`deploy/phala cvms attestation "$OLD_CVM_ID" --profile phala --json`.

| Setting | Reviewed value | Source |
|---|---|---|
| Source production CVM UUID | `58cd45f5-da5c-42c9-80ca-6e825b5e8a94` | Production `TOPUP_CVM_ID`, confirmed by live `vm_uuid` |
| Source app id | `2ed68f31bdcc5a914afaefbfb33e8fbbf7c4dcc1` | Live CVM metadata and verified attestation |
| Source instance id | `09c091860b1885296c455c2c4c1d4f6cf27216de` | Live CVM metadata and verified attestation |
| Source full compose hash | `3b919f9bbceecab4548f7c629735a558f89f7049be14551f318d21d69d51c017` | Live CVM metadata and verified attestation; re-read after the mandatory source upgrade |
| Physical node | `prod5`, catalog id `26`, `US-WEST-1` | Live CVM `node_info` and node catalog |
| Current device id | `c4691f9c88f44e05cbc45521678e72b99fcd54fa35d302f13e8f9fa9727f33a6` | Live node metadata and verified attestation |
| Instance/resources | `tdx.medium`, 2 vCPU, 4 GiB RAM, 20 GiB disk | Live CVM `resource`; request pins instance type and disk size |
| OS | `dstack-0.6.1`, non-dev | Live CVM `os`; request uses `image` and `prefer_dev: false` |
| OS measurement | `ec43cb8c5e04b6bb6c53b33b16076a01d83d01f7f4b703f2f78a0d947d6247e6` | Live `os.os_image_hash`, cross-checked by full official dstack verification |
| Source Phala gateway | `gateway.dstack-pha-prod5.phala.network` | Live CVM `gateway.cname` |
| New Base gateway | `gateway.dstack-base-prod5.phala.network` | The successful staging Base rehearsal on prod5 |
| New request name | `phala-pay-production-okms1` | This migration's reserved production attempt name |
| KMS / listing | `BASE`, `listed: false` | Explicit Base migration selection; listing matches production |
| Owner | `0x8AB55148944F837040bbB887f8A5005f70FF7700` | Treasury Safe in the approved migration plan |
| Base KmsAuth | `0x2f83172A49584C017F2B256F0FB2Dca14126Ba9C` | `deploy/onchain-kms.json`, checked against the live Base KMS catalog |

The source quote and public `https://pay-api.phala.com/v1/attestation/info` were checked with
`deploy/verify-attestation.sh` in full mode: official dstack verifier 0.6.1, TCB `UpToDate`,
RTMR3 replay, OS measurement, app id, full app-compose, sealed-name and service policies passed.
The public Info's OS claim alone is not verification. The source's sealed-name set is exactly:

```text
AWS_ACCESS_KEY_ID
AWS_SECRET_ACCESS_KEY
TOPUP_RPC_ANKR_KEY
TOPUP_RPC_BLOCKPI_BASE_KEY
TOPUP_RPC_BLOCKPI_ETHEREUM_KEY
SENTRY_DSN
```

`topup.yaml` is copied unchanged from production, including its PHA route, public origin,
admin and maintenance public keys. `compose.yaml` changes only WAL-G's prefix, for postgres
and backup, from `s3://phala-pay-production/production-v1` to
**`s3://phala-pay-production/production-okms1`**. The R2 endpoint and `auto` region are unchanged.
The separate transfer prefix is **`s3://phala-pay-production/production-okms1-migration`**.
The migration coordinator verified both new prefixes empty during preparation; recheck them
at T-1. Preserve `production-v1`; never point the new app at old encrypted backups. An
abandoned attempt after old writes resume needs fresh public plans and fresh empty prefixes.

`provision.json` is the full reviewed request and sole app-compose baseline. Its non-render
fields come from the source's verified app-compose: the exact `allowed_envs` list above,
`features: [kms, tproxy-net]`, `manifest_version: "3"`, empty `name`, empty `init_script`,
`runner: docker-compose`, `storage_fs: ext4`, `kms_enabled`, `secure_time`, `gateway_enabled`,
`tproxy_enabled` and `public_tcbinfo` true, with `local_key_provider_enabled`, `no_instance_id`,
`public_logs` and `public_sysinfo` false. It retains the production PROXY-on 443 port policy
(`ports: [{port: 443, pp: true}]`, `restrict_mode: false`). Base adds `key_provider: kms` and
this public `key_provider_id`, read with `node deploy/tools/onchain-kms.mjs identity` after
checking the pinned KmsAuth implementation:

```text
3059301306072a8648ce3d020106082a8648ce3d030107034200048bf84f52911056474c207cb50b4e8010d984f8975ea57b8f0c8b17502181c283e93a68a9864ad19f629508cc699f125be3871ec2e26ab0597e5bca4e29b7425f
```

The read-only `/teepods/available` response confirms prod5 supports onchain KMS and
`kms-base-prod5` supports `dstack-0.6.1`. The new provision response and verified recipient
quote must establish the new device; do not substitute the recorded source device. A measured
standby device remains a separate approval gate. No new CVM, app, instance or standby is
guessed here; `dstack-app` must be added only from batch #1's verified deployment receipt.

The baseline retains both gateway aliases because that is the stored/attested shape. Initial
provisioning omits **only** `tproxy_enabled` on the wire, matching SDK 0.6.5's compatibility
handling. Keep the string manifest version and all other fields/types unchanged; an unexplained
backend normalization must fail before producing a Safe batch. See the
[upstream schema](https://github.com/Phala-Network/phala-docs/blob/c098ff18da4cb66a4691afa0979e210313a1b6c6/openapi.json)
and [Base provisioning rules](../../../../runbooks/onchain-kms-upgrade.md#initial-provisioning).

## Reproduce the seed render

The baseline's two render fields use the **v0.13.13** release's verified `images.json` and
kit `deploy/phala-cloud-pre-launch.sh`, with the Base gateway above. Verify the downloaded
release assets against its `SHA256SUMS` and use that kit's scripts:

```sh
NEW_ENV=deploy/environments/phala-network/production/topup-okms
deploy/render.sh --images images.json --gateway-domain gateway.dstack-base-prod5.phala.network \
  "$NEW_ENV" >"$EVIDENCE/service.yml"
jq --rawfile compose "$EVIDENCE/service.yml" \
  --rawfile pre_launch deploy/phala-cloud-pre-launch.sh \
  '.compose_file.docker_compose_file=$compose | .compose_file.pre_launch_script=$pre_launch' \
  "$NEW_ENV/provision.json" >"$EVIDENCE/provision.json"
cmp "$NEW_ENV/provision.json" "$EVIDENCE/provision.json"
```

This seed render is not a Safe approval. Freeze the exact migration release through closeout;
Deploy replaces these fields with that release's import, service and restore-check bytes.
Every changed release or render input changes the approved full app-compose hashes.

## T-1 public plans and provisioning

The committed import/export files are **inert templates**: `T1_EXPORT_PUBLIC_KEY` and
`T1_AGE_RECIPIENT` deliberately fail explicit migration rendering. Ordinary service renders
do not read these plans. Fill them only from fresh verified public evidence, review/commit
the public plans and rendered composes, and retain the runbook's private evidence locally.

| File / field | Current value | T-1 action |
|---|---|---|
| `migration-import.json` / `source_app_id` | Verified source app id above | Re-read source CVM and verify its service attestation |
| `migration-import.json` / `export_public_key` | Filled November 14, 2026 (PST): `a70d6945…635f`, verified against v0.13.14 service compose `7ed97e22…` (Deploy run 43581474770) | Verify the nonce-challenged source admin export-key response |
| Both plans / `object` | `s3://phala-pay-production/production-okms1-migration` | Recheck empty before this attempt; change both plans together for a new attempt |
| `../topup/migration-export.json` / `recipient` | `T1_AGE_RECIPIENT` | Verify the new import's public recipient quote after sealing/start |
| `dstack-app` | Absent | Add the address from KmsAuth's verified batch #1 deployment receipt |

First upgrade the source service to the **same release as export** and verify it. The recorded
source compose hash will change; use the fresh `OLD_COMPOSE_HASH`, `OLD_OS_HASH`, and the
runbook's `admin` / `verify_key_evidence` helpers below. Keep production and staging evidence
and app ids separate:

```sh
OLD_ENV=deploy/environments/phala-network/production/topup
NEW_ENV=deploy/environments/phala-network/production/topup-okms
OLD_CVM_ID=58cd45f5-da5c-42c9-80ca-6e825b5e8a94
deploy/phala cvms get "$OLD_CVM_ID" --profile phala --json >"$EVIDENCE/old-cvm.json"
OLD_APP_ID=$(jq -er '.app_id | ascii_downcase | ltrimstr("0x")' "$EVIDENCE/old-cvm.json")
test "$OLD_APP_ID" = 2ed68f31bdcc5a914afaefbfb33e8fbbf7c4dcc1
NONCE=$(openssl rand -hex 32)
admin GET "/v1/admin/migration/export-key?nonce=$NONCE" >"$EVIDENCE/export-key.json"
verify_key_evidence export "$EVIDENCE/export-key.json" "$OLD_APP_ID" \
  "$OLD_COMPOSE_HASH" "$OLD_OS_HASH" "$NONCE"
MIGRATION_OBJECT=s3://phala-pay-production/production-okms1-migration
jq -n --arg source "$OLD_APP_ID" \
  --arg key "$(jq -er .public_key "$EVIDENCE/export-key.json")" --arg object "$MIGRATION_OBJECT" \
  '{migration_import:{source_app_id:$source,export_public_key:$key,object:$object}}' \
  >"$NEW_ENV/migration-import.json"
```

After batch #1 and initial provisioning, retain the created CVM id and artifact even if the
workflow's readiness step fails. Do not retry initial provisioning after creation succeeded.
Import cannot boot unsealed: seal locally with CLI 1.2.7 (`envs update -e`) and the runbook's
full plan-aware preflight, then inspect/start the CVM if stopped. Pending import has no full
Cloud attestation and the public origin still serves the source. Verify the recipient's own
quote **before export**; require full Cloud attestation **after import**. Use the runbook's
separate acceptance steps rather than treating creation as acceptance.

Once the new import's recipient is available, fill the source export template:

```sh
aws --endpoint-url "$AWS_ENDPOINT" s3 cp \
  "${MIGRATION_OBJECT%/}/recipient.json" "$EVIDENCE/recipient.json"
verify_key_evidence import "$EVIDENCE/recipient.json" "$NEW_APP_ID" \
  "$IMPORT_HASH" "$(cat "$NEW_ENV/os-image-hash")"
jq -n --arg recipient "$(jq -er .public_key "$EVIDENCE/recipient.json")" \
  --arg object "$MIGRATION_OBJECT" \
  '{migration_export:{recipient:$recipient,object:$object}}' >"$OLD_ENV/migration-export.json"
deploy/render.sh --export --migration-export "$OLD_ENV/migration-export.json" \
  --images images.json "$OLD_ENV" >"$EVIDENCE/export.yml"
```

Complete the [Base gateway routing precheck](../../../../runbooks/onchain-kms-migration.md#base-gateway-routing-precheck)
before production T0. Prepare service/restore-check approvals and batch #3 revocations with the
frozen release. Follow the runbook for fencing/export, import verification, DNS/TLS, immediate
import-hash revocation and service closeout, merchant re-pinning, route resume and retention
of the old CVM/storage for at least 30 days. This directory authorizes none of those live steps.
