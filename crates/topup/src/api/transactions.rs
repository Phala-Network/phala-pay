//! Object-scoped transaction submissions. Responses never report validation or chain results.

use alloy_primitives::{Address, B256};
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Extension, MatchedPath, RawQuery, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    AppState,
    auth::Merchant,
    error::ApiError,
    extract::{ApiJson, ApiPath, query_pairs},
};
use crate::{db, hints::Hint, ids, tenancy::Scope};

/// A quote's chain is derived from its stored terms.
#[derive(Deserialize, ToSchema)]
pub struct SubmitQuoteTransactionRequest {
    /// Transaction hash, exactly 32 hexadecimal bytes prefixed by `0x`.
    #[schema(pattern = "^0x[0-9a-fA-F]{64}$")]
    pub transaction_hash: String,
}
/// The deposit address must already have a network for this chain.
#[derive(Deserialize, ToSchema)]
pub struct SubmitDepositAddressTransactionRequest {
    /// Transaction hash, exactly 32 hexadecimal bytes prefixed by `0x`.
    #[schema(pattern = "^0x[0-9a-fA-F]{64}$")]
    pub transaction_hash: String,
    /// One of this object's issued network identifiers.
    pub chain_id: u64,
}
/// Constant submission object discriminator.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransactionSubmissionObject {
    /// Transaction-submission acknowledgement.
    TransactionSubmission,
}
/// Received acknowledges submission only, never detection or verification.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransactionSubmissionStatus {
    /// Received without a processing result.
    Received,
}
/// A quiet acknowledgement, including for ignored hints.
#[derive(Serialize, ToSchema)]
pub struct TransactionSubmission {
    /// Object discriminator.
    pub object: TransactionSubmissionObject,
    /// Hash echoed from the submission.
    pub transaction_hash: String,
    /// Acknowledgement only; poll the original object for payment status.
    pub status: TransactionSubmissionStatus,
}

fn acknowledgement(hash: String) -> Response {
    let mut response = (
        StatusCode::ACCEPTED,
        Json(TransactionSubmission {
            object: TransactionSubmissionObject::TransactionSubmission,
            transaction_hash: hash,
            status: TransactionSubmissionStatus::Received,
        }),
    )
        .into_response();
    super::allow_cross_origin(&mut response);
    response
}

/// Normalize every submission outcome, including auth, input, rate and infrastructure failures.
pub(super) async fn received(mut request: Request, next: Next) -> Response {
    let hint_route = request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| {
            matches!(
                path.as_str(),
                "/v1/quotes/{id}/transactions" | "/v1/deposit_addresses/{id}/transactions"
            )
        });
    if !hint_route {
        return next.run(request).await;
    }
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        super::allow_cross_origin(&mut response);
        response.headers_mut().insert(
            "access-control-allow-methods",
            axum::http::HeaderValue::from_static("POST, OPTIONS"),
        );
        response.headers_mut().insert(
            "access-control-allow-headers",
            axum::http::HeaderValue::from_static("Content-Type"),
        );
        response.headers_mut().insert(
            "access-control-max-age",
            axum::http::HeaderValue::from_static("600"),
        );
        return response;
    }
    if request.method() != Method::POST {
        return next.run(request).await;
    }
    let body = std::mem::replace(request.body_mut(), Body::empty());
    let bytes =
        match tokio::time::timeout(super::client_limit::READ_TIMEOUT, to_bytes(body, 4096)).await {
            Ok(Ok(bytes)) => bytes,
            _ => return acknowledgement(String::new()),
        };
    let hash = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("transaction_hash")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    *request.body_mut() = Body::from(bytes);
    let _ = next.run(request).await;
    acknowledgement(hash)
}

#[utoipa::path(post, path = "/v1/quotes/{id}/transactions", tag = "quotes", operation_id = "submit_quote_transaction",
    params(("id" = String, Path, description = "Quote identifier"), ("client_secret" = Option<String>, Query, description = "Object's browser client secret; omit with a merchant key")),
    request_body = SubmitQuoteTransactionRequest,
    responses((status = 202, description = "Received, including ignored hints. No payment result is disclosed.", body = TransactionSubmission)),
    security(("api_key" = []), ()))]
