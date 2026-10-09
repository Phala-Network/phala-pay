//! Axum API: merchant routes authenticated by API keys, the admin routes by RFC 9421
//! signatures, and the generated OpenAPI.

mod account;
mod attestation;
mod auth;
mod cache;
mod client_limit;
mod deadlines;
mod deposit_addresses;
mod deposits;
pub(crate) mod error;
mod events;
mod examples;
mod extract;
mod handlers;
mod idempotency;
mod instance_pause;
mod keys;
pub(crate) mod metadata;
pub mod models;
mod openapi;
mod pagination;
mod payment_settings;
mod pending;
mod quotes;
mod rate_limit;
mod repository;
mod restore;
mod sanctions;
mod sweeps;
mod transactions;
mod treasuries;
mod webhook_endpoints;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::locks::QuoteProvider;
use crate::refunds::DestinationScreener;
use crate::routes::RouteSet;
use crate::tenancy::{Permission, Scope};
use crate::treasuries::ContractSignatures;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{Extension, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use sqlx::PgPool;
use topup_core::route::RouteFile;

use tower::{ServiceBuilder, limit::GlobalConcurrencyLimitLayer, load_shed::LoadShedLayer};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub use attestation::{
    AttestationError, AttestationEvidence, AttestationFuture, AttestationRequest, Attestor,
    WebhookKeysFuture,
};
pub use auth::VerificationKey;
pub use client_limit::ClientReadLimiter;
pub use deadlines::DeadlineListener;
pub use idempotency::IdempotencyKeyPruner;
pub(crate) use keys::api_key_object;
pub use rate_limit::{ApiRateLimiter, HintRateLimiter, RateLimits};
pub use topup_adapters::http_signature::PublicOrigin;
pub(crate) use treasuries::treasury_object;

/// Shared state for all API handlers.
#[derive(Clone)]
pub struct AppState {
    /// Application-role PostgreSQL connection pool.
    pub pool: PgPool,
    /// Attested route configurations.
    pub routes: Arc<RouteSet>,
    /// Positive deployment-wide admission cap; staging uses 1 and production 2.
    pub max_attached_pending_refunds: std::num::NonZeroU32,
    /// Separately configured administrative verification key.
    pub admin_key: VerificationKey,
    /// Least-privilege signing keys for POST instance pause/resume only.
    pub maintenance_keys: Vec<VerificationKey>,
    /// Public origin used to rebuild the signed `@target-uri` of every admin request.
    pub public_origin: PublicOrigin,
    /// Current attestation provider.
    pub attestor: Arc<dyn Attestor>,
    /// Validated current-price provider for rate-lock creation.
    pub rate_lock_quotes: Arc<dyn QuoteProvider>,
    /// Rate limit of anonymous quote reads by `client_secret`.
    pub client_reads: Arc<ClientReadLimiter>,
    /// Per-account and platform rate limits of authenticated merchant requests.
    pub rate_limits: Arc<ApiRateLimiter>,
    /// Dedicated authenticated object limits for transaction hints.
    pub hint_limits: Arc<HintRateLimiter>,
    /// Shared bounded process-local hint queue.
    pub transaction_hints: Arc<crate::hints::HintQueue>,
    /// Sanctions screening of refund destinations and treasuries.
    pub screening: Arc<dyn DestinationScreener>,
    /// Wakes the sanctions worker after an audited manual-list mutation.
    pub sanctions_rescreen: Arc<tokio::sync::Notify>,
    /// EIP-1271 checks of contract treasuries' proofs.
    pub contract_signatures: Arc<dyn ContractSignatures>,
}

impl AppState {
    /// The route a refund of the scope's deposit is checked against: the current version of the
    /// deposit's route, or, for a deposit of an asset without a route, the first current route of
    /// its chain in the scope's mode. A deposit outside the scope is `404`.
    pub(crate) async fn refund_route(
        &self,
        scope: Scope,
        deposit_id: uuid::Uuid,
    ) -> Result<&RouteFile, error::ApiError> {
        let (route, chain_id) = sqlx::query_as::<_, (Option<String>, i64)>(
            "SELECT route, chain_id FROM deposits WHERE id = $1 AND account_id = $2 AND livemode = $3",
        )
        .bind(deposit_id)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| error::ApiError::not_found().with_param("deposit"))?;
        let chain_id = u64::try_from(chain_id).map_err(|_| error::ApiError::internal())?;
        let mut current = self.routes.current_in(scope.livemode());
        match route {
            Some(name) => current.find(|route| route.route == name),
            None => current.find(|route| route.chain.chain_id == chain_id),
        }
        .ok_or_else(|| {
            tracing::error!(%deposit_id, "the deposit's route is not loaded");
            error::ApiError::internal()
        })
    }
}

