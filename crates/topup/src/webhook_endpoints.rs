//! Webhook endpoints (docs/design/multi-tenant.md D11, §11): the merchant's receivers, managed
//! through `/v1/webhook_endpoints` with a secret key, at most [`MAX_ENDPOINTS`] per account and
//! mode, Stripe's endpoints.
//!
//! Every change is audited and is an account event, delivered to every enabled endpoint of the
//! mode whatever its `enabled_events`. The endpoint a change is about receives the event first, at
//! the URL it had before the change and even when the change disables or deletes it, the notice
//! GitHub's `meta` event gives a deleted webhook: a leaked key cannot silence an endpoint unseen.
//! A disabled or deleted endpoint's pending deliveries stop; the merchant resends what it missed
//! with `POST /v1/events/{id}/resend`. The delivery worker disables an endpoint only when it
//! answers `410 Gone`, and announces it the same way ([`disable_gone`]); a failing endpoint is
//! retried forever and never disabled (owner decision, design §11): with no email channel, a
//! disabled endpoint would drop the merchant's credits silently.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{Acquire, FromRow, PgConnection, PgPool, Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use crate::api::error::ApiError;
use crate::api::metadata::{Metadata, MetadataUpdate};
use crate::api::models::{DeliveryAttempt, WebhookEndpointObject};
use crate::audit::{self, Actor};
use crate::db::{EventObject, NewOutboxEvent, Notice};
use crate::ids;
use crate::tenancy::Scope;

/// Endpoints per account and mode, Stripe's limit.
pub const MAX_ENDPOINTS: i64 = 16;

/// The event types an endpoint may subscribe to in `enabled_events`, besides `*`. Account events
/// (`account.*`, `api_key.*`, `payment_settings.*`, `treasury.*`, `webhook_endpoint.*`) reach every
/// enabled endpoint
/// whatever it subscribes to; they are listed so a subscription may name them.
pub const EVENT_TYPES: &[&str] = &[
    "account.updated",
    "api_key.created",
    "api_key.revoked",
    "api_key.updated",
    "deposit.credited",
    "deposit.refunded",
    "deposit.rejected",
    "deposit.reversed",
    "payment_settings.updated",
    "quote.canceled",
    "quote.expired",
    "refund.created",
    "refund.failed",
    "refund.updated",
    "treasury.canceled",
    "treasury.created",
    "treasury.updated",
    "webhook_endpoint.created",
    "webhook_endpoint.deleted",
    "webhook_endpoint.updated",
];

/// The test event `POST /v1/webhook_endpoints/{id}/test` sends to one endpoint.
pub const TEST_EVENT: &str = "webhook_endpoint.test";

/// A stored webhook endpoint.
#[derive(Clone, Debug, Eq, PartialEq, FromRow)]
pub struct WebhookEndpoint {
    /// Endpoint id.
    pub id: Uuid,
    /// The account.
    pub account_id: Uuid,
    /// The mode.
    pub livemode: bool,
    /// Where events are delivered.
    pub url: String,
    /// Subscribed event types, or `["*"]`.
    pub enabled_events: Vec<String>,
    /// `enabled` or `disabled`.
    pub status: String,
    /// Why the service disabled it; `None` when enabled or disabled by the merchant.
    pub disabled_reason: Option<String>,
    /// The merchant's description.
    pub description: Option<String>,
    /// The merchant's metadata.
    pub metadata: Json<Metadata>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Deletion time.
    pub deleted_at: Option<DateTime<Utc>>,
    /// When a delivery to the endpoint was last attempted.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// The HTTP status of that attempt; `None` when no response arrived.
    pub last_attempt_status: Option<i32>,
}

macro_rules! endpoint_columns {
    () => {
        "id, account_id, livemode, url, enabled_events, status, disabled_reason, description, \
         metadata, created_at, deleted_at, last_attempt_at, last_attempt_status"
    };
}

