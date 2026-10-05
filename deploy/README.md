# Deployment reference

How an operator deploys and runs its own Phala Pay instance. Phala Pay is self-hosted: each
operator deploys a verified [release](#releases) with the settings of its own environment
repository, to its own Phala Cloud workspace, on its own domain; the
[self-hosting guide](../docs/self-hosting.md) is the order of the steps, and this document their
reference. Names in `$VARIABLES` are the operator's own values; Phala's are given only as labelled
examples, and Phala's own instance, with the demo that only Phala runs, is in [Phala's instance](phala.md).

Every CVM is deployed by GitHub Actions, by the release's [Deploy](#deploy) workflow called from the
environment repository's `main`; nothing is deployed from a laptop.
Steps outside the workflows that change a registry, Phala Cloud, a CVM, a Safe, a contract, DNS, or
a secret are marked **HUMAN-ONLY**. Backup and restore: [RESTORE.md](RESTORE.md). Incident response:
[runbooks](runbooks/README.md). Contracts: [CONTRACTS.md](CONTRACTS.md).

## What runs where

| Where | What | Deployed by |
|---|---|---|
| topup CVM, one per Environment (`staging`, `production`) | [compose.yaml](compose.yaml) with its environment directory ([Attested settings](#attested-settings)). The service variant ([compose.service.yaml](compose.service.yaml)) runs `keys` (derives the database passwords and the backup key), `postgres` (PostgreSQL 18 + WAL-G), `migrate`, `topup`, `smokescreen` (the [webhook egress](#webhook-egress) proxy), `dstack-ingress` (the only public port, 443: TLS for the [custom domain](#custom-domain)), `heartbeat`, and `backup`; the [restore-check variant](RESTORE.md#the-restore-check-variant) ([compose.restore-check.yaml](compose.restore-check.yaml)) runs `keys`, `postgres`, `migrate`, a read-only `topup` published on 8081, and `restore-check` | Deploy, target `topup` |
| Staging reference-product CVM (optional; Phala's demo, [phala.md](phala.md#staging-reference-product)) | [product/compose.yaml](product/compose.yaml) with its environment directory: `product` (on 8089, private) and `dstack-ingress` (the only public port, 443: TLS for its [custom domain](#custom-domain)) | Deploy, target `product` |
| Object storage (S3-compatible; Phala's instance: Cloudflare R2) | encrypted WAL-G base backups and WAL under `WALG_S3_PREFIX` | owner |
| The operator's Sentry project (optional; Phala's: `phala-network/crypto-topup-service`) | errors, alerts, Crons and Uptime monitors | the service itself |
| EVM chains of the routes (the committed routes: Sepolia and Base Sepolia) | the permissionless forwarder factory, at one deterministic address on every chain; forwarders; each account's own treasury | factory: any deployer ([CONTRACTS.md](CONTRACTS.md)); treasuries: each merchant, through the API |
| GitHub Actions | Phala Pay's repository: [Release](../.github/workflows/release.yml) (on a `v<version>` tag), [Verify contracts](../.github/workflows/verify-contracts.yml) (daily, read-only), [Restore drill](../.github/workflows/restore-drill.yml) (weekly, local stack). The operator's environment repository: a workflow that calls the release's [Deploy](../.github/workflows/deploy.yml) | — |

Production CVMs have no SSH, no logs, and no database access. Everything an operator sees comes
from the public endpoints, the admin API, Sentry, and the chain.

### KMS

Every CVM uses Phala Cloud's KMS (`--kms phala`, owner decision): no `DstackApp` contract and no
on-chain compose-hash approval. Fund safety does not depend on upgrade governance: the service
holds no key that moves funds and sends no transactions, and a forwarder pays only the treasury
its address commits to. Credits are what the attested service signs with each account's webhook
key, and merchants pin their keys from attestation and may cap or verify credits on their own
node. A malicious upgrade could cause downtime, read service data, or sign false credits within
what each merchant's own limits accept; the compose hash in the attestation, verified after every
deploy, makes it detectable.

### OS image

The approved OS image is `dstack-0.5.9`, non-dev. [preflight.sh](preflight.sh) accepts only that name and, online, requires a node of the
workspace to offer it. The service speaks the dstack 0.5 guest API (`dstack-sdk = "=0.1.3"`); the
local simulator is built from the same release. dstack 0.6 derives different keys for the same
domain, so moving to it changes every account's webhook keys (which merchants pin), the backup
key, and the database passwords: that is a key migration, not an image bump.

## One-time setup (HUMAN-ONLY, repository owner)

In the operator's environment repository ([self-hosting, "Your environment
repository"](../docs/self-hosting.md#2-your-environment-repository)); Phala's is this repository,
through [Deploy Phala's instance](../.github/workflows/deploy-phala.yml). Workflows run on
GitHub-hosted `ubuntu-latest` runners.

1. **Environments.** Repository Settings > Environments: `production` and, for a pre-production
   instance with test routes only, `staging` (the names Deploy offers), deployment branches `main`
   only. Required reviewers are optional (Phala's repository has none: its plan does not offer
   them). Whoever dispatches Deploy is accountable; the run's actor, summary, and uploaded record
   are the audit trail.
2. **Phala Cloud.** Create an API key for the Environment's workspace and store it as the
   Environment secret `PHALA_CLOUD_API_KEY`, the only secret GitHub holds (a repository secret for
   a caller in another organisation, [Deploy](#deploy)).
3. **Object storage.** Create a bucket (or prefix) per Environment in an S3-compatible store, such
   as Cloudflare R2, that the other Environment's keys cannot reach, and a read-write API token for
   it. The token is sealed into the CVM (below), never stored in GitHub.
4. **Environment variables**, per Environment: only deployment state, none of it attested.

   | Name | Value |
   |---|---|
   | `PHALA_WORKSPACE` | display name of the API key's workspace (preflight checks it) |
   | `TOPUP_CVM_ID` | empty until the first provisioning, then the CVM id from the run summary |
   | `STAGING_PRODUCT_CVM_ID` | `staging` only, for the optional [staging reference product](phala.md#staging-reference-product) |

   Every setting of the CVM is committed instead, in the Environment's directory, for example
   `production/topup/` (Phala's:
   [deploy/environments/phala-network/staging/topup](environments/phala-network/staging/topup)), copied
   from the kit's [deploy/environments/example](environments/example/topup) ([Attested settings](#attested-settings)):
   the domain, the object store's endpoint and backup prefix, the admin public key (from
   `topup-sdk keygen --keyid admin/<Environment>-v1`, a separate key per Environment; the seed stays
   with the admin), the RPC providers, and the routes. Deploy renders that directory and nothing
   else, so a setting changes only through a reviewed commit and a Deploy `upgrade`.
   The example ships production-eligible Chainlink USDC/USDT routes on Sepolia. PHA is
   staging/noncommercial only ([price sources](../docs/configuration.md#price-sources)).

   No setting names a treasury or a transaction-signing key: treasuries are each account's own, set
   through the API, and the service sends no transactions.
5. **Sentry** (project admin; the Crons monitors create themselves on their first check-in):
   - Settings > Security & Privacy: keep *Data Scrubber* and *Use Default Scrubbers* on; turn
     *Prevent Storing of IP Addresses* on.
   - An alert for the environments `staging` and `production` (not `*-restore`) that notifies the
     on-call owner when an issue is created or regresses, with no level filter (most alert lines are
     `warning` events). Confirm that the Crons and Uptime monitors are listed as connected.
   - One Uptime monitor per Environment (UI only): `GET https://<domain>/healthz`, interval
     1 minute, timeout 10 seconds, environment = the Environment's name.
   - The project DSN (Settings > Client Keys) is sealed as `SENTRY_DSN` ([Sealing the
     secrets](#sealing-the-secrets)).

## Release and deploy

### Releases

A release is a `v<version>` tag of Phala Pay on a commit of `main`, the Cargo workspace version,
which the SDKs share ([CONTRIBUTING.md, "Releasing"](../CONTRIBUTING.md#releasing)).
**Prerequisites (repository settings, HUMAN-ONLY, admin; both in place):** a `refs/tags/v*`
ruleset restricting creation, update, and deletion to admins, and immutable releases, so a
published `v<version>` always names the same commit and assets.

CI runs the real-service `make cvm-rehearsal` on every pull request and push to main, and again
before Release builds anything. It asserts a credited payment and an acknowledged, signed event
redelivery with the receiver's credit unchanged. No live deployment or production credentials
are involved; the existing rehearsal uses public HTTPS price sources.

The tag runs [Release](../.github/workflows/release.yml). It runs the whole CI workflow on the
commit first, then builds each image on its own GitHub-hosted runner with
[verify-image.sh](verify-image.sh) (Buildx v0.37.1 and BuildKit v0.33.0 pinned by digest; `make
verify-image` rebuilds the same way), pushes it to `ghcr.io/phala-network/` tagged `v<version>`
(public packages: CVMs pull without credentials), and smoke-tests and attests it by the digest
BuildKit reports for the push (`--metadata-file`), never by the tag.
All three images are reproducible: the pushed build must have the digest of an earlier build,
and anyone rebuilds it from the tag. `postgres-walg` pins the Debian snapshot and direct package
versions, removes apt/dpkg timestamp logs and ldconfig's auxiliary cache, and uses BuildKit's
filesystem timestamp normalization. gosu and WAL-G are rebuilt with a pinned, patched Go
toolchain and dependencies; WAL-G keeps libsodium encryption and Brotli/LZO restore support.
After comparison, Trivy scans each final digest and generates
an SPDX SBOM; `actions/attest-sbom` signs it and attaches it to the digest in GHCR, alongside build
provenance. [verify-release.sh](verify-release.sh) requires both attestations with the release
workflow identity, source commit, and GitHub-hosted runner constraints. A stable release then publishes the SDKs at its version,
`@phala/pay`, `@phala/pay-react`, and `@phala/pay-server` to npm, plus `phala-pay` to PyPI, with trusted publishing. The GitHub release's assets,
each attested, are:

- `images.json`, each image's `repository@sha256` by name ([render.sh](render.sh)'s `--images`);
- the deploy kit `phala-pay-deploy-v<version>.tar.gz` ([build-kit.sh](build-kit.sh): `LICENSE`,
  `deploy/`, and `docs/` of the tag, a tar identical to their `git archive`);
- `phala-cloud-template.yml` ([The Phala Cloud template variant](#the-phala-cloud-template-variant));
- `deploy.sh`, [deploy.sh](deploy.sh) set to deploy this release, which generates an admin key
  with its Python SDK, the one-command deploy that
  `https://pay.phala.com/deploy.sh` (the latest release) and `/deploy/v<version>.sh` redirect to
  ([self-hosting, "One-command deploy"](../docs/self-hosting.md#one-command-deploy));
- `SHA256SUMS`.

[verify-release.sh](verify-release.sh) is the one verification of a release, run by Deploy and by
hand ([self-hosting, "Verify a release"](../docs/self-hosting.md#verify-a-release)). It stops at
the first failure: the tag's commit must be in `main`'s history, the assets must match
`SHA256SUMS`, and every asset and image must have a provenance attestation of `release.yml` at the
tag, on a GitHub-hosted runner, for that commit (`gh attestation verify --source-digest`). Each
image must also have an SPDX 2.3 SBOM attestation under the same identity and source constraints.

### Deploy

[Deploy](../.github/workflows/deploy.yml) is the source of truth for what a deployment checks and
does. It is a reusable workflow, which a repository calls at a release, with the same `version`:

```yaml
uses: Phala-Network/phala-pay/.github/workflows/deploy.yml@<release commit or tag>
```

Its inputs are `version`, `environment` (the GitHub Environment), `target` (`topup`, or `product`
for Phala's reference product), `mode` (`provision` or `upgrade`), and `environment_dir` (the
target's directory in the caller's repository). The caller grants `contents: read` and
`attestations: read`, and Deploy reads `PHALA_CLOUD_API_KEY` plus
`TOPUP_MAINTENANCE_PRIVATE_KEY_PEM` for topup upgrade admission control. A caller in the Phala-Network organisation passes `secrets: inherit`, and the
key is the Environment's secret (an Environment secret resolves empty in a called workflow without
it, [actions/runner#4453](https://github.com/actions/runner/issues/4453)); a caller in another
organisation, where `inherit` is not supported, passes it from a repository secret ([self-hosting,
step 5](../docs/self-hosting.md#2-your-environment-repository)). It runs verify-release.sh of its
own commit, which must be the release's, then only the verified kit's scripts: render, preflight,
the deploy with the kit's pre-launch script, and the verification of the attested compose with the
official dstack verifier; it uploads the rendered compose and the verification as the run's record.
[Deploy Phala's instance](../.github/workflows/deploy-phala.yml) is Phala's caller, the same way;
adopting a release is a pull request that changes its release.

#### Planned upgrade admission and downtime

Before a topup upgrade calls Phala Cloud, the workflow admin-signs
`POST /v1/admin/instance/pause` with scope `mutations`, its run/attempt owner, and a 900-second
lease. Configure the protected GitHub Environment's `TOPUP_MAINTENANCE_PRIVATE_KEY_PEM` secret
(Ed25519 PEM) and `TOPUP_MAINTENANCE_KEY_ID` variable to match an entry in the running instance's
attested `maintenance_keys`. External callers pass the declared signing secret alongside the
Cloud key; organisation callers inherit both. This separate key authorizes only
`POST /v1/admin/instance/pause` and `POST /v1/admin/instance/resume`; all other admin routes,
including maintenance inspection, return audited `403 permission_denied`. The operator's full
admin key still works, but stays with the operator and is never a deployment secret. Restrict
Environment access and deployment branches as for the Cloud key. Key material is temporary, never part of
the uploaded deployment record, and is deleted after each signed request.

Generate an independent key per Environment on the owner's machine using the installed Python
SDK CLI. Convert its seed to the PEM the workflow helper accepts:

```sh
topup-sdk keygen --keyid maintenance/production-v1 --seed-out maintenance.seed &&
(umask 077 && { printf '302e020100300506032b657004220420'; tr -d '\n' < maintenance.seed; } \
  | xxd -r -p | openssl pkey -inform DER -out maintenance.pem)
```

The CLI prints `keyid` and standard-base64 `public_key`. Commit only those public values in the
Environment's `topup/topup.yaml`, next to `admin_key`, before adopting this workflow:

```yaml
maintenance_keys:
  - id: maintenance/production-v1
    public_key: <public_key printed by topup-sdk keygen>
```

The list is optional (empty by default), at most eight keys. IDs and public keys must be unique
and distinct from the admin key; missing/invalid public keys refuse configuration. Configure
`TOPUP_MAINTENANCE_PRIVATE_KEY_PEM` from the complete `maintenance.pem` through the owner's
secret-management flow, and `TOPUP_MAINTENANCE_KEY_ID=maintenance/production-v1`. Never upload the
admin seed or PEM. Maintenance signatures use the same origin, digest, five-minute freshness,
and single-use replay checks as admin signatures; successful changes record the actual key id.

**Rotation:** generate v2 independently, retain v1 and add v2's public entry in a reviewed config
upgrade, using v1 to drain that upgrade. After health and resume pass, switch the Environment's
maintenance PEM/key id to v2, verify a signed maintenance operation, then remove v1's public
entry in another reviewed upgrade using v2. Keep the old credential until the first upgrade's
cleanup completes. Do not rotate credentials during a running deployment. For first adoption,
install the public key in the feature rollout allowed by `bootstrap_maintenance` below; a
pre-feature service cannot recognize the key. Any policy-permitted rollback to a release
predating `maintenance_keys` also needs that release's compatible config without this field,
because its strict config parser rejects unknown fields. This does not override the 0.9.0
declaration: **no rollback to 0.8.3; restore required**, and no rollback to any 0.8.x release.

New authenticated mutations return `503 service_maintenance`, `Retry-After: 5`, before idempotent
execution; reads and already admitted requests continue. Scanners and outbox workers continue until the
existing graceful shutdown drains them. A replacement process starts with a `startup` mutation pause, lifted only by its first successful
`/healthz` database check; the old process's health probes never lift its explicit deployment
drain. The workflow then confirms resume with the deployment owner after health checks. Failed
runs attempt to resume the serving process. Each process lease expires after at most 900 seconds
according to its monotonic clock, even without a runner or expiry worker. Process exit discards
its lease; startup health protects the replacement, so a failed deploy cannot persist maintenance.
Pause/resume are audited using the existing audit table; maintenance adds no database migration
or persistent lease to carry across process replacement. The release's
[rollback compatibility policy](#rollback-compatibility) still governs recovery. Account,
customer, treasury, and route incident pauses remain. See the
[stuck-maintenance runbook](runbooks/instance-maintenance.md) for inspection and manual clear.

Upgrade probes run from before the upgrade call through service readiness, sampling `/healthz`
every 2 seconds with a 3-second timeout. `unavailability.json` records `first_failed_at`,
`first_healthy_at`, and `unavailability_seconds`; the job summary shows the same window in PT
and UTC. Maintenance admission and clear responses are separate artifacts. Failed/canceled runs
keep an incomplete window if recovery was not observed; no observed failure is not a claim of
zero downtime. Staging run 37215125136 took about 160 seconds (upgrade step to service readiness),
which motivates the SDK's opt-in 300-second budget. This masks transient failures for clients;
the CVM still restarts and raw HTTP callers must implement retries.

**First rollout:** a running pre-feature release has no instance pause API, so it cannot emit a
maintenance signal retroactively. Distribute the tolerant JS SDK first. The owner may explicitly
pass `bootstrap_maintenance: true` to the new reusable workflow for that one upgrade: only an
HTTP 404 from the old pause path allows bootstrap. A feature-capable service still requires the
normal signed pause, and missing credentials or any other pause failure aborts before the upgrade
call. Bootstrap is recorded in the job summary and disabled by default; remove it after rollout.
When adopting the feature release in a caller workflow, expose this boolean only if the first
upgrade needs it (the currently pinned Phala caller remains unchanged until release adoption).
For a policy-permitted rollback to a pre-feature release, the new process has no inherited
maintenance lease; a resume response may be 404, but normal admission is already open. This
maintenance compatibility behavior does not authorize rollback to 0.8.3 or any 0.8.x release:
restore from the pre-upgrade backup as declared in [rollback compatibility](#rollback-compatibility).
Deadline expiry remains the fallback if the original process is still serving after a failed
upgrade. No compatibility bypass is automatic.

1. Merge the change to the environment repository's `main`: a setting, or a new `version`.
2. First deployment: Deploy with `mode: provision`, then set `TOPUP_CVM_ID` (or, for the product,
   `STAGING_PRODUCT_CVM_ID`) to the CVM id in the summary, [seal the
   secrets](#sealing-the-secrets), and create the [DNS records](#custom-domain) it lists. A
   provision only creates the CVM, which must boot but shows `error` until it is sealed, so it
   proves nothing about its health: the acceptance step is the `mode: upgrade` run with the same
   release after the sealing, which requires the CVM running, `/healthz`, the attested compose, and
   the certificate evidence. If a provision run fails after the summary shows a CVM id, set the
   variable, seal the secrets, and run that upgrade; never provision twice.
3. Every later change: Deploy with `mode: upgrade`. An upgrade sends only the compose, so the
   sealed env stays. Rollback supports the immediately previous release (N-1) only, after the release
   rollback smoke passes. Keep the upgraded schema; never run down migrations. See
   [Rollback compatibility](#rollback-compatibility) before changing the version.

**Production** with live routes additionally needs the factory at its deterministic address on each
of their chains ([CONTRACTS.md](CONTRACTS.md#mainnet), HUMAN-ONLY, only where it is missing) and a
reviewed route PR putting the live routes into the compose. One production deployment serves both
modes: live routes on mainnets and test routes (`livemode: false`) on test networks such as Sepolia.
Deploy runs [check-route-modes.sh](check-route-modes.sh) on the rendered compose's routes, as topup
reads them, and refuses a route
whose `livemode` does not match its chain, a chain on neither of its lists, a local development
chain, and any live route in `staging`. After the first deploy, in order: seal the secrets; [verify
the attestation](#attestation-ingress-and-egress); have the operator's Finance, Risk, and Operations
approve the pilot limits (route bounds, each account's caps (`limits`: open quotes and their credit
per account and per customer) and `max_unfinalized_credit`; architecture §14) and a passed restore
drill ([RESTORE.md](RESTORE.md)); then [onboard](#operator-onboarding) accounts with
`charges_enabled` (Phala's instance adds its own [onboarding policy](phala.md#onboarding-policy)).

### Sealing the secrets

The CVM's encrypted env holds the rendered compose's sealed names, its `${NAME:-}` references.
The Phala Cloud CLI makes the names of the env file it sends the CVM's `allowed_envs`, and
[verify-attestation.sh](verify-attestation.sh) requires them to be among the sealed names
([compose-policy.jq](compose-policy.jq), `allowed_envs_violations`): another name is refused, since
it would put in the env a value the compose does not reference. A sealed name left out is unset,
like an empty one; a required secret is enforced where it is used, on every start: the
postgres-walg entrypoint refuses S3 storage unless both `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` are set, so neither PostgreSQL nor the backup job starts and the acceptance
upgrade's `/healthz` fails, and topup refuses a keyed provider without its key. The
names are part of the app-compose, so sealing with the names a CVM already allows keeps its compose
hash. The sealed names are:

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, the object store's token (declared in
  [compose.yaml](compose.yaml));
- `SENTRY_DSN`, required for production; staging and templates may leave it empty to turn Sentry off;
- a `TOPUP_RPC_<ID>_KEY` per keyed [RPC provider](#rpc-providers), declared in the environment's
  `compose.yaml` overlay. It is the API key topup puts in place of `{key}` in the provider's
  attested URL. Phala's staging has keyless providers and declares none.

Each line is `NAME=VALUE`, the value as is. The Phala Cloud CLI reads the file as dotenv does, which
would cut a value at `#` and strip quotes and surrounding whitespace, so preflight refuses a value
with any of them. A key is at least 8 characters of `A-Z a-z 0-9 - . _ ~`. topup refuses a provider whose URL and key
disagree: a `{key}` without a key, or a key without a `{key}`. Redaction keeps the URL and the key
out of every log line and error. A new CVM waits for its secrets: PostgreSQL initializes a cluster
only after listing an empty backup prefix ([RESTORE.md](RESTORE.md#bootstrap-from-backup)).
`deploy/validate-compose.sh` fails if staging's sealed names change, since that needs the re-seal
below.

**HUMAN-ONLY, owner, from their own machine**, with the deployed release's verified kit in `kit/`,
the environment repository at the deployed commit, and the rendered compose from the run's
artifact (the provision summary prints these commands):

```sh
docker pull <the compose's phala-pay image>   # --offline pulls nothing; it runs topup config check in it
kit/deploy/preflight.sh --env .env.ENV --compose docker-compose.ENV.yml \
  --environment-dir ENV/topup --offline      # .env.ENV: mode 0600
kit/deploy/phala envs update "$TOPUP_CVM_ID" -e .env.ENV
```

Offline, preflight checks every provider's key against its URL (`topup config check --secrets`)
without printing it. Deploy's preflight has no keys, so with a keyed provider it skips the asset
chain checks; run preflight without `--offline` (with `--workspace` and `--os-image`) to run them
with the keys.

The CVM restarts, `/healthz` answers, and backups have started once a WAL segment younger than two
minutes is listed (`aws s3 ls "${WALG_S3_PREFIX%/}/wal_005/" --endpoint-url "$AWS_ENDPOINT" | tail -1`).
Re-seal the same way whenever a secret changes; never change a setting with `envs update`.

**A new sealed name** (a new keyed provider's `TOPUP_RPC_<ID>_KEY`) is not among the CVM's allowed
names, which Deploy `upgrade` keeps, so after the upgrade it would be unset and topup would refuse
to start (a `{key}` without a key). Once, before that upgrade, run only the `envs update` above with
`.env.ENV` holding the new compose's sealed names: the running compose ignores the new name, and
preflight against it would refuse it. A new keyless provider adds no name.

### Attested settings

A value in the encrypted env is outside the attestation: whoever can run `phala envs update` could
change it without changing the compose hash. So the env holds only the secrets above, and every
other setting is written into the compose that the attestation covers, from reviewed files. There
are four kinds of input, each with one home ([design](../docs/design/deploy-config.md)):

| Kind | Home |
|---|---|
| Topology: the services, their mounts, ports, and flags | [compose.yaml](compose.yaml), [compose.service.yaml](compose.service.yaml), [compose.restore-check.yaml](compose.restore-check.yaml) (and [product/compose.yaml](product/compose.yaml)) |
| The Environment's public settings | its directory in the environment repository, for example `production/topup/`: `topup.yaml` ([the configuration file](../docs/configuration.md#the-configuration-file): `public_origin`, `admin_key`, `rpc_groups`, `rpc_companies`, `rpc_budgets`, `routes`) and a `compose.yaml` overlay with the env interfaces of third-party images: WAL-G's `WALG_S3_PREFIX`, `AWS_ENDPOINT`, `AWS_REGION`, `AWS_S3_FORCE_PATH_STYLE`, dstack-ingress's `DOMAIN`, and the keyed providers' sealed key names |
| Deploy-time facts | [render.sh](render.sh)'s three inputs: `--images` (the release's `images.json`), `--gateway-domain` (the CVM node's gateway, dstack-ingress's `GATEWAY_DOMAIN`), and, for the restore-check variant only, `--origin` (the restore instance's own origin) |
| Secrets | the CVM's sealed env ([Sealing the secrets](#sealing-the-secrets)) |

[render.sh](render.sh) renders them with Docker Compose v2.26.0, the version the CVM runs, pinned by
sha256 ([pinned-compose.sh](pinned-compose.sh)):

```sh
kit/deploy/render.sh --images images.json --gateway-domain gateway.dstack-pha-prod5.phala.network \
  production/topup >docker-compose.production.yml
```

- It merges the stack, the environment overlay, and the variant overlay with
  `config --no-interpolate`, which keeps the sealed `${NAME:-}` references. The overlay may set
  only the settings in the table above: the merge with it may differ from the merge without it
  only in those environment keys, so it can change no image, command, entrypoint, config, mount,
  port, or service.
- It pins each image to the release's digest, and every image must then be one of the release's
  or one the kit pins by digest (dstack-ingress).
- It inlines each config file (the environment's `topup.yaml`, the PostgreSQL init script) as
  content named after its digest. A changed file therefore changes the definition of exactly the
  services that mount it, and Compose recreates them.
- It prints Compose's canonical YAML under the project name `dstack`, which is the name dstack gives
  the stack it runs in `/dstack`, so the volumes keep their names.
- Before printing, it applies [compose-policy.jq](compose-policy.jq), the policy that
  `validate-compose.sh`, preflight, and `verify-attestation.sh` apply to the same artifact:
  - the variant's exact services;
  - the only published port;
  - the dstack socket;
  - each credential volume on tmpfs, mounted by exactly its services, read-only but for `keys`;
  - smokescreen with its exact deny list;
  - restore isolation;
  - each sealed name only as the whole value of its own environment key, so a sealed value can
    never fill the origin, the admin key, or an RPC host.

`topup.yaml` itself is validated by `topup config check`, which reads no secret. To change a
setting, change its file by pull request and run Deploy `upgrade`. Phala's reference product
renders the same way from
[deploy/environments/phala-network/staging/product](environments/phala-network/staging/product)
(its `config.json` and domain).

### The Phala Cloud template variant

`render.sh --template` renders the Phala Cloud template, a one-click testnet quick start
([self-hosting, "The Phala Cloud template"](../docs/self-hosting.md#the-phala-cloud-template)),
from [environments/phala-cloud-template](environments/phala-cloud-template/topup): Phala's staging
routes and keyless providers. Each release publishes it as `phala-cloud-template.yml`, and
Phala Cloud's template, once published, is that file byte for byte.
[compose.template.yaml](compose.template.yaml) removes `dstack-ingress` and `restore-check`, and
topup publishes `80:8080`, which the Phala Cloud gateway serves as
`https://<app-id>.<gateway-domain>`.

A template's per-deployment values come from the CVM's env, never from `topup.yaml`, whose `$` are
escaped like every environment's. The policy's `template` variant allows a runtime reference as
the whole value of exactly these environment keys, and nowhere else:

| Value | Where | Source | Checked at startup by |
|---|---|---|---|
| `DSTACK_APP_DOMAIN` | `topup`, read by `topup run --public-origin-host-env DSTACK_APP_DOMAIN` | the reviewed Phala Cloud pre-launch script (below), from the app id and the gateway domain the host provides | topup: a lowercase DNS name, served as `https://<host>` |
| `TOPUP_ADMIN_PUBLIC_KEY` | `topup`, read by `topup run --admin-public-key-env TOPUP_ADMIN_PUBLIC_KEY` | the deploy form | topup: a standard base64 ed25519 public key |
| `WALG_S3_PREFIX`, `AWS_ENDPOINT`, `AWS_REGION` | `postgres` and `backup` | the deploy form | the postgres-walg entrypoint: `s3://BUCKET[/PATH]`, an `https://` origin, a region name |

The backup and restore helpers also accept these operational limits (the defaults are deliberately
finite and are passed to both `postgres` and `backup`; an environment may override them):

| Variable | Default |
|---|---:|
| `WALG_WAL_TIMEOUT_SECONDS` / `WALG_WAL_ATTEMPTS` | `10` / `1` |
| `WALG_BASE_TIMEOUT_SECONDS` / `WALG_BASE_ATTEMPTS` | `1800` / `2` |
| `WALG_RESTORE_TIMEOUT_SECONDS` / `WALG_RESTORE_ATTEMPTS` | `120` / `3` |
| `WALG_OBSERVABILITY_DIR` | `/run/topup-observability` |
| `WALG_BIN` | `wal-g` |

The heartbeat commits every 15 seconds so WAL upload latency does not consume the full 60-second
recovery point budget. These names are the limits used by the committed WAL-G backup/restore helpers.

The template's `topup.yaml` leaves out `public_origin` and `admin_key.public_key`; `topup run`
refuses to start unless each comes from exactly one place, and the policy fixes topup's command,
so a service compose can never take either from its env. These values are not attested: whoever
controls the workspace can change them without changing the compose hash. Everything else is:
the services, the routes and providers, smokescreen, the credential volumes, and the sealed names.
[verify-attestation.sh](verify-attestation.sh) checks a template CVM with the variant `template`.
The Phala Cloud CLI, and the deploy form likewise, puts in `allowed_envs` only the values it is
given, so a template CVM deployed without the optional `SENTRY_DSN` has the other sealed names
only; the verifier accepts that ([Sealing the secrets](#sealing-the-secrets)) and refuses any other
name, `DSTACK_APP_DOMAIN` included. A missing form value is refused at startup by the service that
reads it (the table above). An instance with merchants uses the service variant.

**No restore-check path.** A template instance cannot be restored through
[RESTORE.md](RESTORE.md)'s restore-check variant. That variant renders from an environment whose
backup prefix, origin, and admin key are attested literals, and its guarantees rest on them: the
verification instance proves which prefix it restored, reads it only with its own read-only
credentials, and is served under an explicit origin. The template's prefix, origin, and key are
runtime values, so no attested restore-check compose could prove any of that, and `render.sh`
refuses `--template` with `--restore-check`. A template instance's data survives only as its app's
backups: another instance of the same app, deployed with the same form values, restores the
newest backup when it first starts, without a read-only verification first. That is acceptable for
a testnet quick start and is why an instance with merchants uses the service path.

### RPC providers

Every route explicitly names `chain.rpc_groups: { a: ..., b: ... }`. A scans logs and
reconciles; B independently confirms receipts, heads and calls. Credit still requires both.
Top-level `rpc_groups` defines 1–8 upstream URL/credential pairs and a bounded failover or
weighted round robin policy per group. `rpc_companies` records reviewed company ownership;
the company sets of A and B must be disjoint. Domains support that review, and aliases/resellers
must not create artificial independence.

Each member's `url` is attested with `{key}` as a whole path segment or query value; its explicit
`sealed_key` names the sealed `TOPUP_RPC_*_KEY`, declared for topup and restore-check in the
compose overlay. The same URL template can use several credentials within one group. Explicit
account/key budgets are shared across methods and chains; keyless members get synthetic scopes.
All redirects are refused, and the in-process clients use no response cache or RPC sidecar.

`topup config check --secrets FILE` validates identities, bounds, quota references and keys
without network access. `topup rpc check --config FILE` performs counted per-member probes;
first acceptance needs one verified serving member in each group, while offline backups may
remain configured. Recovery requires full identity/capability/head probes after cooldown.
A member must support the complete numeric log range and recipient-only filters before serving A.
An empty eligible group waits; no fallback crosses A/B or bypasses the agreement gate.

See [configuration](../docs/configuration.md#the-configuration-file),
[`GroupPolicy`](../crates/adapters/src/chain/evm/group/mod.rs) and [the runbook](RPC.md) for policy fields,
staging singleton migration, historical review, alerts and owner-only recovery. Adding a chain
requires explicit A/B groups and its normal reviewed route/contracts deployment; route versions
are not changed just to migrate RPC configuration.

### Custom domain

The API has one public origin, `public_origin` (`https://$DOMAIN`, where `$DOMAIN` is
dstack-ingress's `DOMAIN` in the environment overlay), a stable name the owner controls, not
the CVM's gateway URL, which changes with the node and the app id: merchants call it and pin its
attestation, treasury challenges (EIP-4361) name it as their `domain` and `uri`, and the admin
API verifies every signed `@target-uri` against it. The official [dstack-ingress](https://github.com/Dstack-TEE/dstack-examples/tree/dstack-ingress-v2.6/custom-domain/dstack-ingress)
2.6 (`ghcr.io/dstack-tee/dstack-ingress`, pinned by the digest of its release notes; `gh
attestation verify oci://ghcr.io/dstack-tee/dstack-ingress@sha256:c212abb7bedec4d7b54a82bcf9972e58e39a4757cc811faeb6017eee6c0673b0
--owner Dstack-TEE` shows it was built from tag `dstack-ingress-v2.6`) publishes the compose's only
port, 443. The gateway passes the TLS connection for the domain through to it, it terminates TLS
inside the CVM, and forwards the stream to `topup:8080`. It gets the Let's Encrypt certificate
with `tls-alpn-01` through that same port, so the CVM holds no DNS credentials; the ACME contact is
unset because the account document is published. Like `keys` and `topup` it mounts the dstack
socket (its instance id and the evidence quote), which is why it is pinned by digest and attested
with the compose.

A reference-product CVM is served the same way on its own overlay's `DOMAIN`
([Phala's instance, "Staging reference product"](phala.md#staging-reference-product)).

**HUMAN-ONLY, owner of the domain's DNS zone** (any DNS provider; the Deploy summary's
wording assumes Cloudflare, as in Phala's instance), once per CVM instance. Every Deploy run
lists the records for its target's domain (`$DOMAIN`, the `DOMAIN` of its environment overlay), in
the tls-alpn-01 format of the pinned README:

| Type | Name | Content |
|---|---|---|
| CNAME | `$DOMAIN` | the CVM node's gateway, `gateway.<base domain>` |
| TXT | `_dstack-app-address.$DOMAIN` | `<instance_id>:443` |
| CAA (optional) | `$DOMAIN` | `0 issue "letsencrypt.org;validationmethods=tls-alpn-01;accounturi=<ACME account>"` |

- Not proxied (on Cloudflare, DNS only, grey cloud): a proxied name resolves to the proxy, so
  neither the CA nor a client reaches the gateway.
- The TXT names the instance, not the app: the CA's validation must reach the one instance that
  holds the ACME order. An upgrade keeps the instance id; a new instance ([Resume](RESTORE.md#resume))
  serves the domain only after the TXT carries its id.
- Until both records resolve, dstack-ingress serves a self-signed placeholder and requests no
  certificate, so Deploy's `/healthz` wait on the domain fails.
- CAA is optional. An existing CAA record on the domain or a parent domain must permit
  `tls-alpn-01` (Phala's `phala.com` has none as of 2026-09-26). Pinning `accounturi` to the account that
  [verify-ingress-evidence.sh](verify-ingress-evidence.sh) prints also means that losing the
  `ingress_certs` volume (a new account) blocks renewal until the record is updated.

**Certificate evidence.** dstack-ingress publishes, at `https://$DOMAIN/evidences/`, the
ACME account, the certificate, `sha256sum.txt` over both, and a TDX quote whose `report_data` is
the hash of `sha256sum.txt`. Its evidence server listens on the ingress container's loopback (port
80) and is reached only through 443, where HAProxy routes `GET /evidences` to it, so there is no
evidence port to publish. [verify-ingress-evidence.sh](verify-ingress-evidence.sh) checks the
chain with the official verifier (the quote is app `APP_ID`'s and binds the files) and that the
domain serves exactly that certificate; Deploy runs it after every topup upgrade:

```sh
kit/deploy/verify-ingress-evidence.sh "$DOMAIN" "$APP_ID"
```

The quote dates from the last issuance, so its compose hash can be an earlier compose of the app.

### Database credentials

PostgreSQL runs inside the CVM, so its passwords are derived there, never supplied. `keys`
(`topup keys`) derives the WAL-G key (`get_key("backup/v1")`) and the owner and application
passwords (hex of `get_key("db/owner/v1")`, `get_key("db/app/v1")`) into tmpfs volumes; each
service mounts only what it needs, read-only, and `validate-compose.sh` enforces it:

| Volume | Files | Mounted by |
|---|---|---|
| `walg_key` (`/run/wal-g`) | `backup.key` | `postgres`, `backup` |
| `db_owner` (`/run/db-owner`) | `postgres.password`, `postgres.pgpass` | `postgres`, `migrate`, `backup`, `restore-check` |
| `db_app` (`/run/db-app`) | `topup_service.pgpass` | `postgres`, `topup`, `heartbeat` |

This is separation by mount and by database role, not by KMS: `keys`, `topup`, `restore-check`, and
dstack-ingress mount the dstack socket, and any holder of it can derive any key path. PostgreSQL,
WAL-G, and `migrate` never see the socket, and topup never gets the owner login's files.
`migrate` and `restore-check` refuse any login but the database owner (`DATABASE_URL`).

The same app id derives the same passwords, so a replacement or restored instance logs in
unchanged. Rotation is an `ALTER ROLE` to a `db/*/v2` value and a new compose.

## Sentry

The service reports to Sentry itself, only while `SENTRY_DSN` is non-empty (a malformed DSN stops
`topup run`). The release is the source commit compiled into the image (`SOURCE_COMMIT`), and the
environment is `topup.yaml`'s `environment`, both attested.

- **Events**: every `ERROR` line and panic, grouped by message, at most one event per issue every
  10 minutes. An event holds the log line's fields minus `account_id`; no request data, no RPC
  URL, no spans, `send_default_pii` off.
- **Alerts**: lines tagged with an alert name, fingerprinted by the name and its grouping
  tags (route, state, check, chain, scope), with a `runbook` tag. The names and their runbooks are
  in the [runbooks index](runbooks/README.md#alert-and-symptom-index).
- **Crons**: each loop checks in and so creates its monitor:

  | Monitor | Checks in | Margin |
  |---|---|---|
  | `topup-scanner-<chain_id>` | after each head poll (every block time), `error` while the finalized backstop fails | 5 min |
  | `topup-pump-<n>`, `topup-outbox-test`, `topup-outbox-live` | each iteration or poll, every minute | 5 min |
  | `topup-lock-expiry` | after each successful expiry scan, every minute | 5 min |
  | `topup-finality-watch` | after each `finalized` advance's passes, and every minute | 5 min |
  | `topup-reconciler` | `ok` after a complete round or one skipped because nothing newly finalized, `error` after failed checks, every 10 min | 10 min |
  | `topup-backup` | `ok` while the WAL-G success marker is at most 120 s old, else `error`; 3 errors open an issue | 2 min |

- **Uptime**: `/healthz` of each Environment ([One-time setup](#one-time-setup-human-only-repository-owner)).
- **Egress**: `topup` sends HTTPS to the DSN's ingest host.

A restore-check instance runs no loop and reports as `<environment>-restore`, so it never checks
in or raises an alert of the live environment.

## Measuring RPC usage

Every JSON-RPC call the service sends is counted by configured provider id (`provider-a`,
`provider-b`, never a URL), chain id, and method (16 named methods; any other counts as
`other`), from process start. The admin-signed `GET /v1/admin/metrics` returns the counters in
the Prometheus text format; with the `admin` helper from the
[runbooks](runbooks/README.md#environment):

```sh
admin GET /v1/admin/metrics
```

```text
topup_rpc_calls_total{provider="provider-a",chain_id="11155111",method="eth_blockNumber"} 8012
topup_rpc_calls_total{provider="provider-a",chain_id="11155111",method="eth_getLogs"} 7390
topup_rpc_calls_total{provider="provider-b",chain_id="11155111",method="eth_getTransactionReceipt"} 214
topup_rpc_calls_since_seconds 1790500000
```

A day's usage is the difference of two readings a day apart (or a counter over
`now − topup_rpc_calls_since_seconds`, scaled to a day); counters restart at zero with the
process. A production CVM has no collector, so read it on staging, or occasionally on
production, with the admin key.

**Cost formula.** For one provider and one chain, with `n(m)` the calls a day of method `m` and
`p(m)` the provider's price of one call of `m` (compute units, credits, or currency: the
provider's current price list is the input, not this document):

```text
cost per day   = Σ_m n(m) × p(m)
cost per month = 30 × cost per day
```

The cadences of docs/architecture.md §8 predict `n(m)` for provider A with `B` scanned heads a day
(86 400 / head poll interval: 7 200 on Ethereum; 43 200 on an OP-stack chain crediting at a depth,
polled every 2-second block; 7 200 on one crediting at `safe` or `finalized`, polled every 12 s),
`F` `finalized` advances a day (225 on Ethereum),
`R` reconciliation rounds a day (at most 144, and at most `F`), `P` payments a day, `A` issued
addresses, `U` forwarders holding unswept funds, and `L = 1` in token mode or `⌈A / 1 000⌉` in
address mode:

| Method | Calls a day, provider A | Provider B |
|---|---|---|
| `eth_blockNumber` | `1.1 B` (head polls) `+ P` (credit check) | `P` |
| `eth_getBlockByNumber` | `86 400 / finalized poll interval` (1 440) `+ F` (backstop), `+ B` for a `safe` route | `≤ min(P, F)` (finality passes) |
| `eth_getLogs` | `L × B` (per-block scan) `+ (L + 1) × F` (backstop) `+ ⌈A / 1 000⌉ × R` (missing-deposit check) | 0 |
| `eth_getTransactionReceipt` | `3 P` (detection, credit, finality) | `2 P` (credit, finality) |
| `eth_getTransactionByHash` | `P` (the nonce, at detection) | 0 |
| `eth_getBlockByHash` | `≤ B` blocks with a payment, 0 on nodes returning `blockTimestamp` with logs | 0 |
| `eth_call` | `P` (sanctions) `+ ⌈U / 200⌉ × R` (custody) `+` new addresses `/ 200` per round (derivation) | `P` (sanctions) |

Retries (a lagging provider B is re-checked every 2 s; a failed read backs off) add to these;
compare the prediction with the counters before relying on it.

## Attestation, ingress, and egress

**HUMAN-ONLY, verifier**, before onboarding any account. Deploy already verifies the attested
compose; to re-check a CVM with the deployed release's verified kit in `kit/`, the run's rendered
compose, and `PHALA_CLOUD_API_KEY` and `DOMAIN` (the host of `topup.yaml`'s `public_origin`)
exported:

```sh
kit/deploy/phala cvms get "$CVM_ID" --json > cvm.json
kit/deploy/phala cvms attestation "$CVM_ID" --json > attestation.json
APP_ID=$(jq -er '.app_id' cvm.json) && GATEWAY_DOMAIN=$(jq -er '.gateway.base_domain' cvm.json)
curl -fsS "https://${APP_ID#0x}-8090.$GATEWAY_DOMAIN/prpc/Info" > info.json
kit/deploy/verify-attestation.sh attestation.json info.json "$APP_ID" docker-compose.ENV.yml service
export ORIGIN="https://$DOMAIN"   # topup.yaml's public_origin
kit/deploy/verify-ingress-evidence.sh "$DOMAIN" "$APP_ID"
```

[verify-attestation.sh](verify-attestation.sh) runs the official dstack verifier
([dstack-verifier.sh](dstack-verifier.sh), `dstacktee/dstack-verifier:0.5.9` pinned by digest:
TDX quote and TCB, RTMR3 event-log replay, OS image measurements). It requires TCB `UpToDate`, the
app id, and a compose hash whose app-compose holds exactly the rendered compose. The compose hash
is the hash of the full app-compose JSON the Phala CLI builds (with `allowed_envs` and the CVM
options), not of the YAML file, so every upgrade's hash is new and goes to merchants
([docs/integration.md §5.3](../docs/integration.md#53-pin-your-accounts-webhook-keys)). It then
requires `allowed_envs` to name only the compose's sealed names, and the variant's
[compose-policy.jq](compose-policy.jq): for the service, the only published port is
`dstack-ingress` on 443 (`tls-alpn-01`, forwarding to `topup:8080`, for the host of
`public_origin`); for the restore-check variant it is `topup` on 8081, with no ingress.

The app-compose also carries a pre-launch script, which the guest sources before `docker compose
up`. Deploy sends the kit's [phala-cloud-pre-launch.sh](phala-cloud-pre-launch.sh) with
`--pre-launch-script` (Phala Cloud's own v0.0.20, byte for byte, reviewed from an attested
app-compose; its source is not published), and verify-attestation.sh requires exactly that file.

An account's webhook keys come only from the nonce-bound attestation, fetched with a secret key of
that account and mode (merchants run the same check, docs/integration.md §5.3):

```sh
export NONCE="$(openssl rand -hex 32)"
curl -fsS -H "Authorization: Bearer $SECRET_KEY" \
  "$ORIGIN/v1/attestation?nonce=$NONCE" > public-attestation.json
jq '{quote: null, attestation: .tdx_quote}' public-attestation.json |
  deploy/dstack-verifier.sh > public-verification.json
jq -e --arg app "$(jq -r '.app_id | ltrimstr("0x") | ascii_downcase' cvm.json)" \
  --arg compose "$(jq -j '.compose_file' attestation.json | sha256sum | cut -d' ' -f1)" \
  --arg report_data "$(jq -r '.report_data' public-attestation.json)" '
  .details.tcb_status == "UpToDate" and .details.app_info.app_id == $app
  and .details.app_info.compose_hash == $compose
  and .details.report_data == $report_data + ("0" * 64)' public-verification.json
```

Then check that `report_data` binds the nonce, the account, the mode, and the listed webhook keys
with the Python SDK's `topup_sdk.verify_attestation_binding(response, nonce, expected_account=…,
expected_livemode=…)` (architecture §14 defines the construction; `TopupClient.attestation` runs
it on every fetch).

**Ingress**: the attested compose must publish only `dstack-ingress` on 443; confirm `/openapi.json`
at `public_origin` with a valid certificate, its [certificate
evidence](#custom-domain), and that PostgreSQL and topup's port 8080 are unreachable. The admin
API verifies every signed `@target-uri` against `public_origin`, so a correctly signed
admin request answered `401` usually means the URL differs from it. **Egress** (HUMAN-ONLY, cloud network
authority; dstack has no hostname allow-list): restrict outbound traffic to the RPC providers' hosts,
the price sources, the object storage host, the Sentry ingest host, DNS, the Phala/dstack
platform endpoints, and public addresses on ports 443 and 80 for webhooks (merchants register
their own endpoints, so their hosts cannot be listed; [webhook egress](#webhook-egress) filters
the addresses), and record the rules.

### Webhook egress

Merchants register their own webhook URLs (`/v1/webhook_endpoints`), so a URL may name any
address, including the CVM's own network or a cloud metadata service. Every delivery therefore
leaves through the `smokescreen` sidecar (`--webhook-proxy http://smokescreen:4750`), Stripe's
[smokescreen](https://github.com/stripe/smokescreen) and the only IP filter (design §8): it resolves
the host itself and refuses, with `407` before connecting, every address that is not publicly
routable. The service checks only a URL's scheme and port when it is registered (`https` on 443;
in test mode also `http` on 80), follows no redirect, and times out after 20 s; `run` refuses to
start without the proxy unless its own origin is `http` (local stacks).

- **Binary.** Stripe publishes no image, so the phala-pay [Dockerfile](../Dockerfile) builds
  smokescreen v0.1.0 (commit `609eb8931420453daf5893509be0b25b21bd9edb`) with its vendored
  modules in `golang:1.27-trixie` pinned by digest, reproducibly, and the sidecar runs it from
  the phala-pay image: it is pinned and attested with the service's digest.
- **Policy.** Its defaults refuse loopback, private (`10/8`, `172.16/12`, `192.168/16`,
  `fc00::/7`, which holds AWS's IPv6 metadata `fd00:ec2::254`), link-local (`169.254/16` with the
  metadata address `169.254.169.254`, `fe80::/10`), CGNAT (`100.64/10`, with Alibaba's metadata
  `100.100.100.200`), multicast, unspecified, and IPv6 that embeds IPv4 (NAT64 `64:ff9b::/96`,
  6to4, Teredo); an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is checked as the IPv4 address it
  maps. The compose adds `--deny-range` for what those defaults count as global: `0.0.0.0/8`,
  `192.0.0.0/24`, `198.18.0.0/15`, and `240.0.0.0/4`, and repeats `100.64.0.0/10` and
  `169.254.0.0/16`. A `--deny-range` of `::ffff:0:0/96` must never be added: Go matches it against
  every IPv4 address and it would refuse all webhooks. No allow-range, ACL, or
  `--unsafe-allow-private-ranges` is set, and
  [validate-compose.sh](validate-compose.sh) fails if one is.
- **Test.** [tests/smokescreen.sh](tests/smokescreen.sh) runs the sidecar from an image with the
  compose's own command and checks, through plain requests and CONNECT tunnels, that private,
  loopback, metadata, CGNAT, `0/8`, IPv4-mapped, NAT64, and unique-local targets are refused and a
  public address is not; CI runs it on every image build.
- **Local stacks** (the sandbox, the CVM rehearsal) deliver directly (no `--webhook-proxy`, under
  an `http` `--public-origin`), because their receivers listen on private compose addresses.
- A merchant's URL that resolves to a refused address fails like an unreachable one: retried with
  backoff (probed about once an hour), never disabled, and visible to the merchant as the
  endpoint's `pending_deliveries`, `oldest_pending_at`, and `last_attempt`, and as each event's
  `pending_webhooks` (`GET /v1/events?delivery_success=false`); the daily report lists endpoints
  failing longer than `failing_for_hours`.

## Contracts

The forwarder factory is permissionless and deterministic (design D3): the committed build deploys,
through the Arachnid proxy, the factory `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` and its
implementation `0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9` at the same addresses on every chain
(`contracts/local-test-vectors.json`; the build since the per-target gas bounds of #202). A route
names only the factory; `topup run` refuses to start unless the chain holds exactly that build's
code there, so the factory is deployed on a chain before any compose with a route on it.

An operator reuses the factory wherever it is deployed (it is on Sepolia and Base Sepolia;
`verify-deployment.sh` below checks a chain read-only) and deploys it only on a chain where it is
missing.
**HUMAN-ONLY, deployer with a funded throwaway EOA**, once per chain, from a clone of Phala Pay at
the release's tag with its submodules, since deploying builds the contracts
([CONTRACTS.md](CONTRACTS.md) has the checks each script makes; `$SEPOLIA_RPC_A` and
`$SEPOLIA_RPC_B` are two providers):

```sh
read -rsp "Deployer private key: " PRIVATE_KEY && printf '\n' && export PRIVATE_KEY
deploy/contracts/deploy-proxy.sh --rpc-url "$SEPOLIA_RPC_A"          # the Arachnid proxy exists
deploy/contracts/deploy-factory.sh --rpc sepolia/a="$SEPOLIA_RPC_A" --dry-run   # prints both addresses
deploy/contracts/deploy-factory.sh --rpc sepolia/a="$SEPOLIA_RPC_A" --broadcast
unset PRIVATE_KEY
deploy/contracts/verify-deployment.sh --rpc sepolia/a="$SEPOLIA_RPC_A" \
  --rpc sepolia/b="$SEPOLIA_RPC_B" > sepolia-contract-verification.json
jq -e '.passed == true' sepolia-contract-verification.json
```

The dry run must print the two addresses above; if the factory already has that code (anyone may
deploy it), the broadcast sends nothing. Then run **Verify contracts** (Actions), which re-checks the
deployment, the Safe of `contracts/safe-expectations.json` (Phala's), and the committed routes daily,
in the `staging` Environment. Repeat the verification for
[mainnet deployment](CONTRACTS.md#mainnet).

## Operator onboarding

<a id="account-credentials"></a>
Accounts are created only by the operator, after due diligence done offline (design D8); there is
no signup, dashboard, or email. The operator creates the account, decides live access, sets
platform limits, and issues first or recovery keys; it never sets a treasury, registers a
merchant's endpoint, moves funds, records refunds, or resends a merchant's events. Every admin call
is RFC 9421-signed with the admin key and audited, and each change to an account is announced to it
as an event (`account.updated`, `api_key.created`). With the [runbooks' `admin`
helper](runbooks/README.md#environment), in order:

1. **Due diligence, offline** (HUMAN-ONLY, operator): the business, its owners, sanctions screening
   of the entity, owners, and intended treasuries, jurisdiction, and the signed merchant agreement,
   under the operator's own policy (Phala's: [onboarding policy](phala.md#onboarding-policy)).
   The product stores only a reference, the date, and the reviewer.
2. **Create** (HUMAN-ONLY, admin key holder, after [attestation](#attestation-ingress-and-egress)).
   The answer holds the account id `acct_…` and, in `api_keys`, its first secret test key
   (`ppay_sk_test_…`) and, with `charges_enabled`, live key; each `secret` is shown only here:

   ```sh
   (umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name: "<merchant name>",
       contact: {name: "<name>", email: "<security email>"},
       due_diligence: {reference: "<review reference>", reviewed_at: "<YYYY-MM-DD>",
                       reviewed_by: "<reviewer>"},
       charges_enabled: false, reason: "<why>"}')" > ~/onboarding/account.json)
   ```

   The admin seed converts to the PEM the helper signs with once:
   `(umask 077 && { printf '302e020100300506032b657004220420'; tr -d '\n' < admin.seed; } | xxd -r -p | openssl pkey -inform DER -out admin.pem)`.
3. **Key hand-over** (HUMAN-ONLY): send the account id and key to the recorded contact through an
   encrypted channel, then delete `account.json`. The merchant rolls the key at once
   (`POST /v1/api_keys/{id}/roll`), keeps secret keys offline for administration, and runs its
   servers with a restricted key (`POST /v1/api_keys {"type": "restricted", "permissions": […]}`,
   `ppay_rk_…`), which cannot manage keys, treasuries, endpoints, webhook keys, or account settings
   (docs/integration.md §5.4). Everything else the merchant does itself through the API or SDKs:
   [treasury](#treasury-setup), [payment settings](#payment-settings) (a new account accepts
   nothing until it configures them), [webhook endpoint](#webhook-endpoint) and webhook key
   pinning, `quotes` pause, refunds, and [sweeps](#sweeping). The operator sets no merchant term:
   it bounds them in each route's `merchant` section (docs/architecture.md §14), and reads an
   account's settings and effective config with `admin GET "/v1/admin/accounts/$ACCOUNT"`.
4. **Live enablement** (HUMAN-ONLY, admin key holder), when the review allows it: the answer holds
   the first live key, handed over as in step 3. Live payments also need a proven live treasury.

   ```sh
   admin POST "/v1/admin/accounts/$ACCOUNT" '{"charges_enabled": true, "reason": "<review reference>"}'
   ```

5. **Exposure cap**: `max_unfinalized_credit` caps, in cents and per mode, the credit of the
   account's deposits credited before they are final (default 100 000, $1 000; design, ledger
   correctness amendment); a deposit past it is credited at finality instead, and `0` credits every
   deposit at finality. Raise it on the merchant's request once its volume and its ability to claw
   back justify it:

   ```sh
   admin POST "/v1/admin/accounts/$ACCOUNT" '{"max_unfinalized_credit": 500000, "reason": "<why>"}'
   ```

A merchant that lost every key or cannot win a leak by rolling asks for a recovery key, verified
with the recorded contact ([API key compromise and key recovery](runbooks/api-key-compromise.md)).

## Merchant setup

These are the merchant's steps, done with its own secret key against the operator's
`https://$DOMAIN`. An operator that is also a merchant of its own instance does them as the
merchant, never with the admin key (Phala's staff, for Phala Cloud and the staging reference
product).
The [integration guide](../docs/integration.md) is the full reference.

### Treasury setup

Follow [Treasuries](../docs/integration.md#16-treasuries) to prove a treasury per chain and mode,
using an EOA or Safe. Update the merchant's pinned treasury when a change takes effect;
previously issued addresses keep paying the old treasury.

### Payment settings

New accounts accept nothing. Configure the chains, assets, and terms with the merchant's secret
key as described in [Payment settings](../docs/integration.md#19-payment-settings). `GET /v1/config`
then lists the effective assets on chains with an active treasury.

### Webhook endpoint

[Pin the account's webhook keys](../docs/integration.md#53-pin-your-accounts-webhook-keys), then
[register its endpoint](../docs/integration.md#511-webhook-endpoints-and-events). Credit from
verified events using [the balance rule](../docs/integration.md#the-balance-rule-and-event-ordering).

### Sweeping

The merchant sweeps with its own wallet or Safe, paying gas. The SDKs build calls offline from the
forwarder export; see [Balance, sweeps, and the forwarder export](../docs/integration.md#17-balance-sweeps-and-the-forwarder-export).
Failed `FlushFailed` targets remain the merchant's to resolve.

## After a restore: the merchant notice

A restored service starts frozen in restore mode (design §13): every merchant request with an API
key, reads included, answers `503 service_restoring` with `Retry-After`, and nothing credits or
delivers until the operator's reconciliation and unfreeze
([Reconciliation after a restore](runbooks/restore.md)).
**HUMAN-ONLY, operator**: before reconciling, send every account's recorded contact the notice:
the restore point (the backup time) and the time the restore was detected; that API requests,
reads included, answer `503` until further notice, so retry with the same `Idempotency-Key`; and
the records the operator needs from the merchant, received after the restore point: key
revocations (id, or prefix and last four), treasury cancellations and crediting pauses, deleted
endpoints, deposit addresses and quotes given to customers, and the raw deliveries of deposit
events with their webhook headers; a quote or delivery it cannot produce is lost (a payment to the
quote is not found; the deposit is re-valued). After the unfreeze, send a second notice: service
resumed, any re-valued deposit flagged, events after the restore point delivered again, and that
its payment settings are held until it sends its complete configuration again with
`POST /v1/payment_settings` ([Payment settings](#payment-settings)). The runbook
[Incident communication](runbooks/incident-communication.md) has the channels.

## Local verification

- `make up` / `make down`: the attested compose rendered from the local environment
  ([local/environment.sh](local/environment.sh): staging's routes, placeholder providers) plus the
  [local overlay](local/docker-compose.yml) (Garage S3, the dstack simulator, a mock product);
  run manual commands through `deploy/local/compose.sh -p PROJECT`.
- `make cvm-rehearsal`: a staging-shaped artifact against Anvil, Garage, and the simulator, from
  the unsealed boot through sealing, a configuration upgrade (topup recreated, PostgreSQL not),
  operator onboarding of the product's account, its treasury proof and webhook endpoint, and one
  credited deposit and a signed event redelivery without a second credit.
- `make restore-drill`: [RESTORE.md](RESTORE.md#local-and-ci-drills).
- `make sandbox-local`: the integrator sandbox ([sandbox/README.md](sandbox/README.md)).
- `deploy/validate-compose.sh`: every committed environment rendered and checked against
  [compose-policy.jq](compose-policy.jq), as CI enforces.

## Phala's instance

Phala's own deployment (its staging routes, the staging reset, the reference product behind the
demo, the website, and its onboarding policy) is in [Phala's instance](phala.md).

Production preflight passes `--require-sentry` to `topup config check`, forwarding `SENTRY_DSN`
from the preflight environment file; a missing or malformed DSN blocks rollout. Configure the
production GitHub Environment secret `SENTRY_DSN` for this read-only validation; the CVM retains
its separately owner-sealed DSN.

Ingress uses the supported dstack-ingress TCP settings `TIMEOUT_CONNECT=5s`,
`TIMEOUT_CLIENT=30s`, and `TIMEOUT_SERVER=30s`, allowing the API deadline response to complete.

## Rollback compatibility

Migrations must be expand-only and preserve N-1 reads, writes, defaults and constraints. Under the
migration advisory lock, protocol-aware binaries validate every newer applied migration against
`public.topup_migration_compatibility`: the applied checksum must match its owner-written ledger
entry, its compatibility floor must be no greater than the binary's maximum known migration,
and it must have succeeded. Missing metadata, a higher floor or mismatched checksums fail closed.
SQLx continues checking all known migration checksums and dirty state. The application role cannot
write the ledger. A rollback never lowers existing floors or edits migration history.

0.9.0 is the first compatibility-ledger protocol release. It declares **no rollback to 0.8.3;
restore required**, and no rollback to any 0.8.x release: follow [RESTORE.md](RESTORE.md) using the
pre-upgrade backup. Earlier immutable images cannot understand newer migrations; route configuration
and SDK changes also prevent rollback. CI resolves the latest stable release as N-1 automatically.
For any N-1, an exact `no rollback to <N-1 version>; restore required` declaration in
`### Breaking (operators)` under `## [Unreleased]` or the dated section matching the workspace
version (including the release PR before tagging)
selects `declared` mode: CI skips the image smoke and prints the declaration in its job summary.
For protocol-era N-1, `COMPATIBILITY_FLOOR` must also exceed the value in N-1's tag; CI fails
if the floor is unchanged or lower. Without a declaration, pre-protocol N-1 fails and protocol-era
N-1 runs the real image rollback smoke. Breaking migrations raise the floor to the new schema's
maximum; retain a tested pre-upgrade restore/reconciliation plan and obtain owner acceptance.

Before upgrade, preserve the verified N-1 deploy kit, image digests and configuration and a tested
pre-upgrade recovery point. To roll back, verify N-1's release again, render its compose/config,
and use Deploy `mode: upgrade`; require migration completion, service health, attestation and TLS
checks. If N's changelog declares no rollback, stop writes and follow the restore
runbook and reconciliation using the pre-upgrade backup instead. Account for external effects
already executed after that backup; restoring blindly can repeat them. Never bypass checksum or
compatibility errors. Use a forward repair release if there is no safe recovery point.

## Disk pressure

Retain all application data for seven years, including append-only tables and undelivered events.
Fund storage growth; no age-based deletion job is introduced. Seven retained full backups is a
recovery rotation, not a seven-year data purge policy.

The read-only `capacity` probe samples the filesystems containing `pgdata` and `observability`
every 30 seconds. `/metrics` exposes `topup_disk_used_ratio{volume="pgdata|observability"}`,
`topup_unarchived_wal_bytes`, `topup_unarchived_wal_age_seconds`, and
`topup_capacity_sample_timestamp_seconds`. Ratios include reserved space; when volumes share a
filesystem they deliberately report the same pressure. Completed WAL segments with `.ready`
markers count as unarchived; `.done` segments do not.

The backup monitor sends Sentry-compatible alerts: `TopupDiskWarning` at >=75%,
`TopupDiskCritical` at >=90%, and `TopupUnarchivedWalPressure` at >=1 GiB pending WAL or oldest age
>=120 seconds. Missing/invalid samples or age >120 seconds emit `TopupCapacityProbeFailed`.
Configure `SENTRY_DSN` and route these issues to operations; with no DSN they remain local logs.
These alerts are sampled from the running service; external uptime/Crons monitoring is necessary
when the service or host itself is down.

1. At warning, inspect growth rates, filesystem free space, largest relations/indexes and
   observability files. Expand the CVM disk/filesystem before headroom is exhausted; estimate
   time to full using WAL growth as well as table growth. Verify actual expansion and new samples.
2. At critical, escalate immediately. If expansion cannot complete safely, use the established
   maintenance/recording hold and stop API writes and workers in a controlled incident procedure.
   Preserve DB and backup continuity. Inspect PostgreSQL archiver errors, credentials, object-store
   availability and backup lag; restore archiving before resuming writes.
3. Never delete undelivered events, financial/audit records, `pg_wal` files, `.ready` markers or
   unarchived WAL. Never disable archiving to hide pressure, truncate tables or run `VACUUM FULL`
   on a nearly full disk. Only remove independently verified disposable logs/cache under their
   approved retention policy; preserve incident evidence. Expand storage or restore on a larger
   fenced instance if necessary.
4. Verify WAL draining, newest archive, fresh samples, both volumes below 75%, service/worker
   progress and pending deliveries before resolving the incident.

Recovery across independent domains remains a proposal in
[recovery-domains.md](../docs/design/recovery-domains.md), pending the owner's cost decision.

## Deployment time budgets

Operator-side scripts require GNU `timeout`, or `gtimeout` from coreutils on macOS, alongside
Bash 4.4 or newer. The quick-start bootstrap downloads the verifier and its deadline helper from
the same release tag; release checksums and provenance verification still run before kit scripts.

Deploy stages have workflow step deadlines (10 minutes, 20 for preflight/attestation/health,
and 25 for deployment plus CVM convergence), in addition
to the 75-minute job ceiling. Shell stages use absolute elapsed-time deadlines: anonymous pulls
share 600 seconds (each pull <=180 seconds, three attempts); Phala preflight shares 120 seconds
(each call <=30 seconds); deployment submission <=300 seconds; CVM convergence <=900 seconds;
attestation and health convergence <=600 seconds each. Each call is capped by remaining stage
time, with TERM and a two-second KILL grace. Poll sleeps cannot extend the stage. Diagnostics
name the stage, elapsed/remaining seconds and exit status without credentials. Contract and
release verification remain mandatory and are also bounded by workflow step deadlines.
