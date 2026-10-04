use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::{Fuse, FusedFuture as _, FutureExt as _};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, Proxy, StatusCode, Url};
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgPool;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use topup_core::{Signer, retry::backoff};
use tracing::Instrument as _;
use uuid::Uuid;

use super::{Event, SignedWebhook, webhook_id};
use crate::audit::RequestRef;
use crate::db::EventObject;
use crate::jitter::{JitterSource, OsJitter};
use crate::tenancy::Scope;
use crate::webhook_endpoints;

const MAX_POSTGRES_INTERVAL_SECONDS: u64 = i32::MAX as u64;
/// Endpoints considered by one scheduling pass.
const MAX_ENDPOINTS_PER_PASS: i64 = 1000;
/// The longest `Retry-After` a delivery honors: the backoff's own ceiling, an hour.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Runtime limits for the webhook delivery loop.
#[derive(Clone, Debug)]
pub struct DeliveryConfig {
    /// Deliveries in flight at once, across endpoints.
    pub max_in_flight: usize,
    /// Deliveries in flight at once to one endpoint (design §11: 4).
    pub endpoint_concurrency: usize,
    /// Complete HTTP request timeout, including reading the response body.
    pub request_timeout: Duration,
    /// Reservation of a claimed delivery; this must exceed the request timeout.
    pub claim_lease: Duration,
    /// Delay between polls that find nothing to do, or after a database failure.
    pub poll_interval: Duration,
    /// Maximum response body bytes retained in `webhook_deliveries.response`.
    pub response_body_limit: usize,
    /// Pending age after which every claimed event emits a warning.
    pub age_alert_threshold: Duration,
    /// The egress proxy every delivery goes through: smokescreen, the only IP filter (design §8).
    /// `None` connects directly, only for local stacks and tests.
    pub proxy: Option<Url>,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 32,
            endpoint_concurrency: 4,
            request_timeout: Duration::from_secs(20),
            claim_lease: Duration::from_secs(5 * 60),
            poll_interval: Duration::from_secs(1),
            response_body_limit: 4 * 1024,
            age_alert_threshold: Duration::from_secs(24 * 60 * 60),
            proxy: None,
        }
    }
}