/// Submit a quote transaction hint. The stored quote selects the chain and recipient.
pub(crate) async fn submit_quote_transaction(
    State(state): State<AppState>,
    merchant: Option<Extension<Merchant>>,
    ApiPath(id): ApiPath<String>,
    RawQuery(query): RawQuery,
    ApiJson(body): ApiJson<SubmitQuoteTransactionRequest>,
) -> Result<StatusCode, ApiError> {
    submit(
        &state,
        merchant.map(|value| value.0),
        &id,
        query.as_deref(),
        &body.transaction_hash,
        None,
        true,
    )
    .await?;
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(post, path = "/v1/deposit_addresses/{id}/transactions", tag = "deposit_addresses", operation_id = "submit_deposit_address_transaction",
    params(("id" = String, Path, description = "Deposit-address identifier"), ("client_secret" = Option<String>, Query, description = "Object's browser client secret; omit with a merchant key")),
    request_body = SubmitDepositAddressTransactionRequest,
    responses((status = 202, description = "Received, including ignored hints. No payment result is disclosed.", body = TransactionSubmission)),
    security(("api_key" = []), ()))]
/// Submit a deposit-address transaction hint on one of its issued networks.
pub(crate) async fn submit_deposit_address_transaction(
    State(state): State<AppState>,
    merchant: Option<Extension<Merchant>>,
    ApiPath(id): ApiPath<String>,
    RawQuery(query): RawQuery,
    ApiJson(body): ApiJson<SubmitDepositAddressTransactionRequest>,
) -> Result<StatusCode, ApiError> {
    submit(
        &state,
        merchant.map(|value| value.0),
        &id,
        query.as_deref(),
        &body.transaction_hash,
        Some(body.chain_id),
        false,
    )
    .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn submit(
    state: &AppState,
    merchant: Option<Merchant>,
    id: &str,
    query: Option<&str>,
    hash: &str,
    chain: Option<u64>,
    quote: bool,
) -> Result<(), ApiError> {
    if hash.len() != 66 || !hash.starts_with("0x") {
        return Ok(());
    }
    let Ok(tx) = hash.parse::<B256>() else {
        return Ok(());
    };
    let Some(object) = ids::parse(
        if quote {
            ids::QUOTE
        } else {
            ids::DEPOSIT_ADDRESS
        },
        id,
    ) else {
        return Ok(());
    };
    let scope = if let Some(merchant) = merchant {
        merchant.scope
    } else {
        let pairs = query_pairs(query);
        let Some((_, secret)) = pairs.iter().find(|(name, _)| name == "client_secret") else {
            return Ok(());
        };
        if !state.client_reads.key().verify(id, secret) {
            return Ok(());
        }
        if crate::restore_mode::is_frozen(&state.pool).await? {
            return Ok(());
        }
        let digest = Sha256::digest(secret.as_bytes());
        let owner: Option<(Uuid, bool)> = if quote {
            sqlx::query_as(
                "SELECT account_id,livemode FROM quotes WHERE client_secret_hash=$1 AND id=$2",
            )
            .bind(digest.as_slice())
            .bind(object)
            .fetch_optional(&state.pool)
            .await?
        } else {
            sqlx::query_as("SELECT d.account_id,d.livemode FROM deposit_address_client_secrets s JOIN deposit_addresses d ON d.id=s.deposit_address_id WHERE s.secret_hash=$1 AND d.id=$2")
                .bind(digest.as_slice()).bind(object).fetch_optional(&state.pool).await?
        };
        let Some((account, livemode)) = owner else {
            return Ok(());
        };
        Scope::new(account, livemode)
    };
    // Only issued, current physical networks of this object are eligible; never create addresses.
    let address: Option<(Uuid, i64, String, i64)> = sqlx::query_as(
        "SELECT id,chain_id,address,created_block FROM addresses WHERE account_id=$1 AND livemode=$2 \
         AND (($3 AND quote_id=$4) OR (NOT $3 AND deposit_address_id=$4 AND chain_id=$5)) \
         AND superseded_at IS NULL LIMIT 1")
        .bind(scope.account_id()).bind(scope.livemode()).bind(quote).bind(object)
        .bind(chain.and_then(|chain| i64::try_from(chain).ok())).fetch_optional(&state.pool).await?;
    let Some((id, chain, recipient, created)) = address else {
        return Ok(());
    };
    if !state.hint_limits.allow_object(object) {
        return Ok(());
    }
    let chain = u64::try_from(chain).map_err(|_| ApiError::internal())?;
    let recipient = recipient
        .parse::<Address>()
        .map_err(|_| ApiError::internal())?;
    let created_block = u64::try_from(created).map_err(|_| ApiError::internal())?;
    state.transaction_hints.enqueue(Hint {
        chain,
        tx,
        object,
        address: db::ScanAddress {
            id,
            address: recipient,
            created_block,
            backfilled: false,
            backfilled_through: None,
        },
    });
    Ok(())
}