/// The API representation of the object an event is about, as `GET /v1/deposits/{id}`,
/// `GET /v1/quotes/{id}`, `GET /v1/refunds/{id}`, `GET /v1/api_keys/{id}`,
/// `GET /v1/treasuries/{id}`, `GET /v1/webhook_endpoints/{id}`, or `GET /v1/account` returns it to
/// `scope`, read on `connection` (the transaction recording the event). `Ok(None)` means the
/// object does not exist in `scope`; `Err(())` means rendering failed and was logged.
pub(crate) async fn render_object(
    connection: &mut sqlx::PgConnection,
    routes: &RouteSet,
    scope: Scope,
    object: crate::db::EventObject,
) -> Result<Option<serde_json::Value>, ()> {
    let rendered = match object {
        crate::db::EventObject::Deposit(id) => {
            deposits::find_deposit(&mut *connection, routes, scope, id)
                .await
                .map(|deposit| deposit.map(serde_json::to_value))
        }
        crate::db::EventObject::Quote(id) => quotes::find_quote(connection, routes, scope, id)
            .await
            .map(|quote| quote.map(serde_json::to_value)),
        crate::db::EventObject::Refund(id) => deposits::find_refund(&mut *connection, scope, id)
            .await
            .map(|refund| refund.map(serde_json::to_value)),
        crate::db::EventObject::ApiKey(id) => crate::api_keys::get(&mut *connection, scope, id)
            .await
            .map(|key| key.map(|key| serde_json::to_value(keys::api_key_object(&key, None))))
            .map_err(error::ApiError::from),
        crate::db::EventObject::Treasury(id) => crate::treasuries::get_in(connection, scope, id)
            .await
            .map(|treasury| {
                treasury
                    .map(|treasury| serde_json::to_value(treasuries::treasury_object(&treasury)))
            })
            .map_err(|_| error::ApiError::internal()),
        crate::db::EventObject::Account(id) if id == scope.account_id() => {
            account::find_account(connection, scope)
                .await
                .map(|account| account.map(serde_json::to_value))
        }
        crate::db::EventObject::Account(_) => Ok(None),
        crate::db::EventObject::PaymentSettings(id) if id == scope.account_id() => {
            payment_settings::payment_settings_object(connection, routes, scope)
                .await
                .map(|settings| Some(serde_json::to_value(settings)))
        }
        crate::db::EventObject::PaymentSettings(_) => Ok(None),
        crate::db::EventObject::WebhookEndpoint(id) => {
            crate::webhook_endpoints::find_any(connection, scope, id)
                .await
                .map(|endpoint| endpoint.map(serde_json::to_value))
                .map_err(error::ApiError::from)
        }
    };
    match rendered {
        Ok(Some(Ok(value))) => Ok(Some(value)),
        Ok(None) => Ok(None),
        Ok(Some(Err(error))) => {
            tracing::error!(%error, "event object serialization failed");
            Err(())
        }
        Err(_) => {
            tracing::error!(object = ?object, "event object rendering failed");
            Err(())
        }
    }
}

/// Lets a payer's page on any origin read a `client_secret` view, with the `Request-Id` and
/// `Retry-After` of its responses (CORS; the view carries no credentials).
pub(crate) fn allow_cross_origin(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
        axum::http::HeaderValue::from_static("Request-Id, Retry-After"),
    );
}

/// The `Idempotency-Key` of a request when it is valid, for the events the request causes.
pub(crate) fn idempotency_key_of(headers: &axum::http::HeaderMap) -> Option<String> {
    extract::idempotency_key(headers).ok().flatten()
}

/// The merchant routes authenticated by a secret key.
fn merchant_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(quotes::get_config))
        .routes(routes!(quotes::list_quotes, quotes::create_quote))
        .routes(routes!(quotes::update_quote))
        .routes(routes!(quotes::cancel_quote))
        .routes(routes!(
            deposit_addresses::list_deposit_addresses,
            deposit_addresses::create_deposit_address
        ))
        .routes(routes!(deposit_addresses::update_deposit_address))
        .routes(routes!(deposit_addresses::rotate_deposit_address))
        .routes(routes!(deposits::list_deposits))
        .routes(routes!(deposits::get_deposit, deposits::update_deposit))
        .routes(routes!(deposits::list_refunds, deposits::create_refund))
        .routes(routes!(deposits::get_refund, deposits::update_refund))
        .routes(routes!(deposits::mark_refund_paid))
        .routes(routes!(deposits::cancel_refund))
        .routes(routes!(account::get_account))
        .routes(routes!(
            payment_settings::get_payment_settings,
            payment_settings::update_payment_settings
        ))
        .routes(routes!(account::pause_account))
        .routes(routes!(account::resume_account))
        .routes(routes!(account::roll_webhook_key))
        .routes(routes!(account::get_attestation))
        .routes(routes!(keys::list_api_keys, keys::create_api_key))
        .routes(routes!(keys::get_api_key, keys::revoke_api_key))
        .routes(routes!(keys::roll_api_key))
        .routes(routes!(treasuries::create_treasury_challenge))
        .routes(routes!(
            treasuries::list_treasuries,
            treasuries::create_treasury
        ))
        .routes(routes!(treasuries::get_treasury))
        .routes(routes!(treasuries::cancel_treasury))
        .routes(routes!(treasuries::pause_treasury))
        .routes(routes!(treasuries::resume_treasury))
        .routes(routes!(
            webhook_endpoints::list_webhook_endpoints,
            webhook_endpoints::create_webhook_endpoint
        ))
        .routes(routes!(
            webhook_endpoints::get_webhook_endpoint,
            webhook_endpoints::update_webhook_endpoint,
            webhook_endpoints::delete_webhook_endpoint
        ))
        .routes(routes!(webhook_endpoints::test_webhook_endpoint))
        .routes(routes!(sweeps::get_balance))
        .routes(routes!(sweeps::list_sweeps))
        .routes(routes!(sweeps::list_forwarders))
        .routes(routes!(events::list_events))
        .routes(routes!(events::get_event))
        .routes(routes!(events::resend_event))
}

