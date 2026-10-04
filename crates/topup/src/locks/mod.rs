//! Quote-first rate-lock lifecycle, exposure reservations, and expiry processing.

pub mod pricing;

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address as EvmAddress, U256};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::types::Json;
use sqlx::{Acquire, FromRow, PgPool, Postgres, Row, Transaction};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use topup_core::address::{forwarder_address, quote_salt};
use topup_core::money::{
    AtomicAmount, MinorAmount, PRICE_SCALE, ScaledPrice, lock_price, round_up_to_decimals,
    tokens_for_credit,
};
use topup_core::route::{RouteFile, UNIT_DECIMALS};
use uuid::Uuid;

use crate::audit::{self, Actor};
use crate::client_secret::ClientSecretKey;
use crate::db::{Account, Customer};
use crate::payment_config::{self, Resolution, Status, Terms};
use crate::routes::RouteSet;
use crate::tenancy::Scope;
use pricing::{PricingRuntime, ValidatedQuote};

/// The columns of [`RateLockRow`]; callers append the `WHERE` clause with `concat!`.
macro_rules! select_lock {
    () => {
        r#"
    SELECT quote.id, quote.livemode, address.id AS address_id, customer.client_reference_id,
           quote.route,
           address.chain_id, address.address, address.treasury,
           quote.amount_atomic::text AS amount_atomic,
           quote.price_scaled::text AS price_scaled,
           quote.credit_minor::text AS credit_minor,
           quote.expires_at, quote.status, quote.created_at, quote.consumed_by, quote.metadata,
           quote.route_version, quote.terms
    FROM quotes AS quote
    JOIN addresses AS address ON address.quote_id = quote.id
    JOIN customers AS customer ON customer.id = quote.customer_id"#
    };
}

const EXPIRY_BATCH_SIZE: i64 = 100;

/// Current persisted rate-lock status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateLockStatus {
    /// Awaiting its first eligible payment.
    Open,
    /// Used by an eligible payment.
    Consumed,
    /// Passed its payment window without consumption.
    Expired,
    /// Cancelled before payment.
    Cancelled,
}

impl RateLockStatus {
    /// Returns the stable API and database status code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Consumed => "consumed",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, RateLockError> {
        match value {
            "open" => Ok(Self::Open),
            "consumed" => Ok(Self::Consumed),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(RateLockError::DatabaseInvariant),
        }
    }
}

/// Merchant-visible immutable rate-lock facts and lifecycle state: the API's quote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLock {
    /// Quote identifier.
    pub id: Uuid,
    /// The quote's mode.
    pub livemode: bool,
    /// The quote's address row.
    pub address_id: Uuid,
    /// The customer's `client_reference_id`.
    pub client_reference_id: String,
    /// Route name.
    pub route: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Single-use forwarder address.
    pub address: EvmAddress,
    /// The treasury the address pays: the account's treasury of the chain when it was issued.
    pub treasury: EvmAddress,
    /// Exact requested token amount.
    pub amount_atomic: AtomicAmount,
    /// Frozen eight-decimal price.
    pub price: ScaledPrice,
    /// Frozen credit.
    pub credit_minor: MinorAmount,
    /// Payment deadline.
    pub expires_at: DateTime<Utc>,
    /// Persisted lifecycle status.
    pub status: RateLockStatus,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// The deposit that consumed the lock, once consumed.
    pub consumed_by: Option<Uuid>,
    /// The merchant's metadata (`crate::api` validates it).
    pub metadata: BTreeMap<String, String>,
    /// The route version the quote was issued on.
    pub route_version: u64,
    /// The terms it was issued with, kept for good (docs/design/payment-settings.md §8).
    pub terms: Terms,
}

/// The public id of a quote: `qt_` and the hex of its id. Quotes derive their address salt from
/// it, so a merchant can recompute the address from the id alone.
#[must_use]
pub fn quote_id(id: Uuid) -> String {
    crate::ids::format(crate::ids::QUOTE, id)
}

/// A validated quote source used by rate-lock creation.
#[async_trait]
pub trait QuoteProvider: Send + Sync {
    /// Fetches and validates the current spot price for the selected route.
    async fn quote(&self, route: &RouteFile) -> Result<ValidatedQuote, Value>;
}

/// Production quote provider configured from attested route files.
pub struct ConfiguredQuoteProvider {
    runtimes: BTreeMap<(String, u64), PricingRuntime>,
}

impl ConfiguredQuoteProvider {
    /// Builds adapters for every route version.
    pub fn from_routes(routes: &crate::routes::RouteSet) -> Result<Self, String> {
        let mut runtimes = BTreeMap::new();
        for route in routes.routes() {
            let key = (route.route.clone(), route.version);
            if runtimes
                .insert(key.clone(), PricingRuntime::configured(route, routes)?)
                .is_some()
            {
                return Err(format!(
                    "duplicate pricing runtime for route `{}` version {}",
                    key.0, key.1
                ));
            }
        }
        Ok(Self { runtimes })
    }
}

#[async_trait]
impl QuoteProvider for ConfiguredQuoteProvider {
    async fn quote(&self, route: &RouteFile) -> Result<ValidatedQuote, Value> {
        let runtime = self
            .runtimes
            .get(&(route.route.clone(), route.version))
            .ok_or_else(|| json!({"stage": "pricing", "error": "missing_route_runtime"}))?;
        runtime.fetch(route).await
    }
}

/// Quote provider used by API surfaces that do not exercise rate-lock creation.
pub struct UnavailableQuoteProvider;

#[async_trait]
impl QuoteProvider for UnavailableQuoteProvider {
    async fn quote(&self, _route: &RouteFile) -> Result<ValidatedQuote, Value> {
        Err(json!({"stage": "pricing", "error": "unavailable"}))
    }
}

