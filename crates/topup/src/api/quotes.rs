//! Quotes (`/v1/quotes`) and the product configuration (`/v1/config`).

use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use axum::response::{IntoResponse as _, Response};
use sha2::{Digest as _, Sha256};
use sqlx::{PgConnection, PgPool};
use topup_core::money::MinorAmount;
use topup_core::route::{PricingMode, RouteFile};

use crate::payment_config::{self, Terms};
use uuid::Uuid;

use crate::db::{Account, Customer};
use crate::ids;
use crate::locks::{self, RateLock, RateLockError, RateLockStatus};
use crate::routes::RouteSet;
use crate::tenancy::Scope;

use super::AppState;
use super::auth::Merchant;
use super::client_limit;
use super::error::{ApiError, ErrorResponse};
use super::extract::{ApiJson, ApiPath, expansions, query_pairs};
use super::handlers::ensure_customer;
use super::idempotency::Idempotent;
use super::metadata::{self, Object};
use super::models::{
    ClientQuote, Config, ConfigAsset, CreateQuoteRequest, ExpandableDeposit, Quote, QuoteList,
    QuoteTerms, QuoteView, UpdateMetadataRequest,
};
use super::repository;

type ApiResult<T> = Result<T, ApiError>;

use topup_core::route::TYPICAL_FINALIZED_SECONDS;

#[utoipa::path(
    get,
    path = "/v1/config",
    responses(
        (status = 200, description = "OK", body = Config),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "config"
)]
/// Your effective payment config in the key's mode: every asset your payment settings accept on a
/// chain where you have a treasury, with its terms (`GET /v1/payment_settings`), and your
/// open-quote caps. An account that accepts nothing yet gets no asset.
pub(crate) async fn get_config(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
) -> ApiResult<Json<Config>> {
    let mut connection = state.pool.acquire().await?;
    let effective =
        payment_config::load_effective(&mut connection, &state.routes, merchant.scope).await?;
    let limits = crate::limits::load(&mut connection, merchant.scope)
        .await
        .map_err(|error| {
            tracing::error!(%error, "account limits are unreadable");
            ApiError::internal()
        })?;
    let assets = effective
        .active()
        .map(|asset| config_asset(asset.route, &asset.terms))
        .collect();
    Ok(Json(Config {
        object: "config".to_owned(),
        livemode: merchant.scope.livemode(),
        currency: "usd".to_owned(),
        max_open_quotes: limits.max_open_quotes,
        max_open_amount_per_account: limits.max_open_amount_per_account,
        max_open_amount_per_customer: limits.max_open_amount_per_customer,
        quote_creations_per_customer_per_minute: effective.quote_creations_per_customer_per_minute,
        assets,
    }))
}

/// One payable asset of the effective config.
fn config_asset(route: &RouteFile, terms: &Terms) -> ConfigAsset {
    let confirmations = route.chain.confirmations.stricter(terms.confirmations);
    ConfigAsset {
        chain_id: route.chain.chain_id,
        asset: route.asset.symbol.clone(),
        contract: format!("{:#x}", route.asset.contract),
        decimals: route.asset.decimals,
        pricing: match route.pricing.mode {
            PricingMode::Spot => "spot",
            PricingMode::Stablecoin => "stablecoin",
        }
        .to_owned(),
        min_amount: terms.min_amount,
        min_deposit_atomic: terms.min_deposit_atomic.value().to_string(),
        max_deposit_atomic: terms.max_deposit_atomic.value().to_string(),
        min_refund_atomic: terms.min_refund_atomic.value().to_string(),
        quote_ttl_seconds: terms.quote_ttl_seconds,
        quote_spread_bps: terms.quote_spread_bps.value(),
        quote_tolerance_bps: terms.quote_tolerance_bps.value(),
        quote_amount_decimals: terms.quote_amount_decimals,
        confirmations: confirmations.policy_value(),
        typical_credit_seconds: confirmations.typical_credit_seconds(route.chain.chain_id),
        typical_finality_seconds: TYPICAL_FINALIZED_SECONDS,
    }
}