/// The permission each merchant route requires (design D13), as `(method, path, permission)`:
/// [`auth::authorize`] checks it after authentication and before the idempotency layer. A route
/// missing here is refused.
const ROUTE_PERMISSIONS: &[(&str, &str, Permission)] = &[
    ("GET", "/v1/config", Permission::AccountRead),
    ("GET", "/v1/quotes", Permission::QuotesRead),
    ("POST", "/v1/quotes", Permission::QuotesWrite),
    ("GET", "/v1/quotes/{id}", Permission::QuotesRead),
    ("POST", "/v1/quotes/{id}", Permission::QuotesWrite),
    ("POST", "/v1/quotes/{id}/cancel", Permission::QuotesWrite),
    (
        "POST",
        "/v1/quotes/{id}/transactions",
        Permission::QuotesWrite,
    ),
    (
        "POST",
        "/v1/deposit_addresses/{id}/transactions",
        Permission::DepositAddressesWrite,
    ),
    (
        "GET",
        "/v1/deposit_addresses",
        Permission::DepositAddressesRead,
    ),
    (
        "POST",
        "/v1/deposit_addresses",
        Permission::DepositAddressesWrite,
    ),
    (
        "GET",
        "/v1/deposit_addresses/{id}",
        Permission::DepositAddressesRead,
    ),
    (
        "POST",
        "/v1/deposit_addresses/{id}",
        Permission::DepositAddressesWrite,
    ),
    (
        "POST",
        "/v1/deposit_addresses/{id}/rotate",
        Permission::DepositAddressesWrite,
    ),
    ("GET", "/v1/deposits", Permission::DepositsRead),
    ("GET", "/v1/deposits/{id}", Permission::DepositsRead),
    ("POST", "/v1/deposits/{id}", Permission::DepositsWrite),
    ("GET", "/v1/refunds", Permission::RefundsRead),
    ("POST", "/v1/refunds", Permission::RefundsWrite),
    ("GET", "/v1/refunds/{id}", Permission::RefundsRead),
    ("POST", "/v1/refunds/{id}", Permission::RefundsWrite),
    (
        "POST",
        "/v1/refunds/{id}/mark_paid",
        Permission::RefundsWrite,
    ),
    ("POST", "/v1/refunds/{id}/cancel", Permission::RefundsWrite),
    ("GET", "/v1/account", Permission::AccountRead),
    ("GET", "/v1/payment_settings", Permission::AccountRead),
    ("POST", "/v1/payment_settings", Permission::AccountWrite),
    ("POST", "/v1/account/pause", Permission::AccountWrite),
    ("POST", "/v1/account/resume", Permission::AccountWrite),
    (
        "POST",
        "/v1/account/webhook_keys/roll",
        Permission::AccountWrite,
    ),
    ("GET", "/v1/attestation", Permission::AccountRead),
    ("GET", "/v1/api_keys", Permission::ApiKeysRead),
    ("POST", "/v1/api_keys", Permission::ApiKeysWrite),
    ("GET", "/v1/api_keys/{id}", Permission::ApiKeysRead),
    ("DELETE", "/v1/api_keys/{id}", Permission::ApiKeysWrite),
    ("POST", "/v1/api_keys/{id}/roll", Permission::ApiKeysWrite),
    (
        "POST",
        "/v1/treasuries/challenge",
        Permission::TreasuryWrite,
    ),
    ("GET", "/v1/treasuries", Permission::TreasuryRead),
    ("POST", "/v1/treasuries", Permission::TreasuryWrite),
    ("GET", "/v1/treasuries/{id}", Permission::TreasuryRead),
    (
        "POST",
        "/v1/treasuries/{id}/cancel",
        Permission::TreasuryWrite,
    ),
    (
        "POST",
        "/v1/treasuries/{id}/pause",
        Permission::TreasuryWrite,
    ),
    (
        "POST",
        "/v1/treasuries/{id}/resume",
        Permission::TreasuryWrite,
    ),
    ("GET", "/v1/webhook_endpoints", Permission::EndpointsRead),
    ("POST", "/v1/webhook_endpoints", Permission::EndpointsWrite),
    (
        "GET",
        "/v1/webhook_endpoints/{id}",
        Permission::EndpointsRead,
    ),
    (
        "POST",
        "/v1/webhook_endpoints/{id}",
        Permission::EndpointsWrite,
    ),
    (
        "DELETE",
        "/v1/webhook_endpoints/{id}",
        Permission::EndpointsWrite,
    ),
    (
        "POST",
        "/v1/webhook_endpoints/{id}/test",
        Permission::EndpointsWrite,
    ),
    ("GET", "/v1/balance", Permission::SweepsRead),
    ("GET", "/v1/sweeps", Permission::SweepsRead),
    ("GET", "/v1/forwarders", Permission::ForwardersRead),
    ("GET", "/v1/events", Permission::EventsRead),
    ("GET", "/v1/events/{id}", Permission::EventsRead),
    ("POST", "/v1/events/{id}/resend", Permission::EndpointsWrite),
];