/// Rate-lock lifecycle failure mapped by the API boundary.
#[derive(Debug, thiserror::Error)]
pub enum RateLockError {
    /// Request input is invalid.
    #[error("{0}")]
    InvalidInput(&'static str),
    /// The requested credit, or its token amount, is below the route's minimum.
    #[error("{0}")]
    AmountTooSmall(&'static str),
    /// The requested credit's token amount is above the route's maximum deposit.
    #[error("{0}")]
    AmountTooLarge(&'static str),
    /// Current validated pricing is unavailable.
    #[error("validated pricing is unavailable")]
    PricingUnavailable,
    /// The account has no treasury on the route's chain.
    #[error("no treasury is set on the chain")]
    TreasuryNotSet,
    /// The account's payment settings do not accept the route's asset on its chain.
    #[error("the account does not accept the asset")]
    AssetNotAccepted,
    /// The account's payment settings are held after a restore until it reconfirms them.
    #[error("the account's payment settings await reconfirmation")]
    SettingsUnconfirmed,
    /// Recording is held for the cutover, so a payment would not be seen.
    #[error("recording is held")]
    RecordingHeld,
    /// The account's payment settings changed between pricing and creation; retry.
    #[error("the account's payment settings changed")]
    SettingsChanged,
    /// The per-customer rolling creation limit was reached; a creation is admitted again after
    /// `retry_after` seconds.
    #[error("rate-lock creation limit exceeded")]
    RateLimited {
        /// Seconds until the customer's oldest counted creation leaves the minute.
        retry_after: u64,
    },
    /// A cap on the credit of open quotes would be exceeded.
    #[error("the {scope} cap on open quotes leaves {remaining} cents")]
    ExposureCap {
        /// `customer` or `account` (the account in the quote's mode).
        scope: &'static str,
        /// Credit, in minor units, still available under the cap.
        remaining: u64,
    },
    /// The account already holds its cap of open quotes in the mode.
    #[error("the account holds its cap of {0} open quotes in this mode")]
    QuoteCountCap(u64),
    /// The tenant-scoped lock does not exist.
    #[error("rate lock not found")]
    NotFound,
    /// The lock can no longer be cancelled.
    #[error("rate lock is {}", .0.code())]
    NotOpen(RateLockStatus),
    /// The lock is still open but its payment window has closed, so it cannot be cancelled.
    #[error("payment window has closed")]
    WindowClosed,
    /// The lock address already received a deposit, so it cannot be cancelled.
    #[error("rate lock address already received a payment")]
    PendingPayment,
    /// Money arithmetic could not be represented.
    #[error("rate-lock arithmetic is out of range")]
    Arithmetic,
    /// The operating system's random number generator failed.
    #[error("operating system entropy is unavailable")]
    EntropyUnavailable,
    /// Persisted data violated an internal invariant.
    #[error("rate-lock database invariant failed")]
    DatabaseInvariant,
    /// PostgreSQL rejected or failed the operation.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

/// Creates a lock for `credit` on `route` for `customer` of `account`, and returns it with its
/// `client_secret`, issued in the same transaction: a quote never exists without the secret its
/// creation returned, so a failed creation leaves nothing a retry would duplicate. It prices the
/// lock ([`price`]) and then creates it in its own transaction ([`create_in`]).
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    quotes: &Arc<dyn QuoteProvider>,
    client_secrets: &ClientSecretKey,
    account: &Account,
    customer: &Customer,
    route: &RouteFile,
    credit_minor: MinorAmount,
    metadata: &BTreeMap<String, String>,
) -> Result<(RateLock, String), RateLockError> {
    let priced = price(pool, quotes, account, customer, route, credit_minor).await?;
    let mut transaction = pool.begin().await?;
    let created = create_in(
        &mut transaction,
        client_secrets,
        account,
        customer,
        route,
        &priced,
        metadata,
    )
    .await?;
    transaction.commit().await?;
    Ok(created)
}

/// A lock's price and amount, fetched and checked before the transaction that creates it, with
/// the terms they were resolved from.
#[derive(Clone, Copy, Debug)]
pub struct PricedLock {
    credit_minor: MinorAmount,
    price: ScaledPrice,
    amount_atomic: AtomicAmount,
    revision: Uuid,
    terms: Terms,
    creations_per_minute: u64,
}

/// Prices a lock for `credit_minor` on `route` for `customer` of `account` with the external
/// price fetch, before any transaction; [`create_in`] then creates it.
///
/// The quote is scoped to the customer's account and mode, and `route` must be a route of that
/// mode. Repeated requests are answered by the API's `Idempotency-Key` layer before they reach
/// this function.
pub async fn price(
    pool: &PgPool,
    quotes: &Arc<dyn QuoteProvider>,
    account: &Account,
    customer: &Customer,
    route: &RouteFile,
    credit_minor: MinorAmount,
) -> Result<PricedLock, RateLockError> {
    if customer.account_id != account.id {
        return Err(RateLockError::NotFound);
    }
    if route.livemode != customer.livemode {
        return Err(RateLockError::InvalidInput(
            "the route's mode differs from the customer's",
        ));
    }
    let scope = Scope::new(account.id, customer.livemode);
    let mut connection = pool.acquire().await?;
    if payment_config::recording_held(&mut connection).await? {
        return Err(RateLockError::RecordingHeld);
    }
    // The account's terms on the route (docs/design/payment-settings.md §6); `create_in` checks
    // that the revision is still current when it creates the quote.
    let settings = payment_config::load(&mut connection, scope).await?;
    if settings.status == Status::Held {
        return Err(RateLockError::SettingsUnconfirmed);
    }
    let Resolution::Accepted(terms) = payment_config::resolve(route, &settings.document) else {
        return Err(RateLockError::AssetNotAccepted);
    };
    let creations_per_minute =
        payment_config::quote_creations_per_customer_per_minute(&settings.document);
    // Cheap pre-checks so a rate-limited caller, or one without a treasury, never triggers an
    // external price fetch; the authoritative checks repeat in `create_in`.
    check_creation_rate(&mut connection, customer.id, creations_per_minute).await?;
    treasury(&mut connection, scope, route.chain.chain_id).await?;
    drop(connection);

    let quote = quotes
        .quote(route)
        .await
        .map_err(|evidence| {
            tracing::warn!(route = %route.route, quote = %evidence, "rate-lock price validation failed");
            RateLockError::PricingUnavailable
        })?;
    let locked_price =
        lock_price(quote.price, terms.quote_spread_bps).map_err(|_| RateLockError::Arithmetic)?;
    if locked_price.value() == 0 {
        return Err(RateLockError::PricingUnavailable);
    }
    let amount_atomic = amount_for_credit(route, &terms, credit_minor, locked_price)?;
    validate_bounds(&terms, amount_atomic, credit_minor)?;
    Ok(PricedLock {
        credit_minor,
        price: locked_price,
        amount_atomic,
        revision: settings.revision,
        terms,
        creations_per_minute,
    })
}

/// Creates the lock `priced` for `customer` of `account` in `transaction`, with its
/// `client_secret`, after checking the customer's creation rate, the exposure caps, and the
/// chain's treasury again; its window starts now.
pub async fn create_in(
    transaction: &mut Transaction<'_, Postgres>,
    client_secrets: &ClientSecretKey,
    account: &Account,
    customer: &Customer,
    route: &RouteFile,
    priced: &PricedLock,
    metadata: &BTreeMap<String, String>,
) -> Result<(RateLock, String), RateLockError> {
    if customer.account_id != account.id || route.livemode != customer.livemode {
        return Err(RateLockError::NotFound);
    }
    let PricedLock {
        credit_minor,
        price: locked_price,
        amount_atomic,
        revision,
        terms,
        creations_per_minute,
    } = *priced;
    let scope = Scope::new(account.id, customer.livemode);
    let window = i64::try_from(terms.quote_ttl_seconds).map_err(|_| RateLockError::Arithmetic)?;

    // One consistent resolution: the revision the quote was priced under, still current and held
    // so `FOR SHARE` until the quote commits (docs/design/payment-settings.md §7). A restore's hold
    // keeps the revision, so it is checked on its own.
    let current = payment_config::load_for_share(transaction, scope).await?;
    if current.status == Status::Held {
        return Err(RateLockError::SettingsUnconfirmed);
    }
    if current.revision != revision {
        return Err(RateLockError::SettingsChanged);
    }
    lock_customer(transaction, customer).await?;
    check_creation_rate(transaction, customer.id, creations_per_minute).await?;
    check_exposure(transaction, account, customer, credit_minor).await?;
    // The account's current treasury of the chain; the shared lock, held to commit, keeps a
    // treasury change from applying meanwhile. The address keeps it for good, as the forwarder
    // does.
    crate::treasuries::lock(transaction, scope, false).await?;
    let treasury = treasury(transaction, scope, route.chain.chain_id).await?;
    // Taken under the treasury lock, so `created` falls while `treasury` is in force.
    let now = Utc::now();
    let expires_at = now
        .checked_add_signed(chrono::Duration::seconds(window))
        .ok_or(RateLockError::Arithmetic)?;
    let id = Uuid::new_v4();
    let address_id = Uuid::new_v4();
    let client_secret = client_secrets
        .issue(&account.public_id, &quote_id(id))
        .map_err(|error| {
            tracing::error!(%error, "no client secret issued");
            RateLockError::EntropyUnavailable
        })?;
    let salt = quote_salt(
        &account.public_id,
        &customer.client_reference_id,
        &quote_id(id),
    );
    let address = forwarder_address(
        route.chain.contracts.forwarder_factory,
        route.chain.contracts.implementation,
        treasury,
        salt,
    );
    sqlx::query(
        r#"
        INSERT INTO quotes (
            id, account_id, livemode, customer_id, route, amount_atomic, price_scaled,
            credit_minor, expires_at, status, exposure_reserved, created_at, metadata,
            client_secret_hash, route_version, settings_revision_id, terms
        )
        VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric, $8::text::numeric,
                $9, 'open', true, $10, $11, $12, $13, $14, $15)
        "#,
    )
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(customer.id)
    .bind(&route.route)
    .bind(amount_atomic.value().to_string())
    .bind(locked_price.value().to_string())
    .bind(credit_minor.value().to_string())
    .bind(expires_at)
    .bind(now)
    .bind(Json(metadata))
    .bind(Sha256::digest(client_secret.as_bytes()).as_slice())
    .bind(i64::try_from(route.version).map_err(|_| RateLockError::Arithmetic)?)
    .bind(revision)
    .bind(Json(terms))
    .execute(&mut **transaction)
    .await?;
    // A freshly derived single-use address cannot hold earlier payments, so the scanner only
    // needs to cover it from the chain's committed cursor instead of backfilling from genesis.
    sqlx::query(
        r#"
        INSERT INTO addresses (
            id, account_id, livemode, chain_id, quote_id, salt, treasury, address, created_block
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            COALESCE((SELECT scanned_block FROM cursors WHERE chain_id = $4), 0)
        )
        "#,
    )
    .bind(address_id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(i64::try_from(route.chain.chain_id).map_err(|_| RateLockError::Arithmetic)?)
    .bind(id)
    .bind(format!("{salt:#x}"))
    .bind(format!("{treasury:#x}"))
    .bind(format!("{address:#x}"))
    .execute(&mut **transaction)
    .await?;
    let lock = RateLock {
        id,
        livemode: scope.livemode(),
        address_id,
        client_reference_id: customer.client_reference_id.clone(),
        route: route.route.clone(),
        chain_id: route.chain.chain_id,
        address,
        treasury,
        amount_atomic,
        price: locked_price,
        credit_minor,
        expires_at,
        status: RateLockStatus::Open,
        created_at: now,
        consumed_by: None,
        metadata: metadata.clone(),
        route_version: route.version,
        terms,
    };
    Ok((lock, client_secret))
}

/// A quote as the merchant's records hold it, for [`reissue`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReissuedTerms {
    /// The quote id, from its `qt_` id.
    pub id: Uuid,
    /// The customer's `client_reference_id`.
    pub client_reference_id: String,
    /// The address the merchant holds.
    pub address: EvmAddress,
    /// The token amount to pay.
    pub amount_atomic: AtomicAmount,
    /// The locked price.
    pub price: ScaledPrice,
    /// The credit.
    pub credit_minor: MinorAmount,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// End of the payment window.
    pub expires_at: DateTime<Utc>,
    /// The quote's `metadata`.
    pub metadata: BTreeMap<String, String>,
    /// The SHA-256 of the quote's `client_secret`, checked by the caller, so the payer's page
    /// reads the quote again.
    pub client_secret_hash: Option<[u8; 32]>,
}

/// Re-issues after a restore a quote the merchant created after the restore point and lost with
/// it (docs/design/multi-tenant.md §13), from its record: the address salt is derived from the
/// account, customer, and quote id, so only the quote's own address over a treasury of the
/// account on `route`'s chain in force within `crate::treasuries::IN_FORCE_TOLERANCE` of the quote's
/// `created` is accepted (a change that applied since, or was restored since, does not move it:
/// `crate::treasuries::in_force`). A quote the restored database holds is returned as it is
/// (idempotent), whenever it was created; any other created more than that before the restore
/// point is refused, since the restore would not have lost it. The address is backfilled from the
/// chain's cursor in `backfill_from` (the restored cursor), so the rescan finds payments made to it
/// since.
///
/// The terms are the merchant's record, not the service's, so they are stored as recorded but
/// never applied: the quote is marked re-issued by `restore`, a payment to it is credited at spot
/// unless a delivered `deposit.credited` the service signed carries its credit
/// (`crate::restore_mode`), and its payment window closes at the restore's detection at the
/// latest, so its page shows it expired instead of asking for a payment at a price that is not
/// honoured. The customer is created only once the address matches. A quote that exists already
/// for the customer at the address is returned as it is, with `false`; a re-issued one without a
/// client secret takes the one in `terms`. Caps and pauses do not
/// apply: nothing new is given out.
#[allow(clippy::too_many_arguments)]
pub async fn reissue(
    pool: &PgPool,
    account: &Account,
    livemode: bool,
    route: &RouteFile,
    terms: &ReissuedTerms,
    restore: &crate::restore_mode::Restore,
    actor: &Actor,
    reason: &str,
) -> Result<(RateLock, bool), RateLockError> {
    if route.livemode != livemode {
        return Err(RateLockError::InvalidInput(
            "the route's mode differs from the quote's",
        ));
    }
    let (restore_id, detected_at, backfill_from) =
        (restore.id, restore.detected_at, &restore.restored_cursors);
    let restore_point = restore.restore_point.ok_or(RateLockError::InvalidInput(
        "the restore has no restore point (the restored database has no heartbeat): escalate",
    ))?;
    let scope = Scope::new(account.id, livemode);
    let mut transaction = pool.begin().await?;
    crate::treasuries::lock(&mut transaction, scope, false).await?;
    if let Some(existing) = get(&mut *transaction, scope, terms.id).await? {
        if existing.address != terms.address
            || existing.client_reference_id != terms.client_reference_id
        {
            return Err(RateLockError::InvalidInput(
                "the quote exists with another customer or address",
            ));
        }
        // A quote re-issued without its secret takes the one the merchant finds later; a quote the
        // service issued, or one re-issued with a secret, keeps its own.
        if let Some(secret_hash) = &terms.client_secret_hash {
            let added = sqlx::query(
                "UPDATE quotes SET client_secret_hash = $2 \
                 WHERE id = $1 AND restore_id IS NOT NULL AND client_secret_hash IS NULL",
            )
            .bind(terms.id)
            .bind(secret_hash.as_slice())
            .execute(&mut *transaction)
            .await?
            .rows_affected()
                > 0;
            if added {
                audit::insert(
                    &mut *transaction,
                    &audit::Entry {
                        account_id: Some(scope.account_id()),
                        actor,
                        action: "quote.client_secret_restore",
                        subject: &format!("quote:{}", quote_id(terms.id)),
                        reason,
                    },
                )
                .await?;
            }
        }
        transaction.commit().await?;
        return Ok((existing, false));
    }
    let tolerance = crate::treasuries::IN_FORCE_TOLERANCE;
    // A quote the restored database holds was returned above, whenever it was created.
    if terms.created_at < restore_point - tolerance {
        return Err(RateLockError::InvalidInput(
            "the quote was created before the restore point, so the restored database holds it",
        ));
    }
    let in_force = crate::treasuries::in_force(
        &mut transaction,
        scope,
        Some(terms.created_at - tolerance),
        terms.created_at + tolerance,
    )
    .await
    .map_err(|error| match error {
        crate::treasuries::TreasuryError::Database(error) => RateLockError::Database(error),
        _ => RateLockError::DatabaseInvariant,
    })?;
    let salt = quote_salt(
        &account.public_id,
        &terms.client_reference_id,
        &quote_id(terms.id),
    );
    let treasury = in_force
        .into_iter()
        .filter(|(chain_id, _)| *chain_id == route.chain.chain_id)
        .map(|(_, treasury)| treasury)
        .find(|treasury| {
            forwarder_address(
                route.chain.contracts.forwarder_factory,
                route.chain.contracts.implementation,
                *treasury,
                salt,
            ) == terms.address
        });
    let Some(treasury) = treasury else {
        // `treasury_not_set` when the chain has no treasury at all.
        self::treasury(&mut transaction, scope, route.chain.chain_id).await?;
        return Err(RateLockError::InvalidInput(
            "the address is not the quote's over the account's treasury in force when it was \
             created",
        ));
    };
    let address = terms.address;
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM quotes WHERE id = $1)")
        .bind(terms.id)
        .fetch_one(&mut *transaction)
        .await?;
    if taken {
        return Err(RateLockError::InvalidInput("id belongs to another quote"));
    }
    let customer_id = crate::db::ensure_customer_in(
        &mut transaction,
        scope.account_id(),
        scope.livemode(),
        &terms.client_reference_id,
    )
    .await?
    .id;
    // Re-issuing does not re-authorize acceptance: the lock never applies, and a payment follows
    // the binding of its deposit. The terms shown are those the current settings resolve, or the
    // route's defaults (docs/design/payment-settings.md §11).
    let settings = payment_config::load(&mut transaction, scope).await?;
    let shown = payment_config::resolve(route, &settings.document)
        .terms()
        .unwrap_or_else(|| Terms::defaults(route));
    sqlx::query(
        r#"
        INSERT INTO quotes (
            id, account_id, livemode, customer_id, route, amount_atomic, price_scaled,
            credit_minor, expires_at, status, exposure_reserved, created_at, metadata,
            client_secret_hash, restore_id, route_version, terms
        )
        VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric, $8::text::numeric,
                $9, 'open', false, $10, $11, $12, $13, $14, $15)
        "#,
    )
    .bind(terms.id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(customer_id)
    .bind(&route.route)
    .bind(terms.amount_atomic.value().to_string())
    .bind(terms.price.value().to_string())
    .bind(terms.credit_minor.value().to_string())
    .bind(terms.expires_at.min(detected_at))
    .bind(terms.created_at)
    .bind(Json(&terms.metadata))
    .bind(terms.client_secret_hash.as_ref().map(<[u8; 32]>::as_slice))
    .bind(restore_id)
    .bind(i64::try_from(route.version).map_err(|_| RateLockError::Arithmetic)?)
    .bind(Json(shown))
    .execute(&mut *transaction)
    .await?;
    let chain_id = i64::try_from(route.chain.chain_id).map_err(|_| RateLockError::Arithmetic)?;
    let restored_block = backfill_from
        .get(&route.chain.chain_id)
        .map(|block| i64::try_from(*block))
        .transpose()
        .map_err(|_| RateLockError::Arithmetic)?;
    sqlx::query(
        r#"
        INSERT INTO addresses (
            id, account_id, livemode, chain_id, quote_id, salt, treasury, address, created_block
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            LEAST(COALESCE((SELECT scanned_block FROM cursors WHERE chain_id = $4), 0),
                  $9::bigint)
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .bind(terms.id)
    .bind(format!("{salt:#x}"))
    .bind(format!("{treasury:#x}"))
    .bind(format!("{address:#x}"))
    .bind(restored_block)
    .execute(&mut *transaction)
    .await?;
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(scope.account_id()),
            actor,
            action: "quote.reissue",
            subject: &format!("quote:{}", quote_id(terms.id)),
            reason,
        },
    )
    .await?;
    let lock = get(&mut *transaction, scope, terms.id)
        .await?
        .ok_or(RateLockError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok((lock, true))
}

/// The scope's current treasury of `chain_id`, or `TreasuryNotSet`.
async fn treasury(
    connection: &mut sqlx::PgConnection,
    scope: Scope,
    chain_id: u64,
) -> Result<EvmAddress, RateLockError> {
    match crate::treasuries::current_on(connection, scope, chain_id).await {
        Ok(Some(treasury)) => Ok(treasury),
        Ok(None) => Err(RateLockError::TreasuryNotSet),
        Err(crate::treasuries::TreasuryError::Database(error)) => Err(error.into()),
        Err(_) => Err(RateLockError::DatabaseInvariant),
    }
}

/// The account and mode of quote `id` when `secret_hash` is the SHA-256 of its client secret.
pub async fn client_secret_scope(
    pool: &PgPool,
    id: Uuid,
    secret_hash: &[u8],
) -> Result<Option<Scope>, RateLockError> {
    let owner: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT account_id, livemode FROM quotes WHERE id = $1 AND client_secret_hash = $2",
    )
    .bind(id)
    .bind(secret_hash)
    .fetch_optional(pool)
    .await?;
    Ok(owner.map(|(account_id, livemode)| Scope::new(account_id, livemode)))
}

/// Loads one lock when it belongs to `scope`.
pub async fn get<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    scope: Scope,
    id: Uuid,
) -> Result<Option<RateLock>, RateLockError> {
    let row = sqlx::query_as::<_, RateLockRow>(concat!(
        select_lock!(),
        " WHERE quote.account_id = $1 AND quote.livemode = $2 AND quote.id = $3"
    ))
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(id)
    .fetch_optional(executor)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// A page of the scope's quotes, newest first, and whether more follow in the direction of the
/// page (Stripe's cursor pagination): `cursor` is the quote after which (or, with `before`,
/// before which) the page starts. A cursor outside the scope is `NotFound`.
pub async fn list(
    pool: &PgPool,
    scope: Scope,
    client_reference_id: Option<&str>,
    status: Option<RateLockStatus>,
    cursor: Option<(Uuid, bool)>,
    limit: i64,
) -> Result<(Vec<RateLock>, bool), RateLockError> {
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(select_lock!());
    builder
        .push(" WHERE quote.account_id = ")
        .push_bind(scope.account_id())
        .push(" AND quote.livemode = ")
        .push_bind(scope.livemode());
    if let Some(client_reference_id) = client_reference_id {
        builder
            .push(" AND customer.client_reference_id = ")
            .push_bind(client_reference_id.to_owned());
    }
    if let Some(status) = status {
        builder
            .push(" AND quote.status = ")
            .push_bind(status.code());
    }
    let before = cursor.is_some_and(|(_, before)| before);
    if let Some((id, _)) = cursor {
        let (created_at,): (DateTime<Utc>,) = sqlx::query_as(
            "SELECT created_at FROM quotes WHERE id = $1 AND account_id = $2 AND livemode = $3",
        )
        .bind(id)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(pool)
        .await?
        .ok_or(RateLockError::NotFound)?;
        builder
            .push(if before {
                " AND (quote.created_at, quote.id) > ("
            } else {
                " AND (quote.created_at, quote.id) < ("
            })
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder
        .push(if before {
            " ORDER BY quote.created_at ASC, quote.id ASC LIMIT "
        } else {
            " ORDER BY quote.created_at DESC, quote.id DESC LIMIT "
        })
        .push_bind(limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<RateLockRow>()
        .fetch_all(pool)
        .await?;
    let limit = usize::try_from(limit).map_err(|_| RateLockError::DatabaseInvariant)?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    if before {
        rows.reverse();
    }
    let locks = rows
        .into_iter()
        .map(TryInto::try_into)
        .collect::<Result<Vec<RateLock>, _>>()?;
    Ok((locks, has_more))
}

/// Cancels an unpaid open lock, releasing its exposure, and appends an audit row atomically.
///
/// Any deposit row for the lock address, including a rejected one, means funds already arrived at
/// the single-use address, so the lock is no longer unpaid and cancellation is refused; such a
/// lock stays open until it is consumed or expires.
pub async fn cancel<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &RouteSet,
    scope: Scope,
    actor: &Actor,
    id: Uuid,
) -> Result<RateLock, RateLockError> {
    let mut transaction = db.begin().await?;
    let result = cancel_in(&mut transaction, routes, scope, actor, id).await;
    crate::db::settle(transaction, result).await
}

async fn cancel_in(
    transaction: &mut Transaction<'_, Postgres>,
    routes: &RouteSet,
    scope: Scope,
    actor: &Actor,
    id: Uuid,
) -> Result<RateLock, RateLockError> {
    let row = get_in(transaction, scope, id)
        .await?
        .ok_or(RateLockError::NotFound)?;
    if row.status == RateLockStatus::Cancelled {
        return Ok(row);
    }
    if row.status != RateLockStatus::Open {
        return Err(RateLockError::NotOpen(row.status));
    }
    // The lock stays `open` until chain-time expiry, but an in-window payment may still be
    // finalizing, so cancellation ends with the payment window.
    if row.expires_at <= Utc::now() {
        return Err(RateLockError::WindowClosed);
    }
    // `FOR UPDATE` conflicts with the `KEY SHARE` lock a scanner deposit insert takes on its
    // address row, so an uncommitted deposit either commits first and is seen below, or waits
    // until this cancellation commits.
    sqlx::query("SELECT 1 FROM addresses WHERE id = $1 FOR UPDATE")
        .bind(row.address_id)
        .execute(&mut **transaction)
        .await?;
    let paid: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deposits WHERE address_id = $1 AND state <> 'reversed')",
    )
    .bind(row.address_id)
    .fetch_one(&mut **transaction)
    .await?;
    if paid {
        return Err(RateLockError::PendingPayment);
    }
    sqlx::query(
        r#"
        UPDATE quotes
        SET status = 'cancelled', exposure_reserved = false, closed_at = now()
        WHERE id = $1 AND status = 'open' AND consumed_by IS NULL
        "#,
    )
    .bind(row.id)
    .execute(&mut **transaction)
    .await?;
    audit::insert(
        &mut **transaction,
        &audit::Entry {
            account_id: Some(scope.account_id()),
            actor,
            action: "quote.cancel",
            subject: &format!("quote:{}", quote_id(row.id)),
            reason: "API request",
        },
    )
    .await?;
    let event = crate::db::NewOutboxEvent::new(
        "quote.canceled",
        scope,
        crate::db::EventObject::Quote(row.id),
        actor,
    );
    crate::db::enqueue_in(transaction, routes, &event, None).await?;
    Ok(RateLock {
        status: RateLockStatus::Cancelled,
        ..row
    })
}

