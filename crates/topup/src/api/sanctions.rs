//! Signed, audited operator supplement to verified OFAC snapshots.
use super::{
    AppState,
    auth::AdminActor,
    error::{ApiError, ErrorResponse},
    extract::ApiJson,
};
use crate::audit::{self, Entry};
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManualEntryRequest {
    /// EVM address with 0x prefix, normalized to 20 bytes without EIP-55 validation.
    address: String,
    /// Operator's audited reason, 1-1000 bytes.
    reason: String,
    /// Source document or entity reference, 1-1000 bytes.
    source_ref: String,
}
#[derive(Serialize, ToSchema)]
pub(crate) struct ManualEntryResponse {
    address: String,
    active: bool,
}

#[utoipa::path(post, path="/v1/admin/sanctions/manual/add", tag="admin", request_body=ManualEntryRequest,
 responses((status=200,description="Manual sanctions entry active",body=ManualEntryResponse),(status=400,description="Invalid input",body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn add(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<ManualEntryRequest>,
) -> Result<Json<ManualEntryResponse>, ApiError> {
    change(state, actor, request, true).await
}
#[utoipa::path(post, path="/v1/admin/sanctions/manual/remove", tag="admin", request_body=ManualEntryRequest,
 responses((status=200,description="Manual sanctions entry removed",body=ManualEntryResponse),(status=404,description="No active entry",body=ErrorResponse),(status=400,description="Invalid input",body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn remove(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<ManualEntryRequest>,
) -> Result<Json<ManualEntryResponse>, ApiError> {
    change(state, actor, request, false).await
}
async fn change(
    state: AppState,
    actor: crate::audit::Actor,
    request: ManualEntryRequest,
    add: bool,
) -> Result<Json<ManualEntryResponse>, ApiError> {
    let address = crate::sanctions::evm_address(&request.address).ok_or_else(|| {
        ApiError::bad_request("invalid EVM address; 0x prefix required").with_param("address")
    })?;
    for (name, value) in [
        ("reason", &request.reason),
        ("source_ref", &request.source_ref),
    ] {
        if value.trim().is_empty() || value.len() > 1000 {
            return Err(
                ApiError::bad_request("a value of 1 to 1000 bytes is required").with_param(name),
            );
        }
    }
    let mut tx = state.pool.begin().await?;
    if add {
        sqlx::query("INSERT INTO sanctions_manual_entries(evm_address,reason,source_ref,created_by) VALUES($1,$2,$3,$4) ON CONFLICT(evm_address) DO UPDATE SET reason=$2,source_ref=$3,created_by=$4,created_at=clock_timestamp(),removed_by=NULL,removed_at=NULL")
            .bind(address.as_slice()).bind(&request.reason).bind(&request.source_ref).bind(actor.to_string()).execute(&mut *tx).await?;
    } else {
        let removed = sqlx::query("UPDATE sanctions_manual_entries SET removed_by=$2,removed_at=clock_timestamp() WHERE evm_address=$1 AND removed_at IS NULL")
            .bind(address.as_slice()).bind(actor.to_string()).execute(&mut *tx).await?;
        if removed.rows_affected() == 0 {
            return Err(ApiError::not_found());
        }
    }
    let subject = format!("sanctions:{address:#x}");
    let reason =
        serde_json::json!({"reason":request.reason,"source_ref":request.source_ref}).to_string();
    audit::insert(
        &mut *tx,
        &Entry {
            account_id: None,
            actor: &actor,
            action: if add {
                "sanctions.manual_added"
            } else {
                "sanctions.manual_removed"
            },
            subject: &subject,
            reason: &reason,
        },
    )
    .await?;
    tx.commit().await?;
    // Notify retains a permit if the worker is busy; failures retry on the hourly tick.
    state.sanctions_rescreen.notify_one();
    Ok(Json(ManualEntryResponse {
        address: format!("{address:#x}"),
        active: add,
    }))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ManualEntryList {
    /// Active supplements in normalized address order.
    entries: Vec<crate::sanctions::ManualEntry>,
}

#[utoipa::path(get, path="/v1/admin/sanctions/manual", tag="admin",
 responses((status=200,description="Active manual sanctions entries",body=ManualEntryList)), security(("http_message_signature"=[])))]
pub(crate) async fn list(
    State(state): State<AppState>,
    AdminActor(_actor): AdminActor,
) -> Result<Json<ManualEntryList>, ApiError> {
    Ok(Json(ManualEntryList {
        entries: crate::sanctions::manual_entries(&state.pool).await?,
    }))
}
