//! Deposits (`/v1/deposits`) and refunds (`/v1/refunds`).

use std::str::FromStr;

use alloy_primitives::{Address as EvmAddress, B256, U256};
use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use sqlx::types::Json as JsonColumn;
use sqlx::{FromRow, PgExecutor, Postgres, QueryBuilder};
use topup_core::money::AtomicAmount;
use uuid::Uuid;

use crate::ids;
use crate::refunds::DestinationScreening;
use crate::routes::RouteSet;
use crate::tenancy::Scope;

use super::AppState;
use super::auth::Merchant;
use super::error::{ApiError, ErrorResponse};
use super::extract::{ApiJson, ApiPath, expansions, query_pairs};
use super::idempotency::Idempotent;
use super::metadata::{self, Metadata, Object};
use super::models::{
    CreateRefundRequest, Deposit, DepositList, ExpandableDeposit, ExpandableQuote,
    MarkRefundPaidRequest, Refund, RefundList, UpdateMetadataRequest,
};
use super::repository::{self, NewRefund};

/// The joined columns used by both refund reads and list pages.
macro_rules! select_refund {
    () => {
        r#"
        SELECT refund.id, refund.livemode, refund.deposit_id, refund.amount_atomic::text AS amount_atomic,
               refund.destination_address, address.treasury, refund.status,
               refund.failure_reason, refund.tx_hash, refund.receipt_log_index,
               refund.created_at,
               refund.metadata
        FROM refunds AS refund
        JOIN deposits AS deposit ON deposit.id = refund.deposit_id
        JOIN addresses AS address ON address.id = deposit.address_id
        "#
    };
}

type ApiResult<T> = Result<T, ApiError>;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;
/// A deposit's merchant-visible `status` and the processing states it covers.
const DEPOSIT_STATUSES: [(&str, &[&str]); 4] = [
    ("pending", &["detected", "confirmed"]),
    ("credited", &["credited", "swept"]),
    ("rejected", &["rejected"]),
    ("reversed", &["reversed"]),
];

/// The merchant-visible `status` of a processing state.
pub(crate) fn deposit_status(state: &str) -> &'static str {
    DEPOSIT_STATUSES
        .iter()
        .find(|(_, states)| states.contains(&state))
        .map_or("pending", |(status, _)| status)
}

