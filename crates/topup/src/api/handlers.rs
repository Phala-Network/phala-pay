//! Typed API handlers for the routes in architecture §12.

use std::str::FromStr;

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::header;
use axum::response::Response;
use topup_core::screening::PauseScope;
use uuid::Uuid;

use crate::audit::Actor;
use crate::db::Customer;
use crate::tenancy::Scope;

use super::AppState;
use super::auth::AdminActor;
use super::error::{ApiError, ErrorResponse};
use super::extract::{ApiJson, ApiPath};
use super::models::{
    AccountLimits, AccountPauseRequest, AccountPaymentSettings, AccountResponse,
    AdminReasonRequest, AdminTreasuryPauseRequest, ApiKeyObject, Contact, CreateAccountRequest,
    CustomerPauseRequest, DailyReportResponse, Deposit, IssueApiKeyRequest, NudgeResponse,
    PauseRequest, PauseResponse, ReconciliationBlockLiftResponse, RoutePauseResponse, Treasury,
    UpdateAccountRequest,
};
use super::repository::{self, IssuedAccount};

type ApiResult<T> = Result<T, ApiError>;

/// The daily report lists webhook endpoints failing for longer than this, by default.
const DEFAULT_FAILING_FOR_HOURS: u32 = 24;

#[utoipa::path(
    post,
    path = "/v1/admin/accounts",
    request_body = CreateAccountRequest,
    responses(
        (status = 200, description = "OK: created, with its first secret keys", body = AccountResponse),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Creates a merchant account after the operator's offline due diligence (design D8): records the
/// contact and the due diligence, decides live mode (`charges_enabled`, D12), and returns the
/// first secret key of test mode and, with live mode, of live mode. Each key's `secret` is shown
/// only in this response; send it to the contact, who rolls it on receipt. Audited.
pub(crate) async fn create_account(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<CreateAccountRequest>,
) -> ApiResult<Json<AccountResponse>> {
    let name = request.name.trim();
    validate_label("name", name)?;
    validate_contact(&request.contact)?;
    validate_label("due_diligence.reference", &request.due_diligence.reference)?;
    validate_label(
        "due_diligence.reviewed_by",
        &request.due_diligence.reviewed_by,
    )?;
    validate_reason(&request.reason)?;
    let account = repository::create_account(
        &state.pool,
        &repository::NewAccount {
            name,
            contact: to_json(&request.contact)?,
            due_diligence: to_json(&request.due_diligence)?,
            charges_enabled: request.charges_enabled,
        },
        &actor,
        &request.reason,
    )
    .await?;
    Ok(Json(account_response(&state, account).await?))
}

#[utoipa::path(
    get,
    path = "/v1/admin/accounts/{account}",
    params(("account" = String, Path, description = "Account id, `acct_…`")),
    responses(
        (status = 200, description = "OK", body = AccountResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no account has this id", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// An account, with its caps and its payment settings in each mode: what it accepts, on what
/// terms, and its effective config (`available`). `api_keys` is empty.
pub(crate) async fn get_account(
    State(state): State<AppState>,
    ApiPath(account): ApiPath<String>,
) -> ApiResult<Json<AccountResponse>> {
    let account_id = parse_account_id(&account)?;
    let account = repository::admin_account(&state.pool, account_id).await?;
    Ok(Json(account_response(&state, account).await?))
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}",
    params(("account" = String, Path, description = "Account id, `acct_…`")),
    request_body = UpdateAccountRequest,
    responses(
        (status = 200, description = "OK: updated, or already holding these values", body = AccountResponse),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no account has this id", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Updates an account: live mode (enabling it returns the first live key), the restricted flag,
/// the contact, the cap on credit before finality, or the caps of one mode (`limits`). Audited, and announced to the account as `account.updated`. The operator does
/// not manage the account's webhook endpoints: the merchant does, with `/v1/webhook_endpoints`.
pub(crate) async fn update_account(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(account): ApiPath<String>,
    ApiJson(request): ApiJson<UpdateAccountRequest>,
) -> ApiResult<Json<AccountResponse>> {
    let account_id = parse_account_id(&account)?;
    if let Some(contact) = &request.contact {
        validate_contact(contact)?;
    }
    validate_reason(&request.reason)?;
    let account = repository::update_account(
        &state.pool,
        &state.routes,
        account_id,
        &repository::AccountChanges {
            charges_enabled: request.charges_enabled,
            restricted: request.restricted,
            contact: request.contact.as_ref().map(to_json).transpose()?,
            max_unfinalized_credit: request
                .max_unfinalized_credit
                .map(|cap| {
                    i64::try_from(cap).map_err(|_| {
                        ApiError::invalid_param(
                            "max_unfinalized_credit",
                            "max_unfinalized_credit is too large",
                        )
                    })
                })
                .transpose()?,
            limits: request.limits.as_ref().map(|limits| {
                (
                    limits.livemode,
                    crate::limits::LimitsChange {
                        max_open_quotes: limits.max_open_quotes,
                        max_open_amount_per_account: limits.max_open_amount_per_account,
                        max_open_amount_per_customer: limits.max_open_amount_per_customer,
                        max_active_deposit_addresses: limits.max_active_deposit_addresses,
                    },
                )
            }),
        },
        &actor,
        &request.reason,
    )
    .await?;
    Ok(Json(account_response(&state, account).await?))
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/api_keys",
    params(("account" = String, Path, description = "Account id, `acct_…`")),
    request_body = IssueApiKeyRequest,
    responses(
        (status = 200, description = "OK: the key with its `secret`, shown once", body = ApiKeyObject),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "`testmode_charges_only`: a live key for an account without live mode", body = ErrorResponse),
        (status = 404, description = "Not Found: no account has this id", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Issues a recovery key (design D7) after the operator verified the request with the recorded
/// contact, optionally revoking every key of the mode first. Audited, and announced as
/// `api_key.*` events with actor `admin`.
pub(crate) async fn issue_api_key(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(account): ApiPath<String>,
    ApiJson(request): ApiJson<IssueApiKeyRequest>,
) -> ApiResult<Response> {
    let account_id = parse_account_id(&account)?;
    validate_reason(&request.reason)?;
    super::keys::validate_name(&request.name)?;
    let issued = crate::api_keys::recover(
        &state.pool,
        Scope::new(account_id, request.livemode),
        &request.name,
        request.revoke_existing,
        &actor,
        &request.reason,
    )
    .await
    .map_err(super::keys::map_error)?;
    Ok(super::keys::issued_response(&issued))
}

#[utoipa::path(
    get,
    path = "/v1/admin/deposits/{id}",
    params(("id" = String, Path, description = "Deposit id, `dep_…`")),
    responses(
        (status = 200, description = "OK: the deposit as its account sees it, with `admin`", body = Deposit),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// One deposit of any account, as its account sees it, with its internals, transitions, and
/// events in `admin`.
pub(crate) async fn admin_get_deposit(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Deposit>> {
    let id = crate::ids::parse(crate::ids::DEPOSIT, &id).ok_or_else(ApiError::not_found)?;
    let (scope, admin) = repository::admin_deposit(&state.pool, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mut deposit = super::deposits::find_deposit(&state.pool, &state.routes, scope, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    deposit.admin = Some(Box::new(admin));
    Ok(Json(deposit))
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/pause",
    params(("account" = String, Path, description = "Account id, `acct_…`")),
    request_body = AccountPauseRequest,
    responses((status = 200, description = "OK", body = PauseResponse), (status = 400, description = "Bad Request", body = ErrorResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Pauses scopes of a whole account in both modes, for example `quotes` and `settlement` for an
/// abusive account or a sanctioned treasury. Audited, and announced as `account.updated`.
pub(crate) async fn pause_account(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(account): ApiPath<String>,
    ApiJson(request): ApiJson<AccountPauseRequest>,
) -> ApiResult<Json<PauseResponse>> {
    mutate_account_scopes(&state, &actor, &account, request, true).await
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/resume",
    params(("account" = String, Path, description = "Account id, `acct_…`")),
    request_body = AccountPauseRequest,
    responses((status = 200, description = "OK", body = PauseResponse), (status = 400, description = "Bad Request", body = ErrorResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Resumes scopes of a whole account, for example after a sanctioned treasury was replaced and
/// reviewed. Audited, and announced as `account.updated`.
pub(crate) async fn resume_account(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(account): ApiPath<String>,
    ApiJson(request): ApiJson<AccountPauseRequest>,
) -> ApiResult<Json<PauseResponse>> {
    mutate_account_scopes(&state, &actor, &account, request, false).await
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/customers/{client_reference_id}/pause",
    params(
        ("account" = String, Path, description = "Account id, `acct_…`"),
        ("client_reference_id" = String, Path, description = "The account's identifier of its customer")
    ),
    request_body = CustomerPauseRequest,
    responses((status = 200, description = "OK", body = PauseResponse), (status = 400, description = "Bad Request", body = ErrorResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Pauses scopes of one customer of an account, for example `settlement` to stop crediting it.
pub(crate) async fn pause_customer(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath((account, client_reference_id)): ApiPath<(String, String)>,
    ApiJson(request): ApiJson<CustomerPauseRequest>,
) -> ApiResult<Json<PauseResponse>> {
    mutate_customer_scopes(
        &state,
        &actor,
        &account,
        &client_reference_id,
        request,
        true,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/customers/{client_reference_id}/resume",
    params(
        ("account" = String, Path, description = "Account id, `acct_…`"),
        ("client_reference_id" = String, Path, description = "The account's identifier of its customer")
    ),
    request_body = CustomerPauseRequest,
    responses((status = 200, description = "OK", body = PauseResponse), (status = 400, description = "Bad Request", body = ErrorResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Resumes scopes of one customer of an account.
pub(crate) async fn resume_customer(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath((account, client_reference_id)): ApiPath<(String, String)>,
    ApiJson(request): ApiJson<CustomerPauseRequest>,
) -> ApiResult<Json<PauseResponse>> {
    mutate_customer_scopes(
        &state,
        &actor,
        &account,
        &client_reference_id,
        request,
        false,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/treasuries/{treasury}/pause",
    params(
        ("account" = String, Path, description = "Account id, `acct_…`"),
        ("treasury" = String, Path, description = "Treasury id, `trs_…`")
    ),
    request_body = AdminTreasuryPauseRequest,
    responses((status = 200, description = "OK", body = Treasury), (status = 400, description = "Bad Request", body = ErrorResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Pauses crediting of deposits to every forwarder over the treasury's address, for an incident
/// such as a compromised former treasury: deposits stay `pending` and no `deposit.credited` is
/// sent until the operator resumes; the merchant's own resume does not lift it. Audited, and
/// announced as `treasury.updated`.
pub(crate) async fn pause_treasury(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath((account, treasury)): ApiPath<(String, String)>,
    ApiJson(request): ApiJson<AdminTreasuryPauseRequest>,
) -> ApiResult<Json<Treasury>> {
    set_treasury_crediting_paused(&state, &actor, &account, &treasury, &request, true).await
}

#[utoipa::path(
    post,
    path = "/v1/admin/accounts/{account}/treasuries/{treasury}/resume",
    params(
        ("account" = String, Path, description = "Account id, `acct_…`"),
        ("treasury" = String, Path, description = "Treasury id, `trs_…`")
    ),
    request_body = AdminTreasuryPauseRequest,
    responses((status = 200, description = "OK", body = Treasury), (status = 400, description = "Bad Request", body = ErrorResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Lifts the operator's crediting pause of the treasury; a pause the merchant set stays. Audited,
/// and announced as `treasury.updated`.
pub(crate) async fn resume_treasury(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath((account, treasury)): ApiPath<(String, String)>,
    ApiJson(request): ApiJson<AdminTreasuryPauseRequest>,
) -> ApiResult<Json<Treasury>> {
    set_treasury_crediting_paused(&state, &actor, &account, &treasury, &request, false).await
}

async fn set_treasury_crediting_paused(
    state: &AppState,
    actor: &Actor,
    account: &str,
    treasury: &str,
    request: &AdminTreasuryPauseRequest,
    pause: bool,
) -> ApiResult<Json<Treasury>> {
    if request.reason.trim().is_empty() {
        return Err(ApiError::invalid_param("reason", "reason is required"));
    }
    let account_id = parse_account_id(account)?;
    let id = crate::ids::parse(crate::ids::TREASURY, treasury).ok_or_else(ApiError::not_found)?;
    let livemode: bool =
        sqlx::query_scalar("SELECT livemode FROM treasuries WHERE id = $1 AND account_id = $2")
            .bind(id)
            .bind(account_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(ApiError::not_found)?;
    let treasury = crate::treasuries::set_crediting_paused(
        &state.pool,
        Scope::new(account_id, livemode),
        id,
        crate::pause::PauseOwner::Operator,
        pause,
        actor,
        &request.reason,
    )
    .await
    .map_err(super::treasuries::map_error)?;
    Ok(Json(super::treasuries::treasury_object(&treasury)))
}

#[utoipa::path(
    post,
    path = "/v1/admin/routes/{route}/pause",
    params(("route" = String, Path, description = "Route name, as in the route file")),
    request_body = PauseRequest,
    responses((status = 200, description = "OK", body = RoutePauseResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
pub(crate) async fn pause_route(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(route): ApiPath<String>,
    ApiJson(request): ApiJson<PauseRequest>,
) -> ApiResult<Json<RoutePauseResponse>> {
    mutate_route_scopes(&state, &actor, &route, request, true).await
}

#[utoipa::path(
    post,
    path = "/v1/admin/routes/{route}/resume",
    params(("route" = String, Path, description = "Route name, as in the route file")),
    request_body = PauseRequest,
    responses((status = 200, description = "OK", body = RoutePauseResponse), (status = 404, description = "Not Found", body = ErrorResponse)),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
pub(crate) async fn resume_route(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(route): ApiPath<String>,
    ApiJson(request): ApiJson<PauseRequest>,
) -> ApiResult<Json<RoutePauseResponse>> {
    mutate_route_scopes(&state, &actor, &route, request, false).await
}

#[utoipa::path(
    post,
    path = "/v1/admin/deposits/{id}/nudge",
    params(("id" = String, Path, description = "Deposit id, `dep_…`")),
    responses(
        (status = 200, description = "OK", body = NudgeResponse),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
pub(crate) async fn nudge_deposit(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(deposit_id): ApiPath<String>,
) -> ApiResult<Json<NudgeResponse>> {
    let deposit_id =
        crate::ids::parse(crate::ids::DEPOSIT, &deposit_id).ok_or_else(ApiError::not_found)?;
    Ok(Json(
        repository::nudge_deposit(&state.pool, deposit_id, &actor).await?,
    ))
}

#[utoipa::path(
    post,
    path = "/v1/admin/reconciliation_blocks/{block_key}/lift",
    params(("block_key" = String, Path, description = "`chain:{chain_id}` or `address:{address_id}`, as listed in the daily report")),
    request_body = AdminReasonRequest,
    responses(
        (status = 200, description = "OK: lifted, or already lifted", body = ReconciliationBlockLiftResponse),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 404, description = "Not Found: no active or lifted block has this key", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
pub(crate) async fn lift_reconciliation_block(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(block_key): ApiPath<String>,
    ApiJson(request): ApiJson<AdminReasonRequest>,
) -> ApiResult<Json<ReconciliationBlockLiftResponse>> {
    validate_reason(&request.reason)?;
    Ok(Json(
        repository::lift_reconciliation_block(&state.pool, &block_key, &actor, &request.reason)
            .await?,
    ))
}

#[utoipa::path(
    get,
    path = "/v1/admin/metrics",
    responses((
        status = 200,
        description = "OK: the process's counters in the Prometheus text format, such as \
                       `topup_rpc_calls_total{provider, chain_id, method}`; they restart at zero \
                       with the process",
        body = String,
        content_type = "text/plain"
    )),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
pub(crate) async fn metrics(
    State(state): State<AppState>,
) -> Result<([(header::HeaderName, &'static str); 1], String), ApiError> {
    let durable = crate::observability::metrics::render_chain_reads(&state.pool)
        .await
        .map_err(|error| {
            tracing::error!(%error,"chain read metrics refresh failed");
            ApiError::internal()
        })?;
    Ok((
        [(
            header::CONTENT_TYPE,
            crate::observability::metrics::CONTENT_TYPE,
        )],
        crate::observability::metrics::render(&state.pool).map_err(|error| {
            tracing::error!(%error, "metrics encoding failed");
            ApiError::internal()
        })? + &durable,
    ))
}

#[utoipa::path(
    get,
    path = "/v1/admin/reports/daily",
    params(
        (
            "failing_for_hours" = Option<u32>, Query,
            description = "List webhook endpoints whose oldest undelivered event is older than \
                           this many hours, 1 to 720; default 24"
        )
    ),
    responses(
        (status = 200, description = "OK", body = DailyReportResponse),
        (status = 400, description = "Bad Request", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Platform health: per-route deposit, refund, and exposure metrics, reconciliation, and the
/// webhook endpoints failing for longer than `failing_for_hours`.
pub(crate) async fn daily_report(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<DailyReportResponse>> {
    let mut failing_for_hours = DEFAULT_FAILING_FOR_HOURS;
    for (name, value) in super::extract::query_pairs(query.as_deref()) {
        if name != "failing_for_hours" {
            return Err(
                ApiError::unknown_param(format!("unknown parameter {name}")).with_param(name)
            );
        }
        failing_for_hours = value
            .parse::<u32>()
            .ok()
            .filter(|hours| (1..=720).contains(hours))
            .ok_or_else(|| {
                ApiError::invalid_param("failing_for_hours", "failing_for_hours must be 1 to 720")
            })?;
    }
    let mut report = repository::daily_report(
        &state.pool,
        state.routes.routes(),
        chrono::Utc::now(),
        failing_for_hours,
    )
    .await?;
    report.reconciliation = crate::observability::reconciliation().map(Into::into);
    Ok(Json(report))
}

/// Finds or creates the scope's customer, so creating a quote is one call.
pub(super) async fn ensure_customer(
    state: &AppState,
    scope: Scope,
    client_reference_id: &str,
) -> ApiResult<Customer> {
    validate_client_reference_id(client_reference_id)?;
    repository::ensure_customer(&state.pool, scope, client_reference_id).await
}

async fn mutate_account_scopes(
    state: &AppState,
    actor: &Actor,
    account: &str,
    request: AccountPauseRequest,
    pause: bool,
) -> ApiResult<Json<PauseResponse>> {
    let scopes = validate_scopes(request.scopes)?;
    if request.reason.trim().is_empty() {
        return Err(ApiError::invalid_param("reason", "reason is required"));
    }
    let account_id = parse_account_id(account)?;
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let mut transaction = state.pool.begin().await?;
    let updated = crate::pause::mutate_account_scopes_in(
        &mut transaction,
        &state.routes,
        account_id,
        crate::pause::PauseOwner::Operator,
        &scopes,
        pause,
        actor,
        &request.reason,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    transaction.commit().await?;
    Ok(Json(PauseResponse {
        paused_scopes: updated,
    }))
}

async fn mutate_customer_scopes(
    state: &AppState,
    actor: &Actor,
    account: &str,
    client_reference_id: &str,
    request: CustomerPauseRequest,
    pause: bool,
) -> ApiResult<Json<PauseResponse>> {
    let scopes = validate_scopes(request.scopes)?;
    let account_id = parse_account_id(account)?;
    let customer = repository::find_customer(
        &state.pool,
        Scope::new(account_id, request.livemode),
        client_reference_id,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    let updated =
        repository::mutate_customer_scopes(&state.pool, &customer, &scopes, pause, actor).await?;
    Ok(Json(PauseResponse {
        paused_scopes: updated,
    }))
}

async fn mutate_route_scopes(
    state: &AppState,
    actor: &Actor,
    route: &str,
    request: PauseRequest,
    pause: bool,
) -> ApiResult<Json<RoutePauseResponse>> {
    if !state
        .routes
        .routes()
        .iter()
        .any(|candidate| candidate.route == route)
    {
        return Err(ApiError::not_found());
    }
    let scopes = validate_scopes(request.scopes)?;
    let updated =
        repository::mutate_route_scopes(&state.pool, route, &scopes, pause, actor).await?;
    Ok(Json(RoutePauseResponse {
        route: route.to_owned(),
        paused_scopes: updated,
    }))
}

/// The customer identifier is stored as `customers.client_reference_id`: 1 to 200 characters
/// (design D6).
pub(super) fn validate_client_reference_id(client_reference_id: &str) -> ApiResult<()> {
    if client_reference_id.is_empty() || client_reference_id.chars().count() > 200 {
        return Err(ApiError::invalid_param(
            "client_reference_id",
            "client_reference_id must contain 1 to 200 characters",
        ));
    }
    Ok(())
}

pub(super) fn validate_reason(reason: &str) -> ApiResult<()> {
    if reason.trim().is_empty() || reason.len() > 1024 {
        return Err(ApiError::bad_request("reason must contain 1 to 1024 bytes"));
    }
    Ok(())
}

fn validate_scopes(scopes: Vec<String>) -> ApiResult<Vec<String>> {
    if scopes.is_empty() {
        return Err(ApiError::bad_request("at least one scope is required"));
    }
    let mut validated = scopes
        .into_iter()
        .map(|scope| {
            PauseScope::from_str(&scope)
                .map(|parsed| parsed.code().to_owned())
                .map_err(|_| ApiError::bad_request(format!("unknown pause scope `{scope}`")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    validated.sort();
    validated.dedup();
    Ok(validated)
}

async fn account_response(state: &AppState, issued: IssuedAccount) -> ApiResult<AccountResponse> {
    let account = issued.account;
    let mut connection = state.pool.acquire().await?;
    let payment_settings = AccountPaymentSettings {
        test: super::payment_settings::payment_settings_object(
            &mut connection,
            &state.routes,
            Scope::new(account.id, false),
        )
        .await?,
        live: super::payment_settings::payment_settings_object(
            &mut connection,
            &state.routes,
            Scope::new(account.id, true),
        )
        .await?,
    };
    Ok(AccountResponse {
        id: account.public_id,
        object: "account".to_owned(),
        name: account.name,
        contact: from_json(account.contact)?,
        due_diligence: from_json(account.due_diligence)?,
        charges_enabled: account.charges_enabled,
        restricted: account.restricted,
        paused_scopes: account.paused_scopes,
        max_unfinalized_credit: u64::try_from(account.max_unfinalized_credit)
            .map_err(|_| ApiError::internal())?,
        limits: AccountLimits {
            test: issued.limits[0],
            live: issued.limits[1],
        },
        payment_settings,
        created: account.created_at.timestamp(),
        api_keys: issued
            .api_keys
            .iter()
            .map(|key| super::keys::api_key_object(&key.key, Some(key.secret.as_str().to_owned())))
            .collect(),
    })
}

fn to_json<T: serde::Serialize>(value: &T) -> ApiResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|error| {
        tracing::error!(%error, "admin record serialization failed");
        ApiError::internal()
    })
}

fn from_json<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> ApiResult<T> {
    serde_json::from_value(value).map_err(|error| {
        tracing::error!(%error, "stored admin record is malformed");
        ApiError::internal()
    })
}

/// A label of 1 to 200 characters.
fn validate_label(param: &str, value: &str) -> ApiResult<()> {
    if value.trim().is_empty() || value.chars().count() > 200 {
        return Err(ApiError::invalid_param(
            param,
            format!("{param} must contain 1 to 200 characters"),
        ));
    }
    Ok(())
}

/// A contact's name and a plausible email address; the operator verifies it offline.
fn validate_contact(contact: &Contact) -> ApiResult<()> {
    validate_label("contact.name", &contact.name)?;
    let email = contact.email.as_str();
    let plausible = email.len() <= 320
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
        && !email.chars().any(char::is_whitespace);
    if !plausible {
        return Err(ApiError::invalid_param(
            "contact.email",
            "contact.email must be an email address",
        ));
    }
    Ok(())
}

pub(super) fn parse_account_id(id: &str) -> ApiResult<Uuid> {
    crate::ids::parse(crate::ids::ACCOUNT, id).ok_or_else(ApiError::not_found)
}