/// An endpoint's undelivered deliveries: how many, and the creation time of the oldest one's
/// event. A notice to a URL the endpoint had before a change is not a delivery to the endpoint as it
/// is now (like its attempts, [`crate::outbox`]), so it is not part of its backlog.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Backlog {
    /// Deliveries neither delivered nor stopped; they are retried until delivered.
    pub pending: i64,
    /// The creation time of the oldest pending delivery's event.
    pub oldest_pending_at: Option<DateTime<Utc>>,
}

/// The backlog of each of `ids`; an endpoint with none is absent.
pub async fn backlogs<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Backlog>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT delivery.endpoint_id, count(*), min(event.created)
        FROM webhook_deliveries AS delivery
        JOIN events AS event ON event.id = delivery.event_id
        WHERE delivery.endpoint_id = ANY($1) AND delivery.url IS NULL
          AND delivery.delivered_at IS NULL AND delivery.failed_at IS NULL
        GROUP BY delivery.endpoint_id
        "#,
    )
    .bind(ids)
    .fetch_all(executor)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, pending, oldest_pending_at)| {
            (
                id,
                Backlog {
                    pending,
                    oldest_pending_at,
                },
            )
        })
        .collect())
}

/// The endpoint's API representation with its delivery health, read on `connection`.
pub async fn render(
    connection: &mut PgConnection,
    endpoint: &WebhookEndpoint,
) -> Result<WebhookEndpointObject, sqlx::Error> {
    let backlog = backlogs(connection, &[endpoint.id])
        .await?
        .remove(&endpoint.id)
        .unwrap_or_default();
    Ok(endpoint.object(backlog))
}

impl WebhookEndpoint {
    /// The endpoint's API id, `we_…`.
    #[must_use]
    pub fn public_id(&self) -> String {
        ids::format(ids::WEBHOOK_ENDPOINT, self.id)
    }

    /// Whether the endpoint receives events.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.status == "enabled" && self.deleted_at.is_none()
    }

    /// The endpoint's API representation, with its `backlog`.
    #[must_use]
    pub fn object(&self, backlog: Backlog) -> WebhookEndpointObject {
        WebhookEndpointObject {
            id: self.public_id(),
            object: "webhook_endpoint".to_owned(),
            livemode: self.livemode,
            url: self.url.clone(),
            enabled_events: self.enabled_events.clone(),
            status: self.status.clone(),
            disabled_reason: self.disabled_reason.clone(),
            description: self.description.clone(),
            metadata: self.metadata.0.clone(),
            created: self.created_at.timestamp(),
            deleted: self.deleted_at.map(|_| true),
            pending_deliveries: backlog.pending,
            oldest_pending_at: backlog.oldest_pending_at.map(|at| at.timestamp()),
            last_attempt: self.last_attempt_at.map(|at| DeliveryAttempt {
                at: at.timestamp(),
                status_code: self
                    .last_attempt_status
                    .and_then(|status| u16::try_from(status).ok()),
            }),
        }
    }
}

/// The fields of an endpoint's delivery health, which deliveries change, not the merchant.
const HEALTH_FIELDS: [&str; 3] = ["pending_deliveries", "oldest_pending_at", "last_attempt"];

/// A `webhook_endpoint.updated` event's `data`: `after`, and the fields the change replaced. The
/// delivery health is not among them: deliveries move it all the time, and it is not what the
/// update changed.
fn updated_data(after: Value, before: &Value) -> Value {
    let mut data = crate::db::event_data(after, Some(before));
    if let Some(previous) = data
        .get_mut("previous_attributes")
        .and_then(Value::as_object_mut)
    {
        for field in HEALTH_FIELDS {
            previous.remove(field);
        }
    }
    data
}

/// The endpoint's API representation with its delivery health, as an event's `data.object`.
async fn snapshot(
    connection: &mut PgConnection,
    endpoint: &WebhookEndpoint,
) -> Result<Value, EndpointError> {
    serde_json::to_value(render(connection, endpoint).await?).map_err(|error| {
        tracing::error!(%error, "webhook endpoint serialization failed");
        EndpointError::Serialization
    })
}