#[utoipa::path(
    get,
    path = "/v1/deposits",
    params(
        ("client_reference_id" = Option<String>, Query, description = "Only this customer's deposits"),
        ("quote" = Option<String>, Query, description = "Only deposits to this quote's address"),
        ("deposit_address" = Option<String>, Query, description = "Only deposits to this deposit address, `da_…`"),
        ("status" = Option<String>, Query, description = "Only deposits in this status: `pending`, `credited`, `rejected`, or `reversed`"),
        ("tx_hash" = Option<String>, Query, description = "Only deposits in this transaction"),
        ("created[gt]" = Option<i64>, Query, description = "Created after, Unix seconds"),
        ("created[gte]" = Option<i64>, Query, description = "Created at or after, Unix seconds"),
        ("created[lt]" = Option<i64>, Query, description = "Created before, Unix seconds"),
        ("created[lte]" = Option<i64>, Query, description = "Created at or before, Unix seconds"),
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`dep_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`dep_` id: the page before it"),
        ("expand[]" = Option<Vec<String>>, Query, description = "`data.quote`")
    ),
    responses(
        (status = 200, description = "OK", body = DepositList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "deposits"
)]
/// The account's deposits in the credential's mode, newest first, with Stripe's cursor
/// pagination.
pub(crate) async fn list_deposits(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<DepositList>> {
    let pairs = query_pairs(query.as_deref());
    let expand = expansions(&pairs, &["data.quote"])?;
    let filters = ListFilters::parse(&pairs)?;
    let mut builder = scoped_deposit_query(merchant.scope);
    if let Some(client_reference_id) = &filters.client_reference_id {
        builder
            .push(" AND customer.client_reference_id = ")
            .push_bind(client_reference_id.clone());
    }
    if let Some(quote) = filters.quote {
        builder.push(" AND address.quote_id = ").push_bind(quote);
    }
    if let Some(deposit_address) = filters.deposit_address {
        builder
            .push(" AND address.deposit_address_id = ")
            .push_bind(deposit_address);
    }
    if let Some(states) = filters.states {
        builder.push(" AND deposit.state = ANY(").push_bind(
            states
                .iter()
                .map(|state| (*state).to_owned())
                .collect::<Vec<_>>(),
        );
        builder.push(")");
    }
    if let Some(tx_hash) = &filters.tx_hash {
        builder
            .push(" AND deposit.tx_hash = ")
            .push_bind(tx_hash.clone());
    }
    for (operator, bound) in &filters.created {
        builder
            .push(format!(" AND deposit.created_at {operator} "))
            .push_bind(*bound);
    }
    let (cursor, before) = match (filters.starting_after, filters.ending_before) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "starting_after and ending_before are mutually exclusive",
            ));
        }
        (Some(id), None) => (Some((id, "starting_after")), false),
        (None, Some(id)) => (Some((id, "ending_before")), true),
        (None, None) => (None, false),
    };
    if let Some((id, param)) = cursor {
        let found: Option<(DateTime<Utc>, Uuid)> = sqlx::query_as(
            r#"
            SELECT created_at, id
            FROM deposits
            WHERE id = $1 AND account_id = $2 AND livemode = $3
            "#,
        )
        .bind(id)
        .bind(merchant.scope.account_id())
        .bind(merchant.scope.livemode())
        .fetch_optional(&state.pool)
        .await?;
        let (created_at, id) =
            found.ok_or_else(|| ApiError::invalid_param(param, "no such deposit"))?;
        builder
            .push(if before {
                " AND (deposit.created_at, deposit.id) > ("
            } else {
                " AND (deposit.created_at, deposit.id) < ("
            })
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder.push(if before {
        " ORDER BY deposit.created_at ASC, deposit.id ASC LIMIT "
    } else {
        " ORDER BY deposit.created_at DESC, deposit.id DESC LIMIT "
    });
    builder.push_bind(filters.limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<DepositRow>()
        .fetch_all(&state.pool)
        .await?;
    let has_more = i64::try_from(rows.len()).map_err(|_| ApiError::internal())? > filters.limit;
    rows.truncate(usize::try_from(filters.limit).map_err(|_| ApiError::internal())?);
    if before {
        rows.reverse();
    }
    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        let mut deposit = deposit_object(&state.routes, row)?;
        if expand.contains(&"data.quote") {
            deposit.quote = expanded_quote(&state, merchant.scope, deposit.quote).await?;
        }
        data.push(deposit);
    }
    Ok(Json(DepositList {
        object: "list".to_owned(),
        url: "/v1/deposits".to_owned(),
        has_more,
        data,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/deposits/{id}",
    params(
        ("id" = String, Path, description = "Deposit id, `dep_…`"),
        ("expand[]" = Option<Vec<String>>, Query, description = "`quote`")
    ),
    responses(
        (status = 200, description = "OK", body = Deposit),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "deposits"
)]
/// One deposit.
pub(crate) async fn get_deposit(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    ApiPath(id): ApiPath<String>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Deposit>> {
    let expand = expansions(&query_pairs(query.as_deref()), &["quote"])?;
    let id = ids::parse(ids::DEPOSIT, &id).ok_or_else(ApiError::not_found)?;
    let mut deposit = find_deposit(&state.pool, &state.routes, merchant.scope, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if expand.contains(&"quote") {
        deposit.quote = expanded_quote(&state, merchant.scope, deposit.quote).await?;
    }
    Ok(Json(deposit))
}

#[utoipa::path(
    post,
    path = "/v1/deposits/{id}",
    params(
        ("id" = String, Path, description = "Deposit id, `dep_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = UpdateMetadataRequest,
    responses(
        (status = 200, description = "OK", body = Deposit),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "deposits"
)]
/// Updates a deposit's `metadata`; parameters not sent are left unchanged. The quote's metadata is
/// not changed.
pub(crate) async fn update_deposit(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<UpdateMetadataRequest>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::DEPOSIT, &id).ok_or_else(ApiError::not_found)?;
    let scope = merchant.scope;
    let mut transaction = idempotent.begin(&state.pool).await?;
    if !metadata::update(
        &mut *transaction,
        &state.routes,
        Object::Deposit,
        scope,
        id,
        request.metadata.as_ref(),
        &merchant.actor(),
    )
    .await?
    {
        return Err(ApiError::not_found());
    }
    let deposit = find_deposit(&mut *transaction, &state.routes, scope, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    idempotent.commit(transaction, Json(deposit)).await
}