/// The terms a quote was issued with, exactly as stored: a later catalog never changes them. A
/// payment's current wait is the payer's view's `typical_credit_seconds`.
pub(super) fn quote_terms(terms: &Terms) -> QuoteTerms {
    QuoteTerms {
        quote_ttl_seconds: terms.quote_ttl_seconds,
        quote_spread_bps: terms.quote_spread_bps.value(),
        quote_tolerance_bps: terms.quote_tolerance_bps.value(),
        quote_amount_decimals: terms.quote_amount_decimals,
        min_amount: terms.min_amount,
        min_deposit_atomic: terms.min_deposit_atomic.value().to_string(),
        max_deposit_atomic: terms.max_deposit_atomic.value().to_string(),
        min_refund_atomic: terms.min_refund_atomic.value().to_string(),
        confirmations: terms.confirmations.policy_value(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/quotes",
    params(
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = CreateQuoteRequest,
    responses(
        (status = 200, description = "OK", body = Quote),
        (
            status = 400,
            description = "Bad Request, `asset_not_accepted`, `payment_settings_unconfirmed`, \
                           `amount_too_small`, `amount_too_large`, `exposure_cap_exceeded`, \
                           `paused`, `chain_frozen`, or `treasury_not_set`",
            body = ErrorResponse
        ),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (
            status = 429,
            description = "`rate_limit`, or `customer_rate_limit`: the customer's quotes per \
                           minute; retry after `Retry-After` seconds",
            body = ErrorResponse
        ),
        (status = 503, description = "Service Unavailable", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "quotes"
)]
/// Quotes `amount` cents payable in `asset` on `chain_id`: a locked price, the exact token amount,
/// and a single-use address, valid until `expires_at`, on the terms your payment settings set for
/// the asset; the quote keeps them for good. An asset your settings do not accept is
/// `asset_not_accepted`.
pub(crate) async fn create_quote(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    Extension(deadline): Extension<tokio::time::Instant>,
    idempotent: Idempotent,
    ApiJson(request): ApiJson<CreateQuoteRequest>,
) -> ApiResult<Response> {
    if request.currency != "usd" {
        return Err(ApiError::invalid_param("currency", "currency must be usd"));
    }
    let route = state
        .routes
        .current_in(merchant.scope.livemode())
        .find(|route| {
            route.chain.chain_id == request.chain_id && route.asset.symbol == request.asset
        })
        .ok_or_else(|| {
            ApiError::invalid_param("asset", "no payable asset matches chain_id and asset")
        })?;
    let effective = payment_config::load_effective(
        &mut *state.pool.acquire().await?,
        &state.routes,
        merchant.scope,
    )
    .await?;
    if effective.settings.status == payment_config::Status::Held {
        return Err(ApiError::payment_settings_unconfirmed());
    }
    let terms = effective
        .asset(route.chain.chain_id, &route.asset.symbol)
        .map(|asset| asset.terms)
        .ok_or_else(|| ApiError::asset_not_accepted(Some("asset")))?;
    if request.amount < terms.min_amount.max(1) {
        return Err(ApiError::amount_too_small(
            "amount",
            format!("amount must be at least {}", terms.min_amount.max(1)),
        ));
    }
    let credit = MinorAmount::new(request.amount);
    let metadata = metadata::on_create(request.metadata.as_ref())?;
    let customer = ensure_customer(&state, merchant.scope, &request.client_reference_id).await?;
    if crate::reconciler::chain_is_blocked(&state.pool, route.chain.chain_id).await? {
        return Err(ApiError::chain_frozen());
    }
    let route_scopes = repository::route_paused_scopes(&state.pool, &route.route).await?;
    if has_quotes_pause(&merchant.account, &customer, &route_scopes) {
        return Err(ApiError::paused("quotes are paused"));
    }
    let priced = locks::price(
        &state.pool,
        &state.rate_lock_quotes,
        &merchant.account,
        &customer,
        route,
        credit,
        deadline,
    )
    .await
    .map_err(map_error)?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    let (lock, client_secret) = locks::create_in(
        &mut transaction,
        state.client_reads.key(),
        &merchant.account,
        &customer,
        route,
        &priced,
        &metadata,
    )
    .await
    .map_err(map_error)?;
    let mut quote = quote_object(&mut transaction, &state.routes, lock).await?;
    quote.client_secret = Some(client_secret);
    idempotent.commit(transaction, Json(quote)).await
}

#[utoipa::path(
    get,
    path = "/v1/quotes",
    params(
        ("client_reference_id" = Option<String>, Query, description = "Only this customer's quotes"),
        ("status" = Option<String>, Query, description = "`open`, `complete`, `expired`, or `canceled`"),
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`qt_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`qt_` id: the page before it")
    ),
    responses(
        (status = 200, description = "OK", body = QuoteList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "quotes"
)]
/// The account's quotes in the key's mode, newest first, with Stripe's cursor pagination.
pub(crate) async fn list_quotes(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<QuoteList>> {
    let mut client_reference_id = None;
    let mut status_filter = None;
    let mut limit = 10;
    let mut starting_after = None;
    let mut ending_before = None;
    for (name, value) in query_pairs(query.as_deref()) {
        match name.as_str() {
            "client_reference_id" => client_reference_id = Some(value),
            "status" => {
                status_filter = Some(match value.as_str() {
                    "open" => RateLockStatus::Open,
                    "complete" => RateLockStatus::Consumed,
                    "expired" => RateLockStatus::Expired,
                    "canceled" => RateLockStatus::Cancelled,
                    _ => return Err(ApiError::invalid_param("status", "unknown status")),
                });
            }
            "limit" => {
                limit = value
                    .parse::<i64>()
                    .ok()
                    .filter(|limit| (1..=100).contains(limit))
                    .ok_or_else(|| ApiError::invalid_param("limit", "limit must be 1 to 100"))?;
            }
            "starting_after" | "ending_before" => {
                let id = ids::parse(ids::QUOTE, &value)
                    .ok_or_else(|| ApiError::invalid_param(name.clone(), "not a qt_ id"))?;
                if name == "starting_after" {
                    starting_after = Some(id);
                } else {
                    ending_before = Some(id);
                }
            }
            other => {
                return Err(
                    ApiError::unknown_param(format!("unknown parameter {other}")).with_param(other),
                );
            }
        }
    }
    let cursor = match (starting_after, ending_before) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "starting_after and ending_before are mutually exclusive",
            ));
        }
        (Some(id), None) => Some((id, false)),
        (None, Some(id)) => Some((id, true)),
        (None, None) => None,
    };
    let (locks, has_more) = locks::list(
        &state.pool,
        merchant.scope,
        client_reference_id.as_deref(),
        status_filter,
        cursor,
        limit,
    )
    .await
    .map_err(|error| match (error, cursor) {
        (RateLockError::NotFound, Some((_, before))) => ApiError::invalid_param(
            if before {
                "ending_before"
            } else {
                "starting_after"
            },
            "no such quote",
        ),
        (error, _) => map_error(error),
    })?;
    let mut connection = state.pool.acquire().await?;
    let address_ids = locks.iter().map(|lock| lock.address_id).collect::<Vec<_>>();
    let payments = super::pending::QuotePayments::load(&mut connection, &address_ids).await?;
    let mut data = Vec::with_capacity(locks.len());
    for lock in locks {
        let route = quote_route(&state.routes, &lock)?;
        let payment = payments.payment(route, &lock);
        data.push(render_quote(route, lock, payment));
    }
    Ok(Json(QuoteList {
        object: "list".to_owned(),
        url: "/v1/quotes".to_owned(),
        has_more,
        data,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/quotes/{id}",
    params(
        ("id" = String, Path, description = "Quote id, `qt_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = UpdateMetadataRequest,
    responses(
        (status = 200, description = "OK", body = Quote),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "quotes"
)]
/// Updates a quote's `metadata`, in any status; parameters not sent are left unchanged.
pub(crate) async fn update_quote(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<UpdateMetadataRequest>,
) -> ApiResult<Response> {
    let quote = ids::parse(ids::QUOTE, &id).ok_or_else(ApiError::not_found)?;
    let scope = merchant.scope;
    let mut transaction = idempotent.begin(&state.pool).await?;
    if !metadata::update(
        &mut *transaction,
        &state.routes,
        Object::Quote,
        scope,
        quote,
        request.metadata.as_ref(),
        &merchant.actor(),
    )
    .await?
    {
        return Err(ApiError::not_found());
    }
    let quote = find_quote(&mut transaction, &state.routes, scope, quote)
        .await?
        .ok_or_else(ApiError::not_found)?;
    idempotent.commit(transaction, Json(quote)).await
}

#[utoipa::path(
    get,
    path = "/v1/quotes/{id}",
    params(
        ("id" = String, Path, description = "Quote id, `qt_…`"),
        ("expand[]" = Option<Vec<String>>, Query, description = "`deposit`; API key requests only"),
        (
            "client_secret" = Option<String>, Query,
            description = "The quote's `client_secret`, to read its public view without an \
                           API key. Send the request without `Authorization`; the response then \
                           allows any origin."
        )
    ),
    responses(
        (
            status = 200,
            description = "OK: a `Quote` to a request with an API key, a `ClientQuote` to a \
                           request by `client_secret`",
            body = QuoteView
        ),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (
            status = 404,
            description = "Not Found, also for a `client_secret` that is not this quote's",
            body = ErrorResponse
        ),
        (status = 429, description = "Too Many Requests: reads by `client_secret`", body = ErrorResponse)
    ),
    security(("api_key" = []), ()),
    tag = "quotes"
)]
/// One quote, for example to resume a checkout page. The payer's browser can read the quote's
/// public view with its `client_secret` instead of an API key, as Stripe.js reads a PaymentIntent.
pub(crate) async fn get_quote(
    State(state): State<AppState>,
    merchant: Option<Extension<Merchant>>,
    ApiPath(id): ApiPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let pairs = query_pairs(query.as_deref());
    let Some(Extension(merchant)) = merchant else {
        let client_secret = pairs
            .iter()
            .find(|(name, _)| name == "client_secret")
            .map(|(_, value)| value.as_str());
        let mut response = match client_quote(&state, &id, client_secret).await {
            Ok(quote) => Json(QuoteView::Client(Box::new(quote))).into_response(),
            Err(error) => error.into_response(),
        };
        super::allow_cross_origin(&mut response);
        return response;
    };
    let quote = async {
        let expand = expansions(&pairs, &["deposit"])?;
        let quote = ids::parse(ids::QUOTE, &id).ok_or_else(ApiError::not_found)?;
        let lock = locks::get(&state.pool, merchant.scope, quote)
            .await
            .map_err(map_error)?
            .ok_or_else(ApiError::not_found)?;
        let consumed_by = lock.consumed_by;
        let mut quote =
            quote_object(&mut *state.pool.acquire().await?, &state.routes, lock).await?;
        if expand.contains(&"deposit")
            && let Some(deposit) = consumed_by
        {
            let deposit =
                super::deposits::find_deposit(&state.pool, &state.routes, merchant.scope, deposit)
                    .await?
                    .ok_or_else(ApiError::internal)?;
            quote.deposit = Some(ExpandableDeposit::Object(Box::new(deposit)));
        }
        Ok::<_, ApiError>(quote)
    };
    match quote.await {
        Ok(quote) => Json(QuoteView::Quote(Box::new(quote))).into_response(),
        Err(error) => error.into_response(),
    }
}

/// The public view of the quote `id` whose client secret is `client_secret`.
async fn client_quote(
    state: &AppState,
    id: &str,
    client_secret: Option<&str>,
) -> ApiResult<ClientQuote> {
    let quote = ids::parse(ids::QUOTE, id).ok_or_else(ApiError::not_found)?;
    let client_secret = client_secret.ok_or_else(ApiError::not_found)?;
    let _slot = state.client_reads.admit(id, quote, client_secret).await?;
    let secret = Sha256::digest(client_secret.as_bytes());
    client_limit::bounded(client_quote_view(state, quote, &secret)).await
}

/// The public view of `quote` if `secret_hash` is the SHA-256 of one of its valid client secrets.
async fn client_quote_view(
    state: &AppState,
    quote: Uuid,
    secret_hash: &[u8],
) -> ApiResult<ClientQuote> {
    let scope = locks::client_secret_scope(&state.pool, quote, secret_hash)
        .await
        .map_err(map_error)?
        .ok_or_else(ApiError::not_found)?;
    let lock = locks::get(&state.pool, scope, quote)
        .await
        .map_err(map_error)?
        .ok_or_else(ApiError::not_found)?;
    let route = state
        .routes
        .current()
        .find(|route| route.route == lock.route)
        .ok_or_else(|| {
            tracing::error!(route = %lock.route, "quote route is not loaded");
            ApiError::internal()
        })?;
    let payment =
        super::pending::quote_payment(&mut *state.pool.acquire().await?, route, &lock).await?;
    let (payment_status, confirmations, amount_credited) = match payment {
        // A reversed deposit is no payment; the page says so rather than ask for one again.
        None if address_has_reversed_deposit(&state.pool, lock.address_id).await? => {
            ("reversed", None, None)
        }
        None => ("none", None, None),
        Some(payment) if payment.status == "seen" => ("seen", payment.confirmations, None),
        Some(payment) => {
            let deposit =
                ids::parse(ids::DEPOSIT, &payment.deposit).ok_or_else(ApiError::internal)?;
            let (deposit_state, credit_minor) = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT state, credit_minor::text FROM deposits WHERE id = $1",
            )
            .bind(deposit)
            .fetch_one(&state.pool)
            .await?;
            match deposit_state.as_str() {
                "credited" | "swept" => {
                    let credit = credit_minor
                        .and_then(|credit| credit.parse::<u64>().ok())
                        .ok_or_else(ApiError::internal)?;
                    ("credited", None, Some(credit))
                }
                "rejected" => ("rejected", None, None),
                "reversed" => ("reversed", None, None),
                _ => ("confirming", None, None),
            }
        }
    };
    let confirmation = route.chain.confirmations.stricter(lock.terms.confirmations);
    Ok(ClientQuote {
        id: locks::quote_id(lock.id),
        object: "quote".to_owned(),
        livemode: lock.livemode,
        status: status(lock.status).to_owned(),
        amount: lock.credit_minor.value(),
        currency: "usd".to_owned(),
        asset: route.asset.symbol.clone(),
        decimals: route.asset.decimals,
        chain_id: lock.chain_id,
        amount_atomic: lock.amount_atomic.value().to_string(),
        address: format!("{:#x}", lock.address),
        payment_uri: payment_uri(route, &lock),
        expires_at: lock.expires_at.timestamp(),
        payment_status: payment_status.to_owned(),
        confirmations,
        amount_credited,
        typical_credit_seconds: confirmation.typical_credit_seconds(lock.chain_id),
    })
}

async fn address_has_reversed_deposit(pool: &PgPool, address_id: Uuid) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deposits WHERE address_id = $1 AND state = 'reversed')",
    )
    .bind(address_id)
    .fetch_one(pool)
    .await?)
}

