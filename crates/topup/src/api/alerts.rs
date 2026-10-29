//! Signed operator management of durable alert instances.

use axum::{
    Json,
    extract::{RawQuery, State},
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sqlx::QueryBuilder;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    AppState,
    auth::AdminActor,
    error::{ApiError, ErrorResponse},
    extract::{ApiJson, ApiPath},
    pagination::Page,
};
use crate::{
    audit::{self, Actor, Entry},
    ids,
};

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    name: String,
    grouping_key: String,
    severity: String,
    tags: serde_json::Value,
    runbook: String,
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    acknowledged_at: Option<DateTime<Utc>>,
    acknowledged_by: Option<String>,
    silenced_until: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct Alert {
    /// Alert instance id, `al_` followed by 32 lowercase hex digits.
    id: String,
    /// Always `alert`.
    object: &'static str,
    name: String,
    /// Canonical JSON of grouping tags, excluding name and severity.
    grouping_key: String,
    severity: String,
    tags: serde_json::Value,
    runbook: String,
    /// Unix seconds of the first observation of this instance.
    first_seen_at: i64,
    /// Unix seconds of the most recent firing observation, including suppressed reminders.
    last_seen_at: i64,
    resolved_at: Option<i64>,
    acknowledged_at: Option<i64>,
    /// Authenticated admin actor, including key id.
    acknowledged_by: Option<String>,
    /// Unix seconds; expired values do not suppress Sentry events.
    silenced_until: Option<i64>,
}

impl From<Row> for Alert {
    fn from(row: Row) -> Self {
        Self {
            id: ids::format(ids::ALERT, row.id),
            object: "alert",
            name: row.name,
            grouping_key: row.grouping_key,
            severity: row.severity,
            tags: row.tags,
            runbook: row.runbook,
            first_seen_at: row.first_seen_at.timestamp(),
            last_seen_at: row.last_seen_at.timestamp(),
            resolved_at: row.resolved_at.map(|at| at.timestamp()),
            acknowledged_at: row.acknowledged_at.map(|at| at.timestamp()),
            acknowledged_by: row.acknowledged_by,
            silenced_until: row.silenced_until.map(|at| at.timestamp()),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AlertList {
    /// Always `list`.
    object: &'static str,
    url: &'static str,
    has_more: bool,
    data: Vec<Alert>,
}

#[utoipa::path(get, path="/v1/admin/alerts", tag="admin",
    params(
        ("state"=Option<String>, Query, description="active (default), resolved, or all; ack/silence never hide an active instance"),
        ("name"=Option<String>, Query, description="Exact alert name"),
        ("limit"=Option<i64>, Query, description="1–100, default 10"),
        ("starting_after"=Option<String>, Query, description="Alert instance id, exclusive cursor"),
        ("ending_before"=Option<String>, Query, description="Alert instance id; mutually exclusive with starting_after")
    ),
    responses((status=200, description="Alert instances, newest first by first_seen_at and id", body=AlertList),
        (status=400, description="Invalid filter or cursor", body=ErrorResponse)),
    security(("http_message_signature"=[])))]
pub(crate) async fn list(
    State(state): State<AppState>,
    AdminActor(_actor): AdminActor,
    RawQuery(query): RawQuery,
) -> Result<Json<AlertList>, ApiError> {
    let mut page = Page::default();
    let mut filter = "active".to_owned();
    let mut name = None;
    for (key, value) in url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
    {
        if page.accept(&key, &value, ids::ALERT)? {
            continue;
        }
        match key.as_ref() {
            "state" if matches!(value.as_ref(), "active" | "resolved" | "all") => {
                filter = value.into_owned()
            }
            "state" => {
                return Err(ApiError::invalid_param(
                    "state",
                    "state must be active, resolved, or all",
                ));
            }
            "name" if !value.is_empty() && value.len() <= 200 => name = Some(value.into_owned()),
            "name" => {
                return Err(ApiError::invalid_param(
                    "name",
                    "name must be 1 to 200 bytes",
                ));
            }
            _ => return Err(ApiError::invalid_param(key.as_ref(), "unknown parameter")),
        }
    }
    let cursor: Option<(DateTime<Utc>, Uuid)> = if let Some(cursor) = page.cursor {
        Some(
            sqlx::query_as("SELECT first_seen_at,id FROM alerts WHERE id=$1")
                .bind(cursor)
                .fetch_optional(&state.pool)
                .await?
                .ok_or_else(|| page.unknown_cursor("alert"))?,
        )
    } else {
        None
    };
    let mut query = QueryBuilder::new("SELECT * FROM alerts WHERE true");
    match filter.as_str() {
        "active" => {
            query.push(" AND resolved_at IS NULL");
        }
        "resolved" => {
            query.push(" AND resolved_at IS NOT NULL");
        }
        _ => {}
    }
    if let Some(name) = name {
        query.push(" AND name=").push_bind(name);
    }
    if let Some((at, id)) = cursor {
        query
            .push(if page.before {
                " AND (first_seen_at,id) > ("
            } else {
                " AND (first_seen_at,id) < ("
            })
            .push_bind(at)
            .push(",")
            .push_bind(id)
            .push(")");
    }
    query
        .push(if page.before {
            " ORDER BY first_seen_at,id LIMIT "
        } else {
            " ORDER BY first_seen_at DESC,id DESC LIMIT "
        })
        .push_bind(page.limit.saturating_add(1));
    let mut rows: Vec<Row> = query.build_query_as().fetch_all(&state.pool).await?;
    let limit = usize::try_from(page.limit).map_err(|_| ApiError::internal())?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    if page.before {
        rows.reverse();
    }
    Ok(Json(AlertList {
        object: "list",
        url: "/v1/admin/alerts",
        has_more,
        data: rows.into_iter().map(Alert::from).collect(),
    }))
}

fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    ids::parse(ids::ALERT, id).ok_or_else(|| ApiError::invalid_param("id", "not an alert id"))
}