/// Atomically consumes an open lock, releasing its exposure reservation.
pub(crate) async fn consume(
    transaction: &mut Transaction<'_, Postgres>,
    address_id: Uuid,
    deposit_id: Uuid,
    idempotent: bool,
) -> Result<bool, sqlx::Error> {
    let Some((id, status, consumed_by)) = sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
        r#"
        SELECT quote.id, quote.status, quote.consumed_by
        FROM quotes AS quote
        JOIN addresses AS address ON address.quote_id = quote.id
        WHERE address.id = $1
        FOR UPDATE OF quote
        "#,
    )
    .bind(address_id)
    .fetch_optional(&mut **transaction)
    .await?
    else {
        return Ok(false);
    };
    let status = RateLockStatus::parse(&status).map_err(rate_lock_sqlx)?;
    if status == RateLockStatus::Consumed && idempotent && consumed_by == Some(deposit_id) {
        return Ok(true);
    }
    if !matches!(status, RateLockStatus::Open | RateLockStatus::Expired) || consumed_by.is_some() {
        return Ok(false);
    }
    sqlx::query(
        r#"
        UPDATE quotes
        SET consumed_by = $2, status = 'consumed', exposure_reserved = false, closed_at = now()
        WHERE id = $1 AND status IN ('open', 'expired') AND consumed_by IS NULL
        "#,
    )
    .bind(id)
    .bind(deposit_id)
    .execute(&mut **transaction)
    .await?;
    Ok(true)
}

