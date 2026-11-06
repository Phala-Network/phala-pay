# Prepared staging Base KMS environment

This directory prepares a **new app**, selected explicitly with Deploy's `environment_dir`.
It does not change the active [`../topup`](../topup/compose.yaml) directory's Phala KMS service.
Follow the [migration runbook](../../../../runbooks/onchain-kms-migration.md) for all live steps;
this preparation does not authorize a deployment. The staging reference product keeps Phala KMS.
Staging and production have separate DstackApps owned by treasury Safe
`0x8AB55148944F837040bbB887f8A5005f70FF7700` on Base (8453).

## Reviewed baseline

`topup.yaml` is copied unchanged from staging: routes, public origin, admin and maintenance
public keys stay the same. `compose.yaml` changes only WAL-G's prefix, for both postgres and
backup, to **`s3://crypto-topup-test/staging-okms1`** on the existing staging R2 endpoint/region.
The owner must verify that prefix is empty before provisioning. Transfer objects use a separate,
unique prefix in the same bucket, selected at T-1. Keep the old `staging-v030` backup prefix.

Read-only Cloud metadata on November 6, 2026 (PST) confirmed:

| Setting | Value |
|---|---|
| Old staging CVM UUID | `1b4460dd-8f32-4851-a1f9-2852c213e4b0` |
| Old app id | `95174fcad739ae76c7510dc1b2bc00a32e25c644` |
| Pinned node | `26` (`prod5`, US-WEST-1) |
| Current device id | `c4691f9c88f44e05cbc45521678e72b99fcd54fa35d302f13e8f9fa9727f33a6` |
| Gateway | `gateway.dstack-pha-prod5.phala.network` |
| Resources | `tdx.medium`, 2 vCPU, 4 GiB RAM, 20 GiB disk |
| OS | `dstack-0.6.1`; measurement in `os-image-hash` |

Re-read node/device/gateway at T-1 with `deploy/phala cvms get "$OLD_CVM_ID" --json`,
`deploy/phala nodes ls --json` and `deploy/phala-cvm.sh gateway-domain "$EVIDENCE/old-cvm.json"`.
The provision response and verified quote must establish the new device; do not substitute the
recorded old device for that check. The read-only `/teepods/available` response advertises
`support_onchain_kms: true` for node 26 and Base KMS support for `dstack-0.6.1`. Actual new-app
boot and a measured standby device remain rehearsal gates. No standby device is guessed here.

`provision.json` is the full reviewed request, pinned to node 26, the same OS/resources,
`prefer_dev: false`, `kms: BASE`, and non-public listing. Its `compose_file` is also the sole
app-compose baseline. It contains the v0.13.7 **no-overlay service render** for this gateway and
the kit's exact pre-launch script; Deploy replaces only those two rendered fields for import.
It retains the reviewed PROXY-on 443 port policy, secure flags, empty `init_script`, storage,
runner, features and the **exact current staging sealed-name set**. Base's `key_provider: kms`
and `key_provider_id` were read using `node deploy/tools/onchain-kms.mjs identity`, which checks
the pinned KmsAuth implementation before reading its public CA identity.