/// The permission the merchant route `method` `path` requires; `path` is the route's template. A
/// `HEAD` requires what its `GET` does, which serves it.
fn required_permission(method: &Method, path: &str) -> Option<Permission> {
    let method = if method == Method::HEAD {
        Method::GET.as_str()
    } else {
        method.as_str()
    };
    ROUTE_PERMISSIONS
        .iter()
        .find(|(route_method, route_path, _)| *route_method == method && *route_path == path)
        .map(|(_, _, permission)| *permission)
}

/// The routes a quote's or deposit address's `client_secret` also reads.
fn client_secret_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(quotes::get_quote))
        .routes(routes!(deposit_addresses::get_deposit_address))
}

/// Object-authenticated hint writes, mounted separately from the shared peer-IP limiter.
fn hint_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(transactions::submit_quote_transaction))
        .routes(routes!(transactions::submit_deposit_address_transaction))
        // Mount preflight routes inside the same global admission and acknowledgement layers.
        .route(
            "/v1/quotes/{id}/transactions",
            axum::routing::options(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/v1/deposit_addresses/{id}/transactions",
            axum::routing::options(|| async { StatusCode::NO_CONTENT }),
        )
}

/// The operator's routes, authenticated by RFC 9421 signatures.
fn admin_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(
            instance_pause::get_instance_pause,
            instance_pause::pause_instance
        ))
        .routes(routes!(instance_pause::resume_instance))
        .routes(routes!(handlers::create_account))
        .routes(routes!(handlers::get_account, handlers::update_account))
        .routes(routes!(handlers::issue_api_key))
        .routes(routes!(handlers::admin_get_deposit))
        .routes(routes!(handlers::pause_account))
        .routes(routes!(handlers::resume_account))
        .routes(routes!(handlers::pause_customer))
        .routes(routes!(handlers::resume_customer))
        .routes(routes!(handlers::pause_treasury))
        .routes(routes!(handlers::resume_treasury))
        .routes(routes!(handlers::pause_route))
        .routes(routes!(handlers::resume_route))
        .routes(routes!(handlers::nudge_deposit))
        .routes(routes!(handlers::lift_reconciliation_block))
        .routes(routes!(handlers::daily_report))
        .routes(routes!(sanctions::list))
        .routes(routes!(sanctions::add))
        .routes(routes!(sanctions::remove))
        .routes(routes!(handlers::metrics))
        .routes(routes!(account::admin_get_attestation))
        .routes(routes!(restore::get_restore))
        .routes(routes!(restore::revoke_api_key))
        .routes(routes!(restore::verify_treasuries))
        .routes(routes!(restore::apply_treasury))
        .routes(routes!(restore::delete_webhook_endpoint))
        .routes(routes!(restore::reissue_deposit_address))
        .routes(routes!(restore::reissue_quote))
        .routes(routes!(restore::import_events))
        .routes(routes!(restore::discard_delivered_credit))
        .routes(routes!(restore::unfreeze))
}

/// utoipa's merchant and admin documents, before [`openapi`] finishes them.
#[cfg(test)]
fn documents() -> (utoipa::openapi::OpenApi, utoipa::openapi::OpenApi) {
    let (_, merchant) = merchant_routes()
        .merge(client_secret_routes())
        .merge(hint_routes())
        .split_for_parts();
    let (_, admin) = admin_routes().split_for_parts();
    (merchant, admin)
}

/// The finished OpenAPI documents: the merchant API's and the operator's.
#[derive(Clone, Debug)]
pub struct ApiDocs {
    /// `openapi.json`: the merchant API.
    pub merchant: serde_json::Value,
    /// `openapi.admin.json`: the admin API.
    pub admin: serde_json::Value,
}