#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/cancel",
    params(
        ("id" = String, Path, description = "Quote id, `qt_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    responses(
        (status = 200, description = "OK", body = Quote),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse),
        (
            status = 400,
            description = "`quote_payment_received`: the address already received a payment; \
                           `quote_window_closed`: past `expires_at`; `quote_unexpected_state`: \
                           complete or expired",
            body = ErrorResponse
        )
    ),
    security(("api_key" = [])),
    tag = "quotes"
)]
/// Cancels an open, unpaid quote; a canceled quote is returned unchanged. Later payments to its
/// address are credited at spot.
pub(crate) async fn cancel_quote(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Response> {
    let quote = ids::parse(ids::QUOTE, &id).ok_or_else(ApiError::not_found)?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    let lock = locks::cancel(
        &mut *transaction,
        &state.routes,
        merchant.scope,
        &merchant.actor(),
        quote,
    )
    .await
    .map_err(map_error)?;
    let quote = quote_object(&mut transaction, &state.routes, lock).await?;
    idempotent.commit(transaction, Json(quote)).await
}

fn payment_uri(route: &RouteFile, lock: &RateLock) -> String {
    format!(
        "ethereum:{:#x}@{}/transfer?address={:#x}&uint256={}",
        route.asset.contract,
        lock.chain_id,
        lock.address,
        lock.amount_atomic.value()
    )
}

/// Returns the scope's quote `id`.
pub(crate) async fn find_quote(
    connection: &mut PgConnection,
    routes: &RouteSet,
    scope: Scope,
    id: Uuid,
) -> ApiResult<Option<Quote>> {
    match locks::get(&mut *connection, scope, id)
        .await
        .map_err(map_error)?
    {
        Some(lock) => quote_object(connection, routes, lock).await.map(Some),
        None => Ok(None),
    }
}

/// The API representation of a quote.
pub(crate) async fn quote_object(
    connection: &mut PgConnection,
    routes: &RouteSet,
    lock: RateLock,
) -> ApiResult<Quote> {
    let route = quote_route(routes, &lock)?;
    let payment = super::pending::quote_payment(connection, route, &lock).await?;
    Ok(render_quote(route, lock, payment))
}

fn quote_route<'a>(routes: &'a RouteSet, lock: &RateLock) -> ApiResult<&'a RouteFile> {
    // Retired quote versions use the current route's unchanged chain and asset, and frozen terms.
    routes
        .current()
        .find(|route| route.route == lock.route)
        .ok_or_else(|| {
            tracing::error!(route = %lock.route, "quote route is not loaded");
            ApiError::internal()
        })
}