/// A failure to change an endpoint.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// The endpoint does not exist in the scope, or is deleted.
    #[error("webhook endpoint not found")]
    NotFound,
    /// The event does not exist in the scope.
    #[error("event not found")]
    EventNotFound,
    /// The endpoint is disabled.
    #[error("webhook endpoint disabled")]
    Disabled,
    /// The account's mode already has [`MAX_ENDPOINTS`] endpoints.
    #[error("the account has {MAX_ENDPOINTS} webhook endpoints in this mode")]
    LimitReached,
    /// The merged metadata is invalid.
    #[error("invalid metadata")]
    Metadata(ApiError),
    /// The endpoint could not be rendered.
    #[error("webhook endpoint serialization failed")]
    Serialization,
    /// PostgreSQL failed.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// A new endpoint's values, validated by the API.
pub struct NewEndpoint<'a> {
    /// Where events are delivered.
    pub url: &'a str,
    /// Subscribed event types, or `["*"]`.
    pub enabled_events: &'a [String],
    /// The merchant's description.
    pub description: Option<&'a str>,
    /// The merchant's metadata.
    pub metadata: Metadata,
}

/// An update; absent fields stay.
#[derive(Default)]
pub struct Changes<'a> {
    /// A new URL.
    pub url: Option<&'a str>,
    /// New subscriptions.
    pub enabled_events: Option<&'a [String]>,
    /// A new description; `Some(None)` unsets it.
    pub description: Option<Option<&'a str>>,
    /// Disables (`true`) or enables (`false`) the endpoint.
    pub disabled: Option<bool>,
    /// The metadata update.
    pub metadata: Option<&'a MetadataUpdate>,
}

/// Creates an endpoint and announces it as `webhook_endpoint.created`, which the new endpoint
/// receives too.
pub async fn create<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    scope: Scope,
    endpoint: &NewEndpoint<'_>,
    actor: &Actor,
) -> Result<WebhookEndpoint, EndpointError> {
    let mut transaction = db.begin().await?;
    // The account's row lock serializes creations, so the limit holds under concurrency.
    sqlx::query("SELECT 1 FROM accounts WHERE id = $1 FOR UPDATE")
        .bind(scope.account_id())
        .fetch_one(&mut *transaction)
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_endpoints \
         WHERE account_id = $1 AND livemode = $2 AND deleted_at IS NULL",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_one(&mut *transaction)
    .await?;
    if count >= MAX_ENDPOINTS {
        return Err(EndpointError::LimitReached);
    }
    let created = sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "INSERT INTO webhook_endpoints \
         (id, account_id, livemode, url, enabled_events, description, metadata) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        endpoint_columns!()
    ))
    .bind(Uuid::new_v4())
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(endpoint.url)
    .bind(endpoint.enabled_events)
    .bind(endpoint.description)
    .bind(Json(&endpoint.metadata))
    .fetch_one(&mut *transaction)
    .await?;
    let data = json!({ "object": snapshot(&mut transaction, &created).await? });
    announce(
        &mut transaction,
        &created,
        "webhook_endpoint.created",
        actor,
        &data,
        None,
    )
    .await?;
    transaction.commit().await?;
    Ok(created)
}

/// Pagination of [`list`], newest first.
#[derive(Clone, Copy, Debug, Default)]
pub struct Page {
    /// At most this many.
    pub limit: i64,
    /// The endpoint the page starts after (or ends before, with `before`).
    pub cursor: Option<Uuid>,
    /// Whether `cursor` is `ending_before`.
    pub before: bool,
}