/// Builds the Axum router, serving both OpenAPI documents, and the documents.
pub fn router(state: AppState) -> (Router, ApiDocs) {
    router_with_pause(state, Arc::new(crate::pause::InstancePause::default()))
}

/// Starts the production API with mutations paused until the first healthy probe. This boot
/// gate replaces a terminated process's lease without persisting maintenance across rollback.
pub fn booting_router(state: AppState) -> (Router, ApiDocs) {
    router_with_pause(state, Arc::new(crate::pause::InstancePause::booting()))
}

fn router_with_pause(
    state: AppState,
    pause: Arc<crate::pause::InstancePause>,
) -> (Router, ApiDocs) {
    let (router, docs) = router_inner(state, pause);
    (
        router
            .layer(middleware::from_fn(cache::no_store))
            .layer(middleware::from_fn(crate::observability::request_context)),
        docs,
    )
}

fn router_inner(state: AppState, pause: Arc<crate::pause::InstancePause>) -> (Router, ApiDocs) {
    let merchant_auth = auth::MerchantAuthState::new(state.clone());
    // Every merchant POST is idempotent by `Idempotency-Key`; authentication and then
    // authorization run first, so a replay needs the route's permission too.
    let merchant = merchant_routes()
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            idempotency::idempotent_post,
        ))
        .route_layer(middleware::from_fn(instance_pause::admit_request))
        .route_layer(middleware::from_fn(auth::authorize))
        // Also the freeze gate: while frozen after a restore, a request with a key is refused
        // here, before authorization and the idempotency layer.
        .route_layer(middleware::from_fn_with_state(
            merchant_auth.clone(),
            auth::authenticate_merchant,
        ));
    // A quote and a deposit address are also readable without an API key by a `client_secret`.
    let client_secret = client_secret_routes()
        .route_layer(middleware::from_fn(auth::authorize))
        .route_layer(middleware::from_fn_with_state(
            merchant_auth.clone(),
            auth::authenticate_merchant_or_client_secret,
        ));
    let admin = admin_routes()
        .route_layer(middleware::from_fn(instance_pause::admit_request))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate_admin,
        ));
    let hints = hint_routes()
        .route_layer(middleware::from_fn(auth::authorize))
        // Keep merchant-key hints on the shared authentication and admission path.
        .route_layer(middleware::from_fn_with_state(
            merchant_auth,
            auth::authenticate_merchant_or_client_secret,
        ));
    // No per-client IP is available behind the ingress, so hints have no per-IP limit.
    // Authenticated object limits, the daily hard cap and the in-flight cap bound hint work.
    let merchant = merchant.merge(client_secret).merge(hints);
    router_from_routes(state, pause, merchant, admin)
}

fn router_from_routes(
    state: AppState,
    pause: Arc<crate::pause::InstancePause>,
    merchant: OpenApiRouter<AppState>,
    admin: OpenApiRouter<AppState>,
) -> (Router, ApiDocs) {
    let (merchant_router, merchant_doc) = merchant.split_for_parts();
    let (admin_router, admin_doc) = admin.split_for_parts();
    let docs = ApiDocs {
        merchant: openapi::merchant(&merchant_doc),
        admin: openapi::admin(&admin_doc),
    };
    let router = merchant_router
        .merge(admin_router)
        // Bound unauthenticated work before API-key verification can touch PostgreSQL.
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_| async {
                    error::ApiError::database_busy().into_response()
                }))
                .layer(LoadShedLayer::new())
                .layer(GlobalConcurrencyLimitLayer::new(256))
                .layer(middleware::from_fn(transactions::read_body))
                .layer(middleware::from_fn(transactions::received))
                .layer(middleware::from_fn(deadlines::request_deadline)),
        )
        .route("/healthz", get(healthz))
        .route("/openapi.json", get(serve_openapi))
        .route("/openapi.admin.json", get(serve_admin_openapi))
        // Stripe's error object for any other path or method too, never an empty body.
        .fallback(unrecognized_request)
        .method_not_allowed_fallback(unrecognized_request)
        .layer(Extension(Arc::new(docs.clone())))
        .layer(Extension(pause))
        .with_state(state);

    (router, docs)
}

async fn unrecognized_request() -> Response {
    error::ApiError::unrecognized_request().into_response()
}

/// Seconds `Retry-After` asks a client to wait before retrying a request refused while the service
/// is frozen after a restore: reconciliation takes minutes to hours.
const RESTORE_RETRY_AFTER_SECONDS: u64 = 300;

/// `503 service_restoring` with `Retry-After`.
fn restoring() -> Response {
    error::ApiError::service_restoring(RESTORE_RETRY_AFTER_SECONDS).into_response()
}

