# Contributing to Phala Pay

Thank you for helping. This guide covers the development setup, the checks CI runs, the commit
and pull request conventions, and releases. Everyone taking part follows the
[code of conduct](CODE_OF_CONDUCT.md). Report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md), never in an issue or pull request.

## Before you start

- **Bugs and features**: report them with the issue forms. For a larger change, open an issue
  first so the approach is agreed before you write it.
- **Design changes**: behavior is specified in [docs/architecture.md](docs/architecture.md). A
  pull request that changes behavior cites the sections it implements and updates them.
- **Documentation**: [docs/README.md](docs/README.md) indexes every document.

## Development setup

The repository is a Rust workspace (`crates/`) with Foundry contracts (`contracts/`), a Python SDK
(`sdk/python`), a TypeScript SDK (`sdk/js`), and deployment files (`deploy/`). Clone it with its
submodules, which the contracts need:

```sh
git clone --recurse-submodules https://github.com/Phala-Network/phala-pay.git
```

Install the pinned tools that CI uses:

| Tool | Version | For |
|---|---|---|
| Rust | 1.98.1, from [rust-toolchain.toml](rust-toolchain.toml) | the service (rustup 1.28 or newer installs it with `rustup toolchain install`) |
| [Foundry](https://getfoundry.sh/) | v1.8.3 | contracts, and the Anvil chain of the integration tests |
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | 0.20.2 | dependency policy (`make lint`) |
| PostgreSQL | 18 | the database integration tests |
| [uv](https://docs.astral.sh/uv/) | 0.12.18 | the Python SDK (Python 3.14 for development, 3.12 or newer supported) |
| Node.js and pnpm | Node 24, pnpm from `packageManager` in [sdk/js/package.json](sdk/js/package.json) | the TypeScript SDK and the website |
| Docker | with Compose and BuildKit | images, local stacks, and the browser end-to-end tests |

```sh
rustup toolchain install
foundryup --install v1.8.3
cargo install cargo-deny --version 0.20.2 --locked
```

Use the committed lockfiles (`Cargo.lock`, `uv.lock`, `pnpm-lock.yaml`), and do not update
dependencies as a side effect of unrelated work.

## Running the checks

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs every check below on each pull
request. Run the ones for the parts you change.

### Rust service

```sh
make build   # cargo build --workspace --locked
make lint    # rustfmt, clippy with and without all features, and cargo-deny: CI's lint job
make test    # cargo test --workspace --locked
```

The database and Anvil integration tests skip unless PostgreSQL and `anvil` are available. To run
them as CI does, start PostgreSQL 18 and point the tests at it. Each test creates its own database
and login role, so any host and port will do:

```sh
docker run -d --name phala-pay-test-postgres -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:18.6-trixie
export SQLX_OFFLINE=true   # compile against the committed .sqlx metadata, as CI does
# The harness's database owner and application login (crates/topup/tests/support).
export OWNER_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
export DATABASE_URL=postgres://topup_ci:topup_ci@localhost:5432/postgres
cargo test --workspace --locked --all-features
```

Keep `SQLX_OFFLINE=true` set: with `DATABASE_URL` set and no offline flag, sqlx checks every query
against that database at compile time, and the build fails before the `topup_ci` role exists.

With `CI=true`, as in GitHub Actions, a missing database or `anvil` fails the tests instead of
skipping them.

SQL queries are checked at compile time against the committed [.sqlx](.sqlx) metadata
(`SQLX_OFFLINE=true`). To change a query, regenerate the metadata with
[sqlx-cli](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli) 0.9.0 against a migrated
database, as CI's check does without `--check`. `topup migrate` needs a `topup` that compiles, so
migrate the database before you edit the queries, while the committed metadata still matches them,
with `SQLX_OFFLINE=true` still exported:

```sh
cargo install sqlx-cli --version 0.9.0 --no-default-features --features rustls,postgres --locked
# With any new migration added, but before editing a query (the committed metadata still matches):
DATABASE_URL="$OWNER_DATABASE_URL" cargo run --locked -p topup -- migrate
psql "$OWNER_DATABASE_URL" -c "CREATE ROLE topup_ci LOGIN PASSWORD 'topup_ci' IN ROLE topup_app"
# Edit the queries, then:
SQLX_OFFLINE=false cargo sqlx prepare --workspace -- --all-targets --all-features
```

### Contracts and deployment

```sh
make deploy-check    # forge fmt, build, and test, and the deterministic deployment checks
make runbook-check   # every runbook command against the CLI and the OpenAPI documents
deploy/validate-compose.sh
```

CI also runs ShellCheck on every shell script and the tests in [deploy/tests](deploy/tests).

### SDKs

```sh
make sdk-check                                  # Python: ruff, mypy --strict, pytest, and the codegen no-op check
make sdk-generate                               # Python: regenerate the client after an openapi.json change
(cd sdk/js && pnpm install && pnpm run check)   # TypeScript: typecheck, lint, unit tests, and build
(cd sdk/js && pnpm run e2e:docker)              # TypeScript: the browser end-to-end tests on Anvil
scripts/version.sh                              # Both: the SDKs name the Cargo workspace version
```

The JavaScript SDK's CI check also runs the full unit, type, build, and browser suite against the
advertised React 18 floor (React and React DOM 18.2.0 with React types 18.3.31 and DOM types
18.3.7). Keep the default local check on the committed React 19 toolchain; use the pinned floor
versions above when reproducing that compatibility job locally.

A pull request that changes `crates/topup/openapi.json` regenerates the Python client in the same
pull request; CI fails if regeneration is not a no-op.

The Python SDK's RFC 9421 signer serves only the operator's admin API. Regenerate its vectors only
after an intentional change to the signing profile, then rerun the Rust test:

```sh
(cd sdk/python && uv run --locked python -m tests.vectors)
cargo test -p topup --lib api::auth
```

### Supply-chain checks

Dependabot checks Cargo, npm, uv, Actions, and Docker weekly. Minor and patch updates are grouped
per ecosystem and project; major updates stay separate. Review lockfile and digest changes.
CI scans the JavaScript and Python lockfiles, including development dependencies, and all three
final images with Trivy. Lockfiles fail on every fixable vulnerability; images fail on fixable
HIGH/CRITICAL vulnerabilities. Unfixed findings are reported for review. Release repeats the scans against the published final digests after the
reproducibility comparison, then signs SPDX SBOM attestations. A scan failure blocks the release.

Vulnerability exceptions belong only in
[.github/security/trivy-ignore.yaml](.github/security/trivy-ignore.yaml), written as JSON (valid
YAML). Each entry needs an advisory `id`, a specific `statement` explaining exposure and mitigation,
and an `expired_at` UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`). Prefer a `paths` restriction where possible. CI rejects
missing reasons, expired exceptions, and expiries over 30 days; do not use blanket exclusions.

Slither 0.11.6 analyzes production contracts with every finding level enforced. Suppress only an
individual finding using `slither-disable-next-line <detector>` and an adjacent reason; dependency,
test, and deployment-script findings are outside this production-contract check. CodeQL analyzes
Actions, JavaScript/TypeScript, Python, and Rust on pull requests, main, and weekly.

The payment gate runs `make cvm-rehearsal` with the real Rust service, PostgreSQL, Anvil, the dstack
simulator and signed receiver. It checks a 2,500-cent credit, redelivers the actual event through
the service, waits for acknowledgement, and compares the receiver's credit, bonus, adjustment, order and
balance records.

Repository owners must enable **Settings > Advanced Security > Dependabot > Dependabot alerts**
and **Dependabot security updates**. Under **Settings > Rules > Rulesets**, add the following CI
checks to main's required status checks, alongside the existing checks:
`dependencies (sdk/js)`, `dependencies (deploy/product/web)`, `dependencies (sdk/python)`,
`dependencies (docs/reference)`, `dependencies (deploy/tools)`,
`slither`, `cvm-rehearsal`, and CodeQL's `analyze (actions)`, `analyze (javascript-typescript)`,
`analyze (python)`, `analyze (rust)`. Keep `image` and `deployment` required: they now scan final
images. Use CodeQL **advanced setup** (this versioned workflow); disable default setup if it is
already enabled to avoid conflicting analyses. Security updates and branch rules are owner
settings, not changed by this pull request.

### Documentation

CI lints every Markdown file with [markdownlint](https://github.com/DavidAnson/markdownlint-cli2)
([.markdownlint-cli2.jsonc](.markdownlint-cli2.jsonc)) and checks every relative link and anchor
offline with [lychee](https://github.com/lycheeverse/lychee) ([lychee.toml](lychee.toml)). To
run both locally:

```sh
npx --yes markdownlint-cli2@0.23.2
docker run --rm -v "$PWD:/input:ro" -w /input lycheeverse/lychee:0.24.2 --no-progress .
```

Write documentation in plain, present-tense English, with sentence-case headings and a language
on every code block. When you rename a heading, search the repository for links to its anchor:
code comments, runbooks, and Sentry alerts link to some of them.

### Local stacks

| Command | What it runs |
|---|---|
| `make up`, `make down` | The attested compose with local settings: PostgreSQL, the dstack simulator, and backups. |
| `make sandbox-local` | The [integrator sandbox](deploy/sandbox/README.md): the whole stack on Anvil, and every payment scenario. |
| `make restore-drill` | A backup and restore drill on a local stack ([deploy/RESTORE.md](deploy/RESTORE.md#local-and-ci-drills)). |
| `make cvm-rehearsal` | The release artifact against Anvil and the dstack simulator, from sealing to one credited deposit. |
| `make image`, `make verify-image` | The runtime image, and the check that two builds are identical. |

## Commits and pull requests

- Create one branch per change from `main`, named `<type>/<slug>` after the
  [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) type, for example
  `fix/scanner-stall`.
- Title the pull request as a Conventional Commit, `<type>(<scope>): <summary>`, for example
  `fix(scanner): retry a stale finalized head`. Pull requests are squash-merged through the merge
  queue, which runs the required checks on each change combined with the ones queued before it, so
  the title becomes the commit message on `main` and `main` stays green.
- Keep each pull request to one change, and fill in the
  [pull request template](.github/PULL_REQUEST_TEMPLATE.md): what changed, the specification
  sections it implements, and the verification commands you ran with their results.
- Record integrator-visible API, webhook, and SDK changes in [CHANGELOG.md](CHANGELOG.md) under
  `## [Unreleased]` (see [Releasing](#releasing)).

## Code conventions

### Lint policy

Every crate forbids unsafe code and warns on missing public documentation. The pure `core` crate
and the boundary-defining `adapters` crate also deny unchecked arithmetic, floating-point
arithmetic, `as` conversions, `unwrap`, `expect`, indexing and slicing, and panics. These rules keep
money paths explicit and make fallible behavior visible at review time.

Tests may use direct assertions and fixtures that intentionally exercise these operations. Put the
exception at the crate root so production code remains covered:

```rust
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::expect_used,
        clippy::float_arithmetic,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]
```

### Dependencies

The `topup-core` dependency policy test
([crates/core/tests/dependency_policy.rs](crates/core/tests/dependency_policy.rs)) reads
`cargo metadata` and allows only an explicit list of reviewed, pure, I/O-free runtime dependencies.
A new runtime dependency of `core` is reviewed as pure and I/O-free, then added to that list in the
same pull request; `tokio` and I/O frameworks never belong in `core`.

Route YAML is parsed at the `topup` I/O boundary with exactly pinned `serde-saphyr`, selected for
its maintained, panic-resistant, unsafe-free implementation. The archived `serde_yaml`, deprecated
`serde_yml`, and newer single-owner `noyalib` were considered but not selected. The temporary
`RUSTSEC-2024-0436` exception in [deny.toml](deny.toml) covers `alloy-primitives 1.7.3 -> paste
1.0.15` at compile time only, and must be removed as soon as an Alloy upgrade drops `paste`.

### Reproducible builds

Always build release artifacts with the committed lockfile. Set `SOURCE_DATE_EPOCH` to the source
commit timestamp when producing an image:

```sh
export SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct)"
cargo build --release --locked -p topup
docker build --build-arg SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" -t phala-pay:dev .
docker run --rm phala-pay:dev topup --help
```

Release artifacts build only the `topup` package. The Debian 13 (trixie) builder and the distroless
`cc-debian13` runtime images are pinned by digest and share the same libc baseline. `make verify-image` builds the image
twice and checks that the digests match, as the Release workflow does before it publishes.

### Review checklist

1. Implements exactly the cited specification sections; no extra features or configuration.
2. Every money path uses the `core` newtypes; no float, unchecked arithmetic, or `as` conversion.
3. Every external effect writes its intent before the call and is idempotent on retry.
4. Tests exist for the change and fail if the behavior is removed.
5. No secret, key, or credential can reach a log, error, or response.
6. Migrations are additive and reversible; append-only tables have no update or delete path.
7. The pull request names the verification commands actually run and their results.

## Releasing

The service, its images, and its deployment files, and both SDKs, `@phala/pay` (sdk/js) and
`phala-pay` (sdk/python), share one version and are released together as `v<version>`, under
[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html): while the major version is 0, a
breaking change bumps the minor version. Every release publishes both SDKs at its version, changed
or not, so an integrator uses the SDK version equal to the operator's service version
([integration guide, §5.9](docs/integration.md#59-versioning-and-deprecation)). Operators deploy
releases, never a commit ([deploy/README.md, "Releases"](deploy/README.md#releases)).

The version's one source is the Cargo workspace version (`version` under `[workspace.package]` in
`Cargo.toml`). `scripts/version.sh` prints it and fails unless `sdk/js/package.json`,
`sdk/python/pyproject.toml`, `sdk/python/uv.lock`, and the Python SDK `deploy/deploy.sh` generates
the admin key with name it too, as CI checks; `scripts/version.sh <version>` sets it in all of them
and in `Cargo.lock`.

The top-level [CHANGELOG.md](CHANGELOG.md) follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/). A pull request that changes what
integrators or operators see adds its entry under `## [Unreleased]`, in `Added`, `Changed`,
`Deprecated`, `Removed`, `Fixed`, or `Security`, and marks breaking changes **Breaking**; an SDK
change goes under ``### JS SDK (`@phala/pay`)`` or ``### Python SDK (`phala-pay`)``, below the
service's entries, in the same kinds one level down (`#### Added`). The SDKs' releases before
v0.5.0, versioned on their own, stay in their frozen `sdk/js/CHANGELOG.md` and
`sdk/python/CHANGELOG.md`.

1. Open a release pull request, `chore(release): v<version>`, that runs
   `scripts/version.sh <version>`, regenerates the OpenAPI snapshots, whose `info.version` is the
   crate version (`UPDATE_OPENAPI=1 cargo test -p topup --test api openapi_snapshot`), renames
   `## [Unreleased]` in `CHANGELOG.md` to `## [<version>] - <YYYY-MM-DD>` above a new empty
   `## [Unreleased]`, and updates the link references at its end:

   ```markdown
   [unreleased]: https://github.com/Phala-Network/phala-pay/compare/v<version>...HEAD
   [<version>]: https://github.com/Phala-Network/phala-pay/releases/tag/v<version>
   ```

2. After it merges, a repository admin tags the merge commit on `main` (only admins may create,
   move, or delete `v*` tags, and a published release is immutable):

   ```sh
   git tag v<version> <merge commit> && git push origin v<version>
   ```

3. [Release](.github/workflows/release.yml) checks that the tag is a commit of `main`, that it
   names the Cargo workspace version, which the SDKs name too, that no stable `v*` tag is newer,
   and that a stable version has its dated changelog section, and runs the whole CI workflow on
   the commit, the SDKs' checks and end-to-end tests included. Then, on GitHub-hosted runners, it
   builds each image with `deploy/verify-image.sh` (`phala-pay` and the reference product must
   build to the same digest twice, including `postgres-walg`), pushes it tagged `v<version>`,
   and attests it. It builds the deploy kit with `deploy/build-kit.sh`, renders every environment
   with it and the Phala Cloud template's compose from it, and attests `images.json`, the kit,
   `phala-cloud-template.yml`, `deploy.sh` (whose admin key generation uses this version's Python
   SDK), and `SHA256SUMS`. Only then does it publish the SDKs from the `npm` and `pypi`
   environments with trusted publishing (npm provenance, PyPI attestations). Last, it publishes
   the GitHub release `v<version>`, the Latest release, with those assets and the changelog
   section, the SDKs' changes included, as its notes (`scripts/changelog-section.sh`), and checks
   that `https://pay.phala.com/deploy.sh` serves its `deploy.sh`.
   `deploy/verify-release.sh v<version> DIR` verifies the result.
4. Adopt it for Phala's instance: a pull request that sets the release in
   [deploy-phala.yml](.github/workflows/deploy-phala.yml) (`uses: …@v<version>` and `version`).

A pre-release, `v<version>-rc.N`, is a candidate for Phala's staging: its pull request runs
`scripts/version.sh <version>-rc.N` (which Python spells `<version>rcN`) and adds no changelog
section, and Release publishes it as a GitHub pre-release of the service alone, without the SDKs
(so its `deploy.sh` needs `TOPUP_ADMIN_PUBLIC_KEY`).

Stable releases go up one at a time: tag the next only after the previous Release run has
finished, and never release a version older than the newest stable tag (Release refuses one), as it
would take the Latest release and npm's `latest` from the newer one.

### Recovering a release

Never delete or move a `v*` tag, and never replace a published release's assets: the tag ruleset
and immutable releases refuse both, and Deploy trusts them. When a Release run fails for a cause
outside the tagged commit (a registry or GitHub outage, a missing setting), fix the cause and use
**Re-run failed jobs** on that run. Each job is safe to re-run:

- `py-publish` uploads the run's own build and skips the files PyPI already has, completing a
  partial upload;
- `js-publish` is done when npm already has the version from the tagged commit (its `gitHead`), and
  fails when npm has it from any other commit;
- `release` creates the GitHub release once. If it failed before the release was published, delete
  the draft release it may have left (a draft is not yet immutable) and re-run it; once the
  release is published, never re-run it;
- `deploy-entry` only reads the published release. If it fails, verify that release as an operator
  would, without re-creating it, and see what `https://pay.phala.com/deploy.sh` serves:

  ```sh
  deploy/verify-release.sh v<version> "$(mktemp -d)"
  gh release view --json tagName --jq .tagName   # the Latest release
  curl -fsSL https://pay.phala.com/deploy.sh | grep '^release='
  ```

  Mark it Latest if it is not (`gh release edit v<version> --latest`), or fix pay.phala.com's
  redirect, then re-run `deploy-entry`.

A failure in the tagged commit itself (a build or a check that fails on it) burns the version: fix
it on `main` and release the next patch version. Whatever the failed run already published (images,
SDKs) stays, unused.

## License

Phala Pay is licensed under the [Apache License 2.0](LICENSE). Unless you state otherwise, a
contribution you submit is licensed under it, as section 5 of the license provides.
