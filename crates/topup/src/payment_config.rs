//! Per-account payment settings and the one resolver of every commercial term
//! (docs/design/payment-settings.md).
//!
//! The attested routes are the operator's catalog: each route's `merchant` section holds the
//! default and bounds of every term an account may set. Each account has, per mode, one state row
//! naming its current revision of payment settings; revisions are immutable, and an account and
//! mode starts `unconfigured`, accepting nothing. [`resolve`] turns a route and a revision's
//! [`Document`] into the [`Terms`] that govern a quote or a deposit, and every consumer (quotes,
//! deposit addresses, `GET /v1/config`, valuation, screening, refunds, admin, and restore) reads
//! terms only through this module.
//!
//! Binding (design §7): a quote stores the terms it was issued with; a deposit is bound, by the
//! statement that records it, to the revision current in that statement's snapshot, or to the
//! restore that holds its account and mode. Every recorder takes the state row `FOR SHARE`
//! ([`lock_for_recording`]) before that statement, and every writer of the state takes it
//! `FOR UPDATE`, so a hold lift binds every deposit recorded while held.

use std::collections::BTreeMap;

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row};
use topup_core::money::{AtomicAmount, Bps};
use topup_core::route::{Confirmations, RouteFile};
use topup_core::screening::Bounds;
use uuid::Uuid;

use crate::routes::RouteSet;
use crate::tenancy::Scope;

/// One customer's quote creations in a rolling minute when the account sets none.
pub const DEFAULT_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE: u64 = 10;
/// The most quote creations per customer and minute an account may allow.
pub const MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE: u64 = 60;

/// What an account accepts in one mode and on what terms: a revision's document. A value left out
/// takes the operator's default of the route, so the account follows a change of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// One customer's quote creations in a rolling minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_creations_per_customer_per_minute: Option<u64>,
    /// The chains accepted, each with its accepted assets.
    #[serde(default)]
    pub chains: Vec<ChainChoice>,
}

/// An accepted chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainChoice {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The confirmation the account requires, never weaker than the chain's floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmations: Option<Confirmations>,
    /// The accepted assets of the chain.
    pub assets: Vec<AssetChoice>,
}

/// An accepted asset and the terms the account sets on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetChoice {
    /// The asset code, a route's `asset.symbol`.
    pub asset: String,
    /// A quote's payment window, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_ttl_seconds: Option<u64>,
    /// A quote's spread below spot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_spread_bps: Option<Bps>,
    /// A quote's two-sided payment tolerance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_tolerance_bps: Option<Bps>,
    /// The minimum credit, in cents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_amount: Option<u64>,
    /// The minimum creditable deposit, in base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_deposit_atomic: Option<AtomicAmount>,
    /// The maximum creditable deposit, in base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_deposit_atomic: Option<AtomicAmount>,
    /// The refund dust floor, in base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_refund_atomic: Option<AtomicAmount>,
}

impl AssetChoice {
    /// An asset accepted on the operator's defaults.
    #[must_use]
    pub fn on_defaults(asset: impl Into<String>) -> Self {
        Self {
            asset: asset.into(),
            quote_ttl_seconds: None,
            quote_spread_bps: None,
            quote_tolerance_bps: None,
            min_amount: None,
            min_deposit_atomic: None,
            max_deposit_atomic: None,
            min_refund_atomic: None,
        }
    }
}

impl Document {
    /// The account's choice of `chain_id`, if it accepts the chain.
    #[must_use]
    pub fn chain(&self, chain_id: u64) -> Option<&ChainChoice> {
        self.chains.iter().find(|chain| chain.chain_id == chain_id)
    }

    /// The account's choice of `asset` on `chain_id`, with its chain's, if it accepts the asset.
    #[must_use]
    pub fn asset(&self, chain_id: u64, asset: &str) -> Option<(&ChainChoice, &AssetChoice)> {
        let chain = self.chain(chain_id)?;
        chain
            .assets
            .iter()
            .find(|choice| choice.asset == asset)
            .map(|choice| (chain, choice))
    }