/// Failure to configure or access the delivery repository.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// Configuration is internally inconsistent.
    #[error("invalid delivery config: {0}")]
    InvalidConfig(&'static str),
    /// The HTTP client could not be constructed.
    #[error("failed to build webhook client: {0}")]
    Client(#[source] reqwest::Error),
    /// PostgreSQL could not claim or persist a delivery.
    #[error("webhook delivery database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    /// An endpoint could not be disabled.
    #[error("failed to disable webhook endpoint: {0}")]
    Endpoint(#[from] webhook_endpoints::EndpointError),
}

/// One event's delivery to one endpoint, reserved by a claim lease.
#[derive(Clone, Debug)]
struct ClaimedEvent {
    id: Uuid,
    endpoint_id: Uuid,
    url: String,
    /// The endpoint's own notice, sent to `url` whatever the endpoint's status.
    notice: bool,
    scope: Scope,
    account: String,
    event_type: String,
    actor: String,
    request: Option<RequestRef>,
    data: Value,
    object: Option<EventObject>,
    /// A key version that signs the event whatever its overlap: the one a roll retired.
    signing_key_version: Option<i32>,
    attempts: i32,
    created_at: DateTime<Utc>,
    claim_until: DateTime<Utc>,
}

/// What one attempt came to.
/// What one attempt says about its endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reach {
    /// The endpoint acknowledged it.
    Reached,
    /// The endpoint failed it: an error status, a redirect, a timeout, or no connection; with the
    /// wait its `Retry-After` asked for.
    Failed { retry_after: Option<Duration> },
    /// Nothing about the endpoint as it is: the service failed before sending it, or it was a
    /// notice to the endpoint's former URL.
    Unknown,
}

/// A failing endpoint's cooldown: while it runs the endpoint is not claimed, and after it the
/// endpoint gets one probe delivery at a time until one succeeds. The cooldown is the delivery
/// backoff over the endpoint's consecutive failures (full jitter, ceiling 30 s doubling to 1 h),
/// so a dead endpoint costs about one attempt per hour, however many deliveries it has queued.
#[derive(Clone, Copy, Debug)]
struct Cooldown {
    failures: u32,
    until: Instant,
}

enum Outcome {
    Delivered(Value),
    Failed {
        status: Option<u16>,
        body: Option<String>,
        error: &'static str,
        /// Whether the endpoint caused it, as opposed to the service failing to render or sign.
        endpoint_fault: bool,
        /// The wait a `429` or `503` asked for in `Retry-After`, at most [`MAX_RETRY_AFTER`].
        retry_after: Option<Duration>,
    },
}

impl Outcome {
    fn internal(error: &'static str) -> Self {
        crate::observability::emit_alert("TopupOutboxInternalFailure", error, "critical", 1, 0);
        Self::Failed {
            status: None,
            body: None,
            error,
            endpoint_fault: false,
            retry_after: None,
        }
    }
}

/// PostgreSQL-backed Standard Webhooks sender of one mode's events.
///
/// Test and live events have separate workers (design §9), so test traffic cannot delay live
/// deliveries. Deliveries run concurrently, at most `endpoint_concurrency` to one endpoint, and
/// free slots are shared round-robin across the endpoints with due deliveries, so a slow endpoint
/// holds only its own slots and never delays another (design §11). No database connection is held
/// while a request is in flight: a delivery is reserved by its lease. Each delivery is signed with
/// the event's account key in the event's mode, once per key version still signing during a
/// rotation (design D11). A delivery that fails is retried with full-jitter backoff capped at an
/// hour until it is delivered: nothing gives up on a timer, since a disabled endpoint would drop a
/// merchant's credits silently (owner decision, design §11). Only `410 Gone` from the receiver, or
/// the merchant disabling or deleting the endpoint, stops deliveries. A permanently failing
/// endpoint costs at most its `endpoint_concurrency` slots and one claim per slot per backoff.
pub struct DeliveryWorker<S> {
    pool: PgPool,
    client: Client,
    signer: Arc<S>,
    livemode: bool,
    config: DeliveryConfig,
    entropy: Arc<dyn JitterSource>,
}

impl<S> DeliveryWorker<S>
where
    S: Signer,
{
    /// Builds a worker of the `livemode` events with redirects disabled, a bounded request
    /// timeout, and every request sent through `config.proxy`.
    pub fn new(
        pool: PgPool,
        signer: Arc<S>,
        livemode: bool,
        config: DeliveryConfig,
    ) -> Result<Self, DeliveryError> {
        validate_config(&config)?;
        // Stripe counts a redirect as a failure; following one would also leave the proxy's
        // decision to the redirect target.
        let builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout);
        let builder = match &config.proxy {
            Some(proxy) => builder.proxy(Proxy::all(proxy.clone()).map_err(DeliveryError::Client)?),
            // Never an ambient HTTP_PROXY: without the configured proxy, connect directly.
            None => builder.no_proxy(),
        };
        let client = builder.build().map_err(DeliveryError::Client)?;
        Ok(Self {
            pool,
            client,
            signer,
            livemode,
            config,
            entropy: Arc::new(OsJitter),
        })
    }

    /// Delivers until shutdown. The worker's monitor is `topup-outbox-live` or
    /// `topup-outbox-test`.
    pub async fn run(&self, shutdown: CancellationToken) {
        let mode = if self.livemode { "live" } else { "test" };
        self.run_with_instance(mode.to_owned(), shutdown).await;
    }

    /// Runs one named delivery worker until shutdown: claims due deliveries whenever slots are
    /// free, again at once after a claim that found some or a delivery that completed, else every
    /// `poll_interval`. Claims and deliveries in flight are polled together, so neither waits on
    /// the other. Deliveries in flight at shutdown are abandoned; their leases expire and they are
    /// retried.
    pub async fn run_with_instance(&self, instance: String, shutdown: CancellationToken) {
        let monitor = crate::observability::CronMonitor::outbox(&instance);
        let mut in_flight = FuturesUnordered::new();
        let mut busy = HashMap::<Uuid, usize>::new();
        let mut cooldowns = HashMap::<Uuid, Cooldown>::new();
        let claiming = Fuse::terminated();
        tokio::pin!(claiming);
        let mut next_claim = Instant::now();
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let free = self.config.max_in_flight.saturating_sub(in_flight.len());
            if claiming.is_terminated() && free > 0 && Instant::now() >= next_claim {
                let now = Instant::now();
                let cooling = cooldowns
                    .iter()
                    .filter(|(_, cooldown)| cooldown.until > now)
                    .map(|(id, _)| *id)
                    .collect();
                let probing = cooldowns.keys().copied().collect();
                claiming.set(self.claim(busy.clone(), cooling, probing, free).fuse());
            }
            let idle = claiming.is_terminated() && free > 0;
            tokio::select! {
                Some((endpoint_id, result)) = in_flight.next(), if !in_flight.is_empty() => {
                    release(&mut busy, endpoint_id);
                    match result {
                        Ok(Reach::Reached) => {
                            cooldowns.remove(&endpoint_id);
                        }
                        Ok(Reach::Failed { retry_after }) => {
                            let failures = cooldowns
                                .get(&endpoint_id)
                                .map_or(0, |cooldown| cooldown.failures);
                            let delay = retry_delay(
                                i32::try_from(failures).unwrap_or(i32::MAX),
                                self.entropy.as_ref(),
                            )
                            .max(retry_after.unwrap_or_default());
                            cooldowns.insert(endpoint_id, Cooldown {
                                failures: failures.saturating_add(1),
                                until: Instant::now() + delay,
                            });
                        }
                        Ok(Reach::Unknown) => {}
                        Err(error) => tracing::error!(%error, "outbox delivery failed"),
                    }
                    next_claim = Instant::now();
                }
                claimed = &mut claiming, if !claiming.is_terminated() => {
                    // Poll success proves liveness, not merchant fulfilment.
                    monitor.check_in(claimed.is_ok());
                    match claimed {
                        Ok(claimed) if !claimed.is_empty() => {
                            for event in claimed {
                                *busy.entry(event.endpoint_id).or_default() += 1;
                                in_flight.push(self.attempt(event));
                            }
                        }
                        Ok(_) => next_claim = Instant::now() + self.config.poll_interval,
                        Err(error) => {
                            tracing::error!(%error, "outbox delivery claim failed");
                            next_claim = Instant::now() + self.config.poll_interval;
                        }
                    }
                },
                () = sleep_until(next_claim), if idle => {}
                () = shutdown.cancelled() => return,
            }
        }
    }

    /// Claims the due deliveries that fit one pass and attempts each once, concurrently; returns
    /// how many were claimed.
    pub async fn run_once(&self) -> Result<usize, DeliveryError> {
        let claimed = self
            .claim(
                HashMap::new(),
                Vec::new(),
                HashSet::new(),
                self.config.max_in_flight,
            )
            .await?;
        let count = claimed.len();
        let mut attempts: FuturesUnordered<_> = claimed
            .into_iter()
            .map(|event| self.attempt(event))
            .collect();
        let mut first_error = None;
        while let Some((_, result)) = attempts.next().await {
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(count), Err)
    }

    /// Reserves up to `free` due deliveries, handing the slots out round-robin across endpoints
    /// in order of their oldest due delivery, each endpoint up to `endpoint_concurrency` including
    /// its deliveries already in flight (`busy`), a `probing` endpoint up to one, and a `cooling`
    /// endpoint none.
    async fn claim(
        &self,
        busy: HashMap<Uuid, usize>,
        cooling: Vec<Uuid>,
        probing: HashSet<Uuid>,
        free: usize,
    ) -> Result<Vec<ClaimedEvent>, sqlx::Error> {
        let endpoints: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT delivery.endpoint_id
            FROM webhook_deliveries AS delivery
            JOIN webhook_endpoints AS endpoint ON endpoint.id = delivery.endpoint_id
            WHERE delivery.delivered_at IS NULL
              AND delivery.failed_at IS NULL
              AND delivery.next_attempt_at <= now()
              AND endpoint.livemode = $1
              AND ((endpoint.status = 'enabled' AND endpoint.deleted_at IS NULL)
                   OR delivery.url IS NOT NULL)
              AND NOT (delivery.endpoint_id = ANY($3))
            GROUP BY delivery.endpoint_id
            ORDER BY min(delivery.next_attempt_at), delivery.endpoint_id
            LIMIT $2
            "#,
        )
        .bind(self.livemode)
        .bind(MAX_ENDPOINTS_PER_PASS)
        .bind(&cooling)
        .fetch_all(&self.pool)
        .await?;
        let concurrency = self.config.endpoint_concurrency;
        let capacity = |id: &Uuid| if probing.contains(id) { 1 } else { concurrency };
        let shares = round_robin(&endpoints, &busy, capacity, free);
        let mut claimed = Vec::new();
        for (endpoint_id, share) in shares {
            claimed.extend(self.claim_endpoint(endpoint_id, share).await?);
        }
        Ok(claimed)
    }

    /// Reserves up to `limit` due deliveries of one endpoint, its own notices first.
    async fn claim_endpoint(
        &self,
        endpoint_id: Uuid,
        limit: usize,
    ) -> Result<Vec<ClaimedEvent>, sqlx::Error> {
        let lease_seconds = i32::try_from(self.config.claim_lease.as_secs()).unwrap_or(i32::MAX);
        let rows = sqlx::query(
            r#"
            WITH picked AS (
                SELECT delivery.event_id
                FROM webhook_deliveries AS delivery
                JOIN webhook_endpoints AS endpoint ON endpoint.id = delivery.endpoint_id
                WHERE delivery.endpoint_id = $1
                  AND delivery.delivered_at IS NULL
                  AND delivery.failed_at IS NULL
                  AND delivery.next_attempt_at <= now()
                  AND ((endpoint.status = 'enabled' AND endpoint.deleted_at IS NULL)
                       OR delivery.url IS NOT NULL)
                ORDER BY delivery.url IS NULL, delivery.next_attempt_at, delivery.event_id
                LIMIT $2
                FOR UPDATE OF delivery SKIP LOCKED
            )
            UPDATE webhook_deliveries AS delivery
            SET next_attempt_at = now() + make_interval(secs => $3)
            FROM picked, events AS event, webhook_endpoints AS endpoint, accounts AS account
            WHERE delivery.event_id = picked.event_id
              AND delivery.endpoint_id = $1
              AND event.id = delivery.event_id
              AND endpoint.id = delivery.endpoint_id
              AND account.id = event.account_id
            RETURNING delivery.event_id, delivery.endpoint_id,
                      COALESCE(delivery.url, endpoint.url) AS url,
                      delivery.url IS NOT NULL AS notice, event.account_id, account.public_id,
                      event.livemode, event.type, event.actor, event.request_id,
                      event.idempotency_key, event.data, event.object_type,
                      event.object_id, event.signing_key_version, delivery.attempts,
                      event.created, delivery.next_attempt_at
            "#,
        )
        .bind(endpoint_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .bind(lease_seconds)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let object_type: String = row.try_get("object_type")?;
                let request_id: Option<String> = row.try_get("request_id")?;
                let idempotency_key: Option<String> = row.try_get("idempotency_key")?;
                Ok(ClaimedEvent {
                    id: row.try_get("event_id")?,
                    endpoint_id: row.try_get("endpoint_id")?,
                    url: row.try_get("url")?,
                    notice: row.try_get("notice")?,
                    scope: Scope::new(row.try_get("account_id")?, row.try_get("livemode")?),
                    account: row.try_get("public_id")?,
                    event_type: row.try_get("type")?,
                    actor: row.try_get("actor")?,
                    request: request_id.map(|id| RequestRef {
                        id,
                        idempotency_key,
                    }),
                    data: row.try_get("data")?,
                    object: EventObject::from_parts(&object_type, row.try_get("object_id")?),
                    signing_key_version: row.try_get("signing_key_version")?,
                    attempts: row.try_get("attempts")?,
                    created_at: row.try_get("created")?,
                    claim_until: row.try_get("next_attempt_at")?,
                })
            })
            .collect()
    }

    /// Attempts one claimed delivery and records its outcome; returns its endpoint.
    async fn attempt(&self, event: ClaimedEvent) -> (Uuid, Result<Reach, DeliveryError>) {
        self.warn_if_old(&event);
        let span = crate::observability::outbox_delivery_span(
            event.id,
            &event.event_type,
            event.object.map(EventObject::public_id),
            event.attempts,
        );
        let result = async {
            let outcome = self.send(&event).await?;
            let reach = reach(event.notice, &outcome);
            self.record(&event, outcome).await.map(|()| reach)
        }
        .instrument(span)
        .await;
        (event.endpoint_id, result)
    }

    fn warn_if_old(&self, event: &ClaimedEvent) {
        let Ok(threshold) = chrono::Duration::from_std(self.config.age_alert_threshold) else {
            return;
        };
        let age = Utc::now().signed_duration_since(event.created_at);
        if age > threshold {
            tracing::warn!(
                tags.alert = "TopupOutboxBacklog",
                tags.component = if self.livemode { "live" } else { "test" },
                event_id = %crate::ids::format(crate::ids::EVENT, event.id),
                event_type = event.event_type,
                age_seconds = age.num_seconds(),
                threshold_seconds = threshold.num_seconds(),
                "outbox event exceeded the delivery age threshold"
            );
        }
    }

    /// Signs and sends the recorded event; no database connection is held during the request.
    async fn send(&self, event: &ClaimedEvent) -> Result<Outcome, DeliveryError> {
        let (webhook_id, body) = match Self::event_body(event) {
            Ok(rendered) => rendered,
            Err(error) => return Ok(Outcome::internal(error)),
        };
        let keys = {
            let mut connection = self.pool.acquire().await?;
            crate::webhook_keys::active(&mut connection, event.scope).await?
        };
        let keys = keys.map(|keys| match event.signing_key_version {
            Some(version) => keys.with_version(version),
            None => keys,
        });
        let Some(keys) = keys.and_then(|keys| keys.ids()) else {
            return Ok(Outcome::internal("signing_failed"));
        };
        let Ok(signed) = SignedWebhook::new(
            self.signer.as_ref(),
            &keys,
            &webhook_id,
            Utc::now().timestamp(),
            &body,
        )
        .await
        else {
            return Ok(Outcome::internal("signing_failed"));
        };

        let response = self
            .client
            .post(&event.url)
            .header("content-type", "application/json")
            .header("webhook-id", &signed.id)
            .header("webhook-timestamp", &signed.timestamp)
            .header("webhook-signature", &signed.signature)
            .body(body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return Ok(request_failure(&error, self.config.proxy.is_some())),
        };
        let status = response.status();
        // Smokescreen v0.1.0 marks its own plain HTTP responses with this header and strips
        // it upstream. HTTPS responses are inside a tunnel and belong to the merchant.
        // Proxy 407 policy denials and 502/504 upstream failures remain endpoint failures.
        if self.config.proxy.is_some()
            && event.url.starts_with("http://")
            && status == StatusCode::INTERNAL_SERVER_ERROR
            && response.headers().contains_key("x-smokescreen-error")
        {
            return Ok(Outcome::internal("proxy_internal_failed"));
        }
        let retry_after = retry_after(status, response.headers(), Utc::now());
        let (body, body_error) =
            read_response_body(response, self.config.response_body_limit).await;
        Ok(if status.is_success() {
            Outcome::Delivered(response_value(Some(status.as_u16()), body, body_error))
        } else {
            Outcome::Failed {
                status: Some(status.as_u16()),
                body,
                error: body_error.unwrap_or("non_2xx_status"),
                endpoint_fault: true,
                retry_after,
            }
        })
    }

    /// Returns the `webhook-id` and body. The event's `data` was rendered when the event was
    /// recorded, so every endpoint, retry, and resend sends it unchanged.
    fn event_body(event: &ClaimedEvent) -> Result<(String, Vec<u8>), &'static str> {
        let data = event.data.clone();
        let id = webhook_id(event.id);
        let envelope = Event {
            id: id.clone(),
            object: "event",
            account: event.account.clone(),
            livemode: event.scope.livemode(),
            event_type: event.event_type.clone(),
            created: event.created_at.timestamp(),
            actor: event.actor.clone(),
            request: event.request.clone(),
            data,
        };
        serde_json::to_vec(&envelope)
            .map(|body| (id, body))
            .map_err(|_| "envelope_serialization")
    }

    /// Records the outcome under the claim's lease. A failure is retried with backoff, forever; a
    /// `410 Gone` stops the delivery and disables its endpoint, except for an endpoint's own notice
    /// sent to its previous URL, which only stops.
    async fn record(&self, event: &ClaimedEvent, outcome: Outcome) -> Result<(), DeliveryError> {
        match &outcome {
            Outcome::Delivered(response) => {
                let status = response.get("status").and_then(Value::as_u64);
                record_attempt(&self.pool, event, status).await?;
            }
            Outcome::Failed {
                status,
                endpoint_fault: true,
                ..
            } => record_attempt(&self.pool, event, status.map(u64::from)).await?,
            Outcome::Failed { .. } => {}
        }
        let (status, body, error, endpoint_fault, retry_after) = match outcome {
            Outcome::Delivered(response) => {
                mark_delivered(&self.pool, event, &response).await?;
                return Ok(());
            }
            Outcome::Failed {
                status,
                body,
                error,
                endpoint_fault,
                retry_after,
            } => (status, body, error, endpoint_fault, retry_after),
        };
        let response = response_value(status, body, Some(error));
        let gone = endpoint_fault && status == Some(410);
        if !gone {
            // A receiver's `Retry-After` defers the retry, never hastens it past the backoff.
            let delay = retry_delay(event.attempts, self.entropy.as_ref())
                .max(retry_after.unwrap_or_default());
            schedule_retry(&self.pool, event, &response, delay).await?;
            return Ok(());
        }
        let stopped = stop(&self.pool, event, &response).await?;
        if stopped && !event.notice {
            let mut connection = self.pool.acquire().await?;
            webhook_endpoints::disable_gone(&mut connection, event.endpoint_id).await?;
        }
        Ok(())
    }
}

