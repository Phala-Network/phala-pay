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
use pricing::{PricingFailure, PricingRuntimes, ValidatedQuote};

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
           quote.expires_at, quote.cancel_requested_at, quote.status, quote.created_at, quote.consumed_by, quote.metadata,
           quote.route_version, quote.terms, quote.window_start, quote.replaced_by,
           address.deposit_address_id, deposit_address.version AS deposit_address_version
    FROM quotes AS quote
    JOIN addresses AS address ON address.id = quote.address_id
    JOIN customers AS customer ON customer.id = quote.customer_id
    LEFT JOIN deposit_addresses AS deposit_address ON deposit_address.id = address.deposit_address_id"#
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
    /// Deposit address owning this network; absent only on historical quote addresses.
    pub deposit_address_id: Option<Uuid>,
    /// Deposit-address derivation version.
    pub deposit_address_version: Option<u64>,
    /// Exclusive start of the payment window.
    pub window_start: DateTime<Utc>,
    /// The quote that closed this window.
    pub replaced_by: Option<Uuid>,
    /// Route name.
    pub route: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Customer deposit-address network.
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
    /// Deferred cancellation request; the payment window ends at this instant.
    pub cancel_requested_at: Option<DateTime<Utc>>,
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

/// The public id of a quote: `qt_` and the hex of its id.
#[must_use]
pub fn quote_id(id: Uuid) -> String {
    crate::ids::format(crate::ids::QUOTE, id)
}

/// A validated quote source used by rate-lock creation.
#[async_trait]
pub trait QuoteProvider: Send + Sync {
    /// Fetches and validates the spot price within the caller's absolute deadline.
    async fn quote(
        &self,
        route: &RouteFile,
        deadline: tokio::time::Instant,
    ) -> Result<ValidatedQuote, Value>;
}

/// Production quote provider configured from attested route files.
pub struct ConfiguredQuoteProvider {
    runtimes: PricingRuntimes,
}

