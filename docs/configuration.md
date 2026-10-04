# Service configuration

This page is the reference for the `topup` binary: its commands, its configuration file, its flags,
and the environment variables it reads. A deployment's configuration file is committed in its
environment directory in the operator's environment repository, for example
`production/topup/topup.yaml` (Phala's staging:
`deploy/environments/phala-network/staging/topup/topup.yaml`), and inlined into the attested
compose. A change is therefore a pull request and a Deploy `upgrade`
([deploy/README.md, "Attested settings"](../deploy/README.md#attested-settings)). The only
environment variables are the database login's and the two kinds of secret the owner seals into
the CVM's encrypted environment: `SENTRY_DSN` and each `TOPUP_RPC_<ID>_KEY`
([deploy/README.md, "Sealing the secrets"](../deploy/README.md#sealing-the-secrets)). The design
is in [design/deploy-config.md](design/deploy-config.md).

## Commands

`topup --help` lists them; each takes `--help`.

| Command | Purpose |
|---|---|
| `topup run --config FILE` | The service: the HTTP API and every worker loop. |
| `topup migrate [--config FILE]` | Applies migrations as the database owner. `--config` also binds existing deposits and quotes during the payment settings cutover. |
| `topup config check [--secrets] FILE`, `topup config show FILE` | Validates a configuration file without any secret; prints it resolved, as JSON with every route default written out and each keyed provider's URL still carrying its `{key}`. `--secrets` also checks each provider's sealed key (`TOPUP_RPC_<ID>_KEY`) against its URL and prints no value. |
| `topup route validate FILE`, `topup route show FILE` | The same for one route file (`examples/`, the sandbox template). `--template` permits the zero factory and implementation placeholders of a deployment template. |
| `topup rpc check --config FILE` | Probes RPC members with their sealed credentials; prints validated member ids. |
| `topup rpc recover --config FILE --chain N --block N --actor NAME --reason TEXT`, `topup rpc resume --config FILE --chain N [--max-windows N]` | Stopped-service owner recovery and bounded replay; see [RPC recovery](../deploy/RPC.md#wrong-watermark-recovery). |
| `topup reconcile --config FILE` | Runs one reconciliation pass and exits. |
| `topup attest --nonce HEX --account acct_… [--live] [--version N]` | Prints the attestation evidence that `GET /v1/attestation` returns to that account. |
| `topup keys` | Derives the backup key and the database credentials from dstack into tmpfs files. |
| `topup heartbeat` | Records the RPO heartbeat every `--interval-s` seconds (60 by default). |
| `topup healthcheck` | Exits zero only when the local API answers `GET /healthz` with `200`. |
| `topup restore-check --config FILE [--report FILE]` | Validates a restored database and runs the post-restore reconciliation ([deploy/RESTORE.md](../deploy/RESTORE.md)). |

## The configuration file

One YAML file, parsed with unknown fields refused, holds every public setting of a deployment:

```yaml
environment: staging                       # the Sentry environment: also gates staging-only price licensing opt-in
public_origin: https://pay-api-staging.phala.com
admin_key:
  id: admin/staging-v1                     # the key id admin requests sign with
  public_key: 23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo=   # standard base64 ed25519
rpc_companies:
  tenderly: { domains: [tenderly.co] }
  publicnode: { domains: [publicnode.com] }
rpc_budgets:
  tenderly-account: { requests_per_second: 10, burst: 10 }
  tenderly-public: { requests_per_second: 10, burst: 5 }
  publicnode-account: { requests_per_second: 10, burst: 10 }
  publicnode-public: { requests_per_second: 10, burst: 5 }
rpc_groups:
  sepolia-a:
    chain_id: 11155111
    policy: { probe: { attempts: 3, deadline: 30000 } }
    members: [{id: provider-a, company: tenderly, url: 'https://sepolia.gateway.tenderly.co',
               account_budget: tenderly-account, key_budget: tenderly-public}]
  sepolia-b:
    chain_id: 11155111
    policy: { probe: { attempts: 3, deadline: 30000 } }
    members: [{id: provider-b, company: publicnode, url: 'https://ethereum-sepolia-rpc.publicnode.com',
               account_budget: publicnode-account, key_budget: publicnode-public}]
routes:                                    # every enabled route version, as route files are written
  - route: phala-cloud-sepolia-pha-usd
    version: 3
    ...
```

- **`public_origin`** is the public scheme and authority clients call (no path). The admin API's
  RFC 9421 signatures are verified against it plus the request path and query, and treasury
  challenges (EIP-4361) name it. Behind an ingress it must be the public URL (in a CVM, the
  [custom domain](../deploy/README.md#custom-domain)), not the internal address. The service never
  trusts `Host` or `X-Forwarded-*` headers.
- **`public_origin`** and **`admin_key.public_key`** may be left out only for `topup run` to take
  them from its environment (`--public-origin-host-env`, `--admin-public-key-env`): the Phala Cloud
  template's, whose deploy form holds them
  ([deploy/README.md](../deploy/README.md#the-phala-cloud-template-variant)). `topup run` refuses
  to start unless each has exactly one source. Every other deployment writes both, attested.
- **`maintenance_keys`** is an optional list of up to eight `{id, public_key}` entries next to
  `admin_key`, empty by default. Each public key is standard-base64 Ed25519; IDs (1–128 ASCII
  letters, digits, `.`, `_`, `/`, `-`) and public keys must be distinct from one another and from
  the admin key, including an admin key supplied at runtime. These keys authorize only
  `POST /v1/admin/instance/pause` and `POST /v1/admin/instance/resume`. Other admin routes return
  audited `403 permission_denied`; `GET /v1/admin/instance/pause` requires the full admin key.
  The same RFC 9421 origin, freshness, digest and single-use replay protections apply. The
  operator's admin key retains full access. For generation, CI configuration and overlapping
  rotation, see [planned upgrades](../deploy/README.md#planned-upgrade-admission-and-downtime).
- **`rpc_groups`** configures independent A/B groups. Each route explicitly names
  `chain.rpc_groups: { a: sepolia-a, b: sepolia-b }`; there is no implicit provider list.
  Members have unique ids, reviewed `company`, `url`, optional `sealed_key`, `account_budget`
  and `key_budget`, plus priority/weight. Companies must be disjoint between A and B.
  Each `{key}` is a whole path segment or query value and cannot change authority. Keys stay
  sealed under the explicit `TOPUP_RPC_*_KEY` name, with at least 8 URL-safe characters.
  Repeated templates with different credentials are allowed; identical URL/credential identities
  and conflicting shared quota scopes are refused. Keyless endpoints still have synthetic key
  budgets. Public policies resolve to bounded failover defaults; weighted round robin is optional.
  `policy.probe: { attempts: 3, deadline: 30000 }` is the default acceptance/readmission policy:
  attempts includes the first send of each RPC; deadline is milliseconds for the complete member
  probe, including all calls, quota admission, backoff and `Retry-After`. Attempts must be 1–16
  and deadline 1–120000, at least `attempt_timeout_ms` and greater than `retry_delay_ms`.
  Only transport/timeouts, classified server errors and throttling retry on the same member,
  with exponential backoff starting at `retry_delay_ms` (100 ms by default). A 429 honors
  `Retry-After`; a pause beyond the deadline fails without another send. Identity/genesis/code
  mismatches, capability failures, stale heads and malformed replies do not retry or admit.
  `recovery_successes` (default 2) counts consecutive complete successful probes, independently
  of the per-RPC attempt count.
  See [the RPC runbook](../deploy/RPC.md) for preflight, recovery and migration; the policy
  fields and validation bounds are defined in
  [`GroupPolicy`](../crates/adapters/src/chain/evm/group/mod.rs).
- **`routes`** are route files, one list item each; their fields and defaults are in
  [architecture §14](architecture.md#14-configuration-and-deployment).

`topup config check` refuses a file that the service would refuse, without reading a secret.
Preflight runs it in the compose's pinned image; to run it by hand, use the release's image:

```sh
docker run --rm -i "$(jq -r '."phala-pay"' images.json)" topup config check /dev/stdin \
  <production/topup/topup.yaml
```

The files it refuses include:

- an invalid origin or admin key;
- a route that fails validation, or routes that disagree on a chain;
- an unknown or unused RPC group, or a group assigned to the wrong chain;
- overlapping company ownership between A and B;
- duplicate member ids or URL/credential identities, or invalid quota references;
- a misplaced `{key}`.

## Flags

`topup run`:

| Flag | Default | Meaning |
|---|---|---|
| `--config FILE` | required | The configuration file. |
| `--bind` | `0.0.0.0:8080` | The API's socket address. |
| `--webhook-proxy URL` | none | The egress proxy every webhook delivery goes through, the smokescreen sidecar in a CVM ([webhook egress](../deploy/README.md#webhook-egress)). Required unless the public origin is `http` (local stacks). |
| `--read-only` | off | Serves only the read API of a database restored from backup (the restore-check variant, [deploy/RESTORE.md](../deploy/RESTORE.md)): no loop, no lease-owner lock, every write refused, and Sentry reports as `<environment>-restore`. |
| `--restore-report FILE` | none | With `--read-only`, the restore-check report `/healthz` serves. |
| `--public-origin URL` | the file's | Replaces `public_origin`: the restore instance's own origin, or a local stack's. The attested service compose never sets it. |
| `--public-origin-host-env NAME` | none | When the file leaves `public_origin` out: the environment variable holding the origin's host, a lowercase DNS name served as `https://HOST` (the template's `DSTACK_APP_DOMAIN`). |
| `--admin-public-key-env NAME` | none | When the file leaves `admin_key.public_key` out: the environment variable holding the admin public key, standard base64 ed25519 (the template's `TOPUP_ADMIN_PUBLIC_KEY`). |
| `--head-poll-interval-s` | one block time: 12 s, or 2 s on an OP-stack chain whose route credits at a depth | Delay between `eth_blockNumber` polls of provider A, for every chain when set. Each new block's transfers to every issued address are read in one request. |
| `--finalized-poll-interval-s` | 60 | Least delay between reads of the `finalized` head. Its advances drive the finalized backstop, the finality watch, and reconciliation. |
| `--reconcile-interval-s` | 600 | Least delay between reconciliation rounds; a round runs only after `finalized` advanced. |
| `--wait-interval-s` | 60 | Delay before retrying an expected wait outcome. |

[Architecture §8](architecture.md#8-chain-valuation-screening) has the cadences, and
[deploy/README.md, "Measuring RPC usage"](../deploy/README.md#measuring-rpc-usage) the call
counters and a cost formula.

## Database roles

Every database command reads `DATABASE_URL`, with its password from libpq's `PGPASSFILE`.

`run`, `heartbeat`, and `reconcile` log in as the application role. That login must be a member of
the migration-created `topup_app` NOLOGIN role. The application role has operational CRUD
privileges but no `TRUNCATE`. Append-only tables (among them `transitions`, `audit`, `events`, and
the finalized chain facts `flushed` and `flush_failures`) permit only `SELECT` and `INSERT`. The
[migrations README](../crates/topup/migrations/README.md#roles-and-privileges) lists every grant.

`migrate`, `restore-check`, `rpc recover`, and `rpc resume` require the trusted database owner;
`migrate` also needs permission to create roles and schema objects. These commands refuse the
application role. Recovery and restore checks require stopped writers.

## Environment

| Variable | Read by | Meaning |
|---|---|---|
| `DATABASE_URL` | database commands | The login above; its password is in `PGPASSFILE`. |
| `TOPUP_RPC_*_KEY` | `run`, `reconcile`, `restore-check`, `config check --secrets`, `rpc` | Each member's explicit `sealed_key`, which fills its URL's `{key}`. |
| `TOPUP_RPC_PROBE_DEBUG` | RPC commands and service startup | Set to `1` for sanitized probe diagnostics ([RPC runbook](../deploy/RPC.md#configuration-and-acceptance)). |
| `DSTACK_APP_DOMAIN`, `TOPUP_ADMIN_PUBLIC_KEY` | `run`, only when named by the flags above | The Phala Cloud template's origin host and admin key. |
| `SENTRY_DSN` | `run` | Sentry reporting, off while unset or empty ([deploy/README.md, "Sentry"](../deploy/README.md#sentry)). The environment is the file's `environment`; the release is the source commit compiled into the image. |

The service also needs the dstack guest API socket (`/var/run/dstack.sock`) for its keys and
attestation; local stacks use the dstack simulator (`DSTACK_SIMULATOR_ENDPOINT`).

## Restore mode

A database restored from backup starts in **restore mode**
([architecture §14](architecture.md#14-configuration-and-deployment)). Every request authenticated with a merchant API key, reads and writes alike,
answers `503 service_restoring`, and nothing is credited or delivered until the operator has
reconciled the restore with each merchant's records through `/v1/admin/restore/…` and unfrozen
it. `topup restore-check` requires stopped writers and the database owner: it validates the restore,
runs reconciliation, and may repair the ledger. The
[restore guide](../deploy/RESTORE.md) and the
[reconciliation runbook](../deploy/runbooks/restore.md) have the steps.

## Price sources

Routes use `price:` with explicit `mode: volatile` or `mode: stablecoin`. Stablecoins require
`sources` and forbid role lists. A source set must cover USDC and USDT or explicitly configure
an Ethereum-mainnet fallback for the route asset. Runtime uses only that asset's observations;
another stablecoin's price cannot authorize its credit. Volatile assets require ordered `primary`, `check`, and `fx`
lists, with disjoint primary/check company identities. PHA uses `uniswap_v2_twap` (company `uniswap-v2-onchain`) primary and Kraken `PHAUSD` check.
Missing either company pauses PHA; Kraken still requires written permission in production.

Source descriptors are `source: kraken`/`binance`, `symbol`, and the canonical `company`, or
`source: chainlink`, `feed`, `chain_id`, `rpc_group`, and an independent `rpc_group_b` for explicit
cross-network groups. Role ids `a`/`b` resolve the route's groups. Testnet mainnet observations
also require `observation_chain_id` equal to the test route chain. Supported pinned feeds are
USDC_USD and USDT_USD on Ethereum (1) and Base (8453), plus Ethereum ETH_USD for the TWAP
composite source. See the
[feed evidence](design/price-feed-registry.json) and [accepted design](design/price-failover.md).

Base and Base Sepolia require `sequencer_uptime: { feed: BASE_SEQUENCER_UPTIME, grace_s: 3600,
rpc_group: base-mainnet-a, rpc_group_b: base-mainnet-b }`. Observation-only groups use existing
shared RPC budgets, counted transports, independent company validation, and recovery probes;
they do not scan payment contracts. Their capability probes read pinned feed decimals instead of
payment receipts or logs. Staging includes `mainnet-a`/`mainnet-b` (chain 1) for price
feeds and `base-mainnet-a`/`base-mainnet-b` (8453) for uptime. Feed addresses and heartbeat values
are pinned in the image; operators configure group references, not arbitrary feed addresses.
Chainlink freshness allows heartbeat + **600 s** for publication delay: the last eight Ethereum
round intervals were already up to 36 s late in calm conditions (see the feed evidence and design
above). Deviation-triggered updates and all agreement/peg checks remain in effect.
Ethereum B uses `https://eth.drpc.org` (company `drpc`): PublicNode Ethereum prunes genesis
history and cannot pass group identity acceptance. Base B remains PublicNode. Both groups require
numeric-block feed state and recent historical calls across the configured TWAP window.

Chainlink public on-chain consumption is Allowed; stablecoin defaults are Chainlink-only and
production-eligible without `allow_unclear_sources`. Kraken public market data is
PermissionRequired; Binance, Coinbase and Coin Metrics are Prohibited for commercial use under
the reviewed terms. Production refuses every non-Allowed source. Explicit non-production
`staging`, `testnet`, `local`, `sandbox` plus route `allow_unclear_sources: true` permits
noncommercial rehearsal of the TWAP/Kraken PHA route; this flag grants no permission.
Coin Metrics remains disabled in every environment. Deploy's production target independently
refuses a staging opt-in. `DEPLOY_ENVIRONMENT=staging` is required for a testnet rehearsal.

PHA production is unavailable until two independent sources are implemented and Allowed. The
follow-up to #331 values PHA at min(Uniswap V2 PHA/WETH TWAP, current spot) × Chainlink ETH/USD.
Agreement compares current Uniswap spot × ETH/USD against current Kraken PHA/USD after written
permission; CoinGecko is dropped. See the
[on-chain plan](design/price-failover.md#pha-on-chain-follow-up). `topup config check` prints ordered
sources, verdicts, pinned feed metadata and testnet markers; `config show` emits resolved `price`.

For PHA, configure `source: uniswap_v2_twap`, `rpc_group`, `rpc_group_b` (independent Ethereum
mainnet groups) and `observation_chain_id` for cross-network routes. The fixed pair and tokens
are pinned in the image. Optional `twap` settings default to `window_s: 1800`,
`max_sample_age_s: 180`, `min_weth_reserve_usd: 100000`, `max_spot_deviation_bps: 300`,
and `max_sample_jump_bps: 500`. Windows below thirty minutes are rejected. The service samples
once/minute into PostgreSQL even without quote traffic; startup and gaps require a continuous
window before prices become available. `config show` includes the resolved guard rails.

For one migration window the parser accepts old `pricing.primary` and `pricing.check` shapes,
maps each to a one-item role list and the FX leg to `fx`, and emits only `price`. Mixing old and
new sections is rejected. Coin Metrics migrates as restricted evidence and fails validation:
replace it explicitly with an eligible source set: Chainlink for stablecoins, or the documented
PHA TWAP/Kraken plan after written Kraken permission. Restricted exchange adapters remain staging-only.
Add an independent check and FX for volatile assets; do not reduce the source count. Existing
route versions and RPC bindings are retained. Release this breaking schema in the next minor
version; rollback requires the previous compatible image and its configuration.