/// The scope's endpoints, newest first, and whether more follow; `NotFound` for a cursor outside
/// the scope.
pub async fn list(
    pool: &PgPool,
    scope: Scope,
    page: Page,
) -> Result<(Vec<WebhookEndpoint>, bool), EndpointError> {
    let mut builder = QueryBuilder::<Postgres>::new(concat!(
        "SELECT ",
        endpoint_columns!(),
        " FROM webhook_endpoints WHERE deleted_at IS NULL AND account_id = "
    ));
    builder
        .push_bind(scope.account_id())
        .push(" AND livemode = ")
        .push_bind(scope.livemode());
    if let Some(cursor) = page.cursor {
        let found: Option<(DateTime<Utc>, Uuid)> = sqlx::query_as(
            "SELECT created_at, id FROM webhook_endpoints \
             WHERE id = $1 AND account_id = $2 AND livemode = $3 AND deleted_at IS NULL",
        )
        .bind(cursor)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(pool)
        .await?;
        let (created_at, id) = found.ok_or(EndpointError::NotFound)?;
        builder
            .push(if page.before {
                " AND (created_at, id) > ("
            } else {
                " AND (created_at, id) < ("
            })
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder.push(if page.before {
        " ORDER BY created_at ASC, id ASC LIMIT "
    } else {
        " ORDER BY created_at DESC, id DESC LIMIT "
    });
    builder.push_bind(page.limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<WebhookEndpoint>()
        .fetch_all(pool)
        .await?;
    let limit = usize::try_from(page.limit).unwrap_or(usize::MAX);
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    if page.before {
        rows.reverse();
    }
    Ok((rows, has_more))
}

/// One endpoint of the scope that is not deleted.
pub async fn get(
    pool: &PgPool,
    scope: Scope,
    id: Uuid,
) -> Result<Option<WebhookEndpoint>, sqlx::Error> {
    sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "SELECT ",
        endpoint_columns!(),
        " FROM webhook_endpoints \
         WHERE id = $1 AND account_id = $2 AND livemode = $3 AND deleted_at IS NULL"
    ))
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(pool)
    .await
}

/// One endpoint of the scope, deleted or not, rendered for an event's `data`.
pub async fn find_any(
    connection: &mut PgConnection,
    scope: Scope,
    id: Uuid,
) -> Result<Option<WebhookEndpointObject>, sqlx::Error> {
    let endpoint = sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "SELECT ",
        endpoint_columns!(),
        " FROM webhook_endpoints WHERE id = $1 AND account_id = $2 AND livemode = $3"
    ))
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(&mut *connection)
    .await?;
    match endpoint {
        Some(endpoint) => render(connection, &endpoint).await.map(Some),
        None => Ok(None),
    }
}

/// Applies `changes` and announces them as `webhook_endpoint.updated`, with the replaced values
/// in `data.previous_attributes`; the endpoint receives it first, at its previous URL. An update
/// that changes nothing writes nothing. Disabling stops the endpoint's pending deliveries.
pub async fn update<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    scope: Scope,
    id: Uuid,
    changes: &Changes<'_>,
    actor: &Actor,
) -> Result<WebhookEndpoint, EndpointError> {
    let mut transaction = db.begin().await?;
    let before = locked(&mut transaction, scope, id).await?;
    let before_object = snapshot(&mut transaction, &before).await?;
    let metadata = match changes.metadata {
        Some(update) => update
            .apply(before.metadata.0.clone())
            .map_err(EndpointError::Metadata)?,
        None => before.metadata.0.clone(),
    };
    let status = match changes.disabled {
        Some(true) => "disabled",
        Some(false) => "enabled",
        None => before.status.as_str(),
    };
    let disabled_reason = if status == before.status {
        before.disabled_reason.as_deref()
    } else {
        None
    };
    let after = sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "UPDATE webhook_endpoints SET url = $2, enabled_events = $3, description = $4, \
         status = $5, disabled_reason = $6, metadata = $7 WHERE id = $1 RETURNING ",
        endpoint_columns!()
    ))
    .bind(id)
    .bind(changes.url.unwrap_or(&before.url))
    .bind(changes.enabled_events.unwrap_or(&before.enabled_events))
    .bind(changes.description.unwrap_or(before.description.as_deref()))
    .bind(status)
    .bind(disabled_reason)
    .bind(Json(&metadata))
    .fetch_one(&mut *transaction)
    .await?;
    if after == before {
        transaction.commit().await?;
        return Ok(after);
    }
    if !after.enabled() {
        stop_pending(&mut transaction, id).await?;
    }
    let data = updated_data(snapshot(&mut transaction, &after).await?, &before_object);
    let notice = Notice {
        endpoint_id: id,
        url: before.url.clone(),
    };
    announce(
        &mut transaction,
        &after,
        "webhook_endpoint.updated",
        actor,
        &data,
        Some(&notice),
    )
    .await?;
    transaction.commit().await?;
    Ok(after)
}