#[utoipa::path(
    post,
    path = "/v1/refunds",
    params(
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = CreateRefundRequest,
    responses(
        (status = 200, description = "OK", body = Refund),
        (
            status = 400,
            description = "Bad Request, `destination_sanctioned`, `amount_too_small`, \
                           `amount_too_large`, `deposit_not_refundable`, `deposit_not_final`, or \
                           `paused`",
            body = ErrorResponse
        ),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 503, description = "Destination screening is unavailable; retry", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "refunds"
)]
/// Creates a `pending` refund of a final deposit (design D5): a rejected deposit other than a
/// sanctioned or dust one, or a credited one. The amount, the unrefunded remainder by default, is
/// reserved until the refund is canceled or fails. The destination must pass sanctions screening
/// (`400 destination_sanctioned`). The merchant then pays it from the refund's `treasury` and
/// attaches the transaction with `mark_paid`.
pub(crate) async fn create_refund(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiJson(request): ApiJson<CreateRefundRequest>,
) -> ApiResult<Response> {
    let deposit_id = ids::parse(ids::DEPOSIT, &request.deposit)
        .ok_or_else(|| ApiError::invalid_param("deposit", "deposit must be a dep_ id"))?;
    let destination = EvmAddress::from_str(&request.destination_address)
        .ok()
        .filter(|address| !address.is_zero())
        .ok_or_else(|| {
            ApiError::invalid_param(
                "destination_address",
                "destination_address must be a nonzero 20-byte hexadecimal address",
            )
        })?;
    let amount = request
        .amount_atomic
        .as_deref()
        .map(|amount| {
            decimal_u256(amount).map(AtomicAmount::new).ok_or_else(|| {
                ApiError::invalid_param(
                    "amount_atomic",
                    "amount_atomic must be a decimal integer string",
                )
            })
        })
        .transpose()?;
    let metadata = metadata::on_create(request.metadata.as_ref())?;
    let route = state.refund_route(merchant.scope, deposit_id).await?;
    let actor = merchant.actor();
    match state.screening.screen(route, destination).await {
        DestinationScreening::Clear => {}
        DestinationScreening::Sanctioned => return Err(ApiError::destination_sanctioned()),
        DestinationScreening::Unavailable => {
            return Err(ApiError::service_unavailable(
                "sanctions screening of the destination is unavailable; retry",
            ));
        }
    }
    // Screened first; the transaction checks the deposit and the amount again.
    let mut transaction = idempotent.begin(&state.pool).await?;
    let refund_id = repository::request_refund(
        &mut *transaction,
        &state.routes,
        &NewRefund {
            scope: merchant.scope,
            deposit_id,
            route,
            destination,
            amount,
            metadata: &metadata,
            actor: &actor,
        },
    )
    .await?;
    let refund = find_refund(&mut *transaction, merchant.scope, refund_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    idempotent.commit(transaction, Json(refund)).await
}

const REFUND_STATUSES: [&str; 4] = ["pending", "succeeded", "failed", "canceled"];

#[utoipa::path(
    get,
    path = "/v1/refunds",
    params(
        ("deposit" = Option<String>, Query, description = "Only this deposit's refunds, `dep_…`"),
        ("status" = Option<String>, Query, description = "`pending`, `succeeded`, `failed`, or `canceled`"),
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`re_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`re_` id: the page before it")
    ),
    responses(
        (status = 200, description = "OK", body = RefundList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "refunds"
)]
/// The account's refunds in the key's mode, newest first, with Stripe's cursor pagination.
pub(crate) async fn list_refunds(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<RefundList>> {
    let scope = merchant.scope;
    let mut builder =
        QueryBuilder::<Postgres>::new(concat!(select_refund!(), " WHERE refund.account_id = "));
    builder
        .push_bind(scope.account_id())
        .push(" AND refund.livemode = ")
        .push_bind(scope.livemode());
    let mut limit = DEFAULT_LIMIT;
    let mut starting_after = None;
    let mut ending_before = None;
    for (name, value) in query_pairs(query.as_deref()) {
        match name.as_str() {
            "deposit" => {
                let deposit = ids::parse(ids::DEPOSIT, &value)
                    .ok_or_else(|| ApiError::invalid_param("deposit", "not a dep_ id"))?;
                builder.push(" AND refund.deposit_id = ").push_bind(deposit);
            }
            "status" => {
                if !REFUND_STATUSES.contains(&value.as_str()) {
                    return Err(ApiError::invalid_param("status", "unknown status"));
                }
                builder.push(" AND refund.status = ").push_bind(value);
            }
            "limit" => {
                limit = value
                    .parse::<i64>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| ApiError::invalid_param("limit", "limit must be 1 to 100"))?;
            }
            "starting_after" | "ending_before" => {
                let id = ids::parse(ids::REFUND, &value)
                    .ok_or_else(|| ApiError::invalid_param(name.clone(), "not a re_ id"))?;
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
    let (cursor, before) = match (starting_after, ending_before) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "starting_after and ending_before are mutually exclusive",
            ));
        }
        (Some(id), None) => (Some((id, "starting_after")), false),
        (None, Some(id)) => (Some((id, "ending_before")), true),
        (None, None) => (None, false),
    };
    if let Some((id, param)) = cursor {
        let created_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT created_at FROM refunds WHERE id = $1 AND account_id = $2 AND livemode = $3",
        )
        .bind(id)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::invalid_param(param, "no such refund"))?;
        builder
            .push(if before {
                " AND (refund.created_at, refund.id) > ("
            } else {
                " AND (refund.created_at, refund.id) < ("
            })
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder
        .push(if before {
            " ORDER BY refund.created_at ASC, refund.id ASC LIMIT "
        } else {
            " ORDER BY refund.created_at DESC, refund.id DESC LIMIT "
        })
        .push_bind(limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<RefundRow>()
        .fetch_all(&state.pool)
        .await?;
    let has_more = i64::try_from(rows.len()).map_err(|_| ApiError::internal())? > limit;
    rows.truncate(usize::try_from(limit).map_err(|_| ApiError::internal())?);
    if before {
        rows.reverse();
    }
    let data = rows
        .into_iter()
        .map(refund_object)
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(RefundList {
        object: "list".to_owned(),
        url: "/v1/refunds".to_owned(),
        has_more,
        data,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/refunds/{id}",
    params(
        ("id" = String, Path, description = "Refund id, `re_…`"),
        ("expand[]" = Option<Vec<String>>, Query, description = "`deposit`")
    ),
    responses(
        (status = 200, description = "OK", body = Refund),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "refunds"
)]
/// One refund.
pub(crate) async fn get_refund(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    ApiPath(id): ApiPath<String>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Refund>> {
    let expand = expansions(&query_pairs(query.as_deref()), &["deposit"])?;
    let id = ids::parse(ids::REFUND, &id).ok_or_else(ApiError::not_found)?;
    let mut refund = find_refund(&state.pool, merchant.scope, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if expand.contains(&"deposit")
        && let ExpandableDeposit::Id(deposit) = &refund.deposit
    {
        let deposit_id = ids::parse(ids::DEPOSIT, deposit).ok_or_else(ApiError::internal)?;
        let deposit = find_deposit(&state.pool, &state.routes, merchant.scope, deposit_id)
            .await?
            .ok_or_else(ApiError::internal)?;
        refund.deposit = ExpandableDeposit::Object(Box::new(deposit));
    }
    Ok(Json(refund))
}

#[utoipa::path(
    post,
    path = "/v1/refunds/{id}",
    params(
        ("id" = String, Path, description = "Refund id, `re_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = UpdateMetadataRequest,
    responses(
        (status = 200, description = "OK", body = Refund),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "refunds"
)]
/// Updates a refund's `metadata`, in any status; parameters not sent are left unchanged.
pub(crate) async fn update_refund(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<UpdateMetadataRequest>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::REFUND, &id).ok_or_else(ApiError::not_found)?;
    let scope = merchant.scope;
    let mut transaction = idempotent.begin(&state.pool).await?;
    if !metadata::update(
        &mut *transaction,
        &state.routes,
        Object::Refund,
        scope,
        id,
        request.metadata.as_ref(),
        &merchant.actor(),
    )
    .await?
    {
        return Err(ApiError::not_found());
    }
    let refund = find_refund(&mut *transaction, scope, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    idempotent.commit(transaction, Json(refund)).await
}

#[utoipa::path(
    post,
    path = "/v1/refunds/{id}/mark_paid",
    params(
        ("id" = String, Path, description = "Refund id, `re_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = MarkRefundPaidRequest,
    responses(
        (status = 200, description = "OK", body = Refund),
        (
            status = 400,
            description = "Bad Request, `refund_unexpected_state` (not pending, or marked paid \
                           with another transaction), or `transfer_already_used`",
            body = ErrorResponse
        ),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse),
        (status = 422, description = "Non-retryable `refund_attachment_limit_exceeded`; contact the operator. The reservation is preserved.", body = ErrorResponse),
    ),
    security(("api_key" = [])),
    tag = "refunds"
)]
/// Attaches the transaction that pays a pending refund, as BTCPay's payout `mark-paid`. At
/// `finalized`, both providers must show a `Transfer` of the deposit's token from the refund's
/// `treasury` to `destination_address` for exactly `amount_atomic`, in a log no other refund uses
/// (`receipt_log_index`, or any such log when absent). Then the refund is `succeeded` and
/// `deposit.refunded` is sent; otherwise it is `failed` with a `failure_reason`. Repeating the same
/// transaction returns the refund. From here on the refund cannot be canceled: it is `failed`
/// only when its transaction is proven not to pay it.
/// Each environment permits at most two attached-pending refunds and one new attachment per
/// rolling 24 hours across all accounts and modes. Repeating the same attachment consumes no
/// quota. A limit refusal preserves the reservation; contact the operator before another payout.
pub(crate) async fn mark_refund_paid(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<MarkRefundPaidRequest>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::REFUND, &id).ok_or_else(ApiError::not_found)?;
    let tx_hash = B256::from_str(&request.transaction_hash)
        .ok()
        .filter(|_| request.transaction_hash.len() == 66)
        .ok_or_else(|| {
            ApiError::invalid_param(
                "transaction_hash",
                "transaction_hash must be 0x and 32 bytes of hex",
            )
        })?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    repository::mark_refund_paid(
        &mut *transaction,
        &state.routes,
        merchant.scope,
        id,
        tx_hash,
        request.receipt_log_index,
        &merchant.actor(),
    )
    .await?;
    let refund = find_refund(&mut *transaction, merchant.scope, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    idempotent.commit(transaction, Json(refund)).await
}