/// Expires one bounded batch and returns the number of rows closed.
///
/// A lock expires by chain time, not wall-clock time: only once its chain's scanner has committed
/// through a finalized block whose time is past `expires_at`, so every payment mined inside the
/// window is already recorded, and only while no such payment still awaits the confirm step that
/// may consume the lock. A stalled scanner therefore holds locks and their exposure open.
pub async fn expire_once(pool: &PgPool, routes: &RouteSet) -> Result<u64, RateLockError> {
    let mut transaction = pool.begin().await?;
    let rows = sqlx::query_as::<_, ExpiringRow>(
        r#"
        SELECT quote.id, quote.account_id, quote.livemode
        FROM quotes AS quote
        JOIN addresses AS address ON address.quote_id = quote.id
        JOIN cursors AS cursor ON cursor.chain_id = address.chain_id
        WHERE quote.status = 'open'
          AND quote.consumed_by IS NULL
          AND quote.expires_at < cursor.scanned_block_time
          AND NOT EXISTS (
              SELECT 1
              FROM deposits AS deposit
              WHERE deposit.address_id = address.id
                AND deposit.state = 'detected'
                AND deposit.block_time <= quote.expires_at
          )
        ORDER BY quote.expires_at, quote.id
        FOR UPDATE OF quote SKIP LOCKED
        LIMIT $1
        "#,
    )
    .bind(EXPIRY_BATCH_SIZE)
    .fetch_all(&mut *transaction)
    .await?;
    if rows.is_empty() {
        transaction.commit().await?;
        return Ok(0);
    }
    let count = u64::try_from(rows.len()).map_err(|_| RateLockError::DatabaseInvariant)?;
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    let updated = sqlx::query(
        r#"
        UPDATE quotes
        SET status = 'expired', exposure_reserved = false, closed_at = now()
        WHERE id = ANY($1) AND status = 'open' AND consumed_by IS NULL
        "#,
    )
    .bind(&ids)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != count {
        return Err(RateLockError::DatabaseInvariant);
    }

    for row in &rows {
        let event = crate::db::NewOutboxEvent::system(
            topup_core::identity::event_id("quote.expired", row.id),
            "quote.expired",
            Scope::new(row.account_id, row.livemode),
            crate::db::EventObject::Quote(row.id),
        );
        crate::db::enqueue_in(&mut transaction, routes, &event, None).await?;
    }
    transaction.commit().await?;
    Ok(count)
}