/// Builds the router of an instance restored from backup (`TOPUP_SERVICE_ENABLED=read-only`,
/// `deploy/RESTORE.md`): every request other than `GET`, `HEAD`, and the operator's restore
/// reconciliation (`/v1/admin/restore/…`) is refused with `503 service_restoring`, and so is every
/// request with a merchant API key, frozen or not. Transaction hints return quiet `202`s and
/// enqueue nothing in this mode. `restore-check` records the freeze in parallel,
/// and may fail before it does. `/healthz` reports the boot-time `restore-check` result read from
/// `restore_report`.
pub fn read_only_router(state: AppState, restore_report: Option<PathBuf>) -> Router {
    let (router, _) = router_inner(state, Arc::new(crate::pause::InstancePause::default()));
    router
        .layer(middleware::from_fn(reject_writes))
        .layer(middleware::from_fn(cache::no_store))
        .layer(Extension(ReadOnly {
            restore_report: restore_report.map(Arc::from),
        }))
        .layer(middleware::from_fn(crate::observability::request_context))
}

/// Marks a read-only router and locates its restore-check report.
#[derive(Clone)]
struct ReadOnly {
    restore_report: Option<Arc<Path>>,
}

async fn reject_writes(request: Request, next: Next) -> Response {
    let merchant_key = request
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION);
    if transactions::is_submission(&request)
        || (!merchant_key
            && (matches!(*request.method(), Method::GET | Method::HEAD)
                || request.uri().path().starts_with("/v1/admin/restore/")))
    {
        next.run(request).await
    } else {
        restoring()
    }
}

/// The merchant API's deterministic pretty-printed OpenAPI document, `openapi.json`.
pub fn openapi_json(state: AppState) -> Result<String, serde_json::Error> {
    let (_, docs) = router(state);
    serde_json::to_string_pretty(&docs.merchant).map(|json| format!("{json}\n"))
}

/// The admin API's deterministic pretty-printed OpenAPI document, `openapi.admin.json`.
pub fn openapi_admin_json(state: AppState) -> Result<String, serde_json::Error> {
    let (_, docs) = router(state);
    serde_json::to_string_pretty(&docs.admin).map(|json| format!("{json}\n"))
}

async fn serve_openapi(Extension(docs): Extension<Arc<ApiDocs>>) -> Json<serde_json::Value> {
    Json(docs.merchant.clone())
}

async fn serve_admin_openapi(Extension(docs): Extension<Arc<ApiDocs>>) -> Json<serde_json::Value> {
    Json(docs.admin.clone())
}

async fn healthz(
    State(state): State<AppState>,
    Extension(pause): Extension<Arc<crate::pause::InstancePause>>,
    read_only: Option<Extension<ReadOnly>>,
) -> Response {
    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&state.pool),
    )
    .await
    {
        Ok(Ok(1)) => StatusCode::OK,
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    if status == StatusCode::OK {
        pause.healthy_boot().await;
    }
    let Some(Extension(read_only)) = read_only else {
        return status.into_response();
    };
    let restore_check = match &read_only.restore_report {
        Some(path) => read_restore_report(path).await,
        None => serde_json::Value::Null,
    };
    (
        status,
        Json(serde_json::json!({ "mode": "read-only", "restore_check": restore_check })),
    )
        .into_response()
}

