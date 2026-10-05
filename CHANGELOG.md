# Changelog

Phala Pay's releases. The service, the JS SDK packages `@phala/pay` (`sdk/js`),
`@phala/pay-react` (`sdk/js-react`), and `@phala/pay-server` (`sdk/js-server`), and the Python SDK
`phala-pay` (`sdk/python`) share one version and are released together (CONTRIBUTING.md,
"Releasing"). Each version records the changes to the HTTP API and webhook payloads that
integrators see and to the deployment operators run, then the SDKs' changes under "JS SDK" and
"Python SDK". Additive fields are not breaking; webhook receivers must ignore unknown fields. Each
version's section is its GitHub release's notes. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The SDKs' own releases before v0.5.0
are in [sdk/js/CHANGELOG.md](sdk/js/CHANGELOG.md) and
[sdk/python/CHANGELOG.md](sdk/python/CHANGELOG.md).

## [Unreleased]

### Fixed

- `GET /v1/forwarders?sweepable=` screens only the requested chain's treasuries, concurrently,
  and reuses a clear sanctions verdict for 10 minutes; under RPC rate budgets it took several
  seconds per chain and could time out.

## [0.9.2] - 2026-10-05

### Fixed

- Chainlink sources omitting `rpc_group_b` with `rpc_group: a` when the route's second RPC group is
  literally `a` (e.g. `["x", "a"]`) previously read one group twice, silently losing A/B
  independence. Upgrading reads two independent groups and may newly reject quotes as `divergent`.
  Configurations setting `rpc_group_b` explicitly are unaffected and need no action.
- Sequencer uptime validation now checks the groups the service reads; previously it looked up
  `a`/`b` literally while the service resolved them as route aliases. Runtime is unchanged.
  Configurations that only validated under the old lookup may now be rejected by `config check`.
  Shipped staging, example, and examples/ configurations use non-alias sequencer group names and are
  unaffected.

### Python SDK (`phala-pay`)

#### Changed

- The 60 resource method signatures that previously declared `request_deadline` and
  `upgrade_tolerance` now show `**options` in `help()` and `inspect.signature()`; the accepted
  runtime keywords are unchanged. Unknown-keyword `TypeError` messages no longer include the
  method name prefix.

#### Deprecated

- The legacy `PhalaPay` constructor is deprecated and emits `DeprecationWarning`; it will be
  removed in 0.10.0. Configure pins or use `PhalaPay.from_env()` instead.

## [0.9.1] - 2026-10-05

### Upgrading from 0.8.x