/// Periodically closes overdue locks and emits expiry events.
pub struct ExpiryWorker {
    pool: PgPool,
    routes: Arc<RouteSet>,
    scan_interval: Duration,
}

impl ExpiryWorker {
    /// Creates an expiry worker; `routes` render the expired quotes in their events.
    #[must_use]
    pub const fn new(pool: PgPool, routes: Arc<RouteSet>, scan_interval: Duration) -> Self {
        Self {
            pool,
            routes,
            scan_interval,
        }
    }

    /// Runs expiry scans until cancellation.
    pub async fn run(&self, cancellation: CancellationToken) {
        let monitor = crate::observability::CronMonitor::lock_expiry();
        let mut ticker = interval(self.scan_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = ticker.tick() => {
                    match expire_once(&self.pool, &self.routes).await {
                        Ok(_) => monitor.check_in(true),
                        Err(error) => {
                            tracing::error!(
                                tags.alert = "TopupLockExpiryFailing",
                                %error,
                                "rate-lock expiry scan failed"
                            );
                        }
                    }
                }
            }
        }
    }
}

fn parse_u64(value: &str) -> Result<u64, RateLockError> {
    value
        .parse::<u64>()
        .map_err(|_| RateLockError::DatabaseInvariant)
}

fn amount_for_credit(
    route: &RouteFile,
    terms: &Terms,
    credit_minor: MinorAmount,
    price: ScaledPrice,
) -> Result<AtomicAmount, RateLockError> {
    if credit_minor.value() == 0 {
        return Err(RateLockError::AmountTooSmall(
            "amount must be greater than zero",
        ));
    }
    // The smallest amount worth the credit, rounded up to the route's shown decimals so the payer
    // reads and types a short amount; the rounding overpays, never underpays, the locked credit.
    tokens_for_credit(credit_minor, price, route.asset.decimals, UNIT_DECIMALS)
        .and_then(|amount| {
            round_up_to_decimals(amount, route.asset.decimals, terms.quote_amount_decimals)
        })
        .map_err(|_| RateLockError::AmountTooLarge("amount is too large to quote"))
}