/// Deletes an endpoint: it receives nothing more but the `webhook_endpoint.deleted` event, which
/// it receives first, and its pending deliveries stop. Returns the deleted endpoint.
pub async fn delete(
    pool: &PgPool,
    scope: Scope,
    id: Uuid,
    actor: &Actor,
) -> Result<WebhookEndpoint, EndpointError> {
    let mut transaction = pool.begin().await?;
    locked(&mut transaction, scope, id).await?;
    let deleted = sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "UPDATE webhook_endpoints SET deleted_at = now() WHERE id = $1 RETURNING ",
        endpoint_columns!()
    ))
    .bind(id)
    .fetch_one(&mut *transaction)
    .await?;
    stop_pending(&mut transaction, id).await?;
    let data = json!({ "object": snapshot(&mut transaction, &deleted).await? });
    let notice = Notice {
        endpoint_id: id,
        url: deleted.url.clone(),
    };
    announce(
        &mut transaction,
        &deleted,
        "webhook_endpoint.deleted",
        actor,
        &data,
        Some(&notice),
    )
    .await?;
    transaction.commit().await?;
    Ok(deleted)
}

/// Sends [`TEST_EVENT`] about the endpoint to the endpoint alone, whatever its status and
/// subscriptions, and returns the event id.
pub async fn send_test<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    scope: Scope,
    id: Uuid,
    actor: &Actor,
) -> Result<Uuid, EndpointError> {
    let mut transaction = db.begin().await?;
    let endpoint = locked(&mut transaction, scope, id).await?;
    let event = new_event(&endpoint, TEST_EVENT, actor);
    let data = json!({ "object": snapshot(&mut transaction, &endpoint).await? });
    let notice = Notice {
        endpoint_id: id,
        url: endpoint.url.clone(),
    };
    crate::db::enqueue_rendered_in(&mut transaction, &event, &data, Some(&notice), false).await?;
    transaction.commit().await?;
    Ok(event.id)
}

/// Delivers the scope's event `event_id` again to its enabled endpoint `endpoint_id`, as the
/// Stripe CLI's `events resend`: due now, whether the event was delivered to it, stopped, or never
/// sent to it. The event and its body are unchanged.
pub async fn resend<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    scope: Scope,
    event_id: Uuid,
    endpoint_id: Uuid,
    actor: &Actor,
) -> Result<(), EndpointError> {
    let mut transaction = db.begin().await?;
    let event: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM events WHERE id = $1 AND account_id = $2 AND livemode = $3",
    )
    .bind(event_id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(&mut *transaction)
    .await?;
    if event.is_none() {
        return Err(EndpointError::EventNotFound);
    }
    let endpoint = locked(&mut transaction, scope, endpoint_id).await?;
    if !endpoint.enabled() {
        return Err(EndpointError::Disabled);
    }
    sqlx::query(
        r#"
        INSERT INTO webhook_deliveries (event_id, endpoint_id, next_attempt_at)
        VALUES ($1, $2, now())
        ON CONFLICT (event_id, endpoint_id) DO UPDATE
        SET next_attempt_at = now(), attempts = 0, delivered_at = NULL,
            failed_at = NULL, url = NULL
        "#,
    )
    .bind(event_id)
    .bind(endpoint_id)
    .execute(&mut *transaction)
    .await?;
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(scope.account_id()),
            actor,
            action: "event.resend",
            subject: &format!("event:{}", ids::format(ids::EVENT, event_id)),
            reason: &endpoint.public_id(),
        },
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Disables an enabled endpoint that answered `410 Gone` (`disabled_reason: "gone"`), the
/// receiver's own request under Standard Webhooks, stops its pending deliveries, and announces it
/// as `webhook_endpoint.updated` to the mode's other endpoints. Returns whether it was enabled.
pub async fn disable_gone(connection: &mut PgConnection, id: Uuid) -> Result<bool, EndpointError> {
    let mut transaction = sqlx::Connection::begin(&mut *connection).await?;
    let before = sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "SELECT ",
        endpoint_columns!(),
        " FROM webhook_endpoints WHERE id = $1 AND status = 'enabled' AND deleted_at IS NULL \
         FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(before) = before else {
        transaction.rollback().await?;
        return Ok(false);
    };
    let before_object = snapshot(&mut transaction, &before).await?;
    let after = sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "UPDATE webhook_endpoints SET status = 'disabled', disabled_reason = 'gone' \
         WHERE id = $1 RETURNING ",
        endpoint_columns!()
    ))
    .bind(id)
    .fetch_one(&mut *transaction)
    .await?;
    stop_pending(&mut transaction, id).await?;
    let data = updated_data(snapshot(&mut transaction, &after).await?, &before_object);
    announce(
        &mut transaction,
        &after,
        "webhook_endpoint.updated",
        &Actor::system("webhook-delivery"),
        &data,
        None,
    )
    .await?;
    transaction.commit().await?;
    tracing::warn!(
        endpoint = %after.public_id(),
        "webhook endpoint answered 410 Gone and was disabled"
    );
    Ok(true)
}