    /// A document accepting every route of `livemode` in `routes` on the operator's defaults.
    #[must_use]
    pub fn accepting_all(routes: &RouteSet, livemode: bool) -> Self {
        Self::accepting(routes.current_in(livemode))
    }

    /// A document accepting the asset of each of `routes` on its chain, on the operator's
    /// defaults.
    #[must_use]
    pub fn accepting<'r>(routes: impl IntoIterator<Item = &'r RouteFile>) -> Self {
        let mut chains = BTreeMap::<u64, Vec<AssetChoice>>::new();
        for route in routes {
            let assets = chains.entry(route.chain.chain_id).or_default();
            if !assets.iter().any(|asset| asset.asset == route.asset.symbol) {
                assets.push(AssetChoice::on_defaults(route.asset.symbol.clone()));
            }
        }
        Self {
            quote_creations_per_customer_per_minute: None,
            chains: chains
                .into_iter()
                .map(|(chain_id, assets)| ChainChoice {
                    chain_id,
                    confirmations: None,
                    assets,
                })
                .collect(),
        }
    }
}

/// The terms that govern a quote or a deposit on one route, every value resolved: stored on a
/// quote as it was issued with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    /// A quote's payment window, in seconds.
    pub quote_ttl_seconds: u64,
    /// A quote's spread below spot.
    pub quote_spread_bps: Bps,
    /// A quote's two-sided payment tolerance.
    pub quote_tolerance_bps: Bps,
    /// Token decimals a quote's amount is rounded up to (the operator's).
    pub quote_amount_decimals: u8,
    /// The minimum credit, in cents.
    pub min_amount: u64,
    /// The minimum creditable deposit, in base units.
    pub min_deposit_atomic: AtomicAmount,
    /// The maximum creditable deposit, in base units.
    pub max_deposit_atomic: AtomicAmount,
    /// The refund dust floor, in base units.
    pub min_refund_atomic: AtomicAmount,
    /// The confirmation required when resolved: the stricter of the chain's floor then and the
    /// account's requirement. A deposit not credited yet waits for the stricter of it and the
    /// current floor ([`required_confirmations`]).
    pub confirmations: Confirmations,
}

impl Terms {
    /// The operator's defaults of `route`: the terms of an account that sets none.
    #[must_use]
    pub fn defaults(route: &RouteFile) -> Self {
        let bounds = &route.merchant;
        Self {
            quote_ttl_seconds: bounds.quote_ttl_seconds.default,
            quote_spread_bps: bounds.quote_spread_bps.default,
            quote_tolerance_bps: bounds.quote_tolerance_bps.default,
            quote_amount_decimals: route.asset.quote_amount_decimals,
            min_amount: bounds.min_amount.default,
            min_deposit_atomic: bounds.min_deposit_atomic.default,
            max_deposit_atomic: bounds.max_deposit_atomic.default,
            min_refund_atomic: bounds.min_refund_atomic.default,
            confirmations: route.chain.confirmations,
        }
    }

    /// The deposit bounds screening applies.
    #[must_use]
    pub const fn bounds(&self) -> Bounds {
        Bounds {
            min_atomic: self.min_deposit_atomic,
            max_atomic: self.max_deposit_atomic,
        }
    }

    /// Why the terms cannot be used together, if they cannot (design §5).
    #[must_use]
    pub fn invalid(&self) -> Option<&'static str> {
        if self.min_deposit_atomic > self.max_deposit_atomic {
            return Some("min_deposit_atomic exceeds max_deposit_atomic");
        }
        if self.min_refund_atomic > self.max_deposit_atomic {
            return Some("min_refund_atomic exceeds max_deposit_atomic");
        }
        if self.min_amount == 0 {
            return Some("min_amount is zero");
        }
        None
    }
}

/// What a revision's document says of one route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The document does not accept the route's asset on its chain.
    NotAccepted,
    /// The document accepts it, but its terms, clamped to the route's bounds, cannot be used
    /// together, so the pair is disabled: never quoted or listed, and not accepted.
    Disabled(&'static str),
    /// The document accepts it on these terms.
    Accepted(Terms),
}

