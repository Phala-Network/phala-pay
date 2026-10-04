# Design: per-account payment settings

Status: implemented in v0.6.0; current behavior is in [the integration guide](../integration.md#19-payment-settings).

Accepted (2026-10-02). Astra's review approved the model with a change list, and the
re-check added three final fixes. The owner adopted every recommendation, and this document
applies them. Ships in 0.6.0, breaking, with no backward compatibility. When
accepted, it amends [multi-tenant design](multi-tenant.md) D1, D16, §12, §14, and §15, and the
[architecture](../architecture.md) §6–§9, §12, and §14.

## 1. Problem

The attested route file mixes two different things:

- the operator's **catalog**: what the instance can safely accept (contracts, RPCs, pricing
  sources and guards);
- **commercial policy**, which belongs to each merchant: which tokens it takes, its quote terms,
  and its minimums and maximums.

Per-account policy is also scattered and partly implicit:

- **Implicit acceptance.** A treasury alone enables a chain for an account, and every routed
  asset on that chain is accepted.
- **Separate confirmations.** `confirmation_policies` is its own table, per account rather than
  per mode.
- **Identical terms for everyone.** Quote terms, deposit bounds, `min_credit_minor`, and
  `min_refund_atomic` are the same for every merchant on the instance.
- **Rules re-derived everywhere.** Quotes, deposit addresses, `GET /v1/config`, valuation,
  screening, and refunds each re-derive these rules from the route.
- **Quotes lose their terms.** A quote's tolerance and its view are read from the route's
  *current* version (`api/quotes.rs` `quote_object`, `core/valuation.rs` `lock_applies`).
- **Refund floors drift.** A refund's dust floor is read from the current route too, and for an
  unrouted token, from another token's route (`api/mod.rs` `refund_route`).
- **An incoherent rate limit.** `quote.max_creations_per_minute` is set per route but enforced
  per customer across every route (`locks::check_creation_rate`).

## 2. Decision

1. **Two layers.**
   - The attested config is the operator's **catalog**: what can be accepted, how it is valued
     and screened, and a **default and hard bounds** for each merchant parameter.
   - Each account has, **per mode**, one **payment settings** object. It chooses the chains and
     assets it accepts from the catalog, and sets its commercial terms within the bounds.
2. **Opt-in, with explicit states.** Every account and mode is in one state:
   - `unconfigured`: accepts nothing. Every account starts here.
   - `configured`: accepts what its current revision lists.
   - `held`: after a restore, until reconfirmed (§11).

   Nothing is ever implied by an absent row or a `NULL`.
3. **Treasury proof stays its own security flow** (challenge, signature, time-lock, cancellation;
   D10). A chain is *active* for issuance only when it has an accepted asset and an active
   treasury.
4. **One resolver** (§6) answers every question about terms, and every consumer uses only it.
5. **Binding.** Every quote and every deposit is bound once to the commercial policy that governs
   it (§7):
   - a quote, to its resolved terms at issuance;
   - a deposit, to the settings revision current in the snapshot of the statement that records
     it.

   A routed deposit its binding does not accept is `rejected(asset_not_accepted)`. It is never
   credited, and it is refundable under the existing conditions.

## 3. Field classification

The rule: a field is **operator** if it describes the chain, the token, the evidence the service
trusts, or the service's own cost or safety. A field is **merchant** if it is a commercial choice
whose consequence the merchant bears (D14). Every merchant field gets an operator default and hard
bounds wherever an extreme value would harm payers, the service, or the evidence. The code also
enforces absolute ceilings that no operator bound may exceed.

### Route fields