async fn locked(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
) -> Result<WebhookEndpoint, EndpointError> {
    sqlx::query_as::<_, WebhookEndpoint>(concat!(
        "SELECT ",
        endpoint_columns!(),
        " FROM webhook_endpoints \
         WHERE id = $1 AND account_id = $2 AND livemode = $3 AND deleted_at IS NULL FOR UPDATE"
    ))
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(EndpointError::NotFound)
}

/// Stops the endpoint's pending deliveries, except its own notices.
async fn stop_pending(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE webhook_deliveries SET failed_at = now() \
         WHERE endpoint_id = $1 AND delivered_at IS NULL AND failed_at IS NULL AND url IS NULL",
    )
    .bind(id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Audits the change and records its event, with the endpoint's `notice` first when given.
async fn announce(
    transaction: &mut Transaction<'_, Postgres>,
    endpoint: &WebhookEndpoint,
    event_type: &str,
    actor: &Actor,
    data: &Value,
    notice: Option<&Notice>,
) -> Result<(), EndpointError> {
    audit::insert(
        &mut **transaction,
        &audit::Entry {
            account_id: Some(endpoint.account_id),
            actor,
            action: event_type,
            subject: &format!("webhook_endpoint:{}", endpoint.public_id()),
            reason: "",
        },
    )
    .await?;
    let event = new_event(endpoint, event_type, actor);
    crate::db::enqueue_rendered_in(transaction, &event, data, notice, true).await?;
    Ok(())
}

fn new_event(endpoint: &WebhookEndpoint, event_type: &str, actor: &Actor) -> NewOutboxEvent {
    NewOutboxEvent::new(
        event_type,
        Scope::new(endpoint.account_id, endpoint.livemode),
        EventObject::WebhookEndpoint(endpoint.id),
        actor,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_names_what_it_changed_but_not_the_delivery_health() {
        let before =
            json!({"url": "https://a.example", "pending_deliveries": 3, "last_attempt": null});
        let after = json!({"url": "https://b.example", "pending_deliveries": 0,
                           "last_attempt": {"at": 1, "status_code": 200}});
        assert_eq!(
            updated_data(after.clone(), &before),
            json!({"object": after, "previous_attributes": {"url": "https://a.example"}})
        );
    }

    #[test]
    fn subscribable_types_are_sorted_and_unique() {
        assert!(
            EVENT_TYPES
                .windows(2)
                .all(|pair| matches!(pair, [first, second] if first < second))
        );
        assert!(!EVENT_TYPES.contains(&TEST_EVENT));
    }
}