impl ConfiguredQuoteProvider {
    /// Uses the service's shared per-route pricing runtimes.
    pub fn from_runtimes(runtimes: PricingRuntimes) -> Self {
        Self { runtimes }
    }
    /// Periodic service sampler, bounded and cancelled with the service task group.
    pub async fn sample_twaps(
        &self,
        routes: &crate::routes::RouteSet,
        cancellation: tokio_util::sync::CancellationToken,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(if routes.staging() {
            topup_adapters::pricing::uniswap_v2::SAMPLE_INTERVAL_S
        } else {
            60
        }));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = tick.tick() => {
                    let arrived = tokio::time::Instant::now();
                    for route in routes.routes() {
                        if let Some(runtime) = self.runtimes.get(&(route.route.clone(), route.version)) {
                            tokio::select! {
                                () = cancellation.cancelled() => return,
                                () = runtime.sample_twaps(route, arrived) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl QuoteProvider for ConfiguredQuoteProvider {
    async fn quote(
        &self,
        route: &RouteFile,
        deadline: tokio::time::Instant,
    ) -> Result<ValidatedQuote, Value> {
        let runtime = self
            .runtimes
            .get(&(route.route.clone(), route.version))
            .ok_or_else(|| Value::from(PricingFailure::new("missing_route_runtime")))?;
        runtime.fetch(route, deadline).await.map_err(Value::from)
    }
}

impl From<PricingFailure> for Value {
    fn from(failure: PricingFailure) -> Self {
        Value::from(&failure)
    }
}

impl From<&PricingFailure> for Value {
    fn from(failure: &PricingFailure) -> Self {
        let mut value = json!({"stage": "pricing", "error": failure.code});
        if !failure.evidence.is_null() {
            value["quote"] = failure.evidence.clone();
        }
        value
    }
}

/// Quote provider used by API surfaces that do not exercise rate-lock creation.
pub struct UnavailableQuoteProvider;

#[async_trait]
impl QuoteProvider for UnavailableQuoteProvider {
    async fn quote(
        &self,
        _route: &RouteFile,
        _deadline: tokio::time::Instant,
    ) -> Result<ValidatedQuote, Value> {
        Err(PricingFailure::new("unavailable").into())
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
    /// Customer deposit-address issuance failed.
    #[error("{0}")]
    DepositAddress(#[from] crate::deposit_addresses::DepositAddressError),
    /// Current validated pricing is unavailable.
    #[error("validated pricing is unavailable")]
    PricingUnavailable,
    /// The chain is not initialized or temporarily unavailable.
    #[error("chain cannot issue an address yet")]
    ChainUnavailable,
    /// The account has no treasury on the route's chain.
    #[error("no treasury is set on the chain")]
    TreasuryNotSet,
    /// The account's payment settings do not accept the route's asset on its chain.
    #[error("the account does not accept the asset")]
    AssetNotAccepted,
    /// The account's payment settings are held after a restore until it reconfirms them.
    #[error("the account's payment settings await reconfirmation")]
    SettingsUnconfirmed,
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
    max_issued_addresses: u32,
    credit_minor: MinorAmount,
    metadata: &BTreeMap<String, String>,
) -> Result<(RateLock, String), RateLockError> {
    let priced = price(
        pool,
        quotes,
        account,
        customer,
        route,
        credit_minor,
        tokio::time::Instant::now() + QUOTE_PRICING_BUDGET,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    let created = create_in(
        &mut transaction,
        client_secrets,
        account,
        customer,
        Creation {
            route,
            max_issued_addresses,
            priced: &priced,
            metadata,
        },
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
    deadline: tokio::time::Instant,
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

    let quote = quote_with_budget(quotes, route, deadline).await?;
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

// Reserve roughly ten seconds for database work within the request budget (D3).
pub(crate) const QUOTE_PRICING_BUDGET: Duration = Duration::from_secs(15);

pub(crate) async fn quote_with_budget(
    quotes: &Arc<dyn QuoteProvider>,
    route: &RouteFile,
    request_deadline: tokio::time::Instant,
) -> Result<ValidatedQuote, RateLockError> {
    let budget_deadline = tokio::time::Instant::now() + QUOTE_PRICING_BUDGET;
    let deadline = request_deadline.min(budget_deadline);
    match tokio::time::timeout_at(deadline, quotes.quote(route, deadline)).await {
        Err(_) => Err(quote_budget_exceeded(route)),
        Ok(Err(_)) if tokio::time::Instant::now() >= deadline => Err(quote_budget_exceeded(route)),
        Ok(result) => result.map_err(|evidence| {
            tracing::warn!(route = %route.route, quote = %evidence, "rate-lock price validation failed");
            RateLockError::PricingUnavailable
        }),
    }
}
fn quote_budget_exceeded(route: &RouteFile) -> RateLockError {
    tracing::warn!(route = %route.route, "rate-lock pricing budget exceeded");
    crate::observability::price_metrics::quote_budget_exceeded(route);
    RateLockError::PricingUnavailable
}

/// Priced quote inputs and the attested address admission limit for its route's chain.
pub struct Creation<'a> {
    /// The selected validated route.
    pub route: &'a RouteFile,
    /// Configured historical issued-address cap on the selected chain.
    pub max_issued_addresses: u32,
    /// Price and terms resolved before beginning the creation transaction.
    pub priced: &'a PricedLock,
    /// Merchant metadata carried by the new quote.
    pub metadata: &'a BTreeMap<String, String>,
}

/// Creates the priced lock for `customer` of `account` in `transaction`, with its
/// `client_secret`, after checking the customer's creation rate, the exposure caps, and the
/// chain's treasury again; its window starts now.
pub async fn create_in(
    transaction: &mut Transaction<'_, Postgres>,
    client_secrets: &ClientSecretKey,
    account: &Account,
    customer: &Customer,
    creation: Creation<'_>,
) -> Result<(RateLock, String), RateLockError> {
    let Creation {
        route,
        max_issued_addresses,
        priced,
        metadata,
    } = creation;
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
    let (deposit_address, _) = crate::deposit_addresses::create(
        &mut **transaction,
        account,
        customer,
        &[crate::deposit_addresses::ChainContracts::of(
            route,
            max_issued_addresses,
        )],
        None,
    )
    .await?;
    let network = deposit_address
        .networks
        .iter()
        .find(|network| network.chain_id == route.chain.chain_id)
        .ok_or(RateLockError::ChainUnavailable)?;
    // Wait for in-flight consumption before reading its canonical block time in a fresh
    // snapshot. A chain slightly ahead of wall time must keep its payment in the old window.
    sqlx::query(
        "SELECT id FROM quotes WHERE customer_id=$1 AND route=$2 \
         AND expires_at>date_trunc('second', clock_timestamp()) ORDER BY id FOR UPDATE",
    )
    .bind(customer.id)
    .bind(&route.route)
    .fetch_all(&mut **transaction)
    .await?;
    let now: DateTime<Utc> = sqlx::query_scalar(
        "SELECT GREATEST(date_trunc('second', clock_timestamp()), \
         max(q.window_start), max(d.block_time)) FROM quotes q \
         LEFT JOIN deposits d ON d.id=q.consumed_by \
         WHERE q.customer_id=$1 AND q.route=$2 \
         AND q.expires_at>date_trunc('second', clock_timestamp())",
    )
    .bind(customer.id)
    .bind(&route.route)
    .fetch_one(&mut **transaction)
    .await?;
    let expires_at = now
        .checked_add_signed(chrono::Duration::seconds(window))
        .ok_or(RateLockError::Arithmetic)?;
    let id = Uuid::new_v4();
    let address_id = network.address_id;
    let address = network.address;
    let treasury = network.treasury;
    let client_secret = client_secrets
        .issue(&account.public_id, &quote_id(id))
        .map_err(|error| {
            tracing::error!(%error, "no client secret issued");
            RateLockError::EntropyUnavailable
        })?;
    // Used quotes are cut too: a reversal must not reopen an overlapping window.
    sqlx::query(
        "UPDATE quotes SET expires_at=$3, replaced_by=$4, \
         cancel_requested_at=CASE WHEN status='open' AND consumed_by IS NULL \
         THEN COALESCE(cancel_requested_at,$3) ELSE cancel_requested_at END \
         WHERE customer_id=$1 AND route=$2 AND expires_at>$3",
    )
    .bind(customer.id)
    .bind(&route.route)
    .bind(now)
    .bind(id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO quotes (
            id, account_id, livemode, customer_id, route, amount_atomic, price_scaled,
            credit_minor, expires_at, status, exposure_reserved, created_at, metadata,
            client_secret_hash, route_version, settings_revision_id, terms, address_id, window_start
        )
        VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric, $8::text::numeric,
                $9, 'open', true, $10, $11, $12, $13, $14, $15, $16, $10)
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
    .bind(address_id)
    .execute(&mut **transaction)
    .await?;
    let lock = RateLock {
        id,
        livemode: scope.livemode(),
        address_id,
        client_reference_id: customer.client_reference_id.clone(),
        deposit_address_id: Some(deposit_address.id),
        deposit_address_version: Some(deposit_address.version),
        window_start: now,
        replaced_by: None,
        route: route.route.clone(),
        chain_id: route.chain.chain_id,
        address,
        treasury,
        amount_atomic,
        price: locked_price,
        credit_minor,
        expires_at,
        cancel_requested_at: None,
        status: RateLockStatus::Open,
        created_at: now,
        consumed_by: None,
        metadata: metadata.clone(),
        route_version: route.version,
        terms,
    };
    Ok((lock, client_secret))
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

/// Ends an unpaid open lock's payment window and appends an audit row atomically.
///
/// Any non-reversed deposit of the quote's asset in its window, including a rejected one,
/// means the lock is no longer unpaid and cancellation is refused; such a lock stays open
/// until it is consumed or expires.
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
    let address_id = get(&mut **transaction, scope, id)
        .await?
        .ok_or(RateLockError::NotFound)?
        .address_id;
    // Match creation's address-before-quote lock order. This also conflicts with the KEY SHARE
    // lock taken by scanner inserts, so a payment commits before the check below or waits.
    sqlx::query("SELECT 1 FROM addresses WHERE id = $1 FOR UPDATE")
        .bind(address_id)
        .execute(&mut **transaction)
        .await?;
    let row = get_in(transaction, scope, id)
        .await?
        .ok_or(RateLockError::NotFound)?;
    if row.status == RateLockStatus::Cancelled || row.cancel_requested_at.is_some() {
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
    let paid: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deposits WHERE address_id = $1 AND state <> 'reversed'
         AND block_time > $2 AND block_time <= $3 AND asset_contract = $4)",
    )
    .bind(row.address_id)
    .bind(row.window_start)
    .bind(row.expires_at)
    .bind(format!(
        "{:#x}",
        routes
            .current()
            .find(|route| route.route == row.route)
            .ok_or(RateLockError::DatabaseInvariant)?
            .asset
            .contract
    ))
    .fetch_one(&mut **transaction)
    .await?;
    if paid {
        return Err(RateLockError::PendingPayment);
    }
    sqlx::query(
        r#"
        UPDATE quotes
        SET cancel_requested_at = clock_timestamp(), expires_at = date_trunc('second', clock_timestamp())
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
    get_in(transaction, scope, id)
        .await?
        .ok_or(RateLockError::NotFound)
}

/// Atomically consumes an open lock, releasing its exposure reservation.
pub(crate) async fn consume(
    transaction: &mut Transaction<'_, Postgres>,
    quote_id: Uuid,
    deposit_id: Uuid,
    block_time: DateTime<Utc>,
    idempotent: bool,
) -> Result<bool, sqlx::Error> {
    let Some((status, consumed_by, start, end)) = sqlx::query_as::<
        _,
        (String, Option<Uuid>, DateTime<Utc>, DateTime<Utc>),
    >(
        "SELECT status, consumed_by, window_start, expires_at FROM quotes WHERE id=$1 FOR UPDATE",
    )
    .bind(quote_id)
    .fetch_optional(&mut **transaction)
    .await?
    else {
        return Ok(false);
    };
    if status == "consumed" && idempotent && consumed_by == Some(deposit_id) {
        return Ok(true);
    }
    if !matches!(status.as_str(), "open" | "expired")
        || consumed_by.is_some()
        || block_time <= start
        || block_time > end
    {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE quotes SET consumed_by=$2, status='consumed', exposure_reserved=false,
         closed_at=now() WHERE id=$1",
    )
    .bind(quote_id)
    .bind(deposit_id)
    .execute(&mut **transaction)
    .await?;
    let associated = sqlx::query(
        "UPDATE deposits d SET quote_id=q.id, metadata=q.metadata FROM quotes q
         WHERE d.id=$2 AND q.id=$1 AND d.account_id=q.account_id AND d.livemode=q.livemode
         AND d.address_id=q.address_id",
    )
    .bind(quote_id)
    .bind(deposit_id)
    .execute(&mut **transaction)
    .await?;
    if associated.rows_affected() != 1 {
        return Err(sqlx::Error::Protocol(
            "quote consumption has no matching deposit".into(),
        ));
    }
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
        SELECT quote.id, quote.account_id, quote.livemode, quote.cancel_requested_at IS NOT NULL AS cancelled
        FROM quotes AS quote
        JOIN addresses AS address ON address.id = quote.address_id
        JOIN chain_coverage AS coverage ON coverage.chain_id = address.chain_id
        WHERE quote.status = 'open'
          AND quote.consumed_by IS NULL
          AND address.dual_covered_through = coverage.through_block
          AND quote.expires_at < coverage.through_time
          AND NOT EXISTS (SELECT 1 FROM reconciliation_blocks WHERE scope='chain' AND chain_id=address.chain_id)
          AND NOT EXISTS (
              SELECT 1
              FROM deposits AS deposit
              WHERE deposit.address_id = address.id
                AND deposit.state = 'detected'
                AND deposit.block_time > quote.window_start
                AND deposit.block_time <= quote.expires_at
          )
        ORDER BY quote.expires_at, quote.id
        FOR UPDATE OF quote, address SKIP LOCKED
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
        SET status = CASE WHEN cancel_requested_at IS NULL THEN 'expired' ELSE 'cancelled' END, exposure_reserved = false, closed_at = now()
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
            topup_core::identity::event_id(
                if row.cancelled {
                    "quote.canceled"
                } else {
                    "quote.expired"
                },
                row.id,
            ),
            if row.cancelled {
                "quote.canceled"
            } else {
                "quote.expired"
            },
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
    deposit_address_id: Option<Uuid>,
    deposit_address_version: Option<i64>,
    window_start: DateTime<Utc>,
    replaced_by: Option<Uuid>,
    chain_id: i64,
    address: String,
    treasury: String,
    amount_atomic: String,
    price_scaled: String,
    credit_minor: String,
    expires_at: DateTime<Utc>,
    cancel_requested_at: Option<DateTime<Utc>>,
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
            deposit_address_id: row.deposit_address_id,
            deposit_address_version: row
                .deposit_address_version
                .map(u64::try_from)
                .transpose()
                .map_err(|_| RateLockError::DatabaseInvariant)?,
            window_start: row.window_start,
            replaced_by: row.replaced_by,
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
            cancel_requested_at: row.cancel_requested_at,
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
    cancelled: bool,
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
