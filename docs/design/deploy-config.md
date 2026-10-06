# Design: a lean, standard deployment configuration

Status: implemented in v0.3.0; RPC configuration superseded by [RPC groups](../../deploy/RPC.md).

Scope: the attested compose, its variants, every setting of `topup` and of the
deployment, and the scripts, workflows, and docs around them. Owner's rules:
"精简优雅重构compose和各种配置项"; use a standard mechanism wherever one exists; don't abuse env.

Claims about third-party behaviour were checked on 2026-09-30 against source or by experiment.
The **dstack-0.5.9 guest runs Docker Compose v2.26.0**: `meta-dstack` v0.5.9 pins
`meta-virtualization` 52cd8a2, whose `docker-compose_git.bb` has `PV = "v2.26.0"`. The guest runs
`docker compose up --remove-orphans -d --build` in `/dstack`, with the sealed env as its process
environment (dstack v0.5.9 `basefiles/app-compose.{sh,service}`:
`EnvironmentFile=-/dstack/.host-shared/.decrypted-env`); `ExecStop` stops the whole stack.

## 1. Decisions

1. **One typed config file for topup** per environment: `topup.yaml`, holding the origin, the
   admin key, the RPC provider URLs, and the routes. It is committed, reviewed by PR, and inlined
   into the attested compose as a Compose `config`. topup reads it with `--config`; no setting is
   an environment variable. `topup config check|show` validate and print it with
   no secret present: a `{key}` stays literal. The service and a preflight that holds the secrets
   resolve the keys through the same code path.
2. **Env only where env is the interface.** That covers the sealed secrets (still `${NAME}`
   references), libpq's `DATABASE_URL`/`PGPASSFILE`, and the env interfaces of third-party images:
   WAL-G's `WALG_*`/`AWS_*` and dstack-ingress's `DOMAIN`. These are written as literals in the
   environment's compose overlay, never taken from the environment at render time.
3. **Variants are standard Compose overrides.**
   - `compose.yaml` holds every service of the topup CVM.
   - Each variant is an override that removes what it does not run with `!reset null`:
     `compose.service.yaml` removes `restore-check`, and `compose.restore-check.yaml` removes
     `dstack-ingress`, `smokescreen`, `heartbeat`, and `backup`.
   - (An inert `profiles:` entry would not do: Compose v2.26 keeps inactive-profile services in
     `config` output, still carrying `profiles:`.)
4. **Public settings live in reviewed files, not GitHub.** The GitHub Environment holds only 3
   deployment-state variables.
5. **Rendering uses Compose itself**, pinned by sha256 to v2.26.0, the version the CVM runs:
   `config --no-interpolate`, plus three bounded deploy-time inputs. Those inputs are image
   digests, the gateway domain (service only), and the restore instance's origin (restore-check
   only). Each has a restricted format and there is no general `--set`. The output is Compose's
   canonical YAML.
6. **One artifact policy** (`deploy/compose-policy.jq`) judges the *merged* artifact wherever it
   is checked: `validate-compose.sh`, `preflight.sh`, and `verify-attestation.sh`. It rules on
   the service set, the ports, credential mounts, the egress, restore isolation, and where each
   secret reference may appear.
7. **Services:** `heartbeat`, `keys`, and `migrate` are services of their own (§5). Config names
   carry their content digest. `restore` is in the local drill overlay only.
8. **Fork-free deployment from versioned releases** (§8).
   - `render.sh` and preflight accept any environment directory; a generic example environment
     ships, and the environment overlay declares its keyed providers' secrets.
   - A `v<version>` tag publishes the images, the deploy kit, and the Phala Cloud template's
     compose (`release.yml`); Deploy is a reusable workflow run at a release against the caller's
     committed environment directory.
9. **A `{key}` may only fill a whole path segment or a whole query value**, and substitution must
   leave the URL's authority unchanged (§4).