impl Resolution {
    /// The terms of an accepted pair.
    #[must_use]
    pub const fn terms(self) -> Option<Terms> {
        match self {
            Self::Accepted(terms) => Some(terms),
            Self::NotAccepted | Self::Disabled(_) => None,
        }
    }
}

/// Resolves `document`'s terms on `route`: each value the account set, or the route's default,
/// clamped to the route's bounds, then validated together.
#[must_use]
pub fn resolve(route: &RouteFile, document: &Document) -> Resolution {
    let Some((chain, asset)) = document.asset(route.chain.chain_id, &route.asset.symbol) else {
        return Resolution::NotAccepted;
    };
    let bounds = &route.merchant;
    let floor = route.chain.confirmations;
    let terms = Terms {
        quote_ttl_seconds: bounds.quote_ttl_seconds.clamp(
            asset
                .quote_ttl_seconds
                .unwrap_or(bounds.quote_ttl_seconds.default),
        ),
        quote_spread_bps: bounds.quote_spread_bps.clamp(
            asset
                .quote_spread_bps
                .unwrap_or(bounds.quote_spread_bps.default),
        ),
        quote_tolerance_bps: bounds.quote_tolerance_bps.clamp(
            asset
                .quote_tolerance_bps
                .unwrap_or(bounds.quote_tolerance_bps.default),
        ),
        quote_amount_decimals: route.asset.quote_amount_decimals,
        min_amount: bounds
            .min_amount
            .clamp(asset.min_amount.unwrap_or(bounds.min_amount.default)),
        min_deposit_atomic: bounds.min_deposit_atomic.clamp(
            asset
                .min_deposit_atomic
                .unwrap_or(bounds.min_deposit_atomic.default),
        ),
        max_deposit_atomic: bounds.max_deposit_atomic.clamp(
            asset
                .max_deposit_atomic
                .unwrap_or(bounds.max_deposit_atomic.default),
        ),
        min_refund_atomic: bounds.min_refund_atomic.clamp(
            asset
                .min_refund_atomic
                .unwrap_or(bounds.min_refund_atomic.default),
        ),
        confirmations: chain
            .confirmations
            .map_or(floor, |required| floor.stricter(required)),
    };
    match terms.invalid() {
        Some(reason) => Resolution::Disabled(reason),
        None => Resolution::Accepted(terms),
    }
}

/// One customer's quote creations per minute under `document`, within the bounds.
#[must_use]
pub fn quote_creations_per_customer_per_minute(document: &Document) -> u64 {
    document
        .quote_creations_per_customer_per_minute
        .unwrap_or(DEFAULT_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE)
        .clamp(1, MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE)
}

/// The confirmation a deposit not credited yet waits for: the stricter of the chain's current
/// floor and every requirement bound to it.
#[must_use]
pub fn required_confirmations(
    floor: Confirmations,
    bound: impl IntoIterator<Item = Confirmations>,
) -> Confirmations {
    bound
        .into_iter()
        .fold(floor, |required, other| required.stricter(other))
}

/// An account and mode's settings state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Accepts nothing: never configured.
    Unconfigured,
    /// Accepts what its current revision lists.
    Configured,
    /// Held after a restore until the merchant reconfirms its configuration.
    Held,
}

impl Status {
    /// The stored and API code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::Configured => "configured",
            Self::Held => "held",
        }
    }

    fn parse(code: &str) -> Result<Self, sqlx::Error> {
        match code {
            "unconfigured" => Ok(Self::Unconfigured),
            "configured" => Ok(Self::Configured),
            "held" => Ok(Self::Held),
            other => Err(decode(format!("unknown payment settings status `{other}`"))),
        }
    }
}

/// An account and mode's current payment settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The state.
    pub status: Status,
    /// The current revision.
    pub revision: Uuid,
    /// The current revision's document.
    pub document: Document,
    /// When the current revision was written.
    pub updated: DateTime<Utc>,
}

