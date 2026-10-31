//! Operator access to finalized ignored accounting and bounded raw samples.

use alloy_primitives::Address;
use axum::{
    Json,
    extract::{RawQuery, State},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::QueryBuilder;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    AppState,
    auth::AdminActor,
    error::{ApiError, ErrorResponse},
    pagination::Page,
};
use crate::ids;

#[derive(Deserialize, Serialize, ToSchema)]
struct Sample {
    transaction_hash: String,
    log_index: i64,
    from_address: String,
    amount_atomic: String,
    reason: String,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    chain_id: i64,
    address_id: Uuid,
    address: String,
    token: String,
    block_number: i64,
    block_hash: String,
    amount_atomic: String,
    event_count: i64,
    created_at: DateTime<Utc>,
    samples: sqlx::types::Json<Vec<Sample>>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IgnoredTransfer {
    id: String,
    /// Always `ignored_transfer`.
    object: &'static str,
    chain_id: i64,
    forwarder: String,
    address: String,
    token: String,
    block_number: i64,
    block_hash: String,
    /// Exact sum of all ignored transfers in this token/address/block, including unsampled logs.
    amount_atomic: String,
    /// Exact number of ignored logs in this group.
    event_count: i64,
    created: i64,
    /// At most 20 raw logs per address/token/UTC observation day; pruned after 30 days.
    samples: Vec<Sample>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IgnoredTransferList {
    /// Always `list`.
    object: &'static str,
    url: &'static str,
    has_more: bool,
    data: Vec<IgnoredTransfer>,
}

#[utoipa::path(get, path="/v1/admin/ignored_transfers", tag="admin",
    params(
        ("chain_id"=Option<i64>, Query, description="Positive EVM chain id"),
        ("forwarder"=Option<String>, Query, description="Issued forwarder id, fwd_..."),
        ("token"=Option<String>, Query, description="ERC-20 contract address"),
        ("limit"=Option<i64>, Query, description="1–100, default 10"),
        ("starting_after"=Option<String>, Query, description="Ignored transfer aggregate id, igt_..."),
        ("ending_before"=Option<String>, Query, description="Exclusive cursor; mutually exclusive with starting_after")
    ),
    responses((status=200, description="Finalized accounting groups, newest observation first; raw samples are capped and expire", body=IgnoredTransferList),
        (status=400, description="Invalid filter or cursor", body=ErrorResponse)),
    security(("http_message_signature"=[])))]
pub(crate) async fn list(
    State(state): State<AppState>,
    AdminActor(_actor): AdminActor,
    RawQuery(query): RawQuery,
) -> Result<Json<IgnoredTransferList>, ApiError> {
    let mut page = Page::default();
    let mut chain = None;
    let mut forwarder = None;
    let mut token = None;
    for (key, value) in url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
    {
        if page.accept(&key, &value, ids::IGNORED_TRANSFER)? {
            continue;
        }
        match key.as_ref() {
            "chain_id" => {
                chain = Some(
                    value
                        .parse::<i64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| {
                            ApiError::invalid_param("chain_id", "must be a positive chain id")
                        })?,
                )
            }
            "forwarder" => {
                forwarder =
                    Some(ids::parse(ids::FORWARDER, &value).ok_or_else(|| {
                        ApiError::invalid_param("forwarder", "not a forwarder id")
                    })?)
            }
            "token" => {
                token = Some(format!(
                    "{:#x}",
                    value
                        .parse::<Address>()
                        .map_err(|_| ApiError::invalid_param("token", "not an EVM address"))?
                ))
            }
            _ => return Err(ApiError::invalid_param(key.as_ref(), "unknown parameter")),
        }
    }
    let cursor: Option<(DateTime<Utc>, Uuid)> = if let Some(cursor) = page.cursor {
        Some(
            sqlx::query_as("SELECT created_at,id FROM ignored_transfer_totals WHERE id=$1")
                .bind(cursor)
                .fetch_optional(&state.pool)
                .await?
                .ok_or_else(|| page.unknown_cursor("ignored transfer"))?,
        )
    } else {
        None
    };
    let mut query = QueryBuilder::new(
        "SELECT t.id,t.chain_id,t.address_id,t.token,t.block_number,t.block_hash, \
        t.amount_atomic::text AS amount_atomic,t.event_count,t.created_at,a.address, \
        COALESCE((SELECT jsonb_agg(jsonb_build_object('transaction_hash',s.tx_hash,'log_index',s.log_index, \
        'from_address',s.from_address,'amount_atomic',s.amount_atomic::text,'reason',s.reason) ORDER BY s.log_index) \
        FROM ignored_transfer_samples s WHERE s.total_id=t.id),'[]'::jsonb) AS samples \
        FROM ignored_transfer_totals t JOIN addresses a ON a.id=t.address_id WHERE true",
    );
    if let Some(chain) = chain {
        query.push(" AND t.chain_id=").push_bind(chain);
    }
    if let Some(forwarder) = forwarder {
        query.push(" AND t.address_id=").push_bind(forwarder);
    }
    if let Some(token) = token {
        query.push(" AND t.token=").push_bind(token);
    }
    if let Some((at, id)) = cursor {
        query
            .push(if page.before {
                " AND (t.created_at,t.id) > ("
            } else {
                " AND (t.created_at,t.id) < ("
            })
            .push_bind(at)
            .push(",")
            .push_bind(id)
            .push(")");
    }
    query
        .push(if page.before {
            " ORDER BY t.created_at,t.id LIMIT "
        } else {
            " ORDER BY t.created_at DESC,t.id DESC LIMIT "
        })
        .push_bind(page.limit.saturating_add(1));
    let mut rows: Vec<Row> = query.build_query_as().fetch_all(&state.pool).await?;
    let limit = usize::try_from(page.limit).map_err(|_| ApiError::internal())?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    if page.before {
        rows.reverse();
    }
    Ok(Json(IgnoredTransferList {
        object: "list",
        url: "/v1/admin/ignored_transfers",
        has_more,
        data: rows
            .into_iter()
            .map(|r| IgnoredTransfer {
                id: ids::format(ids::IGNORED_TRANSFER, r.id),
                object: "ignored_transfer",
                chain_id: r.chain_id,
                forwarder: ids::format(ids::FORWARDER, r.address_id),
                address: r.address,
                token: r.token,
                block_number: r.block_number,
                block_hash: r.block_hash,
                amount_atomic: r.amount_atomic,
                event_count: r.event_count,
                created: r.created_at.timestamp(),
                samples: r.samples.0,
            })
            .collect(),
    }))
}