fn validate_bounds(
    terms: &Terms,
    amount: AtomicAmount,
    credit: MinorAmount,
) -> Result<(), RateLockError> {
    if credit.value() < terms.min_amount {
        return Err(RateLockError::AmountTooSmall(
            "amount is below the minimum credit",
        ));
    }
    if amount < terms.min_deposit_atomic {
        return Err(RateLockError::AmountTooSmall(
            "amount is below the minimum deposit",
        ));
    }
    if amount > terms.max_deposit_atomic {
        return Err(RateLockError::AmountTooLarge(
            "amount is above the maximum deposit",
        ));
    }
    Ok(())
}

/// One customer's quote creations in the last minute, at most `limit` (the account's
/// `quote_creations_per_customer_per_minute`).
async fn check_creation_rate(
    connection: &mut sqlx::PgConnection,
    customer_id: Uuid,
    limit: u64,
) -> Result<(), RateLockError> {
    let recent: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM quotes
        WHERE customer_id = $1
          AND created_at >= now() - interval '1 minute'
        "#,
    )
    .bind(customer_id)
    .fetch_one(&mut *connection)
    .await?;
    let recent = u64::try_from(recent).map_err(|_| RateLockError::DatabaseInvariant)?;
    if recent < limit {
        return Ok(());
    }
    // The limit admits a creation again once the newest `max` creations shrink below `max`: when
    // the `max`-th newest leaves the minute.
    let offset =
        i64::try_from(limit.saturating_sub(1)).map_err(|_| RateLockError::DatabaseInvariant)?;
    let seconds: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT GREATEST(1, ceil(extract(epoch FROM created_at + interval '1 minute' - now())))::bigint
        FROM quotes
        WHERE customer_id = $1 AND created_at >= now() - interval '1 minute'
        ORDER BY created_at DESC
        OFFSET $2 LIMIT 1
        "#,
    )
    .bind(customer_id)
    .bind(offset)
    .fetch_optional(&mut *connection)
    .await?;
    Err(RateLockError::RateLimited {
        retry_after: seconds
            .and_then(|seconds| u64::try_from(seconds).ok())
            .unwrap_or(60),
    })
}