fn decode(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

fn parse_document(value: serde_json::Value) -> Result<Document, sqlx::Error> {
    serde_json::from_value(value).map_err(|error| decode(format!("invalid settings: {error}")))
}

async fn read_settings(
    connection: &mut PgConnection,
    scope: Scope,
    lock: &str,
) -> Result<Settings, sqlx::Error> {
    // The state row is locked by its own statement, then read with the revision by the next one:
    // a statement that waited for another writer's commit locks the state row that writer left,
    // but its snapshot predates the revision that writer added, so a single statement joining
    // the two would find no row (READ COMMITTED takes a snapshot per statement).
    if !lock.is_empty() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT 1 FROM payment_settings_state AS state \
             WHERE state.account_id = $1 AND state.livemode = $2 {lock}"
        )))
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_one(&mut *connection)
        .await?;
    }
    let row = sqlx::query(
        "SELECT state.status, state.current_revision_id, revision.document, revision.created_at \
         FROM payment_settings_state AS state \
         JOIN payment_settings_revisions AS revision ON revision.id = state.current_revision_id \
         WHERE state.account_id = $1 AND state.livemode = $2",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_one(connection)
    .await?;
    Ok(Settings {
        status: Status::parse(row.try_get("status")?)?,
        revision: row.try_get("current_revision_id")?,
        document: parse_document(row.try_get("document")?)?,
        updated: row.try_get("created_at")?,
    })
}

/// The scope's current settings.
pub async fn load(connection: &mut PgConnection, scope: Scope) -> Result<Settings, sqlx::Error> {
    read_settings(connection, scope, "").await
}

/// The scope's current settings, with its state row locked `FOR UPDATE` to the end of the
/// caller's transaction: a writer reads the settings it changes under the lock that orders it.
pub async fn load_for_update(
    connection: &mut PgConnection,
    scope: Scope,
) -> Result<Settings, sqlx::Error> {
    read_settings(connection, scope, "FOR UPDATE").await
}

/// The scope's current settings, with its state row locked `FOR SHARE` to the end of the
/// caller's transaction, so the revision stays current until it commits: a quote resolved from it
/// is issued under it.
pub async fn load_for_share(
    connection: &mut PgConnection,
    scope: Scope,
) -> Result<Settings, sqlx::Error> {
    read_settings(connection, scope, "FOR SHARE").await
}

/// The barrier every recorder of a deposit takes before its insert (design §7): the state row of
/// the account and mode of `address_id`, `FOR SHARE` to the end of the caller's transaction. The
/// insert is a later statement, so its snapshot is taken after this lock is held.
pub async fn lock_for_recording(
    connection: &mut PgConnection,
    address_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT 1 FROM payment_settings_state AS state \
         JOIN addresses AS address \
           ON address.account_id = state.account_id AND address.livemode = state.livemode \
         WHERE address.id = $1 FOR SHARE OF state",
    )
    .bind(address_id)
    .fetch_optional(connection)
    .await?;
    Ok(())
}

/// [`lock_for_recording`] for a recorder that names the account and mode itself: a reversed
/// deposit restored from its delivered `deposit.reversed` (`crate::restore_mode`).
pub async fn lock_scope_for_recording(
    connection: &mut PgConnection,
    scope: Scope,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT 1 FROM payment_settings_state WHERE account_id = $1 AND livemode = $2 FOR SHARE",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(connection)
    .await?;
    Ok(())
}

/// What a written change did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Written {
    /// Whether a revision was written.
    pub changed: bool,
    /// Deposits recorded while held that the write bound to its revision.
    pub bound: u64,
}