/// Hands out `free` slots one at a time to each endpoint in turn, in `endpoints`' order, each up
/// to its `capacity` including its deliveries in flight; returns each endpoint's share.
fn round_robin(
    endpoints: &[Uuid],
    busy: &HashMap<Uuid, usize>,
    capacity: impl Fn(&Uuid) -> usize,
    free: usize,
) -> Vec<(Uuid, usize)> {
    let mut shares: Vec<(Uuid, usize)> = endpoints.iter().map(|id| (*id, 0)).collect();
    let mut remaining = free;
    loop {
        let mut granted = false;
        for (id, share) in &mut shares {
            if remaining == 0 {
                break;
            }
            let in_flight = busy.get(id).copied().unwrap_or(0);
            if in_flight.saturating_add(*share) < capacity(id) {
                *share = share.saturating_add(1);
                remaining = remaining.saturating_sub(1);
                granted = true;
            }
        }
        if !granted || remaining == 0 {
            break;
        }
    }
    shares.retain(|(_, share)| *share > 0);
    shares
}

fn release(busy: &mut HashMap<Uuid, usize>, endpoint_id: Uuid) {
    if let Some(count) = busy.get_mut(&endpoint_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            busy.remove(&endpoint_id);
        }
    }
}

fn validate_config(config: &DeliveryConfig) -> Result<(), DeliveryError> {
    if config.max_in_flight == 0 || config.max_in_flight > 1024 {
        return Err(DeliveryError::InvalidConfig(
            "max_in_flight must be between 1 and 1024",
        ));
    }
    if config.endpoint_concurrency == 0 || config.endpoint_concurrency > config.max_in_flight {
        return Err(DeliveryError::InvalidConfig(
            "endpoint_concurrency must be between 1 and max_in_flight",
        ));
    }
    if config.request_timeout.is_zero() {
        return Err(DeliveryError::InvalidConfig(
            "request_timeout must be positive",
        ));
    }
    if config.claim_lease <= config.request_timeout {
        return Err(DeliveryError::InvalidConfig(
            "claim_lease must exceed the request timeout",
        ));
    }
    if config.claim_lease.as_secs() > MAX_POSTGRES_INTERVAL_SECONDS {
        return Err(DeliveryError::InvalidConfig("claim_lease is too large"));
    }
    Ok(())
}