async fn lock_customer(
    transaction: &mut Transaction<'_, Postgres>,
    customer: &Customer,
) -> Result<(), RateLockError> {
    // `NO KEY UPDATE` serializes creations per customer without blocking the `KEY SHARE` locks
    // that foreign-key checks take when the scanner inserts deposits for this customer.
    let found = sqlx::query(
        "SELECT id FROM customers WHERE id = $1 AND account_id = $2 AND livemode = $3 \
         FOR NO KEY UPDATE",
    )
    .bind(customer.id)
    .bind(customer.account_id)
    .bind(customer.livemode)
    .fetch_optional(&mut **transaction)
    .await?;
    if found.is_none() {
        return Err(RateLockError::NotFound);
    }
    Ok(())
}

/// Rejects a creation that would take the open reserved quotes of the account in its mode past
/// the account's caps (`crate::limits`): their number, their credit, or one customer's credit.
/// Raises `TopupLockExposureNearCap` when it takes the account's credit to 90 percent. Caps are
/// per account and mode only (design §12): a test quote never uses live headroom, and there is no
/// global cap.
///
/// The transaction-level advisory lock serialises creations of the account and mode from this
/// check to commit, and under `READ COMMITTED` the sums, a later statement, see every creation
/// committed before it. Closing a lock only lowers the sums, so cancellation, consumption, and
/// expiry need no lock.
async fn check_exposure(
    transaction: &mut Transaction<'_, Postgres>,
    account: &Account,
    customer: &Customer,
    amount: MinorAmount,
) -> Result<(), RateLockError> {
    let scope = Scope::new(account.id, customer.livemode);
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('rate-lock-exposure:' || $1, 0))")
        .bind(format!("{}:{}", scope.account_id(), scope.livemode()))
        .execute(&mut **transaction)
        .await?;
    let limits = crate::limits::load(transaction, scope)
        .await
        .map_err(|error| match error {
            crate::limits::LimitsError::Database(error) => RateLockError::Database(error),
            _ => RateLockError::DatabaseInvariant,
        })?;
    let open = sqlx::query(
        r#"
        SELECT count(*) AS quotes,
               coalesce(sum(credit_minor) FILTER (WHERE customer_id = $3), 0)::text AS customer,
               coalesce(sum(credit_minor), 0)::text AS account
        FROM quotes
        WHERE account_id = $1 AND livemode = $2 AND status = 'open' AND exposure_reserved
        "#,
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(customer.id)
    .fetch_one(&mut **transaction)
    .await?;
    let quotes = u64::try_from(open.try_get::<i64, _>("quotes")?)
        .map_err(|_| RateLockError::DatabaseInvariant)?;
    if quotes >= limits.max_open_quotes {
        return Err(RateLockError::QuoteCountCap(limits.max_open_quotes));
    }
    for (scope_name, cap) in [
        ("customer", limits.max_open_amount_per_customer),
        ("account", limits.max_open_amount_per_account),
    ] {
        let current = parse_u64(open.try_get(scope_name)?)?;
        let exceeded = RateLockError::ExposureCap {
            scope: scope_name,
            remaining: cap.saturating_sub(current),
        };
        let Some(next) = current.checked_add(amount.value()) else {
            return Err(exceeded);
        };
        if next > cap {
            return Err(exceeded);
        }
        if scope_name == "account" && near_cap(next, cap) {
            tracing::warn!(
                tags.alert = "TopupLockExposureNearCap",
                tags.scope = scope_name,
                tags.id = account.public_id.as_str(),
                tags.livemode = scope.livemode(),
                open_minor = next,
                cap_minor = cap,
                "open rate-lock exposure is at least 90 percent of its cap"
            );
        }
    }
    Ok(())
}