The old Phala app returns `manifest_version: "3"` as a string. This new baseline deliberately
uses **numeric `3`**, as required by the pinned SDK schema and `base_app_violations` policy.
Backend normalization is a staging rehearsal gate: if the new Base app stores or returns `"3"`
as a string, preparation fails closed **before a Safe batch is produced**. Resolve this with a
reviewed code or baseline change; never hand-edit the request during the rehearsal or accept an
unexplained backend field/default. See the [upstream provisioning schema](https://github.com/Phala-Network/phala-docs/blob/361e6c6/openapi.json)
and the [Base provisioning rules](../../../../runbooks/onchain-kms-upgrade.md#initial-provisioning).

To reproduce the baseline's render fields, use verified v0.13.7 `images.json` and kit scripts:

```sh
NEW_ENV=deploy/environments/phala-network/staging/topup-okms
deploy/render.sh --images images.json --gateway-domain gateway.dstack-pha-prod5.phala.network \
  "$NEW_ENV" >"$EVIDENCE/service.yml"
jq --rawfile compose "$EVIDENCE/service.yml" \
  --rawfile pre_launch deploy/phala-cloud-pre-launch.sh \
  '.compose_file.docker_compose_file=$compose | .compose_file.pre_launch_script=$pre_launch' \
  "$NEW_ENV/provision.json" >"$EVIDENCE/provision.json"
cmp "$NEW_ENV/provision.json" "$EVIDENCE/provision.json"
```

## T-1 public plan values

The committed import/export JSON files are **templates**. Their `T1_*` strings deliberately fail
render policy; do not dispatch a migration with these values. The old source app id is already
known and pinned; verify it again before filling the import plan. Generate the remaining values
using the migration runbook's evidence helper and verified release, then review/commit both plans
and their exact rendered composes before use. No secret values belong in these files.

| File / field | Current value | How it is filled |
|---|---|---|
| `migration-import.json` / `source_app_id` | Recorded old app id | Read old CVM metadata; verify old service attestation at T-1 |
| `migration-import.json` / `export_public_key` | `T1_VERIFIED_EXPORT_PUBLIC_KEY` | Nonce-challenged old admin export-key endpoint, verified by `verify_key_evidence export` |
| Both plans / `object` | `T1_MIGRATION_OBJECT_PREFIX` | New unique `s3://crypto-topup-test/...` prefix for this attempt |
| `../topup/migration-export.json` / `recipient` | `T1_VERIFIED_IMPORT_RECIPIENT` | New import's public `recipient.json`, verified by `verify_key_evidence import` |
| `dstack-app` (absent) | Produced after batch #1 | Commit the new address from KmsAuth's verified `AppDeployedViaFactory` receipt |

Before provisioning, after the old app has been upgraded to v0.13.7 and verified, use the
runbook's `admin` and `verify_key_evidence` helpers, `OLD_COMPOSE_HASH`, `OLD_OS_HASH` and private
`EVIDENCE` directory. These commands fill the import fields with public evidence:

```sh
OLD_ENV=deploy/environments/phala-network/staging/topup
NEW_ENV=deploy/environments/phala-network/staging/topup-okms
OLD_CVM_ID=$(gh variable get TOPUP_CVM_ID --env staging --repo Phala-Network/phala-pay)
deploy/phala cvms get "$OLD_CVM_ID" --json >"$EVIDENCE/old-cvm.json"
OLD_APP_ID=$(jq -er '.app_id | ascii_downcase | ltrimstr("0x")' "$EVIDENCE/old-cvm.json")
test "$OLD_APP_ID" = 95174fcad739ae76c7510dc1b2bc00a32e25c644
NONCE=$(openssl rand -hex 32)
admin GET "/v1/admin/migration/export-key?nonce=$NONCE" >"$EVIDENCE/export-key.json"
verify_key_evidence export "$EVIDENCE/export-key.json" "$OLD_APP_ID" \
  "$OLD_COMPOSE_HASH" "$OLD_OS_HASH" "$NONCE"
MIGRATION_OBJECT="s3://crypto-topup-test/kms-migration/staging-$(openssl rand -hex 16)"
jq -n --arg source "$OLD_APP_ID" \
  --arg key "$(jq -er .public_key "$EVIDENCE/export-key.json")" --arg object "$MIGRATION_OBJECT" \
  '{migration_import:{source_app_id:$source,export_public_key:$key,object:$object}}' \
  >"$NEW_ENV/migration-import.json"
```

Record `MIGRATION_OBJECT` in the evidence for this attempt. After the new import is provisioned,
sealed and its public recipient verified, the export plan is filled as follows. `NEW_APP_ID` is
lowercase without `0x`; `IMPORT_HASH` comes from preparation, as in the runbook:

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

The export plan is inert during ordinary old-service renders/upgrades: only the explicit
`--export --migration-export` path reads it. An abandoned attempt after old writes resumed needs
fresh public plans, a fresh object prefix and reviewed approvals/revocations; follow the
[retry rules](../../../../runbooks/onchain-kms-migration.md#retry-and-rollback).