/// The restore-check report, or `null` while it has not been written.
async fn read_restore_report(path: &Arc<Path>) -> serde_json::Value {
    let file = Arc::clone(path);
    let read = tokio::task::spawn_blocking(move || std::fs::read(&file))
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error)));
    match read {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            tracing::warn!(path = %path.display(), "restore-check report is not valid JSON");
            serde_json::Value::Null
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to read restore-check report");
            serde_json::Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::SigningKey;
    use tower::ServiceExt as _;

    use super::{AppState, VerificationKey};

    /// Occupying every unauthenticated permit cannot shed health checks, and trickled bodies
    /// expire even though the sender never signals EOF.
    #[tokio::test(start_paused = true)]
    async fn slow_admin_bodies_expire_and_health_bypasses_capacity() {
        use std::future::Future as _;
        use std::task::Poll;
        let app = super::router(offline_state()).0;
        let stalled = || {
            Request::post("/v1/admin/accounts")
                .body(Body::from_stream(futures_util::stream::pending::<
                    Result<axum::body::Bytes, std::io::Error>,
                >()))
                .unwrap()
        };
        let mut requests = (0..256)
            .map(|_| Box::pin(app.clone().oneshot(stalled())))
            .collect::<Vec<_>>();
        std::future::poll_fn(|cx| {
            for request in &mut requests {
                assert!(request.as_mut().poll(cx).is_pending());
            }
            Poll::Ready(())
        })
        .await;
        let shed = app.clone().oneshot(stalled()).await.unwrap();
        assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);
        let health = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // This offline pool fails health's SELECT 1; its empty body proves health ran rather
        // than the concurrency layer's JSON database_busy response.
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(to_bytes(health.into_body(), 1024).await.unwrap().is_empty());
        tokio::time::advance(super::deadlines::BODY_READ_TIMEOUT).await;
        for request in requests {
            assert_eq!(request.await.unwrap().status(), StatusCode::UNAUTHORIZED);
        }
        let after = app
            .oneshot(
                Request::post("/v1/admin/accounts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    }

    /// Every merchant route declares the permission it requires, and every declaration is a
    /// route, so no route is left to a handler's own check.
    #[test]
    fn every_merchant_route_declares_its_permission() {
        let (merchant, _) = super::documents();
        let mut routes: Vec<(&str, String)> = merchant
            .paths
            .paths
            .iter()
            .flat_map(|(path, item)| {
                [
                    ("GET", item.get.is_some()),
                    ("POST", item.post.is_some()),
                    ("DELETE", item.delete.is_some()),
                    ("PUT", item.put.is_some()),
                    ("PATCH", item.patch.is_some()),
                ]
                .into_iter()
                .filter(|(_, present)| *present)
                .map(|(method, _)| (method, path.clone()))
            })
            .collect();
        let mut declared: Vec<(&str, String)> = super::ROUTE_PERMISSIONS
            .iter()
            .map(|(method, path, _)| (*method, (*path).to_owned()))
            .collect();
        routes.sort();
        declared.sort();
        assert_eq!(routes, declared);
    }

    /// A state whose database is unreachable: a request that reads it fails.
    fn offline_state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool URL is valid");
        let admin_key = SigningKey::from_bytes(&[1; 32]);
        AppState {
            pool,
            max_attached_pending_refunds: std::num::NonZeroU32::new(2)
                .expect("positive refund limit"),
            routes: Arc::default(),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                "admin/v1".to_owned(),
                &STANDARD.encode(admin_key.verifying_key().as_bytes()),
            )
            .expect("admin key is valid"),
            public_origin: super::PublicOrigin::parse("http://api.test")
                .expect("test origin is valid"),
            attestor: Arc::new(topup_adapters::attestation::DstackAttestor::new()),
            rate_lock_quotes: Arc::new(crate::locks::UnavailableQuoteProvider),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            hint_limits: Arc::default(),
            transaction_hints: Arc::default(),
            screening: Arc::new(crate::refunds::UnavailableDestinationScreener),
            sanctions_rescreen: Arc::default(),
            contract_signatures: Arc::new(crate::treasuries::UnavailableContractSignatures),
        }
    }

    /// A merchant request without a well-formed key is refused before the freeze gate or any
    /// other database read: it answers `401` though the database is unreachable, while a
    /// well-formed key reaches the database and fails there.
    #[tokio::test]
    async fn a_request_without_a_well_formed_key_reads_no_database() {
        let router = super::router(offline_state()).0;
        let send = |method: &str, uri: &str, authorization: Option<&str>| {
            let mut request = Request::builder().method(method).uri(uri);
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            let request = request.body(Body::empty()).expect("request builds");
            let router = router.clone();
            async move {
                let response = router.oneshot(request).await.expect("request is served");
                let status = response.status();
                let body = to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("response body reads");
                let body: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
                (status, body["error"]["code"].clone())
            }
        };
        assert_eq!(
            send("POST", "/v1/api_keys", None).await,
            (StatusCode::UNAUTHORIZED, "api_key_missing".into())
        );
        assert_eq!(
            send("GET", "/v1/account", Some("Bearer ppay_sk_live_invalid")).await,
            (StatusCode::UNAUTHORIZED, "api_key_invalid".into())
        );
        assert_eq!(
            send("GET", "/v1/account", Some("Basic dXNlcjpwYXNz")).await,
            (StatusCode::UNAUTHORIZED, "api_key_invalid".into())
        );
        let key = crate::api_keys::generate(crate::api_keys::KeyKind::Secret, true)
            .expect("a key generates");
        let (status, _) = send("GET", "/v1/account", Some(&format!("Bearer {}", *key))).await;
        assert!(status.is_server_error(), "{status}");
    }

    #[tokio::test]
    async fn read_only_and_auth_errors_are_observed_on_the_real_router() {
        for (router, method, uri, expected) in [
            (
                super::router(offline_state()).0,
                "HEAD",
                "/v1/account",
                StatusCode::UNAUTHORIZED,
            ),
            (
                super::read_only_router(offline_state(), None),
                "DELETE",
                "/v1/quotes",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let class = if expected.is_server_error() {
                "5xx"
            } else {
                "4xx"
            };
            let before = crate::observability::metrics::http_observations(uri, method, class);
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let response = router.oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected);
            assert!(response.headers().contains_key("request-id"));
            assert_eq!(response.headers()["cache-control"], "no-store");
            if expected == StatusCode::SERVICE_UNAVAILABLE {
                assert_eq!(response.headers()["retry-after"], "300");
                let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["code"], "service_restoring");
            }
            assert_eq!(
                crate::observability::metrics::http_observations(uri, method, class),
                before + 1
            );
        }
        let metrics = crate::observability::metrics::render(&offline_state().pool).unwrap();
        assert!(metrics.contains("method=\"HEAD\",route=\"/v1/account\",status_class=\"4xx\""));
        assert!(metrics.contains("method=\"DELETE\",route=\"/v1/quotes\",status_class=\"5xx\""));
    }

    #[tokio::test]
    async fn successful_real_router_responses_keep_cache_protection_and_one_observation() {
        for router in [
            super::router(offline_state()).0,
            super::read_only_router(offline_state(), None),
        ] {
            let before = crate::observability::metrics::http_observations(
                "/openapi.admin.json",
                "HEAD",
                "2xx",
            );
            let response = router
                .oneshot(
                    Request::builder()
                        .method("HEAD")
                        .uri("/openapi.admin.json")
                        .header("signature", "invalid")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().contains_key("request-id"));
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(
                crate::observability::metrics::http_observations(
                    "/openapi.admin.json",
                    "HEAD",
                    "2xx"
                ),
                before + 1,
            );
        }
    }

    #[tokio::test]
    async fn real_router_overload_is_observed_once_before_authentication() {
        for read_only in [false, true] {
            let state = offline_state();
            let pool = state.pool.clone();
            let router = if read_only {
                super::read_only_router(state, None)
            } else {
                super::router(state).0
            };
            let mut admitted = Vec::new();
            // Poll slow admin reads into the real concurrency gate before signature verification.
            // No reactor turn occurs before saturation, so failures cannot free permits.
            for _ in 0..256 {
                let request = Request::builder()
                    .uri("/v1/admin/reports/daily")
                    .body(Body::from_stream(futures_util::stream::pending::<
                        Result<axum::body::Bytes, std::io::Error>,
                    >()))
                    .unwrap();
                let mut response = Box::pin(router.clone().oneshot(request));
                assert!(futures_util::poll!(&mut response).is_pending());
                admitted.push(response);
            }
            let before =
                crate::observability::metrics::http_observations("/v1/treasuries", "GET", "5xx");
            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/v1/treasuries")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            drop(admitted);
            pool.close().await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(response.headers().contains_key("request-id"));
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(response.headers()["retry-after"], "1");
            assert_eq!(response.headers()["content-type"], "application/json");
            let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "api_error");
            assert_eq!(body["error"]["code"], "unavailable");
            assert_eq!(
                crate::observability::metrics::http_observations("/v1/treasuries", "GET", "5xx"),
                before + 1,
            );
        }
    }

    #[tokio::test]
    async fn slow_hint_bodies_are_bounded_by_global_admission() {
        for read_only in [false, true] {
            for path in [
                "/v1/quotes/qt_test/transactions",
                "/v1/deposit_addresses/da_test/transactions",
            ] {
                let state = offline_state();
                let pool = state.pool.clone();
                let router = if read_only {
                    super::read_only_router(state, None)
                } else {
                    super::router(state).0
                };
                let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let request = || {
                    let reads = Arc::clone(&reads);
                    Request::post(path)
                        .body(Body::from_stream(futures_util::stream::poll_fn(
                            move |_| {
                                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                std::task::Poll::Pending::<
                                    Option<Result<axum::body::Bytes, std::io::Error>>,
                                >
                            },
                        )))
                        .unwrap()
                };
                let mut admitted = Vec::new();
                for _ in 0..256 {
                    let mut response = Box::pin(router.clone().oneshot(request()));
                    assert!(futures_util::poll!(&mut response).is_pending());
                    admitted.push(response);
                }
                assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 256);
                let mut rejected = Box::pin(router.clone().oneshot(request()));
                let response = match futures_util::poll!(&mut rejected) {
                    std::task::Poll::Ready(Ok(response)) => response,
                    _ => panic!("overloaded hint must be rejected before reading its body"),
                };
                assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 256);
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers()["retry-after"], "1");
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["code"], "unavailable");
                drop(admitted);
                let response = router
                    .oneshot(
                        Request::post(path)
                            .body(Body::from(r#"{"transaction_hash":"after-release"}"#))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::ACCEPTED);
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["transaction_hash"], "after-release");
                pool.close().await;
            }
        }
    }

    #[tokio::test]
    async fn unknown_paths_are_not_found_with_a_request_id() {
        let state = offline_state();
        let response = super::router(state)
            .0
            .oneshot(
                Request::builder()
                    .uri("/unknown")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("public request succeeds");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response
                .headers()
                .get("request-id")
                .and_then(|id| id.to_str().ok())
                .is_some_and(|id| id.starts_with("req_") && id.len() == 36)
        );
        let _ = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body reads");
    }
}