/// Whether `open` reaches 90 percent of a nonzero `cap`.
fn near_cap(open: u64, cap: u64) -> bool {
    cap > 0 && u128::from(open) * 10 >= u128::from(cap) * 9
}

#[derive(FromRow)]
struct RateLockRow {
    id: Uuid,
    livemode: bool,
    address_id: Uuid,
    client_reference_id: String,
    route: String,
    chain_id: i64,
    address: String,
    treasury: String,
    amount_atomic: String,
    price_scaled: String,
    credit_minor: String,
    expires_at: DateTime<Utc>,
    status: String,
    created_at: DateTime<Utc>,
    consumed_by: Option<Uuid>,
    metadata: Json<BTreeMap<String, String>>,
    route_version: i64,
    terms: Json<Terms>,
}

impl TryFrom<RateLockRow> for RateLock {
    type Error = RateLockError;

    fn try_from(row: RateLockRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            livemode: row.livemode,
            address_id: row.address_id,
            client_reference_id: row.client_reference_id,
            route: row.route,
            chain_id: u64::try_from(row.chain_id).map_err(|_| RateLockError::DatabaseInvariant)?,
            address: EvmAddress::from_str(&row.address)
                .map_err(|_| RateLockError::DatabaseInvariant)?,
            treasury: EvmAddress::from_str(&row.treasury)
                .map_err(|_| RateLockError::DatabaseInvariant)?,
            amount_atomic: AtomicAmount::new(
                U256::from_str(&row.amount_atomic).map_err(|_| RateLockError::DatabaseInvariant)?,
            ),
            price: ScaledPrice::new(
                row.price_scaled
                    .parse::<u64>()
                    .map_err(|_| RateLockError::DatabaseInvariant)?,
                PRICE_SCALE,
            )
            .map_err(|_| RateLockError::DatabaseInvariant)?,
            credit_minor: parse_minor(&row.credit_minor)?,
            expires_at: row.expires_at,
            status: RateLockStatus::parse(&row.status)?,
            created_at: row.created_at,
            consumed_by: row.consumed_by,
            metadata: row.metadata.0,
            route_version: u64::try_from(row.route_version)
                .map_err(|_| RateLockError::DatabaseInvariant)?,
            terms: row.terms.0,
        })
    }
}

async fn get_in(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
) -> Result<Option<RateLock>, RateLockError> {
    let row = sqlx::query_as::<_, RateLockRow>(concat!(
        select_lock!(),
        " WHERE quote.account_id = $1 AND quote.livemode = $2 AND quote.id = $3",
        " FOR UPDATE OF quote"
    ))
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(TryInto::try_into).transpose()
}

#[derive(FromRow)]
struct ExpiringRow {
    id: Uuid,
    account_id: Uuid,
    livemode: bool,
}

fn parse_minor(value: &str) -> Result<MinorAmount, RateLockError> {
    value
        .parse::<u64>()
        .map(MinorAmount::new)
        .map_err(|_| RateLockError::DatabaseInvariant)
}

fn rate_lock_sqlx(error: RateLockError) -> sqlx::Error {
    match error {
        RateLockError::Database(error) => error,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}