#[utoipa::path(
    post,
    path = "/v1/refunds/{id}/cancel",
    params(
        ("id" = String, Path, description = "Refund id, `re_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    responses(
        (status = 200, description = "OK", body = Refund),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse),
        (
            status = 400,
            description = "`refund_unexpected_state`: marked paid, succeeded, or failed",
            body = ErrorResponse
        )
    ),
    security(("api_key" = [])),
    tag = "refunds"
)]
/// Cancels a pending refund that has no transaction attached and releases its reservation of the
/// deposit; canceling a canceled refund returns it. Once `mark_paid` attached a transaction, the
/// refund cannot be canceled, so that the deposit is never paid back twice: it stays reserved
/// until dual-source finalized verification ends it, `succeeded`, or `failed` when the transaction
/// does not pay it. A transaction never seen for 24 hours raises an alert and remains pending;
/// contact the operator before taking any further refund action.
pub(crate) async fn cancel_refund(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::REFUND, &id).ok_or_else(ApiError::not_found)?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    repository::cancel_refund(
        &mut *transaction,
        &state.routes,
        merchant.scope,
        id,
        &merchant.actor(),
    )
    .await?;
    let refund = find_refund(&mut *transaction, merchant.scope, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    idempotent.commit(transaction, Json(refund)).await
}