fn render_quote(
    route: &RouteFile,
    lock: RateLock,
    payment: Option<super::models::Payment>,
) -> Quote {
    let payment_uri = payment_uri(route, &lock);
    Quote {
        id: locks::quote_id(lock.id),
        object: "quote".to_owned(),
        livemode: lock.livemode,
        client_reference_id: lock.client_reference_id,
        amount: lock.credit_minor.value(),
        currency: "usd".to_owned(),
        chain_id: lock.chain_id,
        asset: route.asset.symbol.clone(),
        amount_atomic: lock.amount_atomic.value().to_string(),
        exchange_rate: decimal(lock.price.value()),
        address: format!("{:#x}", lock.address),
        treasury: format!("{:#x}", lock.treasury),
        payment_uri,
        status: status(lock.status).to_owned(),
        expires_at: lock.expires_at.timestamp(),
        created: lock.created_at.timestamp(),
        payment,
        deposit: lock
            .consumed_by
            .map(|deposit| ExpandableDeposit::Id(ids::format(ids::DEPOSIT, deposit))),
        client_secret: None,
        terms: quote_terms(&lock.terms),
        metadata: lock.metadata,
    }
}

const fn status(status: RateLockStatus) -> &'static str {
    match status {
        RateLockStatus::Open => "open",
        RateLockStatus::Consumed => "complete",
        RateLockStatus::Expired => "expired",
        RateLockStatus::Cancelled => "canceled",
    }
}