/// Writes `document` as the scope's current settings, unless it is the current configured
/// document; a write while `held` always writes a revision, the merchant's reconfirmation, and
/// binds every deposit recorded while held to it (design §11). The state row is locked
/// `FOR UPDATE`, the writer's side of the barrier. The caller writes the audit row and event.
pub async fn write(
    connection: &mut PgConnection,
    scope: Scope,
    document: &Document,
    created_by: &str,
) -> Result<Written, sqlx::Error> {
    let current = load_for_update(connection, scope).await?;
    if current.status == Status::Configured && current.document == *document {
        return Ok(Written {
            changed: false,
            bound: 0,
        });
    }
    let revision = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO payment_settings_revisions \
             (id, account_id, livemode, kind, document, created_by) \
         VALUES ($1, $2, $3, 'configured', $4, $5)",
    )
    .bind(revision)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(sqlx::types::Json(document))
    .bind(created_by)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE payment_settings_state \
         SET status = 'configured', current_revision_id = $3, held_by = NULL \
         WHERE account_id = $1 AND livemode = $2",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(revision)
    .execute(&mut *connection)
    .await?;
    let bound = if current.status == Status::Held {
        sqlx::query(
            "UPDATE deposits SET settings_revision_id = $3, settings_hold_id = NULL \
             WHERE account_id = $1 AND livemode = $2 AND settings_hold_id IS NOT NULL",
        )
        .bind(scope.account_id())
        .bind(scope.livemode())
        .bind(revision)
        .execute(&mut *connection)
        .await?
        .rows_affected()
    } else {
        0
    };
    Ok(Written {
        changed: true,
        bound,
    })
}

/// Holds every account and mode after the restore `restore_id` (design §11): its settings may
/// be stale, so nothing is decided on them until the merchant reconfirms. Run in the transaction
/// that records the restore, before any recorder.
pub async fn hold_all(connection: &mut PgConnection, restore_id: Uuid) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE payment_settings_state AS state SET status = 'held', held_by = $1 \
         FROM (SELECT account_id, livemode FROM payment_settings_state \
               ORDER BY account_id, livemode FOR UPDATE) AS locked \
         WHERE state.account_id = locked.account_id AND state.livemode = locked.livemode",
    )
    .bind(restore_id)
    .execute(connection)
    .await
    .map(|result| result.rows_affected())
}

/// The payment settings a deposit is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A revision of its account and mode.
    Revision {
        /// The revision id.
        id: Uuid,
        /// Its document.
        document: Document,
    },
    /// Recorded while its account and mode was held after the restore `restore`; it waits for
    /// the merchant's reconfirmation, which binds it.
    Pending {
        /// The restore whose hold it was recorded under.
        restore: Uuid,
    },
}

impl Binding {
    /// The bound account's confirmation requirement on `chain_id`, if any.
    #[must_use]
    pub fn confirmations(&self, chain_id: u64) -> Option<Confirmations> {
        match self {
            Self::Revision { document, .. } => document
                .chain(chain_id)
                .and_then(|chain| chain.confirmations),
            Self::Pending { .. } => None,
        }
    }

    /// What the binding says of `route`; `None` while pending.
    #[must_use]
    pub fn resolve(&self, route: &RouteFile) -> Option<Resolution> {
        match self {
            Self::Revision { document, .. } => Some(resolve(route, document)),
            Self::Pending { .. } => None,
        }
    }
}

/// The binding of deposit `deposit_id`.
pub async fn deposit_binding(
    connection: &mut PgConnection,
    deposit_id: Uuid,
) -> Result<Binding, sqlx::Error> {
    let row = sqlx::query(
        "SELECT deposit.settings_revision_id, deposit.settings_hold_id, revision.document \
         FROM deposits AS deposit \
         LEFT JOIN payment_settings_revisions AS revision \
           ON revision.id = deposit.settings_revision_id \
         WHERE deposit.id = $1",
    )
    .bind(deposit_id)
    .fetch_one(connection)
    .await?;
    let revision: Option<Uuid> = row.try_get("settings_revision_id")?;
    let hold: Option<Uuid> = row.try_get("settings_hold_id")?;
    match (revision, hold) {
        (Some(id), None) => Ok(Binding::Revision {
            id,
            document: parse_document(row.try_get("document")?)?,
        }),
        (None, Some(restore)) => Ok(Binding::Pending { restore }),
        _ => Err(decode("a deposit has no settings binding".to_owned())),
    }
}

/// The terms stored on quote `quote_id`, as it was issued with them.
pub async fn quote_terms(
    connection: &mut PgConnection,
    quote_id: Uuid,
) -> Result<Terms, sqlx::Error> {
    let terms: serde_json::Value = sqlx::query_scalar("SELECT terms FROM quotes WHERE id = $1")
        .bind(quote_id)
        .fetch_one(connection)
        .await?;
    serde_json::from_value(terms).map_err(|error| decode(format!("invalid quote terms: {error}")))
}