/// The scope's deposit `id`, if it exists.
pub(crate) async fn find_deposit<'e>(
    executor: impl PgExecutor<'e>,
    routes: &RouteSet,
    scope: Scope,
    id: Uuid,
) -> ApiResult<Option<Deposit>> {
    let mut builder = scoped_deposit_query(scope);
    builder.push(" AND deposit.id = ").push_bind(id);
    builder
        .build_query_as::<DepositRow>()
        .fetch_optional(executor)
        .await?
        .map(|row| deposit_object(routes, row))
        .transpose()
}

async fn expanded_quote(
    state: &AppState,
    scope: Scope,
    quote: Option<ExpandableQuote>,
) -> ApiResult<Option<ExpandableQuote>> {
    let Some(ExpandableQuote::Id(id)) = quote else {
        return Ok(quote);
    };
    let quote_id = ids::parse(ids::QUOTE, &id).ok_or_else(ApiError::internal)?;
    let lock = crate::locks::get(&state.pool, scope, quote_id)
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or_else(ApiError::internal)?;
    let quote =
        super::quotes::quote_object(&mut *state.pool.acquire().await?, &state.routes, lock).await?;
    Ok(Some(ExpandableQuote::Object(Box::new(quote))))
}

/// The scope's refund `id`, if it exists.
pub(crate) async fn find_refund<'e>(
    executor: impl PgExecutor<'e>,
    scope: Scope,
    id: Uuid,
) -> ApiResult<Option<Refund>> {
    let row = sqlx::query_as::<_, RefundRow>(concat!(
        select_refund!(),
        " WHERE refund.id = $1 AND refund.account_id = $2 AND refund.livemode = $3"
    ))
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(executor)
    .await?;
    row.map(refund_object).transpose()
}