10. **Inline config consumers require a writable root filesystem.** Compose v2.26.0's
    [injectConfigs](https://github.com/docker/compose/blob/v2.26.0/pkg/compose/secrets.go)
    copies a tar archive to `/` through `CopyToContainer` before starting the container. Docker's
    [archive extraction](https://github.com/moby/moby/blob/v26.0.0/daemon/archive_unix.go)
    rejects that extraction point when the root filesystem is read-only, even with tmpfs at the
    config target's parent. topup therefore uses `read_only: false` in every variant; uid/gid 999,
    root-owned `0444` configs in root-owned directories, dropped capabilities, no-new-privileges, finite
    resource limits and read-only credential mounts remain. The root filesystem can now be
    written wherever Unix permissions allow uid 999; `/tmp` remains tmpfs. smokescreen and
    heartbeat consume no configs and retain read-only root filesystems. The reference product
    already uses a writable root filesystem and runs as uid/gid 10001 in its image.
    The rehearsal inherits these same policies. `deploy/tests/compose-startup.sh` exercises
    injection, byte equality, topup config permissions and recreation on real rendered service,
    restore-check, template and product definitions, substituting bounded probes for application
    processes and removing external connections. Application health remains the rehearsal's job.

## 2. Principle: four kinds of input, each with one home

| Kind | Home | Attested | Examples |
|---|---|---|---|
| Topology: which services exist, what they mount, who talks to whom | `deploy/compose.yaml`, `deploy/compose.restore-check.yaml` | yes | uid/gid, the tmpfs keys, the dstack socket mounts, the smokescreen policy, `--webhook-proxy`, ports |
| Environment settings: the public values of one deployment | an environment directory: `compose.yaml` overlay and `topup.yaml` | yes | origin, admin key, RPC URLs, routes, WAL-G location, ingress domain, keyed providers' secret names |
| Deploy-time facts: known only from a release or a CVM | `render.sh` flags, format-checked and variant-scoped | yes | image digests, gateway domain, a restore instance's origin |
| Secrets | the CVM's sealed env | names only | S3 credentials, `SENTRY_DSN`, `TOPUP_RPC_<ID>_KEY` |

The sealed names are the `${…}` references of the rendered artifact: the base compose declares the
common ones, and the environment overlay declares its keyed providers' keys. There is no separate
list to keep in sync. `allowed_envs`, the unsealed env file Deploy sends, and preflight all read the
names from the artifact.

Deploy resolves the environment directory as `deploy/environments/${GITHUB_REPOSITORY_OWNER,,}/<env>`.
A fork therefore finds no directory and fails closed, instead of deploying Phala's admin public key
and domain. `render.sh` and preflight take any directory. `deploy/environments/example/` has no
Phala value, and preflight refuses its placeholders.

## 3. Surface

### Files

```text
deploy/
  compose.yaml                   every service of a topup CVM (topology; pointers into docs)
  compose.service.yaml           variant: !reset null restore-check
  compose.restore-check.yaml     variant: !reset null dstack-ingress, smokescreen, heartbeat, backup;
                                 read-only topup on 8081; archiving off; read-only storage
                                 credentials under their own names
  compose-policy.jq              the policy of the merged artifact (§1.6)
  render.sh                      pinned compose config + the three deploy-time inputs
  pinned-compose.sh              Docker Compose v2.26.0 by sha256, cached; --no-download for offline use
  product/compose.yaml           reference-product stack
  environments/
    example/topup/               a generic environment: compose.yaml + topup.yaml, no Phala values
    phala-network/staging/topup/ compose.yaml (ingress DOMAIN, WAL-G location, RPC key names) + topup.yaml
    phala-network/staging/product/ compose.yaml (ingress DOMAIN) + config.json (every value literal)
  local/environment.sh           writes the local stacks' environment: staging's routes under a local
                                 header (placeholder providers, a local admin key)
```

`check-route-modes.sh` is bound to the environment Deploy selected (§7).

### `topup.yaml`

```yaml
environment: staging                      # the Sentry environment (a tag only; `-restore` when --read-only)
public_origin: https://pay-api-staging.phala.com
admin_key:
  id: admin/staging-v1
  public_key: 23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo=
rpc_providers:                            # id → URL; `{key}` takes the sealed TOPUP_RPC_<ID>_KEY
  provider-a: https://sepolia.gateway.tenderly.co
  provider-b: https://ethereum-sepolia-rpc.publicnode.com
  base-sepolia-a: https://base-sepolia.gateway.tenderly.co
  base-sepolia-b: https://base-sepolia-rpc.publicnode.com
routes:                                   # each item is one route
  - route: phala-cloud-sepolia-pha-usd
    version: 3
    ...
```

`Config::parse` is the one validation path. It uses serde with `deny_unknown_fields`, as routes
do, and every rule below is a Rust test:

- the origin parses (`PublicOrigin`), and the admin key parses (`VerificationKey`);
- provider ids are lowercase letters, digits, and `-`;
- every provider URL is `https`, or `http` only when the origin is `http` (local stacks);
- a `{key}` fills a whole path segment or a whole query value, at most once;
- every route validates and the route set agrees across routes;
- every provider a route names is defined, and none is unused;
- a provider serves one chain, and one chain's providers are different URLs.

`topup config check FILE` and `topup config show FILE` (resolved JSON) run that path with no secret.
`topup config check --secrets FILE` also resolves each provider's key from `TOPUP_RPC_<ID>_KEY`,
with the same function `topup run` uses, and prints no value. `topup route validate|show` stay for
single route files (`examples/`, the sandbox template).

### Commands and flags

| Command | Config and flags |
|---|---|
| `topup run` | `--config FILE [--bind] [--webhook-proxy URL] [--read-only [--public-origin URL] [--restore-report FILE]]`; `--public-origin` and `--restore-report` require `--read-only` |
| `topup restore-check` | `--config FILE [--report FILE] [--failure-at … [--expected-lsn …]]` |
| `topup reconcile` | `--config FILE` |
| `topup config check`, `config show` | `FILE [--secrets]` (`check` only) |
| `topup migrate`, `restore-check` | `DATABASE_URL`, refused unless the login owns the database (or is a superuser) |
| `topup heartbeat`, `keys`, `healthcheck`, `attest`, `route …` | no configuration file |

### Rendering

```sh
deploy/render.sh --images images.json --gateway-domain gateway.dstack-pha-prod5.phala.network \
  deploy/environments/phala-network/staging/topup >docker-compose.staging.yml
deploy/render.sh --restore-check --images images.json --origin https://<app_id>-8081.<gateway> \
  deploy/environments/phala-network/staging/topup >restore-check.yml
deploy/render.sh --images images.json --gateway-domain … deploy/environments/phala-network/staging/product
```

Rendering runs in three steps:

1. The pinned Compose merges the files: `-p dstack --project-directory deploy -f STACK -f ENV/compose.yaml
   -f VARIANT config --no-interpolate --format json`. Here `STACK` is `compose.yaml`, or
   `product/compose.yaml` (with no variant) when the directory has a `config.json`. This keeps the
   secret references and `$$` escapes. Merged environments come out as `KEY=VALUE` lists, and
   render.sh turns them back into maps.
2. `jq` applies the inputs:
   - pins each `phala-pay`, `postgres-walg`, and `phala-pay-reference-product` image to the
     release's `repository@sha256`, and fails on any unpinned image;
   - sets dstack-ingress's `GATEWAY_DOMAIN`, or appends `--public-origin` to the read-only topup;
   - inlines each `file:` config as `content`, with `$` escaped, named `<name>_<sha256[:12]>`, and
     renames every service's `configs[].source` to match.
3. `config --no-interpolate` produces canonical YAML, and `compose-policy.jq` checks the result
   before it is printed.

Formats: `--images` is a JSON object of image name to `repository@sha256:<64 hex>`. `--gateway-domain`
is a lowercase host name. `--origin` is an `https://` origin, restore-check only.
`--project-name` is for local rehearsals only; the CVM's project is `dstack`.

Verified on v2.26.0 and v5.5.1:

- `--no-interpolate` keeps `${S:-}` and `$$`;
- `!reset null` removes a service;
- an inactive-profile service stays in the output, with its `profiles:`;
- both versions produce byte-identical output, which v2.26.0 loads.

The output bakes the project name into resource names (`name: dstack`, `dstack_pgdata`), and these
are exactly the names the CVM derives from `/dstack`.

## 4. Every setting

| Setting | Home |
|---|---|
| The `phala-pay`, `postgres-walg`, and `phala-pay-reference-product` images | `image: phala-pay` etc.; `render.sh --images` pins them |
| The Sentry release | the source commit, compiled in (`SOURCE_COMMIT` build arg beside `SOURCE_DATE_EPOCH`) |
| dstack-ingress image | a pinned digest in the compose |
| The API's domain | overlay `dstack-ingress.environment.DOMAIN` and `topup.yaml` `public_origin`; the policy checks they agree |
| The public origin | `topup.yaml` `public_origin`; restore-check: `render.sh --origin` → `--public-origin` |
| The gateway domain | read from the CVM; `render.sh --gateway-domain` |
| The admin key | `topup.yaml` `admin_key.id` and `admin_key.public_key` |
| RPC provider URLs | `topup.yaml` `rpc_providers` |
| `TOPUP_RPC_<ID>_KEY` | sealed; declared once in the environment overlay |
| `{key}` placement | a whole path segment or query value only; the authority must survive substitution |
| Routes | `topup.yaml` `routes`; one config, mounted in 2 services |
| `WALG_S3_PREFIX`, `AWS_ENDPOINT`, `AWS_REGION`, `AWS_S3_FORCE_PATH_STYLE` | overlay literals (WAL-G's env interface) |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | sealed; the restore-check variant reads `RESTORE_AWS_ACCESS_KEY_ID`/`RESTORE_AWS_SECRET_ACCESS_KEY`, so an instance created without `--env-file` inherits no read-write credential |
| `SENTRY_DSN` | sealed |
| The Sentry environment | `topup.yaml` `environment` (a Sentry tag, never a policy input) |
| Read-only mode | `topup run --read-only` in the restore-check variant; `heartbeat` does not exist there |
| `TOPUP_RESTORE_FROM_BACKUP` | `postgres` only, `"on"` in the restore-check variant (the entrypoint defaults to `off` and turns archiving off); `walg-cron` keeps its guard |
| The webhook proxy | `--webhook-proxy`, beside the smokescreen service it names |
| The restore report | `--restore-report` (run) and `--report` (restore-check) |
| The backup-age marker | the fixed path `/run/topup-observability/last-backup-unix-seconds`, written by the postgres-walg scripts and read by `topup` |
| `DATABASE_URL` | one name; `migrate` and `restore-check` check in code that the login owns the database |
| `PGPASSFILE` | env (libpq) |
| `allowed_envs` | some of the artifact's `${…}` names, and no other (`compose-policy.jq`, `allowed_envs_violations`) |
| `PHALA_WORKSPACE`, `TOPUP_CVM_ID`, `STAGING_PRODUCT_CVM_ID`, secret `PHALA_CLOUD_API_KEY` | GitHub: deployment state and tool credentials, not attested |
| The product's domain, driver key, service origin, and public URL | the product overlay's `DOMAIN` and literal values in `config.json` |

So:

- The `${…}` names in the service artifact are the secrets only.
- The GitHub Environment holds 3 variables.
- The fixed environment variables `topup` reads are `DATABASE_URL` and `SENTRY_DSN`, plus each
  `TOPUP_RPC_<ID>_KEY`. libpq reads `PGPASSFILE`.
- Adding a chain touches the environment directory only.

**Where a secret may appear.** `compose-policy.jq` allows a `${NAME:-}` reference only as the whole
value of an environment key it names:

| Secret | Allowed positions |
|---|---|
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | `postgres`/`backup` env of the same key (service variant) |
| `RESTORE_AWS_*` | `postgres` `AWS_*` (restore-check variant) |
| `SENTRY_DSN` | `topup` `SENTRY_DSN` |
| `TOPUP_RPC_<ID>_KEY` | `topup`/`restore-check` env of the same key |
| `PRODUCT_API_KEY` | `product` `PRODUCT_API_KEY` |

Once every `$$` is removed, no other `$` may remain anywhere: not in a config's content, a command,
or another key. A sealed value therefore cannot fill the admin key, the origin, or an RPC host.

## 5. Services

| Service | Decision | Evidence |
|---|---|---|
| `keys` | a service | See the paragraph below the table. |
| `migrate` | a service | A one-shot init job gated by `service_completed_successfully` is the Compose idiom. Folding it into topup would hand topup the owner login. |
| `heartbeat` | service variant only | Its row is the RPO anchor (`restored_heartbeat_at` within 120 s of the failure point) and the steady commit that keeps WAL flowing. `topup run` checks the contracts over RPC before it touches the database, waits for the lease-owner lock, and exits on any task failure. Folded into it, an RPC outage or a crash loop would stop the heartbeat and produce false RPO findings. |
| `smokescreen`, `backup` | service variant only | They would idle in restore-check. |
| `restore` | `local/restore-drill.compose.yml` only | Only `restore-drill.sh` runs it; it is never started on a CVM. |
| Config names | carry their content digest | See the paragraph below the table. |

**`keys`.** The Postgres and WAL-G images cannot call dstack, so something must derive their
passwords and backup key into tmpfs. The separation this buys is by mount and by database role:
`postgres`, `backup`, and `migrate` see only the credential files they mount, and topup logs in as
`topup_service`, never as the owner. It is not KMS isolation. `keys`, `topup`, `restore-check`,
and dstack-ingress mount the dstack socket, and any socket holder can derive any path, so a
compromised `topup` could derive the owner password.

**Digest-named configs.** Compose v2.26.0 hashes only the service definition
(`pkg/compose/hash.go`) and copies config content only at container creation
(`createMobyContainer` → `injectConfigs`). A content-only change would therefore keep the old file,
and some trigger is still needed. Naming each config after its digest is Compose's documented
config rotation practice (kustomize hashes ConfigMap names the same way). The services'
`configs[].source` references change with the name, so exactly the services that mount a changed
config have a new definition. v2.26.0 also force-recreates every service that `depends_on` a
recreated one (`convergence.go:538`, `setDependentLifecycle`). The goal is therefore scoped: a
route change recreates `topup` and its dependent dstack-ingress, but not the Postgres container.
On a CVM an upgrade restarts the whole stack anyway, since `app-compose.service` stops it, so
recreation is about container identity, not uptime. `make cvm-rehearsal` checks the lifecycle.

## 6. Alternatives rejected, with evidence

- **`profiles` as the variant switch.** The switch is runtime state (`--profile`/`COMPOSE_PROFILES`)
  outside the attested file, and dstack passes neither. Compose v2.26 also keeps an inactive
  profile's service in `config` output, so the artifact would still carry it.
- **`include`.** It imports whole projects and cannot modify an existing service.
- **Compose `secrets:` with `environment:` sources.** v2.26.0 writes a secret only when it creates
  the container. After `phala envs update` (a restart, with an unchanged hash) every container would
  keep the old secret. Tested.
- **`env_file:` with dstack's decrypted env.** It would hand every sealed secret to every service
  that lists it.
- **Interpolating public settings at render time.** `config` would resolve the secret references
  too, and `$${…}` escapes survive as literals. Verified.
- **Two complete compose files.** They duplicate ~200 lines that must then be proved equal.
- **Routes as separate files next to `topup.yaml`.** That keeps 4 configs and mounts, and makes the
  renderer discover files.

## 7. Scripts and workflows

- **Every config consumer reads `--config`:** `topup run`, `reconcile`, and `restore-check` in
  the composes, the local stacks, the sandbox, the drill, and the rehearsal. `cargo test` runs
  `Config::parse` on every committed `topup.yaml` (example and staging).
- **`compose-policy.jq`** (§1.6) is included by `validate-compose.sh`, preflight, and
  `verify-attestation.sh`.
  - Service variant: exactly `keys postgres migrate topup smokescreen dstack-ingress heartbeat
    backup`; only dstack-ingress publishes, on 443 (tls-alpn-01 → `topup:8080`, `DOMAIN` equal to
    `public_origin`'s host); each credential volume on tmpfs with exactly its mounters, read-only
    but for `keys`; smokescreen's exact command and deny list.
  - Restore-check variant: exactly `keys postgres migrate topup restore-check`; only topup
    publishes, on 8081 → 8080; topup `--read-only`; postgres with `TOPUP_RESTORE_FROM_BACKUP=on`
    and no command or entrypoint override (so `archive_mode=off` wins); storage credentials only
    `RESTORE_AWS_*`.
  - Both variants: no password in env; the secret positions of §4; volumes named `<project>_*`.
- **`validate-compose.sh`** renders the example environment, the staging environment, and the
  product in both variants, and applies the policy. It also checks the local, drill, sandbox, and
  rehearsal overlays.
- **`preflight.sh`** takes `--env FILE`, `--compose FILE`, and `--environment-dir DIR`.
  - `--offline` makes no network access and pulls nothing. It checks:
    - the env file names against the artifact's names;
    - that images are pinned;
    - a byte-equal fresh render with the pinned Compose (`--no-download`);
    - the policy;
    - that no example placeholder remains;
    - `topup config check [--secrets]` in the pinned image, which must already be present
      (`--pull never`); otherwise it fails and names the `docker pull`.
  - Online, it adds the anonymous pulls, RPC and asset checks from `topup config show` JSON,
    and the Phala Cloud checks.
  - `--unsealed` skips only `--secrets`.
- **`check-route-modes.sh`** reads the routes as topup reads them: `topup config show`
  on the artifact's inline `topup.yaml`, run from the pinned image preflight pulled, so a route in
  any YAML form (block, flow, JSON) is checked. The environment is the one Deploy selected, its
  first argument; it never reads `topup.yaml`'s `environment`, so a staging config that claims
  `production` does not bypass it. It needs no network.
- **`deploy.yml`**, a reusable workflow called at a release, verifies the release
  (`verify-release.sh`), renders the caller's environment directory with the release's kit and
  images, writes the unsealed env from the artifact's names, and verifies the result;
  **`deploy-phala.yml`** calls it for Phala's own environments.
- **`release.yml`**, on a `v<version>` tag, builds the images with `SOURCE_COMMIT` (the
  reproducible ones twice), and publishes `images.json` keyed by image name, the deploy kit
  (`build-kit.sh`), and the Phala Cloud template's compose, each attested.
- **`verify-contracts.yml`** takes Sepolia's two provider URLs from the committed staging
  `topup.yaml` (`topup config show`). It fails if one needs a `{key}`, since the workflow holds no
  key. It needs no GitHub Environment.
- **Local stacks** (`make up`, the sandbox, the drill) render a local environment with
  `render.sh --project-name <their project>`. That environment is `local/environment.sh`'s, or
  the sandbox's or drill's own. The configs are therefore inline, and nothing is bind-mounted on
  CI's runner. They then add the local overlay and its per-variant part (`local/service.yml` or
  `local/restore-check.yml`). The CVM rehearsal renders the same way.
- **Docs** hold the design prose; the compose keeps one-line pointers.

## 8. Self-hosting without a fork

- `render.sh` and preflight accept any environment directory, and the example environment has no
  Phala value; keyed providers' secret names are in the environment's overlay.
- A `v<version>` tag runs the Release workflow, which publishes the reproducible images, each
  attested, and a GitHub release with `images.json`, the attested deploy kit
  (`deploy/build-kit.sh`), and the Phala Cloud template's compose (`render.sh --template`,
  [deploy/README.md](../../deploy/README.md#the-phala-cloud-template-variant)).
- Deploy is a reusable `workflow_call` workflow that runs the kit against the caller's environment
  directory: operators keep only their environment in their own repository
  ([self-hosting guide](../self-hosting.md#2-your-environment-repository)).

## 9. Verification

- `validate-compose.sh`, `preflight.sh --offline` against the rendered staging artifact, the CI
  `rust`, `deployment`, and image jobs (`cargo test`, `make lint`, the shell tests),
  `make restore-drill` (both modes), and `make cvm-rehearsal`, which runs a staging-shaped
  artifact from the unsealed boot through a configuration upgrade that recreates topup only.
- TLS evidence needs a real CVM and domain; Deploy verifies it on every topup upgrade.

## 10. Risks

- **The artifact is canonical YAML** (no comments, explicit resource names). Merchants pin only
  the compose hash (`docs/integration.md` §5.3).
- **A pinned tool.** `render.sh` refuses any Compose but v2.26.0 by sha256.
- **Volume identity.** Rendering under any project name but `dstack` would move the CVM onto new
  empty volumes. `render.sh` fixes `-p dstack`, and the policy asserts the volume names.
- **Restore credentials.** A restore env file without the `RESTORE_AWS_*` names fails closed:
  PostgreSQL cannot list the prefix. `RESTORE.md` and its preflight name them.

## 11. Guarantees

- Attestation of every public setting that affects money or trust. Each one is in `topup.yaml`,
  an overlay, or a pinned image, all inside the one attested file.
- Sealed secrets and `allowed_envs`. The admin seed never enters the CVM.
- `keys`, the three tmpfs volumes and their mount policy, and the pinned dstack-ingress
  (tls-alpn-01, 443 only).
- The smokescreen policy; WAL-G, its key path, and its guards; `postgres-init`.
- Deploy's two modes, its read-back and verification; preflight online and offline.
- Every guarantee of `RESTORE.md`, checked on the merged artifact. The drill asserts that the
  object listing is unchanged, live isolation holds, merchant requests are refused,
  background work stops, and the freeze persists after the upgrade back to the service.
- YAML; `<owner>/<env>`; the three bounded deploy-time inputs; digest-suffixed configs; `!reset
  null` overrides.