/// Records an attempt that reached the network on the endpoint, its delivery health; a notice to
/// a former URL is not an attempt at the endpoint's current one.
async fn record_attempt(
    pool: &PgPool,
    event: &ClaimedEvent,
    status: Option<u64>,
) -> Result<(), sqlx::Error> {
    if event.notice {
        return Ok(());
    }
    sqlx::query(
        "UPDATE webhook_endpoints SET last_attempt_at = now(), last_attempt_status = $2 \
         WHERE id = $1",
    )
    .bind(event.endpoint_id)
    .bind(status.and_then(|status| i32::try_from(status).ok()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks the delivery delivered. A success after the delivery was stopped (its endpoint disabled
/// while the request was in flight) is still recorded as delivered.
async fn mark_delivered(
    pool: &PgPool,
    event: &ClaimedEvent,
    response: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE webhook_deliveries
        SET delivered_at = now(), failed_at = NULL, response = $4
        WHERE event_id = $1 AND endpoint_id = $2 AND delivered_at IS NULL
          AND next_attempt_at = $3
        "#,
    )
    .bind(event.id)
    .bind(event.endpoint_id)
    .bind(event.claim_until)
    .bind(response)
    .execute(pool)
    .await?;
    Ok(())
}

async fn schedule_retry(
    pool: &PgPool,
    event: &ClaimedEvent,
    response: &Value,
    delay: Duration,
) -> Result<(), sqlx::Error> {
    let delay_seconds = i32::try_from(delay.as_secs()).unwrap_or(i32::MAX);
    sqlx::query(
        r#"
        UPDATE webhook_deliveries
        SET attempts = CASE WHEN attempts < 2147483647 THEN attempts + 1 ELSE attempts END,
            next_attempt_at = now() + make_interval(secs => $4),
            response = $5
        WHERE event_id = $1 AND endpoint_id = $2 AND delivered_at IS NULL AND failed_at IS NULL
          AND next_attempt_at = $3
        "#,
    )
    .bind(event.id)
    .bind(event.endpoint_id)
    .bind(event.claim_until)
    .bind(delay_seconds)
    .bind(response)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stops the delivery after its last failed attempt; returns whether this call stopped it.
async fn stop(pool: &PgPool, event: &ClaimedEvent, response: &Value) -> Result<bool, sqlx::Error> {
    let stopped = sqlx::query(
        r#"
        UPDATE webhook_deliveries
        SET attempts = CASE WHEN attempts < 2147483647 THEN attempts + 1 ELSE attempts END,
            failed_at = now(),
            response = $4
        WHERE event_id = $1 AND endpoint_id = $2 AND delivered_at IS NULL AND failed_at IS NULL
          AND next_attempt_at = $3
        "#,
    )
    .bind(event.id)
    .bind(event.endpoint_id)
    .bind(event.claim_until)
    .bind(response)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(stopped > 0)
}

fn response_value(status: Option<u16>, body: Option<String>, error: Option<&str>) -> Value {
    json!({
        "status": status,
        "body": body,
        "error": error,
    })
}

async fn read_response_body(
    mut response: reqwest::Response,
    limit: usize,
) -> (Option<String>, Option<&'static str>) {
    let mut retained = Vec::with_capacity(limit.min(4 * 1024));
    while retained.len() < limit {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => return (None, Some("response_body_read")),
        };
        let remaining = limit.saturating_sub(retained.len());
        let take = remaining.min(chunk.len());
        retained.extend_from_slice(&chunk[..take]);
    }
    (Some(String::from_utf8_lossy(&retained).into_owned()), None)
}

fn request_failure(error: &reqwest::Error, proxy: bool) -> Outcome {
    if proxy && error.is_connect() {
        return Outcome::internal("proxy_connect_failed");
    }
    Outcome::Failed {
        status: None,
        body: None,
        error: request_error_code(error),
        endpoint_fault: true,
        retry_after: None,
    }
}

fn request_error_code(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "request_timeout"
    } else if error.is_connect() {
        "connection_failed"
    } else if error.is_request() {
        "request_failed"
    } else if error.is_body() {
        "request_body"
    } else if error.is_builder() {
        "request_builder"
    } else if error.is_redirect() {
        "redirect_rejected"
    } else {
        "transport_error"
    }
}

/// What an attempt says about its endpoint, which the worker's cooldowns track. A notice goes to
/// the URL the endpoint had before a change: whatever it meets there says nothing about the
/// endpoint as it is, so it neither cools the endpoint nor clears it. Otherwise a former URL that
/// was taken down would hold the endpoint to probes that always pick the failing notice first.
fn reach(notice: bool, outcome: &Outcome) -> Reach {
    match outcome {
        _ if notice => Reach::Unknown,
        Outcome::Delivered(_) => Reach::Reached,
        Outcome::Failed {
            endpoint_fault: true,
            retry_after,
            ..
        } => Reach::Failed {
            retry_after: *retry_after,
        },
        Outcome::Failed { .. } => Reach::Unknown,
    }
}

/// The wait a `429` or `503` asks for in `Retry-After` (Standard Webhooks; RFC 9110 §10.2.3),
/// delay-seconds or an HTTP date after `now` (IMF-fixdate, RFC 850, or asctime), at most
/// [`MAX_RETRY_AFTER`]: a value too large to represent is the maximum, a past date no wait. A
/// malformed value or any other status asks for nothing, and the backoff alone applies.
fn retry_after(status: StatusCode, headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let wait = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse().map_or(MAX_RETRY_AFTER, Duration::from_secs)
    } else {
        let date = DateTime::<Utc>::from(httpdate::parse_http_date(value).ok()?);
        date.signed_duration_since(now)
            .to_std()
            .unwrap_or(Duration::ZERO)
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

fn retry_delay(attempts: i32, entropy: &dyn JitterSource) -> Duration {
    let attempt = u32::try_from(attempts).unwrap_or(u32::MAX);
    backoff(attempt, entropy.next_u64())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn unreachable_proxy_alerts_without_penalizing_endpoint() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(
                crate::observability::log_subscriber(std::io::sink),
                || {
                    runtime.block_on(async {
                        let client = Client::builder()
                            .proxy(Proxy::all(format!("http://{address}")).unwrap())
                            .timeout(Duration::from_secs(2))
                            .build()
                            .unwrap();
                        let error = client
                            .post("https://merchant.invalid/webhook")
                            .send()
                            .await
                            .unwrap_err();
                        assert!(matches!(
                            request_failure(&error, true),
                            Outcome::Failed {
                                endpoint_fault: false,
                                ..
                            }
                        ));
                        assert!(matches!(
                            request_failure(&error, false),
                            Outcome::Failed {
                                endpoint_fault: true,
                                ..
                            }
                        ));
                    });
                },
            );
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tags["alert"], "TopupOutboxInternalFailure");
        assert_eq!(events[0].tags["component"], "proxy_connect_failed");
    }

    struct SequenceEntropy {
        values: Mutex<VecDeque<u64>>,
    }

    impl SequenceEntropy {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self {
                values: Mutex::new(values.into_iter().collect()),
            }
        }
    }

    impl JitterSource for SequenceEntropy {
        fn next_u64(&self) -> u64 {
            self.values
                .lock()
                .expect("entropy lock is not poisoned")
                .pop_front()
                .expect("test supplied enough entropy")
        }
    }

    #[test]
    fn a_notice_neither_cools_nor_clears_its_endpoint() {
        let failed = |endpoint_fault| Outcome::Failed {
            status: Some(404),
            body: None,
            error: "http_status",
            endpoint_fault,
            retry_after: None,
        };
        assert_eq!(
            reach(false, &Outcome::Delivered(Value::Null)),
            Reach::Reached
        );
        assert_eq!(
            reach(false, &failed(true)),
            Reach::Failed { retry_after: None }
        );
        assert_eq!(reach(false, &failed(false)), Reach::Unknown);
        assert_eq!(
            reach(true, &Outcome::Delivered(Value::Null)),
            Reach::Unknown
        );
        assert_eq!(reach(true, &failed(true)), Reach::Unknown);
    }

    #[test]
    fn retry_after_is_read_from_429_and_503_and_bounded() {
        let headers = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, value.parse().expect("header value"));
            headers
        };
        let now = DateTime::parse_from_rfc3339("2026-10-21T07:28:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let limited = StatusCode::TOO_MANY_REQUESTS;
        let read = |value: &str| retry_after(limited, &headers(value), now);
        assert_eq!(read("120"), Some(Duration::from_secs(120)));
        assert_eq!(
            retry_after(StatusCode::SERVICE_UNAVAILABLE, &headers(" 5 "), now),
            Some(Duration::from_secs(5))
        );
        // Too long, or too large to represent, is the ceiling.
        assert_eq!(read("86400"), Some(MAX_RETRY_AFTER));
        assert_eq!(read(&"9".repeat(40)), Some(MAX_RETRY_AFTER));
        // HTTP dates in each format: two minutes on, and one in the past.
        let two_minutes = Some(Duration::from_secs(120));
        assert_eq!(read("Wed, 21 Oct 2026 07:30:00 GMT"), two_minutes);
        assert_eq!(read("Wednesday, 21-Oct-26 07:30:00 GMT"), two_minutes);
        assert_eq!(read("Wed Oct 21 07:30:00 2026"), two_minutes);
        assert_eq!(read("Wed, 21 Oct 2026 07:00:00 GMT"), Some(Duration::ZERO));
        assert_eq!(read("Thu, 21 Oct 2027 07:28:00 GMT"), Some(MAX_RETRY_AFTER));
        for malformed in ["-1", "1.5", "soon", ""] {
            assert_eq!(read(malformed), None, "{malformed}");
        }
        assert_eq!(retry_after(limited, &HeaderMap::new(), now), None);
        assert_eq!(
            retry_after(StatusCode::BAD_GATEWAY, &headers("120"), now),
            None
        );
    }

    #[test]
    fn retry_delay_uses_fresh_entropy_and_caps_the_exponential_ceiling() {
        let entropy = SequenceEntropy::new([0, u64::MAX, 0]);

        assert_eq!(retry_delay(0, &entropy), Duration::from_secs(30));
        assert_eq!(retry_delay(0, &entropy), Duration::ZERO);
        assert_eq!(
            retry_delay(i32::MAX, &entropy),
            Duration::from_secs(60 * 60)
        );
    }

    #[test]
    fn slots_go_round_robin_up_to_each_endpoints_concurrency() {
        let [a, b, c] = [1_u128, 2, 3].map(Uuid::from_u128);
        // Three endpoints share five slots one at a time, in order.
        assert_eq!(
            round_robin(&[a, b, c], &HashMap::new(), |_| 4, 5),
            vec![(a, 2), (b, 2), (c, 1)]
        );
        // A busy endpoint gets only what its concurrency leaves; the rest go to the others.
        let busy = HashMap::from([(a, 4), (b, 3)]);
        assert_eq!(
            round_robin(&[a, b, c], &busy, |_| 4, 10),
            vec![(b, 1), (c, 4)]
        );
        assert!(round_robin(&[a], &HashMap::from([(a, 4)]), |_| 4, 10).is_empty());
        // A probing endpoint, one whose last attempt failed, gets one slot at a time.
        let probing = |id: &Uuid| if *id == a { 1 } else { 4 };
        assert_eq!(
            round_robin(&[a, b], &HashMap::new(), probing, 10),
            vec![(a, 1), (b, 4)]
        );
        assert!(round_robin(&[a], &HashMap::from([(a, 1)]), probing, 10).is_empty());
    }
}