fn refund_object(row: RefundRow) -> ApiResult<Refund> {
    Ok(Refund {
        id: ids::format(ids::REFUND, row.id),
        object: "refund".to_owned(),
        livemode: row.livemode,
        deposit: ExpandableDeposit::Id(ids::format(ids::DEPOSIT, row.deposit_id)),
        amount_atomic: row.amount_atomic,
        destination_address: row.destination_address,
        treasury: row.treasury,
        status: row.status,
        failure_reason: row.failure_reason,
        transaction_hash: row.tx_hash,
        receipt_log_index: row
            .receipt_log_index
            .map(u64::try_from)
            .transpose()
            .map_err(|_| ApiError::internal())?,
        created: row.created_at.timestamp(),
        metadata: row.metadata.0,
    })
}

#[derive(FromRow)]
struct RefundRow {
    id: Uuid,
    livemode: bool,
    deposit_id: Uuid,
    amount_atomic: String,
    destination_address: String,
    treasury: String,
    status: String,
    failure_reason: Option<String>,
    tx_hash: Option<String>,
    receipt_log_index: Option<i64>,
    created_at: DateTime<Utc>,
    metadata: JsonColumn<Metadata>,
}

#[derive(FromRow)]
struct DepositRow {
    id: Uuid,
    livemode: bool,
    client_reference_id: String,
    quote_id: Option<Uuid>,
    deposit_address_id: Option<Uuid>,
    state: String,
    is_final: bool,
    final_at: Option<DateTime<Utc>>,
    swept: bool,
    reason: Option<String>,
    chain_id: i64,
    route: Option<String>,
    asset_contract: String,
    amount_atomic: String,
    credit_minor: Option<String>,
    price_scaled: Option<String>,
    price_source: Option<String>,
    valuation_at: Option<DateTime<Utc>>,
    address: String,
    from_address: String,
    tx_hash: String,
    receipt_log_index: i64,
    revision: i64,
    log_index: i64,
    block_number: i64,
    block_hash: String,
    block_time: DateTime<Utc>,
    amount_refunded_atomic: String,
    replaces: Option<Uuid>,
    replaced_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    metadata: JsonColumn<Metadata>,
}