/// An eight-decimal scaled price as an exact decimal string, such as `0.24875621`.
pub(super) fn decimal(scaled: u64) -> String {
    let digits = format!("{scaled:09}");
    let (integer, fraction) = digits.split_at(digits.len().saturating_sub(8));
    format!("{integer}.{fraction}")
}

/// The scaled price of an exact decimal string as [`decimal`] writes it, such as `0.24875621`.
pub(super) fn parse_decimal(text: &str) -> Option<u64> {
    let (integer, fraction) = text.split_once('.')?;
    let scaled = format!("{integer}{fraction}")
        .parse::<u64>()
        .ok()
        .filter(|_| fraction.len() == 8 && integer.bytes().all(|byte| byte.is_ascii_digit()))?;
    (decimal(scaled) == text).then_some(scaled)
}

fn has_quotes_pause(account: &Account, customer: &Customer, route_scopes: &[String]) -> bool {
    account.paused_scopes.iter().any(|scope| scope == "quotes")
        || customer.paused_scopes.iter().any(|scope| scope == "quotes")
        || route_scopes.iter().any(|scope| scope == "quotes")
}

pub(super) fn map_error(error: RateLockError) -> ApiError {
    match error {
        RateLockError::InvalidInput(message) => ApiError::bad_request(message),
        RateLockError::AmountTooSmall(message) => ApiError::amount_too_small("amount", message),
        RateLockError::AmountTooLarge(message) => ApiError::amount_too_large("amount", message),
        RateLockError::PricingUnavailable => {
            ApiError::service_unavailable("validated pricing is unavailable")
        }
        RateLockError::RateLimited { retry_after } => ApiError::customer_quote_limit(retry_after),
        RateLockError::TreasuryNotSet => ApiError::treasury_not_set(),
        RateLockError::AssetNotAccepted => ApiError::asset_not_accepted(Some("asset")),
        RateLockError::SettingsUnconfirmed => ApiError::payment_settings_unconfirmed(),
        RateLockError::SettingsChanged => ApiError::database_busy(),
        error @ (RateLockError::ExposureCap { .. } | RateLockError::QuoteCountCap(_)) => {
            ApiError::exposure_cap(error.to_string())
        }
        RateLockError::NotFound => ApiError::not_found(),
        RateLockError::NotOpen(current) => ApiError::quote_unexpected_state(status(current)),
        RateLockError::WindowClosed => ApiError::quote_window_closed(),
        RateLockError::PendingPayment => ApiError::quote_payment_received(),
        RateLockError::Arithmetic
        | RateLockError::EntropyUnavailable
        | RateLockError::DatabaseInvariant => ApiError::internal(),
        RateLockError::Database(error) => ApiError::from(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_prices_render_exactly() {
        assert_eq!(decimal(24_875_621), "0.24875621");
        assert_eq!(decimal(100_000_000), "1.00000000");
        assert_eq!(decimal(1_234_500_000_001), "12345.00000001");
        assert_eq!(decimal(0), "0.00000000");
    }

    #[test]
    fn rendered_prices_parse_back_and_nothing_else_does() {
        for scaled in [24_875_621, 100_000_000, 1_234_500_000_001, 0] {
            assert_eq!(parse_decimal(&decimal(scaled)), Some(scaled));
        }
        for text in [
            "0.2487562",
            "00.24875621",
            "+0.24875621",
            "1",
            "1.000000000",
            ".24875621",
        ] {
            assert_eq!(parse_decimal(text), None, "{text}");
        }
    }
}
