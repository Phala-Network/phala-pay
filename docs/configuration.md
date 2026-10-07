# Service configuration

This page is the reference for the `topup` binary: its commands, its configuration file, its flags,
and the environment variables it reads. A deployment's configuration file is committed in its
environment directory in the operator's environment repository, for example
`production/topup/topup.yaml` (Phala's staging:
`deploy/environments/phala-network/staging/topup/topup.yaml`), and inlined into the attested
compose. A change is therefore a pull request and a Deploy `upgrade`
([deploy/README.md, "Attested settings"](../deploy/README.md#attested-settings)). The only
environment variables are the database login's and the two kinds of secret the owner seals into
the CVM's encrypted environment: `SENTRY_DSN` and each `TOPUP_RPC_ANKR_KEY` and `TOPUP_RPC_INFURA_KEY`
([deploy/README.md, "Sealing the secrets"](../deploy/README.md#sealing-the-secrets)). The design
is in [design/deploy-config.md](design/deploy-config.md).

## API admission limits

The service sees only the gateway's WireGuard address through dstack-ingress,
which forwards TCP traffic. There is no per-client-IP limiting. Protection comes from the
authenticated account-and-mode limits (100 requests/s live, 25 test, and 500 test requests/s
across accounts), the `client_secret` object limits (120 reads/minute), the pre-authentication
database gate, the global limit of 256 concurrent API requests, and the Phala gateway's per-app
connection cap (configured by Phala; value not confirmed).

Well-formed, checksum-valid Bearer keys wait at most 250 ms for an authentication slot. Slots
are half the database pool, with a minimum of one, and cover the restore freeze check and API-key
lookup only. A full gate returns `503 unavailable` with `Retry-After: 1`; malformed or
checksum-invalid keys return `401` before taking a slot. Anonymous `client_secret` reads use
their own slots; `/healthz` and admin authentication do not use this gate. The service admission
limits are built in, with no new configuration setting.

Carrying real client IPs via PROXY protocol is future work: it requires the dstack gateway's
`port_policy` option `pp` and a corresponding ingress change.

## Commands

`topup --help` lists them; each takes `--help`.

| Command | Purpose |
|---|---|
| `topup run --config FILE` | The service: the HTTP API and every worker loop. |
| `topup migrate` | Applies migrations as the database owner. Incomplete payment settings cutovers must be completed on 0.9.x before upgrading. |
| `topup config check [--secrets] FILE`, `topup config show FILE` | Validates a configuration file without any secret; prints it resolved, as JSON with every route default written out and each keyed provider's URL still carrying its `{key}`. `--secrets` also checks each provider's sealed key (`TOPUP_RPC_ANKR_KEY` and `TOPUP_RPC_INFURA_KEY`) against its URL and prints no value. |
| `topup route validate FILE`, `topup route show FILE` | The same for one route file (`examples/`, the sandbox template). `--template` permits the zero factory and implementation placeholders of a deployment template. |
| `topup rpc check --config FILE` | Checks both endpoints with typed production calls through their sealed credentials; prints endpoint ids. |
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
rpc:
  - chain_id: 11155111
    read:
      id: ankr-sepolia
      url: "https://rpc.ankr.com/eth_sepolia/{key}"
      sealed_key: TOPUP_RPC_ANKR_KEY
      max_log_blocks: 3000
    verify:
      id: infura-sepolia
      url: "https://sepolia.infura.io/v3/{key}"
      sealed_key: TOPUP_RPC_INFURA_KEY
      max_log_blocks: 3000
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
- **`rpc`** configures exactly one read/verify pair per route or price chain. Read is Ankr;
  verify is Infura. IDs are globally unique `[a-z0-9-]{1,40}` labels, chain IDs are unique,
  both endpoints use HTTPS and their hosts differ. Each URL has one `{key}` as a whole path
  segment or query value; `sealed_key` names an explicit `TOPUP_RPC_[A-Z0-9_]+_KEY` variable.
  `max_log_blocks` is positive and measured per endpoint. Every configured sealed key must be
  present in the candidate sealed environment and in the topup/restore-check compose mapping.
  `topup config check --secrets` checks them without printing their values.
  Cadences, range limits and budgets are code constants. No in-process endpoint selection or
  failover exists. The single Alloy retry layer retries HTTP 429/503 and JSON-RPC throughput
  errors; Infura HTTP 402 stops verify until the next UTC midnight.
  See [the RPC runbook](../deploy/RPC.md) and [chain reads](design/chain-reads.md).
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
- a missing or unused read/verify chain pair;
- read and verify endpoints on the same host;
- duplicate chain or endpoint ids, an invalid sealed key name, or unknown fields;
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

`migrate` and `restore-check` require the trusted database owner;
`migrate` also needs permission to create roles and schema objects. These commands refuse the
application role. Recovery and restore checks require stopped writers.

## Environment

| Variable | Read by | Meaning |
|---|---|---|
| `DATABASE_URL` | database commands | The login above; its password is in `PGPASSFILE`. |
| `TOPUP_RPC_*_KEY` | `run`, `reconcile`, `restore-check`, `config check --secrets`, `rpc` | Each endpoint's explicit `sealed_key`, which fills its URL's `{key}`. |
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
Missing either company pauses PHA; the Kraken check is staging/noncommercial only.

Source descriptors are `source: kraken`/`binance`, `symbol`, and the canonical `company`, or
`source: chainlink`, `feed`, `chain_id`, and `observation_chain_id` for explicit cross-network
observations. Each price chain resolves its shared `rpc` read/verify pair. Supported pinned feeds
are USDC_USD and USDT_USD on Ethereum (1) and Base (8453), plus Ethereum ETH_USD for the TWAP
composite. See [feed evidence](design/price-feed-registry.json).

Base and Base Sepolia require `sequencer_uptime: { feed: BASE_SEQUENCER_UPTIME, grace_s: 3600 }`.
Configure Ethereum and Base endpoint pairs in `rpc` for staging's price observations; these chains
do not scan payment contracts. Feed addresses and heartbeat values are pinned in the image.
Chainlink freshness allows heartbeat + **600 s** for publication delay: the last eight Ethereum
round intervals were already up to 36 s late in calm conditions (see the feed evidence and design
above). Deviation-triggered updates and all agreement/peg checks remain in effect.
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
Noncommercial rehearsal compares current Uniswap spot × ETH/USD against current Kraken PHA/USD;
CoinGecko is dropped. See the
[on-chain plan](design/price-failover.md#pha-on-chain-follow-up). `topup config check` prints ordered
sources, verdicts, pinned feed metadata and testnet markers; `config show` emits resolved `price`.

For PHA, configure `source: uniswap_v2_twap` and `observation_chain_id` for cross-network routes. The fixed pair and tokens
are pinned in the image. Optional `twap` settings default to `window_s: 1800`,
`max_sample_age_s: 180`, `min_weth_reserve_usd: 100000`, `max_spot_deviation_bps: 300`,
and `max_sample_jump_bps: 500`. Windows below thirty minutes are rejected. The service samples
once/minute into PostgreSQL even without quote traffic (300 seconds on staging PHA); startup and gaps require a continuous
window before prices become available. `config show` includes the resolved guard rails.

The staging PHA routes use 300 s TWAP samples with `max_sample_age_s: 900` and
`max_sample_jump_bps: 1100`. The gap bound retains three sample intervals, tolerating two
missed samples. The jump bound scales the existing 500 bps per minute by
√(300/60) under the random-walk assumption, rounded to 1100 bps. Code defaults remain
180 s and 500 bps. Only `livemode: false` routes allow sample ages up to 900 s. Live routes
retain their original validation bounds: age 60–600 s and jump 1–2000 bps. These are bounds,
not the defaults. Both modes reject `window_s + max_sample_age_s > 2880 s`: the pinned TWAP
observation chain is Ethereum (12 s blocks), and Multicall3's `BLOCKHASH` can reach only 256
blocks. The limit leaves a 16-block margin (240 × 12 s) for the oldest sample. A longer window
cannot be verified by that contract and is refused at config validation.