#[utoipa::path(get, path="/v1/admin/alerts/{id}", tag="admin",
    params(("id"=String, Path, description="Alert instance id")),
    responses((status=200, description="Alert instance", body=Alert),
        (status=404, description="No such alert", body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn get(
    State(state): State<AppState>,
    AdminActor(_actor): AdminActor,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Alert>, ApiError> {
    let row: Row = sqlx::query_as("SELECT * FROM alerts WHERE id=$1")
        .bind(parse_id(&id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row.into()))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SilenceRequest {
    /// Future Unix seconds, at most seven days from the request.
    until: i64,
}

#[utoipa::path(post, path="/v1/admin/alerts/{id}/acknowledge", tag="admin",
    params(("id"=String, Path, description="Alert instance id")),
    responses((status=200, description="Acknowledged instance; repeats preserve the first acknowledgment", body=Alert),
        (status=404, description="No such alert", body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn acknowledge(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Alert>, ApiError> {
    change(&state, &actor, &id, Change::Acknowledge).await
}

#[utoipa::path(post, path="/v1/admin/alerts/{id}/silence", tag="admin", request_body=SilenceRequest,
    params(("id"=String, Path, description="Active alert instance id")),
    responses((status=200, description="Sentry events suppressed until deadline; alert remains visible", body=Alert),
        (status=400, description="Invalid deadline or resolved instance", body=ErrorResponse),
        (status=404, description="No such alert", body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn silence(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<SilenceRequest>,
) -> Result<Json<Alert>, ApiError> {
    let now = Utc::now();
    let until = DateTime::from_timestamp(request.until, 0)
        .filter(|until| *until > now && *until <= now + TimeDelta::days(7))
        .ok_or_else(|| {
            ApiError::invalid_param(
                "until",
                "until must be future Unix seconds, at most seven days away",
            )
        })?;
    change(&state, &actor, &id, Change::Silence(until)).await
}

#[utoipa::path(post, path="/v1/admin/alerts/{id}/unsilence", tag="admin",
    params(("id"=String, Path, description="Alert instance id")),
    responses((status=200, description="Silence cleared", body=Alert),
        (status=404, description="No such alert", body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn unsilence(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Alert>, ApiError> {
    change(&state, &actor, &id, Change::Unsilence).await
}

#[utoipa::path(post, path="/v1/admin/alerts/{id}/resolve", tag="admin",
    params(("id"=String, Path, description="Alert instance id")),
    responses((status=200, description="Resolved; for event-only legacy alerts after checking recovery", body=Alert),
        (status=404, description="No such alert", body=ErrorResponse)), security(("http_message_signature"=[])))]
pub(crate) async fn resolve(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Alert>, ApiError> {
    change(&state, &actor, &id, Change::Resolve).await
}

enum Change {
    Acknowledge,
    Silence(DateTime<Utc>),
    Unsilence,
    Resolve,
}

async fn change(
    state: &AppState,
    actor: &Actor,
    id: &str,
    change: Change,
) -> Result<Json<Alert>, ApiError> {
    let id = parse_id(id)?;
    let _guard = crate::alerts::silence_change_lock().await;
    let mut tx = state.pool.begin().await?;
    let row: Row = sqlx::query_as("SELECT * FROM alerts WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (action, reason) = match change {
        Change::Acknowledge => {
            sqlx::query(
                "UPDATE alerts SET acknowledged_at=COALESCE(acknowledged_at,clock_timestamp()), \
                acknowledged_by=COALESCE(acknowledged_by,$2) WHERE id=$1",
            )
            .bind(id)
            .bind(actor.to_string())
            .execute(&mut *tx)
            .await?;
            (
                "alert.acknowledged",
                "operator acknowledged instance".to_owned(),
            )
        }
        Change::Silence(until) => {
            if row.resolved_at.is_some() {
                return Err(ApiError::bad_request("a resolved alert cannot be silenced"));
            }
            sqlx::query("UPDATE alerts SET silenced_until=$2 WHERE id=$1")
                .bind(id)
                .bind(until)
                .execute(&mut *tx)
                .await?;
            (
                "alert.silenced",
                serde_json::json!({"until":until.timestamp()}).to_string(),
            )
        }
        Change::Unsilence => {
            sqlx::query("UPDATE alerts SET silenced_until=NULL WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            ("alert.unsilenced", "operator cleared silence".to_owned())
        }
        Change::Resolve => {
            sqlx::query("UPDATE alerts SET resolved_at=COALESCE(resolved_at,clock_timestamp()),silenced_until=NULL WHERE id=$1")
                .bind(id).execute(&mut *tx).await?;
            ("alert.resolved", "operator verified recovery".to_owned())
        }
    };
    audit::insert(
        &mut *tx,
        &Entry {
            account_id: None,
            actor,
            action,
            subject: &ids::format(ids::ALERT, id),
            reason: &reason,
        },
    )
    .await?;
    let updated: Row = sqlx::query_as("SELECT * FROM alerts WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    crate::alerts::update_silence(
        &updated.name,
        &updated.grouping_key,
        id,
        updated
            .silenced_until
            .filter(|_| updated.resolved_at.is_none()),
    );
    Ok(Json(updated.into()))
}
