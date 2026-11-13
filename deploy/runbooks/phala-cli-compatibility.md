# Phala Cloud CLI compatibility

The deploy kit pins `phala` **1.2.7**, `@phala/cloud` **0.6.5**, and viem **2.57.1** in
`deploy/tools/package.json` and its npm lockfile. These were the latest stable CLI/SDK releases
checked on 2026-11-12 PST. The CLI's
[`@phala/cloud: ^0.6.4` dependency](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/package.json)
admits SDK 0.6.5. Both parents require patched Zod 4.6.7, so the old deployment tooling Zod
exception is removed. Install with `npm ci --prefix deploy/tools --ignore-scripts`.

## Changes relevant to deployment

- The CLI and SDK now default to API **2026-11-11**, replacing **2026-06-23**. The new CVM
  detail schema requires `resource.memory_in_mb`; string CVM IDs remain supported. See the
  [CLI client](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/lib/client.ts#L12),
  [SDK version](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/types/client.ts#L8-L13)
  and [CVM detail schema](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/types/cvm_info_v20260121.ts#L231-L250).
- CVM reads now retain `kms_info.k256_pubkey`, which older SDK schemas stripped. The
  [new schema](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/types/cvm_info_v20260121.ts#L74-L82)
  lets the CLI's [Base encryption-key verifier](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/envs/get-encrypt-pubkey.ts)
  compare the recovered KMS signer with that key. `envs update -e` follows this verifier before
  encrypting. Read-only CLI and SDK reads of the staging Base CVM retained the real key on
  2026-11-12 PST. A local HTTP regression runs the installed CLI with a valid signature through
  encryption and the env PATCH, and proves invalid signatures cannot reach the PATCH. No live
  env update was performed. Changing allowed env names still requires onchain compose approval;
  see the [env update flow](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/envs/update/index.ts#L69-L180).
- Cloud SDK `getComposeHash` now
  [normalizes before hashing](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/utils/get_compose_hash.ts):
  it adds a missing `tproxy_enabled`, removes empty/null fields and, with an OS version, rewrites
  `manifest_version` and `init_script`. Approval decisions continue hashing the exact sorted,
  compact JSON without normalization. The raw dstack SDK export is the generic test oracle.
  Public app-compose fixtures reproduce the production `ed7dc915…`, staging Base service
  `b3ef3745…`, and staging restore-check `b7017913…` attested hashes.
- SDK [`provisionCvm`](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/provision_cvm.ts#L354-L364)
  now prepares with `POST /cvms` and requires a returned token. Its open `compose_file` schema
  preserves string `manifest_version` and newer fields. Gateway compatibility still removes
  `tproxy_enabled` from the creation payload when `gateway_enabled` is supplied. The kit's
  reviewed raw `POST /cvms/provision` then `POST /cvms` pair remains explicitly supported;
  [`commitCvmProvision`](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/commit_cvm_provision.ts#L208-L240)
  accepts legacy `app_id` + `compose_hash`. See [initial provisioning](onchain-kms-upgrade.md#initial-provisioning).
- SDK [`patchCvm`](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/patch_cvm.ts#L251-L291)
  now maps a successful `no_change` response without `correlation_id` to `correlationId: ""`.
  Ordinary updates and prepare-only success still return `requiresOnChainHash: false`.
- [`logs --serial`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/logs/index.ts#L104-L144)
  now rejects `--timestamps`, `--since`, and `--until` instead of forwarding them to serial
  channels. `--stdout`/`--stderr` are mutually exclusive container filters, and container logs
  now include stderr by default. The kit's plain serial-log command needs none of those flags.

## Contracts retained by the kit

The [PATCH action](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/patch_cvm.ts#L251-L291)
sends `X-Prepare-Only: true` only for `prepareOnly`; it never sends that option in the JSON body.
HTTP 465 still maps structured `details` entries to the compose hash, app, device, KMS info and
commit token. A successful prepare PATCH can apply directly, as the
[CLI warns](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/deploy/handler.ts#L1322-L1333).
Approved hashes therefore skip prepare PATCH and use ordinary PATCH only after maintenance
pauses. [`commitCvmUpdate`](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/commit_cvm_update.ts#L67-L91)
still POSTs `token`, `compose_hash`, and `transaction_hash` to `/cvms/{id}/commit-update`.

| Command/path | Contract consumed by deployment |
|---|---|
| [`cvms get --json`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/cvms/get/index.ts#L25-L55) | CVM fields remain at the top level: `status`, `in_progress`, `compose_hash`, `gateway`, `os`, `kms_info`. |
| [`cvms start --json`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/cvms/start/index.ts#L25-L53) | `POST /cvms/{id}/start`; JSON contains `cvm`, `dashboardUrl`, and `message`. The workflow waits for a fresh settled running read. |
| [`cvms list --json`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/lib/apps/list-apps-with-cvm-status.ts#L125-L141) | `page`, `pageSize`, `total`, `totalPages`, `items`; rows retain `appId` and `cvmName`. The CLI success envelope remains checked by `deploy.sh`. |
| [`logs --cvm-id ID --serial`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/logs/index.ts#L235-L247) | Plain console text by default; `--json` wraps `logs`, `cvm_id`, and `channel`. |
| [`api`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/api/index.ts#L343-L433) | JSON bodies remain JSON on stdout; empty 202 bodies contain no JSON object. Binary responses remain raw bytes. `--input` and `-F` still send JSON. |
| [`status --json`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/status/index.ts#L51-L58) | `team_name` remains the workspace display name, without a workspace ID. |
| [`instances add`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/instances/add/index.ts#L239-L302) | Explicit env file is encrypted; request sends `encrypted_env`, compose and pre-launch overrides without a new `allowed_envs`. The restore flow keeps inherited allowed names. |
| [`cvms attestation`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/commands/cvms/attestation/index.ts#L43-L51) | Still resolves the app before fetching attestation. Restore drills with multiple instances retain the raw UUID-addressed GET. |

The Phala KMS compose-update helper still uses the supported
[`/compose_file/provision`](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/provision_cvm_compose_file_update.ts#L151-L162)
then [`PATCH /compose_file`](https://github.com/Phala-Network/phala-cloud/blob/js-v0.6.5/js/src/actions/cvms/commit_cvm_compose_file_update.ts#L136-L140)
pair, carrying the full reviewed compose and omitting env-update fields. OS upgrades retain
`api /cvms/{id}/os-image -X PATCH -F os_image_name=…` and bounded convergence polling.
New provisioning still returns before readiness even with `deploy --wait`; the wrapper's
polling, JSON extraction and error propagation remain covered by the deployment tests.

Authentication still prefers `PHALA_CLOUD_API_KEY` over a stored profile, and
[`PHALA_CLOUD_DIR`](https://github.com/Phala-Network/phala-cloud/blob/cli-v1.2.7/cli/src/utils/credentials.ts)
isolates credentials. CI supplies an empty directory; the SDK helper gets its API key and API
prefix from the documented environment variables. No Cloud/Base write is needed to run the
local contract tests in `deploy/tests/phala-cli.mjs` and `deploy/tests/onchain-kms.mjs`.