/// The terms that govern a recorded deposit after its valuation: those of the quote it paid
/// (valued at the quote's lock), or those its binding resolves on its route version in `routes`.
/// `None` for a deposit its binding does not accept, one still pending, or one without a route.
pub async fn deposit_terms(
    connection: &mut PgConnection,
    routes: &RouteSet,
    deposit_id: Uuid,
) -> Result<Option<Terms>, sqlx::Error> {
    match recorded_route(connection, routes, deposit_id).await? {
        Some(route) => deposit_terms_on(connection, route, deposit_id).await,
        None => Ok(None),
    }
}

/// The deposit's recorded route version; an unloaded bound version is an error.
async fn recorded_route<'r>(
    connection: &mut PgConnection,
    routes: &'r RouteSet,
    deposit_id: Uuid,
) -> Result<Option<&'r RouteFile>, sqlx::Error> {
    let (route, version): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT route, route_version FROM deposits WHERE id = $1")
            .bind(deposit_id)
            .fetch_one(&mut *connection)
            .await?;
    let (route, version) = match (route, version) {
        (None, None) => return Ok(None),
        (Some(route), Some(version)) => (route, version),
        _ => return Err(decode("incomplete deposit route binding".to_owned())),
    };
    let version = u64::try_from(version).map_err(|_| decode("route_version".to_owned()))?;
    routes
        .routes()
        .iter()
        .find(|candidate| candidate.route == route && candidate.version == version)
        .map(Some)
        .ok_or_else(|| {
            decode(format!(
                "deposit route `{route}` version {version} is not loaded"
            ))
        })
}

/// [`deposit_terms`] of a deposit recorded on `route`.
pub async fn deposit_terms_on(
    connection: &mut PgConnection,
    route: &RouteFile,
    deposit_id: Uuid,
) -> Result<Option<Terms>, sqlx::Error> {
    let quote: Option<Uuid> = sqlx::query_scalar(
        "SELECT quote.id FROM deposits AS deposit \
         JOIN quotes AS quote ON quote.consumed_by = deposit.id \
         WHERE deposit.id = $1 AND deposit.price_source = 'lock'",
    )
    .bind(deposit_id)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(quote) = quote {
        return quote_terms(connection, quote).await.map(Some);
    }
    Ok(deposit_binding(connection, deposit_id)
        .await?
        .resolve(route)
        .and_then(Resolution::terms))
}

/// A deposit's refund dust floor (design §9): its governing terms' floor, or, for a deposit no
/// terms govern (not accepted, or still pending), the operator's default of the route version it
/// was recorded on, so a later default never strands it. `route` (the token's current route)
/// stands in only for an unrouted deposit; a missing bound version fails closed.
pub async fn refund_floor(
    connection: &mut PgConnection,
    routes: &RouteSet,
    deposit_id: Uuid,
    route: &RouteFile,
) -> Result<AtomicAmount, sqlx::Error> {
    let Some(recorded) = recorded_route(connection, routes, deposit_id).await? else {
        return Ok(route.merchant.min_refund_atomic.default);
    };
    Ok(deposit_terms_on(connection, recorded, deposit_id)
        .await?
        .map_or(recorded.merchant.min_refund_atomic.default, |terms| {
            terms.min_refund_atomic
        }))
}

/// A pair the account accepts and can be quoted on now.
#[derive(Clone, Debug)]
pub struct EffectiveAsset<'r> {
    /// The current route of the pair.
    pub route: &'r RouteFile,
    /// Its resolved terms.
    pub terms: Terms,
    /// The account's current treasury of the chain, if it has one.
    pub treasury: Option<Address>,
}

/// The effective config of an account and mode (design §6): its current settings, and every
/// pair of the mode's current routes they accept and enable, with the chain's treasury. A held
/// account accepts nothing until it reconfirms.
#[derive(Clone, Debug)]
pub struct Effective<'r> {
    /// The current settings.
    pub settings: Settings,
    /// One customer's quote creations per minute.
    pub quote_creations_per_customer_per_minute: u64,
    /// The accepted and enabled pairs.
    pub assets: Vec<EffectiveAsset<'r>>,
}