/// Deposits of `scope`; callers append further `AND` conditions.
fn scoped_deposit_query(scope: Scope) -> QueryBuilder<Postgres> {
    let mut builder = QueryBuilder::new(
        r#"
        SELECT deposit.id, deposit.livemode, customer.client_reference_id,
               address.quote_id,
               address.deposit_address_id,
               deposit.state, deposit.final_at IS NOT NULL AS is_final, deposit.final_at,
               deposit.state = 'swept' OR EXISTS (
                   SELECT 1 FROM flushed
                   WHERE flushed.address_id = deposit.address_id
                     AND flushed.token = deposit.asset_contract
                     AND (flushed.block_number, flushed.log_index)
                         > (deposit.block_number, deposit.log_index)
               ) AS swept,
               deposit.reason, deposit.chain_id, deposit.route,
               deposit.asset_contract, deposit.amount_atomic::text AS amount_atomic,
               deposit.credit_minor::text AS credit_minor,
               deposit.price_scaled::text AS price_scaled, deposit.price_source,
               deposit.valuation_at, address.address, deposit.from_address, deposit.tx_hash,
               deposit.receipt_log_index, deposit.revision, deposit.log_index,
               deposit.block_number, deposit.block_hash, deposit.block_time,
               COALESCE((
                   SELECT sum(refund.amount_atomic)
                   FROM refunds AS refund
                   WHERE refund.deposit_id = deposit.id AND refund.status = 'succeeded'
               ), 0)::text AS amount_refunded_atomic,
               deposit.replaces,
               (
                   SELECT successor.id FROM deposits AS successor
                   WHERE successor.replaces = deposit.id
               ) AS replaced_by,
               deposit.created_at, deposit.metadata
        FROM deposits AS deposit
        JOIN customers AS customer ON customer.id = deposit.customer_id
        JOIN addresses AS address ON address.id = deposit.address_id
        WHERE deposit.account_id = "#,
    );
    builder
        .push_bind(scope.account_id())
        .push(" AND deposit.livemode = ")
        .push_bind(scope.livemode());
    builder
}

fn deposit_object(routes: &RouteSet, row: DepositRow) -> ApiResult<Deposit> {
    let asset = row.route.as_deref().and_then(|name| {
        routes
            .routes()
            .iter()
            .find(|route| route.route == name)
            .map(|route| route.asset.symbol.clone())
    });
    let refunded = row.amount_refunded_atomic == row.amount_atomic;
    let amount = row
        .credit_minor
        .map(|credit| credit.parse::<u64>())
        .transpose()
        .map_err(|_| ApiError::internal())?;
    let status = deposit_status(&row.state);
    let amount_refunded = match amount {
        Some(credit) => topup_core::refund::refunded_credit(
            credit,
            decimal_u256(&row.amount_atomic).ok_or_else(ApiError::internal)?,
            decimal_u256(&row.amount_refunded_atomic).ok_or_else(ApiError::internal)?,
        ),
        None => 0,
    };
    let amount_reversed = if status == "reversed" {
        amount.unwrap_or(0)
    } else {
        0
    };
    Ok(Deposit {
        id: ids::format(ids::DEPOSIT, row.id),
        object: "deposit".to_owned(),
        livemode: row.livemode,
        client_reference_id: row.client_reference_id,
        quote: row
            .quote_id
            .map(|quote| ExpandableQuote::Id(ids::format(ids::QUOTE, quote))),
        deposit_address: row
            .deposit_address_id
            .map(crate::deposit_addresses::public_id),
        status: status.to_owned(),
        is_final: row.is_final,
        final_at: row.final_at.map(|at| at.timestamp()),
        swept: row.swept,
        rejection_reason: row.reason,
        chain_id: u64::try_from(row.chain_id).map_err(|_| ApiError::internal())?,
        asset,
        asset_contract: row.asset_contract,
        amount_atomic: row.amount_atomic,
        amount,
        currency: "usd".to_owned(),
        exchange_rate: row
            .price_scaled
            .map(|price| price.parse::<u64>().map(super::quotes::decimal))
            .transpose()
            .map_err(|_| ApiError::internal())?,
        price_source: row.price_source.map(|source| {
            if source == "lock" {
                "quote".to_owned()
            } else {
                source
            }
        }),
        valued_at: row.valuation_at.map(|at| at.timestamp()),
        address: row.address,
        from_address: row.from_address,
        tx_hash: row.tx_hash,
        receipt_log_index: u64::try_from(row.receipt_log_index)
            .map_err(|_| ApiError::internal())?,
        revision: u64::try_from(row.revision).map_err(|_| ApiError::internal())?,
        log_index: u64::try_from(row.log_index).map_err(|_| ApiError::internal())?,
        block_number: u64::try_from(row.block_number).map_err(|_| ApiError::internal())?,
        block_hash: row.block_hash,
        block_time: row.block_time.timestamp(),
        amount_refunded_atomic: row.amount_refunded_atomic,
        refunded,
        amount_refunded,
        amount_reversed,
        replaces: row.replaces.map(|id| ids::format(ids::DEPOSIT, id)),
        replaced_by: row.replaced_by.map(|id| ids::format(ids::DEPOSIT, id)),
        created: row.created_at.timestamp(),
        metadata: row.metadata.0,
        admin: None,
    })
}