| Field (today) | Owner | Reasoning |
|---|---|---|
| `route`, `version`, `livemode` | Operator (catalog identity) | Attested identity. A deposit records the route version that values it. |
| `chain.chain_id`, `forwarder_factory`, `implementation` | Operator | Contracts: the custody model (D2, D3). |
| `chain.rpc_providers` | Operator | Evidence sources and their keys. |
| `chain.sanctions_oracle` | Operator | Screening evidence (compliance is the operator's, 2026-10-01). |
| `chain.confirmations` | Operator **floor**; the merchant may require more | The floor is the operator's reorg-risk judgment (D1). The merchant's requirement is bound with the deposit, and an uncredited deposit waits for the stricter of it and the **current** floor (§7). |
| `asset.symbol`, `contract`, `decimals` | Operator | Token identity. Credit arithmetic depends on `decimals`. |
| `asset.backstop` | Operator | Scanner strategy and RPC cost. |
| `pricing.mode` (`spot` or `stablecoin`) | Operator | A property of the asset's valuation. |
| `pricing.primary`, `check`, `fx` | Operator | Price evidence sources. |
| `pricing.max_age_s`, `max_deviation_bps`, `max_fx_deviation_bps` (depeg guard) | Operator | Guards on the evidence the attestation vouches for. |
| `alerts.stuck_after_s.*` | Operator | Operations. |
| `quote.amount_decimals` | **Operator only** | Rounding the token amount up at low precision can exceed the spread cap. For a $1 token at 0 decimals, a 1-cent quote costs the payer 1 token, 100 times the quote. The operator sets it with the token's price in mind. |
| `quote.window_s` | Merchant: `min`, `max` | The merchant bears an open quote's price exposure. The `max` bounds how stale an attested locked price gets, and how long a quote holds the open-quote caps. The `min` keeps a quote payable. |
| `quote.spread_bps` | Merchant: `max`, absolute ceiling below 100% | The merchant's pricing. The spread must be non-negative, below 100%, and leave a representable price (§5). The `max` protects payers from a marked-down attested price. |
| `quote.tolerance_bps` | Merchant: `max`, absolute ceiling | Tolerance is two-sided (§8). The underpaid side is forgiven at the quote's credit, which the merchant bears. The overpaid side, within tolerance, is credited at the quote's credit too, which the payer bears. The ceiling keeps it far from 10 000 bps, where a zero payment would match. |
| `quote.max_creations_per_minute` | Merchant, renamed `quote_creations_per_customer_per_minute`, **per account and mode**: `max` | It stops one customer from exhausting the merchant's open-quote caps, which is a merchant concern. The operator's `max` protects pricing and API budgets. The API's own rate limits (§12) stay the operator's and are unchanged. |
| `limits.min_credit_minor` (API `min_amount`) | Merchant: `min` | The smallest sale. The `min` is the operator's dust floor: a deposit costs RPC reads and a webhook. |
| `limits.min_deposit_atomic` | Merchant: `min` | As above, in token units. |
| `limits.max_deposit_atomic` | Merchant: `max` | Spot valuation is meaningful only within market depth, and screening is per deposit. The merchant may set it lower. |
| `limits.min_refund_atomic` | Merchant: `min` and `max` | The merchant pays refunds (D5, D14). The `min` is the operator's cost and abuse floor on refunds the service verifies. The `max` protects payers: a merchant cannot set a floor that makes refunds impossible. |

### Account fields

| Field | Owner | Reasoning |
|---|---|---|
| `name`, `contact`, `due_diligence`, `restricted` | Operator | Onboarding record (D8). |
| `charges_enabled` | Operator | Go-live gate (D12). |
| `max_unfinalized_credit` | Operator | Bounds the reorg exposure (§7 of the architecture). |
| `account_limits` (open quotes, open credit per account and per customer, active deposit addresses) | Operator | Resource and abuse caps. Unchanged; merchant-side lower caps are deferred. |
| `paused_scopes` (operator), route pauses, customer pauses | Operator | Incident controls. |
| `self_paused_scopes` (`quotes`), treasury crediting pause | Merchant, **not settings** | Emergency switches that must act at once and alone. They stay their own endpoints. |
| `confirmation_policies` | Merchant: **moved into payment settings**, per mode | The table and the field are removed (§13). |
| Treasuries | Merchant, own security flow | Unchanged. Settings never set or imply a treasury. |
| Webhook keys and endpoints | Merchant, own resources | Unchanged. |

## 4. API: `GET` and `POST /v1/payment_settings`

**Shape.** A singleton resource per account and mode, in the key's mode, like Stripe's singleton
settings resources: [Tax Settings](https://docs.stripe.com/api/tax/settings) (`GET`/`POST
/v1/tax/settings`, object `tax.settings`, event `tax.settings.updated`) and
[Balance Settings](https://docs.stripe.com/api/balance-settings). The catalog view follows
Stripe's [payment method configurations](https://docs.stripe.com/api/payment_method_configurations/object),
which list each method with whether it is available and the account's preference.

**Why it is not a field on the account.** The settings are a large nested document with their own
validation paths, revisions, and change event. On the account, every `account.updated` would carry
the document, and operator and merchant changes would share one event. A singleton keeps the
account about the operator's decisions.

```json
{
  "object": "payment_settings",
  "livemode": false,
  "status": "configured",
  "revision": "psrev_9f2c…",
  "updated": 1790000000,
  "quote_creations_per_customer_per_minute": null,
  "chains": [
    {
      "chain_id": 84532,
      "confirmations": null,
      "assets": [{"asset": "usdc", "quote_spread_bps": 0}, {"asset": "usdt"}]
    }
  ],
  "available": [
    {
      "chain_id": 84532,
      "status": "active",
      "confirmations": {"floor": "3", "default": "3"},
      "assets": [
        {
          "asset": "usdc", "contract": "0x…", "decimals": 6, "pricing": "stablecoin",
          "quote_amount_decimals": 4,
          "accepted": true, "enabled": true,
          "quote_ttl_seconds": {"default": 900, "min": 60, "max": 3600},
          "quote_spread_bps": {"default": 0, "max": 500}
        }
      ]
    }
  ]
}
```

**Fields.**

- `status`: `unconfigured`, `configured`, or `held` (§2).
- `revision`: an opaque id of the current revision. It is unique and never reused, and it carries
  no ordering or gapless promise.
- `chains`: what the merchant set. `null` means the operator's default, so the merchant follows a
  change of the default. A chain or asset that is not listed is not accepted.
- `available` (read-only): the catalog of the key's mode with defaults and bounds. Each chain has
  a `status` (`active`, `treasury_not_set`, or `not_configured`). Each asset has:
  - `accepted`: listed by the merchant;
  - `enabled`: its resolved terms are valid (§5).
- Parameter names match `GET /v1/config`'s: `confirmations`, `min_amount`, `min_deposit_atomic`,
  `max_deposit_atomic`, `min_refund_atomic`, `quote_ttl_seconds`, `quote_spread_bps`, and
  `quote_tolerance_bps`. `quote_amount_decimals` is shown, read-only.

```http
POST /v1/payment_settings
{"chains": [{"chain_id": 84532, "assets": [{"asset": "usdc", "quote_spread_bps": 0}, {"asset": "usdt"}]}]}
```

**Update semantics.**

- A top-level parameter that is not sent is unchanged. `null` restores the default.
- `chains`, when sent, replaces the whole list. In it, an optional field an element omits resets
  to its default (`null`). `chains: []` accepts nothing.
- Writes are **last-write-wins**: there is no compare-and-swap. Concurrent writers apply one after
  the other under the account row's lock, and the later one is current. The documentation says so.
- **While `held`, a `POST` must be the complete configuration, and writes a new revision even
  with an identical document.** This is the merchant's explicit reconfirmation (§11). `chains` is
  required (`400 parameter_missing` without it), and a top-level parameter not sent takes its
  default, never the restored value. Otherwise, a request that changes nothing writes nothing.

**Validation** runs against the catalog of the key's mode. A failure is `400 parameter_invalid`,
with `param` naming the exact path, such as `chains[0][assets][1][quote_spread_bps]`. It fails on:

- an unknown chain;
- an asset not routed on that chain (the message lists the routed ones);
- a chain or asset listed twice;
- a value outside its bounds (the message states the bounds);
- `confirmations` weaker than the floor, or of the wrong chain family;
- a resolved tuple that is invalid (§5).

**Event.** Each change writes a revision, an audit row (`payment_settings.update`), and
`payment_settings.updated` with the revision id and `previous_attributes`. It is the only event
for settings (decided). It is an account event, delivered to every enabled endpoint whatever it
subscribes to.

**Permissions.** Reading needs `account.read`; writing needs `account.write`, which a restricted
key never holds. A restricted production key, the reference product's included, can read the
settings and `GET /v1/config`, but cannot widen what the account accepts.

**Errors.**

| Request | Error |
|---|---|
| `POST /v1/quotes` for a pair not accepted, or not enabled | `400 asset_not_accepted` (new), `param: asset` |
| A quote or a new address while `held` | `400 payment_settings_unconfirmed` (new) |
| `POST /v1/deposit_addresses` with no active chain | `asset_not_accepted` when nothing is accepted; `treasury_not_set` when the accepted chains have no treasury |
| A quote on an accepted chain without a treasury | `400 treasury_not_set`, as today |

**`GET /v1/config`** returns exactly the effective config of the key's scope (§6):

- only active, enabled pairs, each with its resolved terms, the effective `confirmations`, and
  `typical_credit_seconds`;
- `quote_creations_per_customer_per_minute`;
- the operator's open-quote caps, as today.

An unconfigured or held account gets `assets: []`.

## 5. Bounds and validity

The route file keeps its catalog sections. `quote.amount_decimals` stays in the catalog, as
`asset.quote_amount_decimals`. The `quote:` and `limits:` sections become one `merchant:` section
of defaults and bounds:

```yaml
merchant:                          # each account's terms on this route: default, and hard bounds
  quote_ttl_seconds:   { default: 900, min: 30, max: 3600 }
  quote_spread_bps:    { default: 50, max: 500 }
  quote_tolerance_bps: { default: 100, max: 500 }
  min_amount:          { default: 100, min: 1 }                       # cents
  min_deposit_atomic:  { default: "0" }
  max_deposit_atomic:  { default: "200000000000000000000000", max: "200000000000000000000000" }
  min_refund_atomic:   { default: "20000000000000000000", min: "1000000000000000000",
                         max: "100000000000000000000" }
```

A bound left out keeps the account at the default, so the operator opens each term explicitly. The
exceptions are three terms with code defaults:

- the quote window, 30 to 3600 seconds;
- the spread and the tolerance, 0 to 500 basis points (or the default, if higher);
- `max_deposit_atomic` and `min_deposit_atomic`, which an account may move toward each other: the
  maximum down to 0, and the minimum up.

**Absolute ceilings in code.** No operator bound may exceed them; `topup config check` and startup
refuse a catalog that does.

- `quote_spread_bps` ≤ 5 000. The spread is integer basis points, so it is non-negative. The
  locked price `spot / (1 + spread)` must be at least one unit of the price scale; a quote whose
  locked price is not representable is refused like an unavailable price.
- `quote_tolerance_bps` ≤ 1 000, so no payment far below the quoted amount, let alone zero,
  matches a quote.
- `quote_ttl_seconds` within 30 s to 24 h.
- `min_refund_atomic` `max` ≤ `max_deposit_atomic` `max`.

**Other catalog rules.**

- `chain.confirmations` is the chain's floor and default. Every current route of a chain must
  agree. An earlier version keeps its own value only for the terms of what it governed, so a new
  version may raise the floor.
- `quote_creations_per_customer_per_minute` is not per route. Its default (10) and maximum (60)
  are code constants (`crate::payment_config`), as the API's rate limits are.
- Every bound left out has a code default, printed by `topup config show`. A default outside its
  own bounds is refused.

**Validity of a resolved tuple.** Resolution clamps each value to its route version's bounds and
then validates the whole tuple. A merchant cannot write an invalid tuple: `POST` refuses it. The
operator can still make one invalid by tightening a bound, for example lowering
`max_deposit_atomic.max` below a merchant's `min_deposit_atomic`. The tuple is invalid when any
of these fails:

- `min_deposit_atomic` ≤ `max_deposit_atomic`;
- `min_refund_atomic` ≤ `max_deposit_atomic`;
- `min_amount` ≥ 1;
- every value within its bounds after clamping.

An invalid tuple **disables the pair**:

- `enabled: false` in `available`;
- absent from `GET /v1/config`;
- never quoted or listed;
- a deposit bound to it is not accepted (§9);
- the service raises `TopupPaymentSettingsInvalid` naming the accounts, so the operator and
  merchant fix it.

## 6. The resolver

One module (`crate::payment_config`) holds the pure functions; every consumer uses only them.

```text
handling(route_version, binding, current_floor) -> Handling
    -- defined for every routed deposit, accepted or not:
    -- confirmations = stricter(current operator floor, the binding's chain requirement)
    -- refund_floor  = the binding's resolved min_refund_atomic if the pair is accepted and
    --                 enabled, else the route version's operator default
    -- basis         = which binding decided (quote terms, or settings revision), for evidence

accepted(binding, chain_id, asset) -> bool
    -- listed by the binding's document, and the resolved tuple is valid (enabled)

terms(route_version, document) -> Result<Terms, Disabled>
    -- commercial terms of an accepted pair: each value or default, clamped, tuple validated

effective_config(catalog, scope) -> EffectiveConfig
    -- issuance view: current route versions of the mode, accepted and enabled pairs of the
    -- current revision, on chains with an active treasury; plus typical_credit_seconds
```

**Identity.** A settings revision alone is not the identity of an effective config, because the
operator's defaults and bounds change with route versions. A resolution is identified by:

- the route version (catalog);
- the settings revision (merchant);
- the current confirmation floor, for `handling` only.

A quote stores its resolved terms, so its terms never depend on a later catalog.

| Consumer | Uses |
|---|---|
| `GET /v1/config`, `available` | `effective_config`, `terms` |
| `POST /v1/quotes` | `effective_config`, then stores the resolved terms with their provenance (§7) |
| Quote views, the pending payment, lock matching | the quote's stored terms only |
| Deposit addresses (issuance, networks, listed assets, client view) | `effective_config` |
| Scanner, finality-watch successor, reconciler, restore-check inserts | bind in the insert statement (§7); no resolution |
| Confirm step: wait, acceptance, `below_minimum`, valuation | `handling`, `accepted`, `terms` (or the quote's terms) |
| Screen step (`out_of_bounds`) | the deposit's terms |
| Refunds (dust floor) | `handling.refund_floor` |
| Admin views (account, deposit) | each mode's state and revision, `effective_config`, a deposit's binding and resolution |
| Restore | §11 |

Runtime gates (pauses, frozen chains, caps, rate limits) apply on top of the effective config, as
today. They are state, not configuration.

## 7. Binding

**Revisions.** Each change appends an immutable revision: an opaque id, account, mode, document,
actor, and time. The account and mode's state row (`status`, `current_revision`) points at the
current one. Writers lock that row `FOR UPDATE`, and the last write is current. Every reference to
a revision is scoped by `(account_id, livemode)`: a composite foreign key ensures that a deposit or
quote can name only a revision of its own account and mode.

**The barrier.** Every recorder of a deposit takes the account and mode's state row `FOR SHARE`, in
its own transaction, before the insert statement. The recorders are the scanners, the
finality-watch successor, the reconciler, and the restore-check round, all through one insert
function. Every writer of the state takes it `FOR UPDATE`: a settings write, the hold set at a
freeze, and the hold lift. The insert is a separate statement after the lock, so under PostgreSQL's
`READ COMMITTED` its snapshot is established after the barrier is acquired. A writer therefore
waits for every recorder transaction that read the old state to commit, and a recorder that starts
after the writer waits for the writer's commit and then reads the new state.

**Record time** is the snapshot of the statement that successfully inserts the record.

- A deposit's insert reads its account and mode's state in that statement and stores the binding:
  - its `current_revision` when the state is `unconfigured` or `configured`;
  - `pending`, naming the restore that holds it, when the state is `held` (§11).
- A settings change committed after that snapshot does not govern the deposit; one committed
  before it does. Nothing is claimed about commit order beyond that.
- `unconfigured` is itself a revision, with an empty document and status `unconfigured`, so a
  bound deposit always names a real revision.

**The database invariant.** A deposit has exactly one of:

- `settings_revision_id`: a revision of its own account and mode (composite foreign key);
- `settings_hold_id`: the restore that held its account and mode (foreign key to `restores`).

A `CHECK` requires exactly one. A `pending` deposit never borrows an old revision; the lift
replaces its hold with the revision that ends it (§11).

**A rescan never overwrites a binding.** Inserts are `ON CONFLICT DO NOTHING` on the receipt
position, so a transfer seen again keeps the deposit, and the deposit keeps its binding.

**A quote's terms** come from one consistent resolution in its creation transaction. It reads the
current revision with `FOR SHARE` on the state row, resolves it against the route's current
version, and inserts on the quote, together:

- the revision id;
- the route version;
- the resolved terms: TTL, spread, tolerance, amount decimals, `min_amount`, deposit bounds,
  refund floor, and confirmations.

The `FOR SHARE` lock stops a concurrent write from changing the revision between the read and the
insert.

**Confirmations.** The merchant's requirement is frozen at binding, whether a quote's or a
deposit's. Until it is credited, a deposit waits for the stricter of its bound requirement and
the **current** operator floor. When a deposit pays a quote's address in the quote's asset, it
waits for the stricter of the quote's and the deposit's requirements, since which one governs is
known only after confirmation.

**Reorgs and rescans.**

| Case | Binding |
|---|---|
| Same transfer re-included (the finality watch follows it) | Keep the original binding |
| Rescan of the same deposit (scanner, backstop, reconciler, restore-check) | Unchanged |
| Successor or new revision after a reversal | Record-time revision of the actual recipient address's account and mode |
| Payment of a valid quote (§8) | The quote's terms |
| In-place token, amount, or route correction before credit (confirm step) | Keep the settings revision; re-resolve `accepted`, `terms`, and `handling` on the corrected facts |
| Recorded while the account and mode is `held` | `pending`; bound to the revision that ends the hold (§11) |

## 8. Quote lifecycle

- **Views keep the original terms.** `GET /v1/quotes/{id}`, events, and the pending payment show
  the stored terms, never the current route or settings.
- **A valid payment honours the quote.** It must be:
  - the quote's asset;
  - to the quote's address;
  - in a block at or before `expires_at`;
  - within the quote's tolerance;
  - the payment that consumes the quote (one per quote, as today).

  It is valued at the quote's terms, even if the merchant has since stopped accepting the asset.
- **Tolerance** is two-sided: `|paid − quoted| × 10 000 ≤ quoted × tolerance_bps`.
  - An underpayment within it is credited at the quote's credit, which the merchant forgives.
  - An overpayment within it is also credited at the quote's credit; the excess is the payer's
    and stays in the treasury.
  - A payment outside it is not a valid payment of the quote. It is valued at spot under its
    record-time binding, so an overpayment beyond the tolerance is credited in full.
- **Every other payment** to a quote's address follows the record-time binding, with no
  grandfathering. This covers a late, wrong-amount, or second payment, and a payment to a
  canceled, expired, or consumed quote.
- **Re-issued quotes after a restore** (§11) never apply their lock, as today. Their payments
  follow the record-time binding.

## 9. Deposits the binding does not accept

A transfer of a routed token is recorded on its route as today. If its binding does not accept
the `(chain, asset)` pair, the confirm step rejects it as `rejected(asset_not_accepted)` with
`deposit.rejected`. That happens when the asset or chain is not listed, the pair is disabled, or
the account is `unconfigured`. The rejection comes once both providers agree on the transfer and
after any delivered-outcome branch (§11), before valuation. The evidence records the basis: the
revision, or the quote's terms.

`asset_not_accepted` makes a deposit **eligible** for the existing refund flow; it bypasses
nothing (`core::refund_eligibility`). The usual conditions still apply:

- the deposit must be final;
- the amount must reach the dust floor, `handling.refund_floor`: for a pair not accepted, the
  route version's operator default, since the merchant set none;
- a sanctioned deposit is never refundable. A sanctions hit blocks mark-paid and verification of
  an already pending refund: mark-paid, verification claim, and the final verification transaction
  lock and recheck the deposit.

The merchant declares the refund, pays it from the treasury of the deposit's address, marks it
paid, and the service verifies it, as for any other refund. A deposit bound to a route version
that is no longer loaded fails closed; its refund floor never falls back to the current version.

A token without a route stays `unsupported_asset`. Its dust floor today is another token's route
default; that is a known defect and out of this design's scope.

## 10. Migration and cutover

**Schema** (`…_payment_settings`):

- the append-only `payment_settings_revisions` table (`topup_app`: `SELECT`, `INSERT`);
- the `payment_settings_state` table, one row per account and mode;
- the deposit binding columns: kind `settings` or `pending`, and the revision;
- the quote terms columns: revision, route version, and resolved terms;
- `asset_not_accepted` added to the deposit reasons;
- `confirmation_policies` dropped, after the backfill has read it.

**One-time backfill.** It runs in `topup migrate` with the attested config (the compose `migrate`
service mounts it), as the database owner, in one transaction with the schema change and the
validation of the binding constraints. The config is loaded and validated before anything is
migrated, so a failure leaves the database as 0.5.0 left it. Afterwards every deposit has its
binding and every quote its terms, so no runtime rule reads a missing binding. A transaction
advisory lock serializes concurrent migrations through schema changes, backfill, validation, and
commit. This atomic prefix ends at migration `20261024000000`; later migrations run after its
commit under the existing migration advisory lock. Concurrent queue indexes therefore run outside an
outer transaction and recover through the normal migration service
([recovery behavior](db-api-query-plans.md#deploy-recovery)).

1. Every account and mode gets a state row, `unconfigured`, and an `unconfigured` revision as
   current.
2. Every account and mode gets one historical **`legacy` revision**, never current, that
   materializes the 0.5.0 model with explicit values:
   - every route of the mode in the cutover catalog, accepted;
   - each term at the route's value;
   - each chain's confirmations: the stricter of the floor and the account's
     `confirmation_policies`, read before the table is dropped.

   **Every existing deposit**, terminal or not, binds to it. A pending deposit is therefore
   handled exactly as in 0.5.0, its confirmation policy included, and every deposit has a refund
   floor.
3. **Every quote** (open, consumed, expired, or canceled) gets terms resolved from its account's
   `legacy` revision and its route's version in the cutover catalog. This is deterministic and
   matches what 0.5.0 showed at cutover: 0.5.0 stored no route version on a quote and read the
   current one. A quote whose route is not in the cutover catalog fails the backfill, so the
   operator keeps every quoted route in the 0.6.0 config.
4. A durable **recording hold** is set (§10, procedure).

**Cutover procedure** (each step on staging, then on any other instance):

1. **Stop every recorder.** The upgrade stops the 0.5.0 `topup` process: the scanners, the
   finality watch, the reconciler, and the pumps. The 0.6.0 service starts under the recording
   hold the backfill set. In it, the API serves, but no recorder or pump runs, and issuance
   answers `400 paused`. A settlement pause is not enough: the scanners would keep recording.
2. **Migrate:** the `migrate` service applies the schema and the backfill.
3. **Configure each authorized account** explicitly with its secret key (`POST
   /v1/payment_settings` per mode). The confirmations to use come from the `legacy` revisions,
   which `GET /v1/admin/accounts/{id}` shows.
4. **Verify the effective config:**
   - each account's `GET /v1/admin/accounts/{id}` shows `status: configured` and the expected
     `effective_config` per mode;
   - `GET /v1/config` with each product key lists the expected pairs;
   - no `TopupPaymentSettingsInvalid` is raised.
5. **Resume:** `POST /v1/admin/recording/resume {reason}` (signed, audited) lifts the hold. The
   recorders start from their cursors, so a payment made during the hold is recorded then, bound
   to the configured revision.

**Staging, step 3.** The reference product account (`acct_3a36c44a…`) accepts every staging
route:

```sh
curl -fsS https://pay-api-staging.phala.com/v1/payment_settings \
  -H "Authorization: Bearer $SECRET_TEST_KEY" -H 'content-type: application/json' \
  -d '{"chains": [
        {"chain_id": 11155111, "assets": [{"asset": "pha"}, {"asset": "usdc"}, {"asset": "usdt"}]},
        {"chain_id": 84532, "assets": [{"asset": "pha"}, {"asset": "usdc"}, {"asset": "usdt"}]}]}'
```

The implementation PR's report lists the exact call for every staging account, with the
confirmations from its `legacy` revision.

**Configuration and docs.**

- The route files of every environment (staging, the example, the Phala Cloud template) move
  `quote:` and `limits:` to `merchant:` with explicit bounds, by pull request.
- The setup steps gain `POST /v1/payment_settings` after the treasury, in:
  - [integration](../integration.md);
  - [operator onboarding](../../deploy/README.md#operator-onboarding);
  - [self-hosting](../self-hosting.md);
  - the [reference product setup](../../deploy/phala.md#staging-reference-product).
- The local stacks, the sandbox, the restore drill, the CVM rehearsal, and the `product/web` e2e
  fake service configure every chain and asset they use.

## 11. Restore

**Hold at the freeze.** The settings in a restored database may be stale, and a merchant may not
hold its latest `payment_settings.updated`: it may be undelivered, or the merchant may have no
endpoint. So the transaction that records the restore (`restore_mode::freeze_in`) also sets every
account and mode to `held`. That transaction runs before any recorder:

- `restore-check` freezes before its post-restore reconciliation (`restore.rs` `check`);
- `topup run` detects a new timeline before starting its workers.

From then on:

- every insert by the rescan, the finality watch, the reconciler's missing-deposit pass
  (`reconciler/mod.rs`), or the restore-check round binds `pending`;
- a `pending` deposit waits, with no rejection and no credit, unless its outcome was delivered
  (below);
- issuance answers `payment_settings_unconfirmed`.

**Ending the hold requires the merchant.** After the unfreeze, the merchant sends its complete
chosen configuration with `POST /v1/payment_settings`, its explicit confirmation, which always
writes a new revision, even with an unchanged document. No signed event lifts a hold. A merchant's
`payment_settings.updated` deliveries are history it holds, and cannot prove that no later update
exists: one may be undelivered, for example an asset removed, and an event's `created` has
one-second precision. The service imports none and adds no ordered-version mechanism: the restored
revisions are its history, and each id is random and never reused, so an id never names two
documents across a restore.

The lift is one transaction under the barrier (§7). It takes the state row `FOR UPDATE`, writes
the revision, sets the state `configured`, and binds every `pending` deposit of the account and
mode to that revision. Since recorders hold the row `FOR SHARE` from before their insert to their
commit, the lift sees every deposit recorded as `pending`. A recorder that starts after the lift
reads `configured`.

**Delivered outcomes stand.** A rebuilt deposit whose `deposit.credited` or `deposit.rejected`
was imported keeps that outcome. No later commercial-policy check rewrites it: not acceptance,
not `below_minimum`, not the deposit bounds, not `out_of_bounds` screening. This holds whether the
deposit is bound or `pending`.

- **Chain-identity contradictions** go to the hold and reconciliation path, as today: recipient,
  token, amount, account, or position.
- **A sanctions hit** on a delivered credit is compliance, not commercial policy. It raises the
  sanctioned-deposit alert and blocks sweeping that forwarder (`GET /v1/forwarders?sweepable`).
  It does not rewrite the delivered outcome.

**Quotes.** Re-issuing a quote (`POST /v1/admin/restore/quotes`) restores its id, address, and
client secret, so its payments are found. It does **not** re-authorize acceptance:

- its lock never applies, as today;
- its payments bind like any other: `pending` while held, then to the revision that ends the
  hold.

Re-issued deposit addresses are rebuilt on every chain they had, as today: an address already
given out is never blocked by settings.

## 12. Acceptance scenarios

| Scenario | Expected |
|---|---|
| Concurrent updates | Two `POST`s apply one after the other under the account lock. The later one is current, and each writes a revision and an event. A deposit is bound to the revision current in its insert statement's snapshot, never to a mix. |
| Unconfigured account | `GET /v1/config` lists nothing. Quotes and addresses answer `asset_not_accepted`. A payment to an old address is bound to the `unconfigured` revision, then `rejected(asset_not_accepted)`, and refundable once final and above the route's default dust floor. |
| Reorg successor | The watch reverses the deposit and records the transfer now at the position. The successor binds to the record-time revision of its recipient's account and mode; the reversed deposit keeps its own binding. |
| Late quote payment | A payment after `expires_at`, or out of tolerance, follows its record-time binding at spot: rejected if the asset is no longer accepted, credited at spot otherwise. A valid payment is honoured at the quote's terms even after the asset was removed. |
| Operator tightens a floor | A new route version raises `chain.confirmations`. Uncredited deposits wait for the stricter of the new floor and their bound requirement; credited ones are unaffected. A tightened bound that invalidates a merchant's tuple disables that pair, which raises `TopupPaymentSettingsInvalid`. |
| Lost webhook | A settings change whose `payment_settings.updated` was never delivered is lost in a restore. Every account is `held`, so nothing is decided on stale settings. The merchant re-POSTs after the unfreeze, and the pending deposits bind to the new revision. |
| Delivered-outcome restore | A rebuilt deposit with an imported `deposit.credited` is credited at the delivered credit, even if the restored settings no longer accept its asset or its amount is now out of bounds. One with an imported `deposit.rejected` stays rejected with the delivered reason, even if the settings would now accept it. |
| Restore: an undelivered later update | The merchant removed an asset after the restore point, and that `payment_settings.updated` was never delivered. The account is `held`, and its signed deliveries show the asset still accepted. No delivery lifts the hold, so payments of the removed asset stay `pending` until the merchant re-POSTs its configuration without it; they are then rejected. |
| Restore: a recorder racing the lift | A rescan holds the state row `FOR SHARE`, reads `held`, and records a `pending` deposit. The merchant's re-POST waits for that transaction, then binds the deposit with every other `pending` one. A rescan that starts during the lift waits for it and binds to the new revision. No deposit stays `pending` after the lift. |
| Restore: two updates in one second | Two deliveries carry the same `created`. Neither is used to order or choose a configuration; the merchant's re-POST decides. |
| Restore: no endpoint | The merchant had no webhook endpoint and holds no event. The account stays `held` until the merchant re-POSTs; nothing about its configuration is inferred. |

## 13. What is removed or cut

**Removed:**

- `confirmation_policies`: the table, `GET /v1/account`'s field, and `POST /v1/account` (its only
  parameter; the endpoint is removed). The pause, resume, and webhook key roll endpoints stay.
- The implicit model: acceptance of every routed asset, and a chain enabled by a treasury alone.
- The route's `quote:` and `limits:` sections, and the per-route `max_creations_per_minute`.
- Every read of merchant terms outside `crate::payment_config`, including quote views and refunds
  reading the current route.
- The Python `account.update(confirmation_policies=…)` helper. The SDKs gain
  `payment_settings.retrieve()` and `update()` and the generated types.

**Cut from scope:**

- no gapless or ordered version numbers;
- no history listing, rollback, or named profiles;
- no merchant-controlled rounding;
- no merchant-side caps below the operator's `account_limits`.

## 14. Decisions (2026-10-02, owner, on Astra's review)

1. The only settings event is `payment_settings.updated`.
2. The merchant's confirmation requirement is frozen at binding, and an uncredited deposit waits
   for the stricter of it and the current operator floor.
3. Merchant-side caps are deferred.
4. Every item of the review's change list is applied as written above.