- 0.9.0 cannot be deployed with Deploy. Upgrade 0.8.x directly to 0.9.1 and follow every step of
  [0.9.0's "Upgrading from 0.8.x"](https://github.com/Phala-Network/phala-pay/releases/tag/v0.9.0),
  including the pre-upgrade backup and the one-time `bootstrap_maintenance: true`.

### Fixed

- Deploy fetches `deploy/deadline.sh` with `deploy/verify-release.sh` at the release commit, so release
  verification no longer fails before any change (0.9.0's Deploy stopped at "Verify the release").
  A CI check keeps all three consumers fetching every script `verify-release.sh` sources.
- The 0.9.0 kit's example environment failed `topup config check`; production examples now use
  Chainlink USDC/USDT routes, and PHA is explicitly limited to noncommercial rehearsal.
  CI validates every shipped environment config and example route template.
- Cross-org callers can now pass `SENTRY_DSN`; the example passes the maintenance key and
  `bootstrap_maintenance` input.

## [0.9.0] - 2026-10-05

### Upgrading from 0.8.x

- **Breaking:** rollback is restore-only: **no rollback to 0.8.3; restore required**. Keep a
  pre-upgrade backup and follow the [restore runbook](deploy/RESTORE.md); no 0.8.x rollback is
  supported.
- **Breaking:** migrate route `pricing` to `price`, configure independent observation RPC groups
  and the Base sequencer gate, and replace restricted price sources. See the
  [configuration migration](docs/configuration.md#price-sources).
- Set up an independent maintenance key in attested `maintenance_keys` and the deployment
  Environment before using planned upgrade admission. Follow
  [maintenance key setup](deploy/README.md#planned-upgrade-admission-and-downtime), including
  the explicit first-rollout bootstrap procedure.
- **Breaking:** migrate JavaScript imports to the three packages using the
  [import table below](#js-sdk-phalapay-phalapay-react-phalapay-server); review Python's typed
  transport errors and stricter validation before upgrading.
- Pin every SDK to the service version: `@phala/pay`, `@phala/pay-react`, `@phala/pay-server`, and
  `phala-pay` must all use **0.9.0** with service **0.9.0**.

### Added

- Planned upgrade admission uses an audited, process-owned mutation pause with an expiring lease.
  New mutations return `503 service_maintenance` with `Retry-After`; reads and in-flight work
  continue. Deployment health checks resume admission, with failure cleanup, automatic expiry,
  and a [manual-clear runbook](deploy/runbooks/instance-maintenance.md). Business pauses remain
  in effect. Deploy artifacts and the job summary record sampled first-failed/first-healthy
  downtime.
- Independent maintenance signing keys and key IDs: attested `maintenance_keys` authorize only
  instance pause/resume; other admin routes return audited `403 permission_denied`. Full
  operator admin authority remains outside deployment CI.
- Sentry business alerts for webhook backlog and stalled delivery, internal signer/egress
  failures, stale RPO heartbeats, overdue treasury/refund work, and ingress certificate expiry.
  Probes emit on state transitions with hourly reminders and log recovery without an event.
  Outbox alerts detect overdue processing and exclude known failing merchant endpoints; backlog
  warnings require at least two eligible endpoints. Staging synthetic alert commands and a
  production DSN preflight check support verification.
- Capacity alerts for pgdata/observability disk usage at 75%/90%, pending WAL size/age, and stale
  probes. Data retention remains seven years, with documented safe disk-pressure response.
- Licence-free `uniswap_v2_twap` PHA/WETH × Chainlink ETH/USD pricing, pinned to one A/B-agreed
  Ethereum block. One-minute samples persist across restarts; prices require a continuous window
  of at least thirty minutes, with liquidity, spot divergence, freshness and sample jump guard
  rails and distinct price refusal alerts. PHA defaults use TWAP/Kraken: value at
  min(TWAP, current spot) × ETH/USD and compare current Uniswap spot × ETH/USD against current
  Kraken PHA/USD. Default spot/TWAP divergence is 3%, pausing faster moves; the stricter policy
  starts a new thirty-minute observation window. PHA remains staging-only because no second
  Allowed independent source exists; production stablecoin routes are unaffected.
- Migration compatibility floors and checksum checks for N-1 rollback, verified against the
  actual previous-release image. 0.9.0 establishes the compatibility-ledger protocol with an
  explicit legacy bootstrap boundary; the restore-only declaration below governs this upgrade.

### Changed

- **Breaking:** replace route `pricing` with explicit `price` stablecoin sources or ordered
  volatile primary/check/FX lists. The legacy parser remains available for one migration window
  and emits the new shape; mixed schemas and restricted legacy sources fail validation. Replace Coin Metrics
  with reviewed Chainlink/exchange sources, configure mainnet A/B observation groups and testnet
  markers, and add the Base sequencer gate. Stablecoin defaults use only Allowed on-chain
  Chainlink data; production rejects PermissionRequired/Prohibited sources. See the
  [migration guide](docs/configuration.md#price-sources).
- Price sources fail over independently within ordered roles and fail closed on insufficient
  evidence. Stablecoins credit exactly $1 only with a fresh in-band observation and no fresh
  depeg; volatile assets require two agreeing independent companies. Chainlink freshness allows
  the pinned heartbeat plus 600 seconds without weakening completeness, agreement or peg gates.
- WAL-G uploads, backup listings and restores have separate configurable deadlines and retry
  budgets. Timed-out recovery fetches abort recovery instead of promoting a partial restore.
- Ingress TCP connect/client/server timeouts are 5s/30s/30s. Production preflight enforces Sentry
  configuration; capacity thresholds use the shared business alert emitter. Deploy stages and
  network calls have elapsed-time deadlines with timeout diagnostics.
- Expand-only migration `20261029000000_uniswap_twap` adds immutable observation history without
  changing payment tables or permissions. Scale migrations `20261029030000`–`20261029030005` add
  two cursor tables and five indexes without changing existing rows or constraints. Their
  compatibility ledger retains floor `20261028000002`: compatible protocol-aware binary rollback
  uses the previous config and retains observations. This does not permit rollback to 0.8.x.
- Local SDK sandbox and CVM rehearsals use disposable TLS ingress with a run-scoped certificate,
  preserving HTTPS validation and normal system trust roots. Hermetic local Chainlink, Uniswap
  and exchange fixtures support price rehearsal.

### Fixed

- Price RPC groups tolerate small pinned-read head regressions and fail over within a group.
- Image builds retry once without caches for recognized Actions cache transport failures only;
  cache export failures no longer fail builds, and diagnostics remain available.
- Backup health requires a current-timeline base backup and fresh WAL data, backlog and LSN
  progress; old backlog uploads cannot refresh recovery age. First base backups retry with
  capped backoff until successful.
- Failed critical restore checks block acceptance and unfreeze. An explicit administrator
  override requires a reason and is recorded atomically in audit history.
- Local restore drills use disposable Sepolia and Base Sepolia Anvil chains with distinct A/B
  RPC endpoints and contract fixtures. They enforce the 60-second RPO from failure to the last
  replayed committed marker, including upload latency; RTO includes unfreeze and a successful
  merchant API request.
- API body reads are bounded to 5 seconds and request processing to 25 seconds; health checks
  have independent capacity and a 2-second database deadline. API connections have a 30-second
  read idle timeout and a 60-second hard lifetime, bounding incomplete headers and keep-alive work.
- Scan all issued addresses, including retired addresses, in durable pages of 1,000. Finalized
  and confirmation cursors advance only after every address page commits. Reconciliation uses
  persistent rotating pages for historical credit, derivation, custody, and flush-link checks,
  with a bounded 4,000-entry derivation cache and concurrent query indexes. Post-restore row checks
  still traverse the complete ledger.

### Breaking (operators)

- 0.9.0: **no rollback to 0.8.3; restore required**. No rollback to any 0.8.x release:
  use the pre-upgrade backup and [restore runbook](deploy/RESTORE.md). Route configuration and
  SDK changes also prevent rollback. 0.9.0 is the first compatibility-ledger protocol release;
  from 0.10.0 onward, real N-1 rollback is enforced unless an operator declaration and a raised
  compatibility floor require restoration. N-1 is always the latest stable release.

### JS SDK (`@phala/pay`, `@phala/pay-react`, `@phala/pay-server`)

#### Added

- `<DepositAddress onChange(state)>` receives the public view on its first read and once per
  content change, so integrators can update their UI without additional polling.
- Opt-in `upgradeTolerance` in `@phala/pay-server` for GET and idempotent POST retries across
  maintenance, connection failures, and gateway 502/503/504 for up to five minutes. Explicit
  deadlines and cancellation remain effective; keys and bodies stay fixed. Browser core checkout
  and React deposit address polling preserve their last view through outages and show a neutral
  reconnecting state.
- Merchant server client at `@phala/pay-server`: all OpenAPI resources, typed parameters/responses,
  verified quote/address results, two-variable `PhalaPay.fromEnv()`, frozen pins parse/encode,
  bound Ed25519 webhooks and `checkoutParams` with browser-exported `CheckoutParams`.
- Deadline-bounded transport with attempt timeouts, jitter, Retry-After, frozen POST idempotency,
  redirect refusal, lossless integer validation, cancellation and typed redacted errors.
- Pure `depositNetAmount` / `balanceDelta` helpers with monotone ledger convergence and strict
  validation, covering all five shared fixture groups.
- `@phala/pay-server/helpers` for keyless address, webhook and offline sweep builders. Existing
  server helper exports remain available; `ForwarderResponse` names the generated API forwarder
  while `Forwarder` retains its legacy contract-pins type.

#### Changed

- Checkout and React deposit-address polling use uniform ±20% jitter, pause in hidden tabs, and
  read immediately on visibility regain. `Retry-After` remains a minimum delay.
- `<DepositAddress>` slows to a 15-second interval after ten minutes without an observed change;
  any view change or visibility regain restores the normal interval and resets the idle window.
- Checkout stops after three consecutive non-terminal 4xx responses other than 408/429 and
  surfaces its existing error state. 404 still stops immediately as `invalid_client_secret`.
- **Breaking:** the JavaScript SDK is now three packages. `@phala/pay` is framework-free browser
  code, `@phala/pay-react` contains the React components and `styles.css`, and `@phala/pay-server`
  contains the merchant client and server helpers. Migrate the removed entry points as follows:

  | Old import | New import |
  | --- | --- |
  | `@phala/pay/react` | `@phala/pay-react` |
  | `@phala/pay/styles.css` | `@phala/pay-react/styles.css` |
  | `@phala/pay/server` | `@phala/pay-server` |
  | `@phala/pay/server/helpers` | `@phala/pay-server/helpers` |

  Install the new package explicitly at the service version. `@phala/pay` has no React peer
  dependency or server/key-handling code.
- **Breaking:** `@phala/pay-server` rejects browser-like environments before reading credentials.
  Move browser offline helper imports to `@phala/pay-server/helpers`. Node.js >=20.3, Bun and
  Deno server runtimes are supported when fetch, AbortSignal.any and WebCrypto are available.
  The browser core entry and existing checkout props remain keyless.
- **Breaking:** duplicate webhook signing headers and non-exact UTF-8 bodies are rejected. The
  legacy `WebhookSignatureError` export is an alias of `SignatureVerificationError`.

### Python SDK (`phala-pay`)

#### Added

- Pins-based `PhalaPay` configuration and `from_env()`, bound webhook verification, checkout
  parameters, and paginated address validation. Checkout handoffs bind the originating verified
  quote and revalidate its address.
- Pure ledger helpers validate snapshots and merge cumulative refunds and reversals without
  floats or input mutation. Unknown webhook types are ignored while their raw objects remain
  available.
- Opt-in `upgrade_tolerance` at client construction and per resource call for GET and replayable
  idempotent POST retries through maintenance, network/attempt timeouts, and gateway 502/503/504
  (including HTML) for at most five minutes. Explicit deadlines stay hard limits, body/key stay
  fixed, and interrupts or replayed errors terminate retries. Interactive defaults remain
  15 seconds per attempt, four attempts, and 60 seconds total.

#### Changed

- **Breaking:** transport failures are wrapped in typed SDK errors; update handlers that catch
  underlying transport exceptions. Diagnostics redact API keys and client secrets.
- **Breaking:** enforce immutable canonical pins, API key checksums, origin and response identity
  checks, and checkout handoffs from the originating verified quote. Redirects and invalid
  pagination are rejected; callers relying on previously accepted invalid inputs must migrate.
- Use one transport path for POST replay, explicit per-call controls, deadline and body timeouts,
  bounded jitter and Retry-After; borrowed transports retain their original ownership.

## [0.8.3] - 2026-10-04

### JS SDK (`@phala/pay`)

#### Added

- Branded chain and token icons in Checkout and DepositAddress, with mainnet-family artwork on testnets
  and neutral monogram fallbacks. Export `NetworkIcon` / `AssetIcon` from `@phala/pay/react`
  and framework-free `networkIcon` / `assetIcon` markup accessors from `@phala/pay`.
  Six SVGs are vendored from MIT-licensed web3icons, without a runtime dependency or injected styles.

#### Changed

- Checkout and deposit-address copy is shorter, with exchange withdrawal-fee guidance in the
  manual-transfer panel. Checkout defaults to QR when no browser wallet is discovered; payment
  tabs use single-line labels. Spacing is tighter with 44px touch targets.
- The React frame supports `--pp-root-border`, `--pp-root-padding`, `--pp-root-max-width`, and
  `--pp-root-background` overrides for embedding in host dialogs; frame defaults are unchanged.

#### Fixed

- `retrieveQuote` and `retrieveDepositAddress` trim `apiBase`'s trailing slashes in linear time;
  the previous `/\/+$/` pattern was quadratic on a long run of slashes
  (CodeQL `js/polynomial-redos`).

## [0.8.2] - 2026-10-03

### Added

- Live provider and stage elapsed-time diagnostics for read-only contract verification during
  deployment preflight, without printing RPC URLs or credentials.

### JS SDK (`@phala/pay`)

#### Fixed

- Reject invalid webhook time options and malformed signed resource objects; preserve the
  default 300-second window, exact matching at zero, raw-body verification, and key rotation.

### Python SDK (`phala-pay`)

#### Fixed

- Validate webhook time options and delivery timestamps consistently with the JS SDK; the
  low-level event verifier rejects malformed envelope fields instead of converting their types.

## [0.8.1] - 2026-10-03

### Fixed

- CVM startup with inline Compose configs: topup uses the writable root filesystem required by
  Docker's config injection, while retaining its non-root user and other service hardening.
  The rehearsal now shares the release policy; Docker creation and recreation of every rendered
  variant, including the reference product, run in CI.

## [0.8.0] - 2026-10-03

### Added

- Bounded HTTP request counters and latency histograms in signed admin metrics, including
  authentication, load-shed and read-only errors; documented operational service objectives.
- RPC operational alerts through the existing Sentry integration, with a one-minute group outage
  threshold and an RPC health runbook. Durable metrics expose their last successful refresh time.

### Changed

- Encode metrics with the pinned standard Prometheus client and enforce merchant/client-secret
  quotas with the existing pinned governor library, preserving tenant/mode and joint admission.
- Separate treasury ownership proofs, tenant-scoped reads and periodic work into focused modules.

- Quote lists batch deposit and pending-payment reads after scoped pagination, keeping detail
  payment selection, tenant/mode isolation, filters and cursor semantics.
- RPC review and reorg replay queues use concurrent partial expression indexes keyed by chain
  and recovery epoch; the superseded review index is removed. Normal Deploy migration retries
  verify and rebuild interrupted invalid queue indexes without database shell access; concurrent
  index work runs after the atomic payment-settings cutover transaction.
- Global request overload returns the shared JSON `503 unavailable` error with `Retry-After: 1`,
  request identifiers and tenant cache protection.
- OpenAPI includes shared merchant `403` and merchant/admin `503` errors, documents required
  `429` and optional `503` retry delays, and describes tenant response cache protection.

### Fixed

- A failed RPC metrics refresh retains the previous snapshot and allows recovery to continue.

- RPC acceptance and readmission now retry only transient failures within explicit probe attempt
  and deadline bounds, honoring throttling delays. Capability checks share one finalized snapshot
  so redundant tagged reads cannot spuriously reject a load-balanced endpoint's regressing head.
  Persisted head floors, chain/genesis identity and contract validation remain mandatory.
- `topup rpc check` reserves stdout for JSON and reports sanitized member/probe failure reasons
  on stderr, which the Deploy preflight now surfaces. A capability probes require an unsplit
  address-less 2 000-block log window; B probes verify recent addressed logs.
- Staging has keyless Sentio A backups on both chains and ethPandaOps B on Sepolia, with
  endpoint/operator evidence, distinct company domains and shared quota budgets. Base Sepolia B
  stays singleton after rejecting Coinbase's pruned genesis and removing Pocket's unverifiable
  Supplier independence; the Phala Cloud template routes are unchanged.
- Member probes reject same-height snapshot/anchor/read hash conflicts and persistently freeze
  the chain when finalized validation detects a conflict, including early snapshot rejection.
  Owner recovery uses fresh full-probe deadlines for each member and an independent full
  operation deadline for anchor agreement, allowing healthy low-rate providers to complete
  route verification.

### Reference product

- Replace the HTTP development server with pinned Starlette and Granian, bounded connections
  and workers, streamed body limits, request deadlines, uniform application transport errors,
  and graceful shutdown. Bound worker-accepted connections to 32 with a five-second total
  header-read deadline after acceptance; excess connections wait in the OS listen backlog
  configured at 128 (Granian's minimum), rather than entering the worker. Outbound SDK calls share a 25-second operation deadline without automatic
  retries; a single supervised worker has a 35-second shutdown limit within the container's
  45-second grace period. The demo forwards TanStack Query cancellation to deadline-bound fetches
  and uses a native network select to avoid dynamic inline scrollbar styles.

### Security

- Tenant and credential HTTP responses, including public payer views, idempotent replays,
  authentication errors and restore rejections, send `Cache-Control: no-store`.
- **Breaking:** live reference products now require pre-verified pinned webhook keys; unpinned
  attestation key fetches remain available only in test mode.
- Price-provider responses are bounded while streaming, and API ingress and service containers
  have bounded resource and privilege exposure.
- Database sessions use role-specific time budgets; service shutdown bounds task draining,
  advisory-lock cleanup, and pool closure, including read-only restore instances.

### JS SDK (`@phala/pay`)

#### Changed

- **Breaking:** React components require an explicit `@phala/pay/styles.css` import. Removed
  `appearance.variables`; set the `--pp-*` custom properties in your stylesheet. Components no
  longer inject inline styles, allowing `style-src 'self'` in integrations. The demo permits
  only its own CSS and the exact hash of Radix's fixed scrollbar stylesheet.
- Public reads accept a composable `AbortSignal` and a request deadline (10 seconds by default).
  **Breaking:** Node.js 20.3 or later is required for the standard `AbortSignal.any` API.
  Sessions and React components abort active reads on destruction or unmount.

#### Fixed

- Checkout calls `onSuccess` after a local expiry followed by credit, once per quote and
  notification type. DepositAddress uses native keyboard-accessible network/token radio groups.

### Python SDK (`phala-pay`)

#### Changed

- Regenerate the client with shared forbidden and unavailable error responses.

## [0.7.0] - 2026-10-02

### Changed

- **Breaking:** RPC configuration now requires explicit `chain.rpc_groups: { a: ..., b: ... }` and the
  `rpc_groups`, `rpc_companies` and `rpc_budgets` registries. The old provider list/flat registry
  is rejected, including a third id that 0.6.0 silently ignored. Reviewed provider companies
  must be disjoint between A and B; repeated URL templates with different sealed credentials
  are allowed within a group.
- RPC groups select in-process with bounded failover or weighted round robin, shared account/key
  admission, refused redirects and no cache. Every real member send retains the existing usage
  counter. Fixed numeric log windows commit evidence, review coverage and progress atomically.
- **Breaking:** migrate the database before starting 0.7.0: persisted hash watermarks, configuration acceptance,
  historical review coverage and audited recovery are required. Legacy height-only cursors need
  an A/B-agreed hash anchor. Rollback requires the stopped-service recovery procedure in
  [the RPC runbook](deploy/RPC.md); configuration rollback never lowers a watermark.
- RPC company aliases additionally use pinned PSL registrable-domain evidence; accepted A/B
  role/group ids cannot be renamed or swapped. Receipt/head/nonce evidence is member-pinned,
  and typed decoding participates in failover. Dense-window deadlines account for real
  verification work, recursive log splits, factory checks and shared quotas; pinned sends
  retain the operation deadline. Same-height latest/safe reorgs replay through the production
  poll, recording deposits below prior confirmation progress without skipping unread ranges.
  Recovery probes isolate member heads and validate existing deposit and flush ledgers.
  Singleton historical replay and nonfinal reorg replay
  retain durable progress; authorized numeric-watermark recovery isolates old branch evidence
  in its audit epoch.

## [0.6.0] - 2026-10-02

### Added

- Per-account payment settings, `GET` and `POST /v1/payment_settings` (docs/integration.md §1.9,
  docs/design/payment-settings.md): per mode, the chains and assets the account accepts, a stricter
  `confirmations` per chain, and its terms per asset (quote window, spread, two-sided tolerance,
  minimum credit, deposit bounds, refund floor) within the operator's bounds, plus one customer's
  quote creations per minute. `chains`, when sent, replaces the list; writes are last-write-wins.
  `available` lists the operator's catalog of the mode with its defaults, bounds, and each chain's
  status. Each change has a new `revision` and is announced as `payment_settings.updated`, an
  account event delivered to every endpoint. Reading needs `account.read`; writing needs
  `account.write`, which a restricted key never holds.
- `rejected` deposits carry `rejection_reason: "asset_not_accepted"` for a routed token the payment
  settings the deposit is bound to do not accept; it is refundable like any rejected deposit.
- Error codes `asset_not_accepted` (a quote or deposit address of an asset the account does not
  accept) and `payment_settings_unconfirmed` (after a service restore, until the merchant
  reconfirms its settings).
- A quote carries `terms`, the terms it was issued with, which it keeps whatever the settings say
  later. `GET /v1/config` adds `quote_creations_per_customer_per_minute`, and each asset
  `min_deposit_atomic` and `quote_amount_decimals`.
- Admin: `GET /v1/admin/accounts/{account}`, with each mode's payment settings and the `legacy`
  revision the 0.6.0 cutover bound its earlier deposits and quotes to, and
  `POST /v1/admin/recording/resume`, which lifts the 0.6.0 cutover's recording hold. The admin
  deposit view names the deposit's `settings_revision` or `settings_hold`.

### Changed

- **Breaking:** an account accepts nothing until it configures its payment settings. A treasury no
  longer enables a chain by itself, and a routed asset is no longer accepted by default: quotes and
  deposit addresses answer `400 asset_not_accepted`, `GET /v1/config` lists no asset, and a
  payment to an existing address is `rejected(asset_not_accepted)` until the merchant lists what it
  takes. `GET /v1/config` is the account's effective payment config: the accepted assets on chains
  with a treasury, with the account's terms.
- **Breaking:** a deposit is governed by the payment settings current when it is recorded, or, for
  a valid payment of a quote, by the quote's terms; a change applies to deposits recorded after it.
  A deposit waits for the stricter of its bound confirmation and the chain's current floor; a
  stricter confirmation no longer applies to deposits already recorded. A payment the receipt
  corrects to the quote's asset waits for the quote's confirmation too.
- A chain's confirmation floor is that of its current route versions: a new version may raise it,
  and an earlier version keeps its own value only for the terms of what it governed. A quote's
  `terms` are always the stored ones, `confirmations` included; a deposit's refund floor, when no
  accepted terms govern it, is the default of the route version it was recorded on. An unloaded
  bound version fails closed rather than using the current default.
- **Breaking:** route files replace `quote:` and `limits:` with a `merchant:` section of each
  term's operator default and bounds (`quote_ttl_seconds`, `quote_spread_bps`,
  `quote_tolerance_bps`, `min_amount`, `min_deposit_atomic`, `max_deposit_atomic`,
  `min_refund_atomic`), with code ceilings; quote amount decimals become
  `asset.quote_amount_decimals`; `quote.max_creations_per_minute` is removed (now per account and
  mode, default 10, at most 60). Every environment's route files are converted.
- **Breaking:** after a restore from backup, every account's payment settings are `held` until the
  merchant sends its complete configuration with `POST /v1/payment_settings` (`chains` required,
  and nothing of the restored settings carried over); deposits recorded meanwhile wait, and the
  reconfirmation binds them. No imported delivery lifts the hold. A delivered outcome stands once
  the chain shows the transfer it names (account, transaction, recipient, token, sender, and
  amount; otherwise the deposit is held for reconciliation): a rebuilt deposit with an imported
  `deposit.rejected` stays rejected, and a delivered credit is never rejected or bounded. A
  sanctions hit on a delivered credit keeps the credit, raises `TopupDeliveredCreditSanctioned`,
  blocks refunds (including pending refunds from being marked paid or completing verification),
  and keeps its forwarder out of `GET /v1/forwarders?sweepable`. Delivered rejection identity
  contradictions also appear in `GET /v1/admin/restore` for reconciliation.
- **Breaking (operators):** the upgrade to this release is a cutover (docs/architecture.md §14):
  `topup migrate --config FILE` binds every existing deposit and quote to the 0.5.0 model in one
  transaction with the schema change, serialized through commit by an advisory lock, after loading
  the configuration (the compose's `migrate` service mounts it), `topup run` refuses to start before
  it, and recording stays held until the accounts are configured and
  `POST /v1/admin/recording/resume`.

### Removed

- **Breaking:** `confirmation_policies` (the `GET /v1/account` field) and `POST /v1/account`: a
  chain's stricter confirmation is part of the payment settings.

### Python SDK (`phala-pay`)

#### Added

- `pay.payment_settings.retrieve()` and `.update(chains=, quote_creations_per_customer_per_minute=)`
  (`TopupClient.get_payment_settings`, `update_payment_settings`), and the generated
  `PaymentSettingsObject`, `UpdatePaymentSettingsRequest`, `QuoteTerms`, and catalog models. The
  account export writes `payment_settings.json`. `PaymentSettingsStatus` and `RejectionReason`
  name the documented values.

#### Removed

- **Breaking:** `pay.account.update(confirmation_policies=)` and `TopupClient.update_account`,
  the generated `ConfirmationPolicy` and `UpdateAccountObjectRequest` models, and
  `AccountObject.confirmation_policies`.

## [0.5.0] - 2026-10-01

### Added

- The customer's view of a deposit address (`GET /v1/deposit_addresses/{id}?client_secret=…`,
  `ClientDepositAddress`) carries `typical_credit_seconds` on each network: the typical time from
  paying to the credit at the account's confirmation for that chain, as `GET /v1/config` and the
  payer's view of a quote report it, so the page no longer assumes Ethereum's 30 seconds (Base's
  depth 3 is about 7 seconds, a `safe` policy about 5 minutes, a `finalized` policy about 15).
  It is required: a chain's routes share one confirmation floor, so every network has one value.

### Changed

- **Breaking:** the service and both SDKs share one version from this release, released together
  by one `v<version>` tag: `@phala/pay` and `phala-pay` go from 0.4.0 to 0.5.0. Use the SDK
  version equal to your operator's service version (docs/integration.md §5.9); the SDKs' earlier
  releases stay in their frozen changelogs in `sdk/js` and `sdk/python`.
- OP-stack chains (OP Mainnet, Base, Base Sepolia, OP Sepolia) credit at a depth on the sequencer's
  unsafe head, as Ethereum does, by owner decision (design D1): the family default is 3 blocks,
  about 7 seconds after paying (`GET /v1/config` reports `confirmations: "3"` and
  `typical_credit_seconds: 7`), instead of `safe`, about 5 minutes. This is a product-risk
  choice: Base reports a single reorged L2 block ever, and none after batching to Ethereum. As on
  Ethereum, the per-account `max_unfinalized_credit` cap bounds credit that is not final, and the
  finality watch sends `deposit.reversed` when a reorganization proves the payment replaced (its
  nonce spent by another transaction, or another transfer at its receipt position at finality). A
  payment removed with its nonce unspent is not reversed: it stays credited and not final, holding
  its share of the cap, and raises `TopupDepositPendingAfterReorg` after an hour. Staging's Base
  Sepolia routes credit at depth 3.
- Every route of a chain must resolve to the same `chain.confirmations`, defaults included: a
  route set whose routes of one chain disagree is refused at load and by `topup config check`.
  Before, the first loaded route's value silently governed the scanner and the confirm step while
  the API could report another route's.
- A route's `chain.confirmations` and an account's `confirmation_policies` accept a depth on an
  OP-stack chain; `safe` and `finalized` remain as stricter values. A policy may be any depth
  deeper than the route's, `safe` (OP-stack only), or `finalized`: from weaker to stricter, a depth,
  `safe`, `finalized`. A value the chain's family does not accept, such as `safe` on Ethereum, is
  `400` with its own message.
- The head loop polls a chain whose route credits at a depth once per block of its family: every
  2 s on an OP-stack chain, 12 s on Ethereum (`--head-poll-interval-s` still overrides it). An
  OP-stack chain crediting at a depth takes about six times the head polls and per-block log
  requests on provider A of one crediting at `safe` (deploy/README.md, "Measuring RPC usage").
- **Breaking:** `deploy.sh` generates the admin key with the Python SDK of its own release, which
  `scripts/version.sh` pins with the other version files, instead of `phala-pay==0.3.0`. A
  pre-release publishes no SDK, so its `deploy.sh` needs `TOPUP_ADMIN_PUBLIC_KEY`.

### Removed

- **Breaking:** Every backward-compatibility path for an earlier release, in the service, the SDKs,
  the demo, and the documentation. An SDK works only with the service release of its own version
  (integration guide §5.9, "Compatibility"); no other pairing is supported.

### JS SDK (`@phala/pay`)

#### Added

- `ClientDepositAddress.networks` (`parseClientDepositAddress`, `retrieveDepositAddress`): each
  network's `chain_id`, `address`, and `typical_credit_seconds`, the typical credit time at the
  account's confirmation on that chain. `ClientDepositAddressNetwork` is exported.

#### Removed

- **Breaking:** Support for a service of another version: `parseClientDepositAddress` requires each
  network's `typical_credit_seconds`, so it refuses a v0.3.5 service's view of a deposit address.

#### Fixed

- `<DepositAddress>` told the payer "usually in about 30 seconds" on every network. With
  `clientSecret` and `apiBase` it now states each network's typical credit time from the address's
  public view ("usually in about 30 seconds on Sepolia and about 7 seconds on Base Sepolia"; 15
  minutes under a `finalized` policy). Before the view is read it names none: "credited at the
  market rate once it is confirmed on its network".

### Python SDK (`phala-pay`)

#### Added

- `ClientDepositAddressNetwork.typical_credit_seconds` (`topup_client`): the typical credit time at
  the account's confirmation on the network's chain.

#### Removed

- **Breaking:** Support for a service of another version: `ClientDepositAddressNetwork` requires
  `typical_credit_seconds`, so it refuses a v0.3.5 service's view of a deposit address.

## [0.3.5] - 2026-10-01

### Added

- Staging adds USDT on both chains: `phala-cloud-sepolia-usdt-usd` and
  `phala-cloud-base-sepolia-usdt-usd`, for Aave's testnet USDT (Sepolia
  `0xaA8E23Fb1079EA71e0a56F48a2aA51851D8433D0`, Base Sepolia
  `0x0a215D8ba66387DCA84B284D18c3B4ec3de6E54a`, 6 decimals; Tether publishes no testnet USDT, and
  Aave's faucet contracts mint it to anyone, which the demo's mint button does from the visitor's
  wallet), valued at one dollar (pricing mode `stablecoin`, guarded by Coin Metrics' `usdt`
  reference rate) with no quote spread, as USDC is. `GET /v1/config` lists it beside PHA and USDC,
  and a deposit address takes all three tokens.
- `examples/phala-cloud-usdt.yaml`, a mainnet route template for Tether's USDT on Ethereum
  (`0xdAC17F958D2ee523a2206206994597C13D831ec7`), and the runbook
  `deploy/runbooks/usdt-issuer-controls.md` for Tether's fee switch and blacklist: a blacklisted
  forwarder still receives and is credited but cannot be flushed, a blacklisted treasury still
  receives flushes, and a nonzero fee makes every sweep deliver less than it credited.

### Changed

- Compliance is each operator's responsibility: Phala Pay is software, and beyond the service's
  direct sanctions screening, KYC, KYT, and the Travel Rule are for each operator and its merchants
  to handle. The legal review that was to gate live mode for third-party merchants on Phala's
  instance is removed from the design (D12, former §17), the architecture, and the plan: Phala's
  instance serves only Phala Cloud.

## [0.3.4] - 2026-10-01

### Added

- The payer's view of a quote (`GET /v1/quotes/{id}?client_secret=…`, `ClientQuote`) carries
  `amount_credited`: while `payment_status` is `credited`, what the payment credited in cents (the
  deposit's `amount`), which differs from the quoted `amount` for a payment valued at spot (another
  amount, or paid late); otherwise `null`. It also carries `typical_credit_seconds`, the typical
  time from paying to the credit at the account's confirmation for the quote's chain, as
  `GET /v1/config` reports it, so the page no longer assumes Ethereum's 30 seconds.

## [0.3.3] - 2026-10-01

### Fixed

- A provision completes on Phala Cloud. v0.3.1 and v0.3.2 waited for a new CVM to boot by its
  `instance_id` in `cvms get`, which Phala Cloud reports as `null` in practice, so the one-command
  deploy's quick start timed out after 15 minutes with nothing printed, though the CVM was healthy,
  and Deploy's provision timed out the same way. `deploy/phala-cvm.sh wait --unsealed` now waits
  only until the CVM is settled with the new compose, in any status; that the CVM booted the compose
  is the attestation's to show, and the instance id comes from its event log's `instance-id` event
  (`deploy/phala-cvm.sh instance-id`), as Deploy's DNS records already did. The quick start waits
  for no instance id, as it is served at its gateway domain; a custom domain waits for the
  attestation of its gateway's compose and reads the TXT record's instance id from it.
- The one-command deploy binds a custom domain's gateway upgrade to the compose it rendered, as
  Deploy does: the attested app-compose must carry that compose and the kit's pre-launch script,
  and both the compose hash the API reports and the event log's must be that app-compose's hash,
  so another update's compose fails instead of being accepted as "a new hash". The binding is
  `deploy/attested-compose.sh`, which `deploy/verify-attestation.sh` now uses too; it also requires
  the event log to name that hash exactly once.
- The one-command deploy prints the CVM id, the URL, and how to finish or remove the CVM on every
  exit once the CVM exists, Ctrl-C included, including a step that fails or times out later.
- The one-command deploy refuses an instance name the Phala Cloud workspace already has, before
  writing anything (an admin seed included), with how to find, finish, or remove that CVM: its
  recorded `cvm-id` is a file in the directory it ran from or the chosen environment directory,
  which a run from elsewhere cannot see. It fails closed: a CVM list it cannot read as one complete
  page of CVMs (page 1 of 1, or of 0 for no match; every match on it, its total the items' count;
  each item's name and app id) is refused, never read as "no such name".
- The one-command deploy writes the sealed env file on tmpfs (`$XDG_RUNTIME_DIR`) where the session
  has one, mode 0600, and shreds it (where `shred` exists) and removes it on every exit.
- The one-command deploy generates an admin key with the Python SDK pinned to `phala-pay==0.3.0`,
  the version this release documents, as does the guide's `uvx` command; its messages name the
  release's locked CLI (`kit/deploy/phala`) and link the guide at the release's tag.

## [0.3.2] - 2026-10-01

### Added

- A one-command deploy, `curl -fsSL https://pay.phala.com/deploy.sh | bash`
  (docs/self-hosting.md, "One-command deploy"). Each release publishes `deploy/deploy.sh` as its
  attested asset `deploy.sh`, set to deploy that release and listed in `SHA256SUMS`, which
  `deploy/verify-release.sh` verifies; pay.phala.com redirects `/deploy.sh` to the latest
  release's asset and `/deploy/v<version>.sh` to that release's. The command is an HTTPS bootstrap,
  trusting pay.phala.com and Cloudflare as well as GitHub; the guide's high-assurance path verifies
  the script's attestation first. From the owner's machine it verifies the release (with the
  GitHub CLI 2.101 or later as Deploy does, otherwise against `SHA256SUMS` only, which it says
  proves no provenance; `--strict` or `PHALA_PAY_REQUIRE_ATTESTATION=1` requires the GitHub CLI),
  asks for the settings and secrets or reads them with `--non-interactive` from the environment,
  and provisions either the Phala Cloud template variant
  (a testnet quick start at the gateway domain) or the service variant on a custom domain, with
  the kit's render, preflight, route-mode check, locked CLI, and pre-launch script. It seals the
  secrets at provision from a mode 0600 file in a temporary directory it removes, prints none, and
  writes a generated admin seed only to the file the owner names. The Phala Cloud CLI runs in an
  empty directory, so a `phala.toml` in the caller's directory cannot turn the new CVM into an
  update of another; the new CVM's id is recorded (`cvm-id`), and a rerun creates no second CVM.
  It prints the CVM id, the URL,
  a custom domain's DNS records, and the acceptance steps: a provision proves nothing about the
  instance's health.
- `deploy/preflight.sh --template` checks a Phala Cloud template compose and its env file, with the
  deploy form's values.
- Preflight refuses an env file value with a `#`, a quote, or surrounding whitespace: the Phala
  Cloud CLI reads the file as dotenv does and would seal another value (`alpha#bravo` as `alpha`).
- SDK releases are never marked Latest, and stable service releases always are, so
  `releases/latest` (and `https://pay.phala.com/deploy.sh`) is the service's; the Release workflow
  checks that the URL serves the new release's `deploy.sh`.

### Fixed

- `deploy/verify-attestation.sh` accepts a CVM whose `allowed_envs` leaves out a sealed name, such
  as the optional `SENTRY_DSN`. The Phala Cloud CLI, and the template's deploy form, put in
  `allowed_envs` only the names of the env they send, so a template CVM deployed without a Sentry
  DSN failed verification, which required exactly the compose's sealed names. `allowed_envs` must
  now name only sealed names of the compose (`compose-policy.jq`, `allowed_envs_violations`): any
  other name, the template's `DSTACK_APP_DOMAIN` included, is still refused. A name left out is
  unset. Preflight applies the same rule to the env file, and still refuses a required secret that
  is empty or missing.
- The postgres-walg entrypoint refuses S3 storage unless both `AWS_ACCESS_KEY_ID` and
  `AWS_SECRET_ACCESS_KEY` are set, on every start of PostgreSQL and the backup job. Only an empty
  data directory listed the prefix, and `/healthz` reads only PostgreSQL, so a restarted CVM whose
  storage credentials were unset served without archiving and passed the acceptance upgrade.

## [0.3.1] - 2026-10-01

### Fixed

- Deploy declares its one secret, `PHALA_CLOUD_API_KEY` (optional), so that an operator's
  repository in another organisation can pass it from a repository secret: GitHub supports
  `secrets: inherit` only within an organisation or enterprise, and without it a called workflow's
  Environment secret resolves empty (actions/runner#4453). v0.3.0 read only the Environment
  secret, so every caller passing no secrets failed with an empty key. A caller in Phala-Network's
  organisation passes `secrets: inherit` and keeps the key as the Environment's secret. An empty
  key now fails with the setup each caller needs (docs/self-hosting.md, step 5, with the trade-off
  of a repository secret).
- Deploy runs when called at a release's annotated tag, not only pinned to its commit. A workflow
  called at an annotated tag runs with `job.workflow_sha` the tag object's SHA, so v0.3.0's Deploy,
  called at `v0.3.0`, refused its own release ("Deploy runs at e42120e…, not v0.3.0's commit
  659bd26…"). `deploy/verify-release.sh VERSION DIR CALLED_AT` peels the tag to its commit and
  accepts either SHA. Pinning the commit SHA remains the recommendation.
- Deploy's provision completes on the unsealed CVM it creates. Without the storage credentials
  PostgreSQL refuses to start, so app-compose fails by design and Phala Cloud shows the CVM as
  `error`; provision waited for `running` and timed out after 15 minutes, before the DNS records and
  the sealing step. A provision now only creates the CVM: it waits until the CVM is settled
  (`in_progress` false) with the new compose and has booted (an instance id), in any status
  (`deploy/phala-cvm.sh wait --unsealed`), and redeploys the gateway's compose without the CLI's
  `--wait`. It proves nothing about the CVM's health: the upgrade after the sealing is the acceptance
  step, and still requires `running`, `/healthz`, the attestation, and the certificate evidence.
- An upgrade retries the certificate evidence every 30 seconds for at most 10 minutes instead of
  failing at once: right after a DNS change the domain can still reach the previous instance, or the
  new one can still be obtaining its certificate. `deploy/verify-ingress-evidence.sh` bounds its TLS
  handshake to 30 seconds.
- Deploy names a new CVM after its run (`phala-pay-<environment>-<run id>`,
  `phala-pay-staging-product-<run id>`): Phala Cloud refuses a duplicate name (`ERR-01-004`), so a
  provision beside a stopped CVM kept for rollback, as in a reset, failed. A CVM is identified by
  its id variable, never by its name.

## [0.3.0] - 2026-10-01

### Added

- Versioned releases. A `v<version>` tag of a `main` commit runs CI, then publishes the images to
  GHCR, built on GitHub-hosted runners with pinned Buildx and BuildKit, each with a GitHub build
  provenance attestation (`phala-pay` and the reference product are reproducible; `postgres-walg`
  has provenance only), and a GitHub release whose assets are `images.json`, the deploy kit
  `phala-pay-deploy-v<version>.tar.gz` (`git archive` of `LICENSE`, `deploy/`, and `docs/`), the
  Phala Cloud template's compose `phala-cloud-template.yml`, and `SHA256SUMS`, each attested.
  `deploy/verify-release.sh` verifies a release: its commit in `main`'s history, the checksums,
  and every asset's and image's provenance for that commit.
- Self-hosting without a fork: an operator's repository holds only its environment directory and
  a workflow that calls Deploy at a release (`docs/self-hosting.md`); Deploy runs the release's
  verify-release.sh and requires its own commit to be the release's. The kit's `deploy/phala`
  runs the Phala Cloud CLI with its dependencies locked (`deploy/tools`).
- The environment's `compose.yaml` may set only its documented settings (WAL-G's location,
  dstack-ingress's `DOMAIN`, the RPC key names): `render.sh` refuses any other change, and every
  rendered image must be the release's or one the kit pins.
- Deploy sends the kit's reviewed pre-launch script (`deploy/phala-cloud-pre-launch.sh`, Phala
  Cloud's v0.0.20) on provision and upgrade, and `verify-attestation.sh` requires exactly it.
- The Phala Cloud template variant (`deploy/render.sh --template`): Phala's staging routes on a
  one-click testnet instance served at the app's gateway domain. Exactly the deploy form's values
  come from the CVM's env and are unattested: the admin public key and the origin's host, which
  `topup run` reads with `--admin-public-key-env` and `--public-origin-host-env` when the
  configuration leaves them out, and the backup location. A template instance has no restore-check
  path.
- postgres-walg starts PostgreSQL or a backup only with WAL-G's file backend or all three S3
  settings well formed: `WALG_S3_PREFIX` a valid bucket, `AWS_ENDPOINT` an `https` origin with a
  valid host and port, and `AWS_REGION`.
- Deposits carry `receipt_log_index` and `revision`, the position and revision their `id` is
  derived from, and `block_hash` and `block_time` (Unix seconds), in the object and every
  `deposit.*` snapshot.
- Admin: `POST /v1/admin/restore/treasuries/apply` applies again, while frozen after a restore, a
  treasury change that applied after the restore point, from the merchant's delivery of its
  `treasury.updated`: only a delivery the service signed, of the restored pending change becoming
  `active`, whose time-lock ended, and screened again when screening answers (a treasury a
  sanctions list names now is refused). It applies at the event's `created`, audited in the same
  transaction, and its events are not sent again. `POST /v1/admin/restore/treasuries/verify`
  reports such a change as `application_lost`, and the treasury it replaced, received `replaced`,
  as `replacement_lost` when that change is sent in the same request as `active` (both were
  `differs`); both are `matches` once the change is restored. A change on a chain without a current
  route stays pending, as the time-lock leaves it.
- Admin: each result of `POST /v1/admin/restore/events` carries `reversed_deposit` for a
  `deposit.reversed`: `restored`, `recorded`, `address_unknown`, or `rescanned` (the rescan recorded
  its position first; also a finding status of `GET /v1/admin/restore`).
- Deposits carry `replaces` and `replaced_by` (`dep_…` or `null`), in the object and every
  `deposit.*` snapshot: a deposit recorded for the transfer that took a reversed deposit's receipt
  position after a reorganization names that deposit, and the reversed one names it (see Fixed).
  Both are `null` when the other deposit is in another account or mode.
- Webhook delivery honors a receiver's `Retry-After` on `429` and `503`, in seconds or as an HTTP
  date: the retry waits at least that long, at most an hour.
- Admin: `POST /v1/admin/deposits/{id}/nudge` answers `400 deposit_unexpected_state` for a deposit
  the pump does not process (anything but `detected` or `confirmed`); it was a silent no-op.
- Admin: `POST /v1/admin/restore/quotes` re-issues a quote given out after the restore point from
  the merchant's record of it (the address must be the one its `qt_` id derives over the current
  treasury), backfilled from the restored cursor so a payment made to it is found. Its terms are
  the merchant's record, kept but never applied: a payment to it is credited at spot unless an
  imported, signed `deposit.credited` carries its credit, and its `expires_at` is the restore's
  detection at the latest. A `client_secret` the service issued for the quote is kept, so the
  payer's page reads it again; `POST /v1/admin/restore/deposit_addresses` takes one too.
- Integration guide §2.3, obligation 6: keep each webhook delivery as your receiver got it, once
  per `webhook-id`, in the transaction that applies it: the raw body bytes and the `webhook-id`,
  `webhook-timestamp`, and `webhook-signature` headers. After a service restore only a delivery
  the service signed is imported (§5.12), so a parsed or re-serialized event cannot prove a credit;
  keep every quote and deposit address response whole too, its `client_secret` stored like a
  credential.
- The reference product (`deploy/product`) keeps a webhook inbox (each verified delivery as
  received, with its processing state, committed with its ledger effect; a redelivery with another
  body keeps the first and is logged) and each quote and deposit address response it gets, client
  secret included, in its ledger, now mode 0600.
  `python -m reference_product export-restore-records` (from the ledger, read-only) or
  `fetch-restore-records` (from its account API, `GET /accounts/restore-records`, signed with the
  driver key) prints them as the bodies of `POST /v1/admin/restore/treasuries/verify`,
  `/treasuries/apply`, `/deposit_addresses`, `/quotes`, and `/events`; `--since` keeps what was
  created or recorded from five minutes before the restore point on. The controlled restore drill runs the product's receiver and uses
  only what it exports, not events the drill made up; it now also restores a treasury change lost
  with the restore, an unvalued reversed deposit, and a reorganized deposit's revisions.
- Admin: `GET /v1/admin/attestation?account=&livemode=&nonce=` returns `GET /v1/attestation` of any
  account and mode, so the operator verifies a restored instance, where merchant keys are refused.
- Admin: `POST /v1/admin/restore/unfreeze` requires `quotes_reissued`.
- Admin: `POST /v1/admin/restore/delivered_credits/discard` releases a deposit held because its
  transfer contradicts the delivered event imported for it (a `contradicted` finding of
  `GET /v1/admin/restore`); the deposit is then valued from the chain.
- Deposits carry `final_at` (Unix seconds; `null` until `final`), when the finality watch found
  the deposit's block final, in the object and every `deposit.*` snapshot.
- A path or method the API does not serve answers `404 resource_missing` with the error object
  (it had an empty body). The error object's `doc_url` is optional in the OpenAPI schema, as in
  Stripe's; every error of the service still carries it.

- Staging's route `phala-cloud-sepolia-pha-usd` is version 3 on the deterministic factory
  `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` (implementation
  `0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9`), with `confirmations: 2`.
- Staging adds a second test-mode route, `phala-cloud-sepolia-usdc-usd`: Circle's testnet USDC on
  Sepolia (`0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238`, 6 decimals; <https://faucet.circle.com>),
  valued at one dollar (pricing mode `stablecoin`) with no quote spread, so a $10 quote asks 10 USDC.
  `GET /v1/config` lists it beside PHA, and a deposit address takes both tokens.
- Staging adds Base Sepolia (84532), with the same two test-mode routes:
  `phala-cloud-base-sepolia-pha-usd` (test PHA `0x1a6F260377e42ead1418C7C1afDFD5DE371A9284`, 18
  decimals, public `mint`) and `phala-cloud-base-sepolia-usdc-usd` (Circle's testnet USDC
  `0x036CbD53842c5426634e7929541eC2318f3dCF7e`, 6 decimals, at one dollar with no quote spread), on
  the same factory and implementation. Deposits there are credited at the chain's `safe` head
  (`GET /v1/config` reports `confirmations: "safe"`), typically about 5 minutes after inclusion.

- Launch hardening (docs/design/multi-tenant.md, "launch hardening" amendment):
  - Restricted keys: `POST /v1/api_keys {"type": "restricted", "permissions": [...]}` issues a
    `ppay_rk_{test,live}_` key holding only those permissions (a `write` includes its `read`);
    `api_key` objects gain `permissions`. Restricted keys can no longer be granted `account.write`
    or `endpoints.write`: keys, treasuries, webhook endpoints, webhook keys, and account settings
    need a secret key.
  - `POST /v1/account/webhook_keys/roll`: in live mode `expires_in` is 172800 (48 hours) to 604800;
    `0` is refused there. The default is 172800 (was `0`). The roll's `account.updated` is signed
    by the retiring key too, whenever it is delivered.
  - Treasuries gain `crediting_paused` and `crediting_paused_by`; `POST /v1/treasuries/{id}/pause`
    and `/resume` (secret key), and the admin `POST
    /v1/admin/accounts/{account}/treasuries/{treasury}/pause|resume {reason}`, hold deposits to
    every forwarder over a treasury `pending` without `deposit.credited` until resumed; each change
    is `treasury.updated`.

- Ledger correctness (docs/design/multi-tenant.md, "ledger correctness" amendment):
  - Deposits carry `amount_refunded` (the cents of `amount` succeeded refunds take back, pro rata
    to the refunded tokens, rounded down, cumulative) and `amount_reversed` (`amount` once
    `reversed`), in every `deposit.*` snapshot. Balance rule: a deposit nets to
    `amount - amount_refunded - amount_reversed` while `credited` or `reversed`, 0 otherwise;
    merge snapshots per deposit (the later status and the larger amounts win) whatever the order.
  - A refund with a transaction attached (`mark_paid`) can no longer be canceled
    (`400 refund_unexpected_state`); it is `failed` with the new `failure_reason`s
    `transaction_dropped` (its nonce consumed by another transaction at finality) or
    `transaction_not_found` (never seen within 24 hours), after which a new refund can be
    requested. A deposit's reversal cancels only its refunds without a transaction.
  - `mark_paid` takes `receipt_log_index`, the paying log's position in the transaction's receipt,
    and the refund reports it, replacing the block-wide `log_index`.
  - Per account and mode, the credit of deposits credited but not final is capped
    (`max_unfinalized_credit`, 100 000 cents by default, set by the operator with
    `POST /v1/admin/accounts/{account}`); a deposit past it stays `pending` and is credited once
    final.

- API conformance with Stripe (docs/design/multi-tenant.md, "API conformance" amendment):
  - Business-state failures are `400` (`deposit_not_final`, `deposit_not_refundable`,
    `quote_unexpected_state`, `quote_payment_received`, `quote_window_closed`, `paused`,
    `chain_frozen`, `treasury_not_set`, `treasury_change_pending`, `treasury_unchanged`,
    `treasury_unexpected_state`, `exposure_cap_exceeded`, `deposit_address_cap_exceeded`,
    `webhook_endpoint_cap_exceeded`, `webhook_endpoint_disabled`, `deposit_address_retired`,
    `refund_unexpected_state`, `transfer_already_used`, `api_key_inactive`, `last_api_key`); `409`
    is only `idempotency_key_in_use`. The unused generic `conflict` code is gone. The admin API's
    `signature_replayed` is `401`.
  - A customer's quote-creation and deposit address rotation limits are `429 customer_rate_limit`
    (was `rate_limit`); every `429` carries `Retry-After`.
  - Every response carries `Request-Id: req_…`, replacing `x-request-id`.
  - An `Idempotency-Key` saves the result of every request that started executing, `500`s
    included, and replays it; a request that failed validation (`parameter_*`), was rate limited,
    or met `503 unavailable` is not saved. Validation failures were saved before, `500`s were not.
  - Event `data.object` is rendered in the transaction of the change, not at the first delivery or
    read, and never changes.
  - Treasury events are `treasury.created` (every proven treasury, pending or at once active),
    `treasury.updated` (a pending treasury took effect, or was replaced), and `treasury.canceled`,
    replacing `account.treasury.pending|updated|canceled`; stored events and endpoints'
    `enabled_events` are renamed.
  - A quote's address salt is `keccak256(abi.encode(account, client_reference_id, "quote",
    quote_id))` (was tagged `"lock"`).
  - Admin paths: `POST /v1/admin/reconciliation_blocks/{block_key}/lift` and
    `GET /v1/admin/reports/daily` (were `reconciliation-blocks` and `report/daily`); a route pause
    names its path parameter `{route}`.
  - The OpenAPI document `openapi.json` is the merchant API only; the admin API is
    `openapi.admin.json` (also served at `/openapi.admin.json`). Each object's `object` is a
    single-value enum.

- API vocabulary (docs/design/multi-tenant.md §16 PR 10, and the Stripe-conventions audit): the
  merchant's customer is `client_reference_id` everywhere (`POST /v1/quotes`, quotes, deposits,
  `GET /v1/deposits?client_reference_id=`, and the admin path
  `/v1/admin/accounts/{acct}/customers/{client_reference_id}/pause|resume`).
- Deposit `status` is `pending` (recorded at the route's confirmation, being valued and
  screened, or held by a `settlement` pause), `credited`, `rejected`, or `reversed`; the new
  booleans `final` (its block is final) and `swept` (a finalized `Flushed` event after it moved its
  forwarder's balance) replace the `detected`, `confirmed`, and `swept` statuses. The status filter
  takes the new values.
- A quote's `payment` is the shared `Payment` object: `status` `seen` or `recorded` (was `final`),
  with `chain_id` and `asset`; `matches_quote` is `null` on a deposit address.
- `GET /v1/attestation`'s `quote` is `tdx_quote` (and so is `topup attest`'s output).
- `GET /v1/admin/deposits/{id}` returns the `Deposit` with an `admin` object (`state`, route,
  `transitions`, `events`); the separate admin deposit shapes are gone.
- `livemode` and `metadata` are required on every object in the OpenAPI document.
- `ForwarderFactory` bounds the gas of every call a target makes (`BALANCE_OF_GAS` 30 000 for a
  token's `balanceOf`, `FLUSH_GAS` 200 000 for a forwarder's `flush`), so a token whose
  `balanceOf` reverts, burns gas, or returns short data, or a transfer or treasury hook that burns
  gas, emits `FlushFailed` for its own target only; a `flush` whose gas cannot cover a call's
  whole bound reverts with `InsufficientGas`. The factory and implementation addresses change
  (`deploy/CONTRACTS.md`).

- Events carry `request: {id, idempotency_key}` (the request that caused them; `null` for the
  service's workers), and every `*.updated` event `data.previous_attributes`. New events
  `refund.created`, `refund.updated` (marked paid, canceled, succeeded, failed, metadata), and
  `quote.canceled`.
- Delivery health: webhook endpoints report `pending_deliveries`, `oldest_pending_at`, and
  `last_attempt {at, status_code}`; `GET /v1/events` takes `delivery_success` and `types[]`; the
  admin daily report lists `failing_webhook_endpoints` older than `failing_for_hours` (24).
- Error objects carry `doc_url`, the code's section of the API reference
  (<https://phala-network.github.io/phala-pay/>), built with Redoc from `openapi.json` and
  published from `main`.
- `GET /v1/api_keys` and `GET /v1/treasuries` take `limit`, `starting_after`, and `ending_before`;
  `GET /v1/deposits` takes `created[gt]` and `created[lt]` beside `created[gte]` and
  `created[lte]`, all compared at whole seconds.
- The OpenAPI documents have `servers`, `tags`, and an example of every object and body.
- Restore mode (docs/design/multi-tenant.md §13, architecture §14): after a restore from backup
  the service is frozen until the operator has reconciled it. Every request with an API key
  answers `503 service_restoring` with `Retry-After: 300`, reads included, and no event is
  delivered and no deposit credited meanwhile. Merchants give the operator their records since the restore point
  (integration guide §5.12). Operators: `GET /v1/admin/restore` and
  `POST /v1/admin/restore/{api_keys/revoke, treasuries/verify, webhook_endpoints/delete,
  deposit_addresses, quotes, events, delivered_credits/discard, unfreeze}`
  (`deploy/runbooks/restore.md`); `treasuries/verify`
  re-applies lost treasury cancellations and the merchant's crediting pauses and resumes. On a restore-check
  instance, writes other than these answer `503 service_restoring` (was `503 unavailable`).
- `POST /v1/account {confirmation_policies}` requires, per chain, a confirmation stricter than the
  route's floor (a depth, `safe`, or `finalized`), applied to every deposit not credited yet;
  `GET /v1/config` reports the effective `confirmations` and `typical_credit_seconds`, and the
  account lists its `confirmation_policies`. `POST /v1/account/pause|resume {scopes: ["quotes"]}`
  pauses the merchant's own quotes and deposit addresses; an operator's pause stays until the
  operator lifts it. Both announce `account.updated`.
- `GET /v1/balance` (per chain and token, unswept and final unswept amounts), `GET /v1/sweeps`
  (finalized `Flushed` events as `sw_…` objects), and `GET /v1/forwarders` (`fwd_…`, every issued
  address with its `factory`, `salt`, `treasury`, `quote` or `deposit_address`, and
  `superseded_at`; `sweepable=<token>` lists only forwarders safe to sweep, never one holding a
  sanctioned deposit or paying a sanctioned treasury).
- `GET /v1/quotes` and `GET /v1/refunds` lists.
- Deposit addresses carry `payments` (the last 24 hours, `seen` within about a block, then
  `recorded`) and each create or rotation returns a `client_secret`; with it and no API key,
  `GET /v1/deposit_addresses/{id}?client_secret=` returns the customer's `ClientDepositAddress`.
- `ClientQuote.livemode`, and `payment_status: "reversed"`.

- Treasuries through the API (docs/design/multi-tenant.md D10, §16 PR 7).
  `POST /v1/treasuries/challenge {chain_id, address}` returns an EIP-4361 `treasury_challenge`
  (`message`, `nonce`, `expires_at`; single-use, 10 minutes, or 24 hours for an address that holds
  code); `POST /v1/treasuries {chain_id,
  message, signature}` sets the chain's treasury in the key's mode when the signature is the
  address's EIP-191 signature, or a contract deployed at the address returns `0x1626ba7e` from
  EIP-1271 `isValidSignature` at `finalized` on both providers (ERC-6492 and undeployed contracts
  are refused; the address is screened for sanctions). `GET /v1/treasuries`,
  `GET /v1/treasuries/{id}`, and `POST /v1/treasuries/{id}/cancel`. A `treasury` (`trs_…`) has
  `chain_id`, `address`, `kind` (`eoa` or `contract`), `status` (`pending`, `active`, `replaced`,
  `canceled`), `effective_at`, `replaced_at`, `canceled_at`, and `cancellation_reason`
  (`requested`, or `sanctioned` when a sanctions list named it at its effective time). Current
  treasuries are screened again daily; a listed one pauses the account's `quotes` and
  `settlement`. Safe owners sign the challenge as a Safe message (EIP-712 `SafeMessage`, the
  Safe{Core} SDK's `signMessage`), or approve it with `SignMessageLib`. A chain's first treasury and
  test-mode changes apply at once; a later live change applies after 48 hours unless canceled.
  New events `account.treasury.pending`, `account.treasury.updated`, and
  `account.treasury.canceled` go to every enabled endpoint of the mode whatever its
  `enabled_events`. When a change applies, the chain's network of every deposit address moves to
  a forwarder over the new treasury; the old address stays credited and pays the old treasury.
  New errors: `treasury_proof_invalid`, `treasury_challenge_expired`, `treasury_challenge_used`,
  `treasury_not_deployed`, `treasury_sanctioned`, `treasury_change_pending`,
  `treasury_unchanged`, `treasury_unexpected_state`, and `treasury_not_set`.
- Quotes carry `treasury`, the treasury their address pays.
- Admin `POST /v1/admin/accounts/{account}/pause` and `/resume` `{scopes, reason}`: pause or resume
  scopes of a whole account in both modes, audited and announced as `account.updated`.
- Webhook endpoints managed by the merchant (docs/design/multi-tenant.md D11, §11, §16 PR 8):
  `POST /v1/webhook_endpoints {url, enabled_events, description?, metadata?}`,
  `GET /v1/webhook_endpoints` (cursor pagination), `GET|POST|DELETE /v1/webhook_endpoints/{id}`
  (`disabled: true|false` disables or re-enables), and `POST /v1/webhook_endpoints/{id}/test`
  (a `webhook_endpoint.test` event to that endpoint only). At most 16 per account and mode
  (`409 webhook_endpoint_cap_exceeded`); `url` is `https` on port 443, or in test mode also `http`
  on port 80. The object carries `livemode`, `url`, `enabled_events` (types or `["*"]`), `status`
  (`enabled`, `disabled`), `disabled_reason` (`gone`), `description`, and `metadata`.
- Events API: `GET /v1/events?type&created[gt|gte|lt|lte]` (a type, or a group such as
  `deposit.*`; cursor pagination), `GET /v1/events/{id}`, and
  `POST /v1/events/{id}/resend {webhook_endpoint}` (the same event to one enabled endpoint;
  `409 webhook_endpoint_disabled` otherwise). Events carry `actor` (`key_…`, `admin`, or
  `system`) in the API and in webhook bodies, and `pending_webhooks` in the API: the audit log.
- Account events `webhook_endpoint.created`, `webhook_endpoint.updated` (with
  `data.previous_attributes`), and `webhook_endpoint.deleted`. Account events (`account.*`,
  `api_key.*`, `webhook_endpoint.*`) reach every enabled endpoint of the mode whatever its
  `enabled_events`, and a changed or deleted endpoint receives the event about itself first, at
  its previous URL.
  Permissions `endpoints.read`, `endpoints.write`, and `events.read` are enforced.

- Deposit addresses (docs/design/multi-tenant.md "Deposit addresses"), restored per the owner's
  2026-09-21 requirement, with one address per customer for every supported token on every chain
  (the owner's 2026-09-28 decision, exchange practice). `POST /v1/deposit_addresses
  {client_reference_id}` returns the customer's active `deposit_address` (`da_…`), issuing it the
  first time and adding a network supported since; `GET /v1/deposit_addresses/{id}`,
  `GET /v1/deposit_addresses?client_reference_id&status` (cursor pagination), and
  `POST /v1/deposit_addresses/{id}/rotate`, which retires it and returns the next version, a new
  address on every chain. The object carries `livemode`, `address` (the address shared by every
  network, or `null` when a network's treasury, and so its address, differs), `version`, `salt`
  (`keccak256(abi.encode(account, livemode, client_reference_id, "deposit_address", version))`, no
  chain or asset), `status` (`active` or `retired`), `created`, `retired_at`, `metadata` (set on
  create by merging, updated by `POST /v1/deposit_addresses/{id}`, carried by rotation, and copied
  to each deposit to the address), and `networks`: per chain of the mode, `chain_id`, `address`,
  `treasury`, and `assets` (`asset`, `contract`, `decimals`, and an EIP-681 `payment_uri` without an
  amount). A transfer of any supported token to an active or retired deposit address is credited
  at spot through the quote pipeline, in about 30 seconds; an unsupported token is rejected as at
  a quote's address. Its deposit has `quote: null` and the new field `deposit_address`, and
  `GET /v1/deposits` filters by `deposit_address`. A treasury change on one chain changes only that
  chain's address; the old one stays credited and its refunds are paid from the old treasury. New
  errors: `409 deposit_address_cap_exceeded` (active addresses per account and mode, default
  100 000 live and 1 000 test), `409 deposit_address_retired`, and `429 rate_limit` past 10
  rotations per customer per hour. New addresses are refused with `409 paused` while `quotes` is
  paused, and with `409 chain_frozen` when every chain is frozen; a frozen chain gets no new
  network.
  Permissions `deposit_addresses.read` and `.write` join the authorization table.

- `POST /v1/account/webhook_keys/roll {expires_in}` (docs/design/multi-tenant.md D11, §16 PR 6):
  the next version of the mode's webhook key signs every delivery, and the current one keeps
  signing beside it for up to 7 days (`0` stops it at once), so each delivery carries one `v1a`
  entry per key. `GET /v1/account` lists the versions as `webhook_keys` (`version`, `expires_at`);
  the roll is announced as `account.updated`. Needs `account.write`.
- `livemode` on quotes, deposits, refunds, and `/v1/config`; events carry `account` (`acct_…`) and
  `livemode`.

- `GET /v1/account`; `GET|POST /v1/api_keys`, `GET|DELETE /v1/api_keys/{id}`, and
  `POST /v1/api_keys/{id}/roll {expires_in}` (the old key works for up to 7 days; `0` revokes it).
- Events `api_key.created`, `api_key.updated`, `api_key.revoked`, and `account.updated`; every
  event records its `actor` (an API key id, `admin`, or `system`).
- Stripe-style `metadata` on quotes, deposits, and refunds (docs/design/multi-tenant.md D15,
  [docs.stripe.com/api/metadata](https://docs.stripe.com/api/metadata)): up to 50 string
  key/value pairs, keys of up to 40 characters without square brackets, values of up to 500
  characters. Set it with `metadata` on `POST /v1/quotes` and `POST /v1/refunds`; update it with
  the new `POST /v1/quotes/{id}`, `POST /v1/deposits/{id}`, and `POST /v1/refunds/{id}`, which
  merge (`""` unsets a key, `metadata: ""` unsets all). Invalid metadata is
  `400 parameter_invalid` with `param` `metadata[key]` or `metadata`. A deposit starts with a
  copy of its quote's metadata, so it arrives in `deposit.credited`'s `data.object`. Objects and
  webhook payloads always carry `metadata` (`{}` when empty); the `client_secret` view does not.
  Secret keys gain `deposits.write`. Do not store sensitive information in metadata.

- Accounts and tenancy (docs/design/multi-tenant.md §14, D13, PR 3). The tenant is an account,
  `acct_…`. Its request signing key id is `{acct_…}/v1`, and the key is live or test: it quotes on
  the routes of its mode and reads only its mode's objects. Another account's object, or the same
  account's object in the other mode, answers `404` like a missing one. Webhook events go to every
  enabled endpoint of the event's account and mode.
- `POST /v1/admin/accounts {name, livemode, public_key, webhook_url}` issues an account and
  `PUT /v1/admin/accounts/{account}` replaces its key and webhook URL;
  `POST /v1/admin/accounts/{account}/customers/{customer}/pause | resume` pauses one customer.
  The admin deposit view carries the deposit's `account` and `livemode`.

- Fast credit and reversal (docs/design/multi-tenant.md §4, D1). A route's
  `chain.confirmations` (a depth, `safe`, or `finalized`, per chain family; default 2 on
  Ethereum L1, `safe` on OP-stack, `finalized` elsewhere) sets when a deposit is credited: at the
  default, `deposit.credited` is sent about 30 seconds after paying instead of about 15 minutes.
  Deposits are watched to finality. A transaction re-included in another block keeps its deposit
  and is followed; one proven dropped (its nonce consumed by another transaction), or whose
  transfer is missing from its final receipt, makes the deposit `reversed` and sends
  **`deposit.reversed`** (event id `uuid_v5(NS, "deposit.reversed:" + deposit UUID)`) when the
  deposit was reported credited or rejected. Claw the credit back as for `deposit.refunded`. A
  quote the deposit completed opens again while its window lasts, otherwise expires with
  `quote.expired`. `confirmations: finalized` keeps the earlier behaviour.
- `GET /v1/config` assets carry `confirmations` (`"2"`, `"safe"`, or `"finalized"`) and
  `typical_credit_seconds`; the admin deposit view carries `receipt_log_index` and `final_at`.
- `POST /v1/refunds` answers `409 deposit_not_final` for a deposit that is not final yet, so
  nothing is paid back for a payment that could still be reversed.

### Changed

- **Breaking**: the SDKs of this release are `@phala/pay` 0.3.0 and `phala-pay` 0.3.0
  ([sdk/js/CHANGELOG.md](sdk/js/CHANGELOG.md), [sdk/python/CHANGELOG.md](sdk/python/CHANGELOG.md)):
  webhook keys only as `whpk_…`, and the event envelope's `actor` and `request`, a deposit's
  receipt position, revision, and block, and a client view's `confirmations` required.
- **Breaking**: `GET /v1/attestation` lists each webhook key's `public_key` in Standard Webhooks'
  form, `whpk_` and the standard base64 of its 32 raw bytes, which `report_data` binds; it was
  lowercase hex, and `standard_webhooks_public_key` is gone. Pin the keys in this form: both SDKs'
  verifiers take no other. `topup attest` prints them the same way.
- **Breaking**: a deposit's `receipt_log_index`, `revision`, `block_hash`, and `block_time` are
  required in the OpenAPI schema, and `POST /v1/admin/restore/events` refuses a deposit event whose
  snapshot lacks them; `reversed_deposit` is never `identity_missing`.
- **Breaking**: Admin: `POST /v1/admin/restore/events` and `/treasuries/apply` take event ids only
  as `evt_…`, as every delivery's `webhook-id` is.
- **Breaking**: Admin: the deposit view and `nudge` take a deposit only by its `dep_…` id; a bare
  UUID is `404`. The service's logs, and a nudge's audit subject, name deposits, events, refunds,
  and an outbox delivery's object by their prefixed ids (`dep_…`, `evt_…`, `re_…`), the form the
  API takes.
- **Breaking** for operators: the database enforces what the service reads as constraints
  (`20261023000000_current_invariants`): a deposit carries its transaction's `tx_from` and
  `tx_nonce` unless it is a reversed deposit restored from a delivered event, every event's `data`
  holds its `object`, and a deposit event's snapshot its `receipt_log_index`, `revision`,
  `block_hash`, and `block_time`. A row that breaks one fails `topup migrate`.
- **Breaking**: a client quote's `confirmations`, and a client deposit address payment's, are
  required in the OpenAPI schema, an integer or `null`, as the service always sends them.
- The reference product applies a `deposit.*` event only with a valid deposit snapshot: a
  `deposit.credited` without one is logged and credits nothing.
- **Breaking** for operators: Deploy is a reusable workflow that deploys a release (`version`),
  not a Release images run (`release_run_id`), from the caller's `environment_dir`; Phala's
  instance deploys through "Deploy Phala's instance". Images are published only by the Release
  workflow, from a tag, tagged with the version.
- `deploy/contracts/verify-deployment.sh` compares each chain with the committed reference
  deployment `deploy/contracts/reference.json`, which CI checks against a fresh build
  (`reference-manifest.sh --check`), so it needs no Solidity build and runs from the kit.
- The example environment's header no longer names a value preflight refuses, so a filled-in copy
  that keeps its comments passes.
- While the service is frozen after a restore, a merchant request without a well-formed API key
  answers `401 api_key_missing` or `401 api_key_invalid`, as when not frozen (it answered
  `503 service_restoring`): the key's form and checksum are checked first, without a database
  read. A well-formed key still answers `503 service_restoring`, reads included, and nothing is
  saved for its `Idempotency-Key`.
- An `Idempotency-Key` older than 24 hours is pruned in the background, not while a `POST` claims
  its own key, so no request pays for pruning every account's keys; such a key is still free for
  any request.
- **Breaking**: A key rolling itself (`POST /v1/api_keys/{id}/roll` with the
  requesting key's own id) must keep working for at least an hour: `expires_in` under `3600`,
  including the default `0`, is `400 parameter_invalid`. A replay never returns the new key's
  secret, so an immediate self-roll whose response was lost locked the account out; now the old
  key rolls the new one (its id is in the replay) to recover. To stop the old key sooner, revoke
  it with the new key. Another key may still roll a key with `expires_in: 0`.
- **Breaking**: A `client_secret` is `{id}_secret_{nonce}{tag}`, 64 lowercase hex
  digits after `_secret_` (was 48), where `tag` is the service's HMAC of everything before it. A
  forged or malformed secret is refused in memory (`404`) without touching the database or any
  budget, so forgeries can no longer throttle checkout polling; a genuine secret is limited to 120
  reads per minute of its quote or deposit address, with `Retry-After`.
- **Breaking**: Route files no longer take `unit_decimals`: credit is always USD
  cents, the API's `amount`. A route file that sets it is refused.
- **Breaking**: Open-quote caps are per account and mode only (design §12), set by the operator per account
  and mode (defaults: 1 000 open quotes, $50 000 of open quotes per account, $5 000 per customer
  in live mode; 100, $10 000, and $5 000 in test mode). There is no global cap, and test-mode
  quotes never count against live mode. `GET /v1/config` reports the effective caps:
  `max_open_quotes` and `max_open_amount_per_customer` are new, and `max_open_amount_per_account`
  is now the account's cap in the mode (it was the per-customer cap). `400 exposure_cap_exceeded`
  also answers a quote past `max_open_quotes`. Route files no longer take
  `limits.max_open_minor`.
- **Breaking**: Admin: `POST /v1/admin/accounts/{account}` takes `limits {livemode, max_open_quotes,
  max_open_amount_per_account, max_open_amount_per_customer, max_active_deposit_addresses}`, and
  the admin account response carries the effective `limits` of both modes.
- **Breaking**: Deploy runs one production deployment for both modes: every route's `livemode` must match its
  chain (live on a mainnet, test on a test network), and staging takes no live route
  (`deploy/check-route-modes.sh`); production no longer requires every route to be on chain 1.
- **Breaking**: while the service is frozen after a restore from backup, no API
  key authenticates, reads included: every request with a key answers `503 service_restoring`
  with `Retry-After` (only writes did). The restored database can hold a key you revoked after the
  restore point as valid; keys work again once the operator has revoked such keys again and
  unfrozen the service. A quote's or deposit address's `client_secret` read, the admin API, and
  `/healthz` are unaffected. A restore-check instance refuses every merchant key, whether or not
  the freeze is recorded yet.
- **Breaking**: Admin: `POST /v1/admin/restore/events` takes `deliveries`, each
  delivery as the merchant's receiver got it (`webhook_id`, `webhook_timestamp`,
  `webhook_signature`, and the raw `body`), instead of bare event objects, and imports only
  deliveries whose `v1a` signature verifies with the account's webhook keys.

- **Breaking**: quotes and deposit address networks pay the account's treasury of the chain, set
  through the API, instead of the route's: `POST /v1/quotes` is `409 treasury_not_set` on a chain
  without one, and `POST /v1/deposit_addresses` issues networks only on chains with one (`409
  treasury_not_set` when none has). Route files no longer have `chain.treasury` (a route file that
  still names it is refused), and the admin daily report drops `treasury_balance_atomic` and
  `treasury_balance_note`.
- **Breaking**: webhook delivery (§16 PR 8). Retries still continue until delivered (backoff
  capped at 1 h) and a failing endpoint is never disabled (owner decision); `410 Gone` from the
  receiver disables its endpoint at once (`disabled_reason: gone`), announced to the account's
  other endpoints as `webhook_endpoint.updated`. A disabled or deleted endpoint's pending
  deliveries stop; resend them with `POST /v1/events/{id}/resend`. Each endpoint has at most 4
  deliveries in flight, slots go round-robin across endpoints, a failing endpoint is probed one
  delivery at a time after a backoff, and events are not ordered. Deliveries leave through an egress proxy (smokescreen) that refuses
  addresses that are not publicly routable.
- **Breaking**: the operator no longer manages merchants' webhooks: `webhook_url` is removed from
  `POST /v1/admin/accounts` and `POST /v1/admin/accounts/{account}` (an unknown field is `400`),
  and `POST /v1/admin/outbox/{event_id}/replay` and the `topup outbox replay` command are removed.
  Existing endpoints are kept and are now managed through `/v1/webhook_endpoints`.
- **Breaking**: webhooks are signed with a key per account and mode (docs/design/multi-tenant.md
  D11, §16 PR 6), derived in the attested CVM at `settlement/{acct}/{live|test}/v{n}`, instead of
  the one shared `settlement/v1` key: an event signed for one account never verifies at another.
  `GET /v1/attestation?nonce=` now needs an API key (`401` without) and returns `{object:
  "attestation", account, livemode, webhook_keys: [{version, public_key, expires_at}],
  report_data, quote}`, where `report_data = sha256(len(nonce) ‖ nonce ‖ len(account) ‖ account ‖
  livemode ‖ (version ‖ public_key)*)`; `keyid` and `settlement_pubkey` are removed. Pin your
  account's key per mode and check the event's `account` and `livemode`. Test and live events
  are delivered by separate workers.
- **Breaking**: API keys replace RFC 9421 request signing for merchants
  (docs/design/multi-tenant.md D7, D8, D12, §16 PR 5). Send `Authorization: Bearer
  ppay_sk_test_…` or `ppay_sk_live_…`; the key selects the account and the mode. A missing,
  invalid, or revoked key is `401 api_key_missing` or `401 api_key_invalid`, a rolled key past its
  expiry `401 api_key_expired`, and a live key of an account not enabled for live mode
  `403 testmode_charges_only`. Requests are limited per account and mode (100/s live, 25/s test,
  with a test-mode platform ceiling): `429 rate_limit`.
- **Breaking**: every `POST` is idempotent by `Idempotency-Key` for 24 hours per account and mode:
  a repeat of the same request replays the first response (`Idempotent-Replayed: true`), another
  request with the same key is `400 idempotency_key_reused` (was `409`), and a repeat while the
  first runs is `409 idempotency_key_in_use`. A repeated quote creation now returns the same
  `client_secret` instead of a new one.
- **Breaking**: accounts are created only by the operator: `POST /v1/admin/accounts {name,
  contact, due_diligence, charges_enabled, reason, webhook_url?}` returns the first secret keys;
  `POST /v1/admin/accounts/{account}` (was `PUT`) updates live mode, the restricted flag, the
  contact, or the webhook URL, and enabling live mode returns the first live key;
  `POST /v1/admin/accounts/{account}/api_keys {livemode, revoke_existing, reason}` issues a
  recovery key. Customer pauses take `livemode`.

- Every issued address is scanned at every block, not only open quotes' addresses: a late,
  repeated, or wrong-amount payment, or one to a persistent address, is credited at the route's
  confirmation (about 15 seconds after inclusion on Ethereum at depth 2) instead of at finality,
  and shows as `seen` in the quote's `payment` meanwhile. RPC usage no longer grows with polling
  (docs/architecture.md §8): one `eth_blockNumber` per block time and one `eth_getLogs` per new
  block per chain, whatever the number of addresses; the admin-signed `GET /v1/admin/metrics`
  reports the calls per provider, chain, and method (deploy/README.md, "Measuring RPC usage").

- **Breaking:** refunds are paid by the merchant (docs/design/multi-tenant.md D5, PR 9), in
  BTCPay's two-step payout flow. `POST /v1/refunds` creates a `pending` refund of a final deposit
  and reserves its amount; its destination is screened for sanctions
  (`400 destination_sanctioned`, `503 unavailable` when screening cannot answer). Pay it from the
  refund's new `treasury` field (the treasury of the deposit's own address, not the account's
  current one), then attach the transaction with **`POST /v1/refunds/{id}/mark_paid
  {transaction_hash, log_index?}`**. At finality on both providers, a `Transfer` of the deposit's
  token from that treasury to the destination for exactly the amount, in a log no other refund
  holds, makes the refund `succeeded` and sends `deposit.refunded`; anything else makes it `failed`
  with a `failure_reason`, releases the reservation, and sends **`refund.failed`** (Stripe's
  event; event id `uuid_v5(NS, "refund.failed:" + refund UUID)`, `data.object` the refund). **`POST /v1/refunds/{id}/cancel`** cancels
  a pending refund; a reversed deposit cancels its pending refunds. The Refund object gains
  `treasury`, `failure_reason`, and `log_index`, renames `tx_hash` to `transaction_hash`, and its
  `status` is Stripe's `pending`, `succeeded`, `failed`, or `canceled`. New `409` codes:
  `refund_unexpected_state`, `transfer_already_used`. The operator's
  `POST /v1/admin/refunds/{id}/approve` and `/record` are removed, and the daily report's
  `refunds_by_status` counts the new statuses.
- **Breaking:** the service sends no transactions (docs/design/multi-tenant.md §5, §13, PR 4).
  Anyone, usually the merchant with its own wallet or Safe, sweeps forwarders with the
  permissionless factory's `flush(treasury, salts, token)` and pays the gas. The finalized scanner
  indexes the factory's `ForwarderCreated`, `Flushed`, and `FlushFailed` events for the account's
  own `(address, treasury)` pairs, whoever sent them, and a final credited deposit becomes `swept`
  once a finalized `Flushed` event follows it. A `FlushFailed` target keeps its balance and its
  deposits stay `credited`. Reconciliation compares every active forwarder's finalized balance
  with its deposits minus its finalized sweeps; a mismatch freezes crediting on the chain
  (`409 chain_frozen`) until the operator lifts it.
- **Breaking:** attestation binds only the settlement key: `GET /v1/attestation` drops
  `operators`, `report_data` is `sha256(nonce ‖ settlement_pubkey)`, and `topup attest` drops
  `--route` and `--operator-key-version` and its `operators`, `operator_keyid`, and
  `operator_address` fields.
- **Breaking:** route files drop `chain.operator_key_version`, `chain.flush`,
  `limits.min_flush_atomic`, and `alerts.stuck_after_s.credited` (a credited deposit waits for
  its merchant's sweep); the pause scope `flush` is gone. The daily report drops `flush_planning`,
  and its `unflushed_balance_atomic` is deposits not reversed minus finalized `Flushed` amounts.
- **Breaking:** route files drop `product` and require `livemode`, checked against the chain.
- **Breaking:** products are gone. `POST /v1/admin/products`, `PUT /v1/admin/products/{slug}`,
  and `POST /v1/admin/products/{slug}/accounts/{account_id}/pause | resume` are replaced by the
  account endpoints above; the quote address salt's first input is the `acct_…` id instead of the
  product slug.
- A quote's `account_id` holds 1 to 200 characters (was 255 bytes).

- **Breaking**: a deposit id is `uuid_v5(NS,
  "{chain_id}:{tx_hash}:{receipt_log_index}")`, the transfer's position among its transaction's
  receipt logs (0 for a plain token transfer), instead of the block-wide `log_index`, so a
  re-included transaction keeps its id. `log_index` and `block_number` stay on the deposit as
  evidence and change when the transaction is re-included.
- Deposit `status` gains `reversed`; the quote's `payment.status` `final` now means recorded at
  the route's confirmation, and the payer's `payment_status` `confirming` likewise.

- `POST /v1/products/{p}/accounts/{ext}/rate-locks` and `POST …/deposit-address` create the
  account when it does not exist, so a quote or an address is one call; `POST …/accounts` is no
  longer required. Reads (`GET`, rotate, cancel, pause) of an unknown account still answer `404`.

- `DepositResponse` (deposits, deposit, and support lookup) carries `external_id`, the account
  of the receiving address, and `price_source` (`lock` or `spot`).

- `PUT /v1/admin/products/{slug} {public_key, webhook_url, reason}` (administrative API) replaces
  an issued product's verification key and webhook URL, which `POST /v1/admin/products` refuses
  with `409`. The key id stays the route's `destination.product_kid`. The cut is immediate: the
  old key stops verifying when the change commits, with no overlap. The `audit` row
  (`product.update`) carries the reason and the replaced values; a repeat with the stored values
  changes nothing. An unknown slug is `404`, an unrouted slug or invalid value `400`.

- `POST /v1/admin/reconciliation-blocks/{block_key}/lift {reason}` lifts a reconciliation block
  (`chain:{chain_id}` or `address:{address_id}`), which production could not do without a database
  owner session. Lifting is manual: the reconciler blocks again if the finding still reproduces.
  The `audit` row (`reconciliation_block.lift`) carries the reason and the removed block; a repeat
  returns the first lift, and a key that never blocked is `404`. `GET /v1/admin/report/daily`
  lists the active `reconciliation_blocks`.

- `POST /v1/admin/outbox/{event_id}/replay {reason}` queues an existing webhook event for
  delivery again with the same id and payload (audited as `outbox.replay`); a repeat while the
  event is due changes nothing. The product-signed support lookup
  (`GET /v1/products/{p}/deposits?tx_hash=|address=|lock_ref=`) lists each deposit's webhook
  `events` (`id`, `event_type`, `created_at`, `delivered_at`).

- `GET /v1/admin/report/daily` returns `exposure_minor`, the global open rate-lock credit in
  destination minor units (#94).

- **Breaking**: webhooks are Stripe's Event object, `{"id": "evt_…", "object": "event", "type",
  "created", "data": {"object": …}}`, where `data.object` is the deposit (`deposit.credited`,
  `deposit.rejected`, `deposit.refunded`) or the quote (`quote.expired`, which replaces
  `rate_lock.expired`) as the API returns it, rendered at the first delivery attempt. The
  `webhook-id` is the `evt_` id, derived for every type from the event type and its object, so
  every re-emission deduplicates. `deposit.pending` and `deposit.confirmed` are no longer sent:
  the quote's `payment` shows a transfer before finality. The admin outbox replay and deposit view take and show
  `evt_` ids.
- **Breaking**: quotes replace rate locks (docs/architecture.md §9, §12). `POST /v1/quotes
  {account_id, amount, currency, chain_id, asset}`, `GET /v1/quotes/{id}`, and
  `POST /v1/quotes/{id}/cancel` replace `…/accounts/{ext}/rate-locks[/{ref}]`; the quote id
  (`qt_…`) replaces `product_lock_ref`, `Idempotency-Key` makes creation safe to retry, amounts
  are integer cents with `currency: "usd"`, timestamps are Unix seconds, and statuses are `open`,
  `complete`, `expired`, and `canceled`. A new quote's address salt uses its id as the reference.
  Quoting by token amount is removed. `GET /v1/config` lists the payable assets, limits, and
  quote terms; `POST …/accounts` and `GET …/accounts/{ext}/limits` are removed (the first quote or
  address creates the account).
- **Breaking**: quotes are the only flow. `GET|POST …/accounts/{ext}/deposit-address`,
  `…/deposit-address/rotate`, `GET …/accounts/{ext}/pending-deposits`, and the `addresses` pause
  scope are removed.
- **Breaking**: deposits and refunds are top-level resources. `GET /v1/deposits` (a Stripe list
  object with `starting_after`/`ending_before`/`limit` and filters `account_id`, `quote`, `status`,
  `tx_hash`, `created[gte|lte]`) and `GET /v1/deposits/{id}` return `Deposit` objects (`dep_` ids,
  `status`, `amount` in cents, `exchange_rate`, `price_source` `quote` or `spot`, `quote`,
  `amount_refunded_atomic`, `refunded`, Unix timestamps); `POST /v1/refunds {deposit,
  destination_address, amount_atomic?}` with `Idempotency-Key` and `GET /v1/refunds/{id}` return
  `Refund` objects (`re_` ids, status `pending` or `succeeded`). `expand[]` expands a deposit's
  `quote`, a quote's `deposit`, and a refund's `deposit`. The old deposit list, deposit, support
  lookup, and refund-request paths are removed; the operator's `GET /v1/admin/deposits/{id}` shows
  a deposit's transitions and webhook events, and account pause and resume move to
  `POST /v1/admin/products/{slug}/accounts/{account_id}/pause|resume`.
- `POST /v1/quotes` returns a `client_secret`, like Stripe's PaymentIntent. The payer's browser
  reads the quote's public view, `ClientQuote`, from `GET /v1/quotes/{id}?client_secret=…` without
  a signature (any origin; rate-limited). Only the secret's hash is stored: `GET` returns `null`,
  and a repeat with the same `Idempotency-Key` returns a new secret.
- **Breaking**: the product is identified by the request signature's key id, `{product}/v1`,
  not by the path.
- **Breaking**: errors are Stripe's error object, `{"error": {"type", "code", "message",
  "param"}}`, with Stripe-style codes (`parameter_invalid`, `resource_missing`,
  `signature_invalid`, `rate_limit`, `idempotency_key_reused`, …). `paused` and `chain_frozen`
  answer `409` instead of `423`.
- Route files name only what differs per route or environment (`route`, `version`, `product`,
  `chain.{chain_id, forwarder_factory, treasury}`, `asset.{symbol, contract, decimals}`,
  `pricing.{primary, check}`, and `limits`); every other value is a code default, overridable
  under its key (architecture §14). `topup route show FILE` prints the resolved route. The
  implementation defaults to the factory's first `CREATE`, the sanctions oracle to Chainalysis's
  address on chains that have one, and a product's key id is `{product}/v1`. `finality`,
  `destination.product_kid`, and `rate_lock.enabled` are removed; the `quotes` pause scope stops
  quote creation. Staging's route moves to version 2.
- A new quote's `amount_atomic` (and its `payment_uri`) is rounded up to the route's
  `quote.amount_decimals` token decimals, default 4, so the payer is asked for `273.9185` PHA
  rather than 18 decimals. The rounding overpays by less than one unit of the last decimal; the
  quote's `amount` credit is unchanged.
- The admin deposit `nudge` and refund `approve`/`record` paths take the `dep_` and `re_` ids the
  product API returns, as the admin deposit view and outbox replay already did. Their responses (`AdminRefundResponse.id`, `NudgeResponse.deposit_id`, and the
  deposit view's `id`) show the prefixed id instead of the UUID.
- A malformed path parameter, query string, or JSON body on any route, product or admin, answers
  the `400` error object (`parameter_invalid`, `parameter_missing`, or `parameter_unknown`, with
  `param`) instead of plain text; the admin routes' JSON bodies were plain text before.

- **Breaking: webhook fulfillment replaces the settlement protocol**
  (architecture §7, §11; integration guide §5). A deposit that passes screening is `credited`
  directly (`confirmed → credited`; the `cleared` state is gone), and `deposit.credited` is the
  fulfillment event: the product credits `amount_minor` to `external_id` once per deposit id and
  answers `2xx`. Its payload is now `product_id`, `external_id`, `deposit_id`, `state`, `unit`,
  `amount_minor`, `price_source`, `price_scaled`, `price_scale`, `valuation_at`,
  `product_lock_ref` (the receiving address's lock, also for spot-priced payments), `address`,
  `route`, `route_version`, `chain_id`, `asset_contract`, `tx_hash`, `log_index`, and
  `amount_atomic` (no `destination_tx_id`), and its `webhook-id` is
  `uuid_v5(DEPOSIT_NAMESPACE, "deposit.credited:<deposit_id>")`, the same on every delivery and
  after a restore. Deliveries retry until `2xx`. The service no longer sends
  `POST {settlement_url}` or `GET {settlement_url}/{key}`, and no deposit becomes
  `rejected(product_refused)` any more: a product refuses a credit by holding it and requesting a
  refund. Allowed inside `/v1` without a deprecation window because no product consumed the
  settlement protocol in production (owner decision on #143).

- The attested route's `destination.settlement_url` is removed (routes that set it no longer
  load). A product's `webhook_url` may be `http` only when the service's own public origin is.

- `GET /v1/admin/report/daily` route entries replace `settlements_by_status` with
  `credited_undelivered` and `credited_undelivered_max_age_seconds`: `deposit.credited` events the
  product has not acknowledged yet (administrative API).

- `POST /v1/products/{p}/deposits/{id}/refund-requests` accepts `credited` and `swept` deposits
  too (still not `sanctioned`, still at least the route's `min_refund_atomic`): a product asks to
  refund a credit it did not apply or has reversed, for example for a closed workspace; finance
  approves every request (integration guide §5.4).

- **Settlement conformance:** the `unknown_get` case now requires `404` for `GET` of an unknown
  settlement key and fails `200 {"status":"unknown"}`, which it used to accept. The service
  resends a settlement only after a `404` by key; the other answer made it poll without ever
  resending. `topup-conformance-reference --broken unknown-status` answers the old form and fails
  exactly that case.

- **Breaking (base URL):** the service is served on a custom domain, `https://crypto-topup-api.phala.com`
  (staging `https://crypto-topup-api-staging.phala.com`), with TLS terminated inside the CVM and
  its certificate evidence at `/evidences/`. Sign `@target-uri` for that origin; the gateway URL
  `https://<app_id>-8080.<gateway domain>` no longer answers.

- Removed the unreachable `501` response from `/v1/attestation` and the `work_package` error field
  from `openapi.json`; both belonged only to the pre-C11 placeholder, and production never
  returned them (#90).
- Rate locks carry an optional `payment` object, chosen by the lock consumption rule: the
  deposit that consumed the lock, otherwise the first payment that would consume it, otherwise
  the first payment. `status` is `"seen"` while it is above `finalized` (with `confirmations`
  and `estimated_final_at`, block time plus 15 minutes) and `"finalized"` once it is a deposit
  (`deposit_id` locates it); `supported`, `in_time`, and `amount_within_tolerance` describe it
  against the lock and are false on a cancelled lock.
- New `GET /v1/products/{p}/accounts/{ext}/pending-deposits` lists transfers to the account's
  persistent addresses seen above `finalized`. They are not deposits and are not credited; once
  final they leave this list and appear under `deposits`, and a reorg can remove them.

See `docs/architecture.md` §8 and §12. Both are display only: crediting is unchanged and still
happens only from two-provider finalized data.

- New event `deposit.pending`, sent at most once per chain event when a non-zero transfer of a
  routed token to a watched address is first seen above `finalized`. Its payload is marked
  `provisional: true`; it never changes a balance, and the transfer may still disappear in a
  reorg. It may arrive after `deposit.credited` for the same deposit, so act on fetched state,
  not event order. Receivers that do not handle it must ignore it, as with any unknown event
  type.
- `deposit.credited` and `deposit.rejected` payloads now include `chain_id`, `state`
  (`credited` or `rejected`), and `route` (null when no route was selected); `chain_id` and
  `route` match the fields already on `deposit.confirmed`. This covers every producer, including
  the scanner's `unsupported_asset` rejection. The change is additive; existing fields are
  unchanged. See `docs/architecture.md` §12.

- A lock now expires by chain time: `rate_lock.expired` is emitted only once the finalized chain
  has passed `expires_at` and no payment mined inside the window awaits confirmation, so a payment
  made in the last minutes of the window is consumed at the lock price and never reported as
  expired. Until then `GET …/rate-locks/{ref}` returns `status: open` with
  `remaining_seconds: 0`; expiry events arrive about 15 minutes after `expires_at`. See
  `docs/architecture.md` §9.
- `DELETE …/rate-locks/{ref}` on a lock whose payment window has closed but which has not yet
  expired now answers `409` with the new error code `window_closed` ("payment window has
  closed") instead of `conflict`. `conflict` remains for consumed or expired locks.
- `GET …/limits` `reset_at` is the earliest payment-window close among open reserved locks. It can
  be in the past: exposure is released only at chain finality, about 15 minutes later.

### Removed

- The Release images workflow and the fork requirement: nobody needs a fork or their own image
  build to deploy (building from source stays possible and yields the same `phala-pay` digest).
- The reference product's `team_addresses` table, written with each quote and deposit address but
  never read: the quote and deposit address records hold them.
- The reference product's ledger migrations: it creates its one current schema in a new ledger
  and reads no older one, so an existing ledger is reset (Phala's staging's is).
- Webhook events written before Stripe-style events (outbox format 1) and their old envelope;
  every event is `{id: "evt_…", object: "event", type, created, data: {object}}`.
- The retired `rejected(product_refused)` reason and `cleared` state, and addresses issued before
  quotes (persistent addresses).
- **Settlement conformance suite** (`topup-conformance`, `topup-conformance-reference`,
  `docs/conformance.md`, `make product-conformance`) and the reference product's conformance mode
  (test accounts and the `_conformance/ledger` hook). The settlement endpoint it tested is being
  replaced by webhook fulfillment (integration guide §5); webhook receivers are
  tested with `topup-sdk send-test-event`.

- **Breaking (administrative API):** `GET /v1/admin/report/daily` route entries no longer carry
  `exposure_minor`, `exposure_minor_reason`, `pnl_minor`, or `pnl_minor_reason` (#94). They were
  always null placeholders; route exposure now comes from the report-level `exposure_minor`, and
  PnL is not defined precisely enough in the design to compute.

### Fixed

- A `client_secret`'s 120 reads per minute of its quote or deposit address refill at two a
  second instead of all at once when a one-minute window rolls over, so a page can no longer read
  120 times just before a rollover and 120 more just after it. A read over the budget is retryable
  after `Retry-After: 1` rather than at the end of the window.
- Admin: `POST /v1/admin/restore/deposit_addresses` refuses, `400`, a `version` more than 32 past
  the customer's latest one, before anything is issued: it issued every version between in one
  transaction, however many, so a huge `version` held its connection and locks until it exhausted
  them. A customer further behind is re-issued in steps (`version` 32, 64, …); an `address` was
  already looked for only that far.
- Admin: a restore across a treasury change no longer deadlocks the reconciliation. Deposit
  addresses and quotes are re-issued over a treasury in force when they were issued, within 5
  minutes (any since the restore point for a deposit address, around its `created` for a quote),
  not only the current one, once the lost change is applied again with `POST
  /v1/admin/restore/treasuries/apply`; before, they could be re-issued only over the restored
  current treasury, while the change would apply only after the unfreeze those re-issues must
  precede. Each re-issued deposit address version, by address or by version, keeps a superseded
  network over every treasury in force since the restore point, still credited. A restore without
  a restore point re-issues nothing, and a quote the restored database does not hold, created well
  before the restore point, is refused (one it holds is returned, `reissued: false`).
- A quote's `created` is taken once its chain's treasury is read under the treasury lock, and a
  time-locked treasury change records `applied_at` as it applies under that lock rather than when
  the time-lock's pass started, so each falls while the other's treasury is in force.
- Admin: a deposit reversed after the restore point because a re-included transaction put
  another transfer at its receipt position keeps its identity through the restore. Its imported
  `deposit.reversed` rebuilds it, reversed, at its revision, so the rescan records the final
  transfer as its successor again, with the same id, `replaces`, and delivered credit; before, the
  rescan recorded it under the reversed deposit's id, held as `contradicted`, and the successor
  could never be rebuilt.
- Admin: `POST /v1/admin/restore/events` imports the `deposit.reversed` of a deposit that was never
  valued (rejected, such as a token without a route); it refused the whole request with `400`. A
  `deposit.credited` still needs its valuation.
- Idempotent requests are atomic (architecture §12; Brandur Leach's
  [Stripe-like idempotency keys in Postgres](https://brandur.org/idempotency-keys)): every
  merchant `POST` saves its response in the transaction of its changes, so a retry after a crash,
  a dropped connection, or a request slower than a minute replays the result and never runs the
  request twice (a quote, a partial refund, an API key, a webhook endpoint, or a webhook key roll
  was created twice before). A key whose request never saved a response is still taken over by the
  same request after a minute, and the request it replaced can no longer commit: it answers
  `409 idempotency_key_in_use`. A request already making its changes commits, and the repeat
  waits for it (at most 5 seconds, then `409 idempotency_key_in_use`) and replays its response. A
  failure while rendering a response now creates nothing and is replayed as it failed (a quote was
  created before). A request the database cannot begin, or rolls back on a deadlock or
  serialization failure, is an unsaved `503` with `Retry-After`, as is one that finds no database
  connection to claim its key (it was a `500`).
- Admin: `POST /v1/admin/restore/quotes` and `/deposit_addresses` accept a `client_secret` only
  when the service issued it for that id to that account. The secret's nonce now carries an owner
  tag of the account (its length and format are unchanged, and it is still opaque): before, any
  account's record of a lost id with the owner's secret, which the payer's page holds, re-issued
  it under that account and address, and the owner's payer page showed that address.
- Authorization runs before the idempotency lookup, as Stripe's: a restricted key no longer
  replays a response to a request its permissions refuse, and a `401` or `403` is no longer saved,
  so the same request by a key that holds the permission then runs.

- A payment made through a contract (a router, a swap output) whose transaction is re-included
  before finality against other state, so that the transfer at the same receipt position pays
  another amount or another issued address, is now recorded and credited. The first deposit is
  `reversed` (`deposit.reversed` if you were told of it), and the transfer in the final chain is a
  new deposit with a new id and its own `deposit.credited` (or `deposit.rejected`), already final,
  whose `replaces` names the first one; a quote the first one completed goes to it without
  `quote.expired`. The two deposits' events arrive in no set order: the balance rule nets them
  whatever the order. The new transfer was never recorded, and custody reconciliation froze the
  chain. Existing deposit ids are unchanged, and a transaction re-included unchanged keeps its
  deposit.
- After a restore from backup, a deposit the merchant was told was credited keeps that credit: the
  amount, exchange rate, price source, and valuation time of the imported `deposit.credited` or
  `deposit.reversed` are the deposit's valuation when the rescan re-derives it, instead of a
  re-valuation at spot, so `amount`, `amount_refunded`, and `amount_reversed` match what was
  delivered. A deposit whose transfer on chain contradicts the delivered event is held, not
  credited, until the operator discards the delivered credit.
- A token without a route sent to an issued address is again recorded as
  `rejected(unsupported_asset)`, with its `deposit.rejected` event, once final. Since the per-block
  scanning change, routes in token mode (the default) never saw such transfers: the missing-deposit
  check now reads every issued address's transfers of any token in both modes.
- A webhook endpoint's `pending_deliveries` and `oldest_pending_at`, and the admin daily report's
  `failing_webhook_endpoints`, no longer count the notice of a URL change still pending at the
  endpoint's former URL: it is not a delivery to the endpoint as it is now, so a former URL that
  was taken down no longer makes the endpoint look unhealthy. The notice is still retried until
  delivered.
- Webhook deliveries no longer stall behind a URL change's notice. The notice goes to the
  endpoint's former URL, and its failures there counted as the endpoint's: once that URL was
  taken down, the endpoint cooled down and every probe picked the failing notice first, so new
  events were held for up to an hour at a time. A notice's outcome now neither cools nor clears
  the endpoint.

[unreleased]: https://github.com/Phala-Network/phala-pay/compare/v0.9.2...HEAD
[0.9.2]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.9.2
[0.9.1]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.9.1
[0.9.0]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.9.0
[0.8.3]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.8.3
[0.8.2]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.8.2
[0.8.1]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.8.1
[0.8.0]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.8.0
[0.7.0]: https://github.com/Phala-Network/phala-pay/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/Phala-Network/phala-pay/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/Phala-Network/phala-pay/compare/v0.3.5...v0.5.0
[0.3.5]: https://github.com/Phala-Network/phala-pay/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/Phala-Network/phala-pay/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/Phala-Network/phala-pay/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/Phala-Network/phala-pay/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.3.1
[0.3.0]: https://github.com/Phala-Network/phala-pay/releases/tag/v0.3.0