struct ListFilters {
    client_reference_id: Option<String>,
    quote: Option<Uuid>,
    deposit_address: Option<Uuid>,
    states: Option<&'static [&'static str]>,
    tx_hash: Option<String>,
    /// `created[gt|gte|lt|lte]` bounds, as SQL comparison operators.
    created: Vec<(&'static str, DateTime<Utc>)>,
    limit: i64,
    starting_after: Option<Uuid>,
    ending_before: Option<Uuid>,
}

impl ListFilters {
    fn parse(pairs: &[(String, String)]) -> ApiResult<Self> {
        let mut filters = Self {
            client_reference_id: None,
            quote: None,
            deposit_address: None,
            states: None,
            tx_hash: None,
            created: Vec::new(),
            limit: DEFAULT_LIMIT,
            starting_after: None,
            ending_before: None,
        };
        for (name, value) in pairs {
            match name.as_str() {
                "client_reference_id" => filters.client_reference_id = Some(value.clone()),
                "quote" => {
                    filters.quote = Some(
                        ids::parse(ids::QUOTE, value)
                            .ok_or_else(|| ApiError::invalid_param("quote", "not a qt_ id"))?,
                    );
                }
                "deposit_address" => {
                    filters.deposit_address =
                        Some(ids::parse(ids::DEPOSIT_ADDRESS, value).ok_or_else(|| {
                            ApiError::invalid_param("deposit_address", "not a da_ id")
                        })?);
                }
                "status" => {
                    let (_, states) = DEPOSIT_STATUSES
                        .iter()
                        .find(|(status, _)| status == value)
                        .ok_or_else(|| ApiError::invalid_param("status", "unknown status"))?;
                    filters.states = Some(states);
                }
                "tx_hash" => {
                    let hash = B256::from_str(value).map_err(|_| {
                        ApiError::invalid_param("tx_hash", "tx_hash must be 32 bytes of hex")
                    })?;
                    filters.tx_hash = Some(format!("{hash:#x}"));
                }
                "created[gt]" | "created[gte]" | "created[lt]" | "created[lte]" => {
                    filters
                        .created
                        .extend(super::pagination::created_bound(name, value)?);
                }
                "limit" => {
                    filters.limit = value
                        .parse::<i64>()
                        .ok()
                        .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                        .ok_or_else(|| {
                            ApiError::invalid_param("limit", "limit must be 1 to 100")
                        })?;
                }
                "starting_after" | "ending_before" => {
                    let id = ids::parse(ids::DEPOSIT, value)
                        .ok_or_else(|| ApiError::invalid_param(name.clone(), "not a dep_ id"))?;
                    if name == "starting_after" {
                        filters.starting_after = Some(id);
                    } else {
                        filters.ending_before = Some(id);
                    }
                }
                "expand[]" | "expand" => {}
                other => {
                    return Err(
                        ApiError::unknown_param(format!("unknown parameter {other}"))
                            .with_param(other),
                    );
                }
            }
        }
        Ok(filters)
    }
}

fn decimal_u256(value: &str) -> Option<U256> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    U256::from_str(value).ok()
}