impl<'r> Effective<'r> {
    /// The accepted pairs on chains with an active treasury: what quotes, deposit addresses, and
    /// `GET /v1/config` offer.
    pub fn active(&self) -> impl Iterator<Item = &EffectiveAsset<'r>> {
        self.assets.iter().filter(|asset| asset.treasury.is_some())
    }

    /// The accepted pair of `asset` on `chain_id`.
    #[must_use]
    pub fn asset(&self, chain_id: u64, asset: &str) -> Option<&EffectiveAsset<'r>> {
        self.assets.iter().find(|candidate| {
            candidate.route.chain.chain_id == chain_id && candidate.route.asset.symbol == asset
        })
    }
}

/// Resolves the effective config of `settings` for `scope`.
pub async fn effective<'r>(
    connection: &mut PgConnection,
    routes: &'r RouteSet,
    scope: Scope,
    settings: Settings,
) -> Result<Effective<'r>, sqlx::Error> {
    let treasuries = crate::treasuries::current(connection, scope)
        .await
        .map_err(|error| match error {
            crate::treasuries::TreasuryError::Database(error) => error,
            _ => decode("a treasury row is invalid".to_owned()),
        })?;
    let assets = if settings.status == Status::Held {
        Vec::new()
    } else {
        routes
            .current_in(scope.livemode())
            .filter_map(|route| {
                resolve(route, &settings.document)
                    .terms()
                    .map(|terms| EffectiveAsset {
                        route,
                        terms,
                        treasury: treasuries.get(&route.chain.chain_id).copied(),
                    })
            })
            .collect()
    };
    Ok(Effective {
        quote_creations_per_customer_per_minute: quote_creations_per_customer_per_minute(
            &settings.document,
        ),
        settings,
        assets,
    })
}

/// The scope's current effective config.
pub async fn load_effective<'r>(
    connection: &mut PgConnection,
    routes: &'r RouteSet,
    scope: Scope,
) -> Result<Effective<'r>, sqlx::Error> {
    let settings = load(connection, scope).await?;
    effective(connection, routes, scope, settings).await
}

/// Accounts and modes whose settings accept a pair whose terms are disabled: the operator
/// tightened a bound below a merchant's value (`TopupPaymentSettingsInvalid`).
pub async fn invalid_pairs(
    pool: &PgPool,
    routes: &RouteSet,
) -> Result<Vec<(Uuid, bool, String, &'static str)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT state.account_id, state.livemode, revision.document \
         FROM payment_settings_state AS state \
         JOIN payment_settings_revisions AS revision ON revision.id = state.current_revision_id \
         WHERE state.status = 'configured'",
    )
    .fetch_all(pool)
    .await?;
    let mut invalid = Vec::new();
    for row in rows {
        let livemode: bool = row.try_get("livemode")?;
        let document = parse_document(row.try_get("document")?)?;
        for route in routes.current_in(livemode) {
            if let Resolution::Disabled(reason) = resolve(route, &document) {
                invalid.push((
                    row.try_get("account_id")?,
                    livemode,
                    route.route.clone(),
                    reason,
                ));
            }
        }
    }
    Ok(invalid)
}

/// Raises `TopupPaymentSettingsInvalid` for each accepted pair the current catalog disables.
pub async fn report_invalid(pool: &PgPool, routes: &RouteSet) -> Result<(), sqlx::Error> {
    for (account_id, livemode, route, reason) in invalid_pairs(pool, routes).await? {
        tracing::error!(
            tags.alert = "TopupPaymentSettingsInvalid",
            %account_id,
            livemode,
            route = %route,
            reason,
            "an account's payment settings accept a pair the route's bounds disable; it is not \
             quoted, listed, or credited until the account or the operator changes it"
        );
    }
    Ok(())
}

/// Whether an existing payment-settings cutover still requires an upgrade through 0.9.x.
pub async fn cutover_incomplete(connection: &mut PgConnection) -> Result<bool, sqlx::Error> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.payment_settings_cutover') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    if !exists {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM payment_settings_cutover WHERE recording_resumed_at IS NULL)",
    )
    .fetch_one(connection)
    .await
}

