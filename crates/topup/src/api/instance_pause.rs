//! Planned upgrades pause request admission, without changing business pause scopes.
use axum::Json;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::auth::AdminActor;
use super::error::{ApiError, ErrorResponse};
use super::extract::ApiJson;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstancePauseRequest {
    /// Deployment identifier, reused when resuming. An older deployment cannot clear a newer one.
    owner: String,
    /// Audit reason.
    reason: String,
    /// Lease duration, 1 to 900 seconds. Required for pause, ignored for resume.
    duration_seconds: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct InstancePauseResponse {
    /// `mutations` during the lease; empty once resumed or expired.
    paused_scopes: Vec<String>,
    owner: String,
    /// Unix seconds, according to the database clock.
    expires_at: i64,
}

#[utoipa::path(get, path = "/v1/admin/instance/pause", tag = "admin",
    responses((status = 200, description = "Current instance pause", body = InstancePauseResponse)),
    security(("http_message_signature" = [])))]
pub(crate) async fn get_instance_pause(
    State(state): State<AppState>,
) -> Result<Json<InstancePauseResponse>, ApiError> {
    let (scopes, owner, expires_at) = crate::pause::instance_pause(&state.pool).await?;
    Ok(Json(InstancePauseResponse {
        paused_scopes: scopes,
        owner,
        expires_at,
    }))
}

#[utoipa::path(post, path = "/v1/admin/instance/pause", tag = "admin",
    request_body = InstancePauseRequest,
    responses((status = 200, description = "Mutations paused", body = InstancePauseResponse),
        (status = 400, description = "Invalid lease", body = ErrorResponse)),
    security(("http_message_signature" = [])))]
pub(crate) async fn pause_instance(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<InstancePauseRequest>,
) -> Result<Json<InstancePauseResponse>, ApiError> {
    change(&state, &actor, request, true).await
}

#[utoipa::path(post, path = "/v1/admin/instance/resume", tag = "admin",
    request_body = InstancePauseRequest,
    responses((status = 200, description = "Mutations resumed", body = InstancePauseResponse),
        (status = 400, description = "Invalid lease", body = ErrorResponse)),
    security(("http_message_signature" = [])))]
pub(crate) async fn resume_instance(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<InstancePauseRequest>,
) -> Result<Json<InstancePauseResponse>, ApiError> {
    change(&state, &actor, request, false).await
}

async fn change(
    state: &AppState,
    actor: &crate::audit::Actor,
    request: InstancePauseRequest,
    paused: bool,
) -> Result<Json<InstancePauseResponse>, ApiError> {
    if request.owner.is_empty()
        || request.owner.len() > 128
        || !request
            .owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_/.:".contains(&b))
    {
        return Err(ApiError::bad_request("invalid pause owner").with_param("owner"));
    }
    if request.reason.trim().is_empty() || request.reason.len() > 1000 {
        return Err(
            ApiError::bad_request("a reason of 1 to 1000 bytes is required").with_param("reason"),
        );
    }
    let duration = if paused {
        request
            .duration_seconds
            .filter(|n| (1..=900).contains(n))
            .ok_or_else(|| {
                ApiError::bad_request("duration must be 1 to 900 seconds")
                    .with_param("duration_seconds")
            })?
    } else {
        0
    };
    if !crate::pause::mutate_instance_pause(
        &state.pool,
        &request.owner,
        duration,
        actor,
        &request.reason,
    )
    .await?
    {
        return Err(
            ApiError::bad_request("another deployment owns the active pause").with_param("owner"),
        );
    }
    get_instance_pause(State(state.clone())).await
}

pub(crate) async fn admit_request(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) || matches!(
        request.uri().path(),
        "/v1/admin/instance/pause" | "/v1/admin/instance/resume"
    ) {
        return next.run(request).await;
    }
    match crate::pause::mutations_paused(&state.pool).await {
        Ok(true) => ApiError::service_maintenance().into_response(),
        Ok(false) => next.run(request).await,
        Err(error) => ApiError::from(error).into_response(),
    }
}