#[cfg(test)]
mod tests {
    use topup_core::route::Bounded;

    use super::*;

    fn route() -> RouteFile {
        serde_saphyr::from_str(include_str!("../tests/fixtures/phala-cloud-pha.yaml"))
            .expect("fixture route")
    }

    fn accepting(choice: AssetChoice, confirmations: Option<Confirmations>) -> Document {
        Document {
            quote_creations_per_customer_per_minute: None,
            chains: vec![ChainChoice {
                chain_id: 1,
                confirmations,
                assets: vec![choice],
            }],
        }
    }

    #[test]
    fn nothing_is_accepted_until_listed() {
        let route = route();
        assert_eq!(
            resolve(&route, &Document::default()),
            Resolution::NotAccepted
        );
        let other_asset = accepting(AssetChoice::on_defaults("usdc"), None);
        assert_eq!(resolve(&route, &other_asset), Resolution::NotAccepted);
        assert_eq!(
            resolve(&route, &accepting(AssetChoice::on_defaults("pha"), None)),
            Resolution::Accepted(Terms::defaults(&route))
        );
    }

    #[test]
    fn values_are_clamped_to_the_bounds_and_confirmations_never_weaken() {
        let mut route = route();
        route.merchant.quote_spread_bps = Bounded {
            default: Bps::new(50).expect("bps"),
            min: Bps::default(),
            max: Bps::new(100).expect("bps"),
        };
        let choice = AssetChoice {
            quote_spread_bps: Some(Bps::new(300).expect("bps")),
            ..AssetChoice::on_defaults("pha")
        };
        let terms = resolve(
            &route,
            &accepting(choice.clone(), Some(Confirmations::Depth(1))),
        )
        .terms()
        .expect("accepted");
        // A value set before the operator tightened the bound is held to the new bound.
        assert_eq!(terms.quote_spread_bps, Bps::new(100).expect("bps"));
        // A requirement weaker than the floor never lowers it.
        assert_eq!(terms.confirmations, route.chain.confirmations);
        let finalized = resolve(&route, &accepting(choice, Some(Confirmations::Finalized)))
            .terms()
            .expect("accepted");
        assert_eq!(finalized.confirmations, Confirmations::Finalized);
    }

    #[test]
    fn a_tightened_bound_that_breaks_the_terms_disables_the_pair() {
        let mut route = route();
        let choice = AssetChoice {
            min_deposit_atomic: Some(route.merchant.max_deposit_atomic.default),
            ..AssetChoice::on_defaults("pha")
        };
        let document = accepting(choice, None);
        assert!(resolve(&route, &document).terms().is_some());
        // The operator lowers the maximum deposit below the merchant's minimum.
        route.merchant.min_deposit_atomic.max = route.merchant.max_deposit_atomic.default;
        route.merchant.max_deposit_atomic = Bounded {
            default: AtomicAmount::new(alloy_primitives::U256::from(1_u8)),
            min: AtomicAmount::default(),
            max: AtomicAmount::new(alloy_primitives::U256::from(1_u8)),
        };
        assert_eq!(
            resolve(&route, &document),
            Resolution::Disabled("min_deposit_atomic exceeds max_deposit_atomic")
        );
    }

    #[test]
    fn a_deposit_waits_for_the_current_floor_when_it_is_stricter_than_its_binding() {
        assert_eq!(
            required_confirmations(Confirmations::Depth(2), [Confirmations::Depth(5)]),
            Confirmations::Depth(5)
        );
        assert_eq!(
            required_confirmations(Confirmations::Finalized, [Confirmations::Depth(5)]),
            Confirmations::Finalized
        );
    }

    #[test]
    fn the_quote_creation_rate_stays_within_its_bounds() {
        assert_eq!(
            quote_creations_per_customer_per_minute(&Document::default()),
            DEFAULT_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE
        );
        let document = Document {
            quote_creations_per_customer_per_minute: Some(1_000),
            chains: Vec::new(),
        };
        assert_eq!(
            quote_creations_per_customer_per_minute(&document),
            MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE
        );
    }
}
