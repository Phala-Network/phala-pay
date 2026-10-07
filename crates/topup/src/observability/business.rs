//! Independent probes: retries are not evidence of business progress.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const STALL_SECONDS: i64 = 15 * 60;
const BACKLOG_COUNT: i64 = 1000;
const BACKLOG_AGE: i64 = 24 * 60 * 60;
const PROGRESS_SECONDS: i64 = 60 * 60;
const REMINDER_SECONDS: i64 = 60 * 60;

/// Emit a Sentry alert from an operator probe, including disk capacity checks.
/// Use stable names and low-cardinality component/severity values. Numeric measurements
/// are fields, not grouping tags. Never pass secrets or user input as dimensions.
pub fn emit_alert(alert: &str, component: &str, severity: &str, observed: i64, threshold: i64) {
    tracing::warn!(
        tags.alert = alert,
        tags.component = component,
        tags.severity = severity,
        observed,
        threshold,
        "business health threshold exceeded"
    );
}

/// One state entry per alert/component, independent of how often a probe runs.
/// The supplied clock makes transitions and reminders deterministic in tests.
#[derive(Default)]
struct AlertState {
    active: BTreeMap<(&'static str, String), (&'static str, i64)>,
}

impl AlertState {
    fn observe(
        &mut self,
        now: i64,
        alert: &'static str,
        component: &str,
        severity: Option<&'static str>,
        observed: i64,
        threshold: i64,
    ) {
        let key = (alert, component.to_owned());
        let Some(severity) = severity else {
            if self.active.remove(&key).is_some() {
                // No alert tag, and INFO stays a breadcrumb rather than a Sentry event.
                tracing::info!(alert, component, "business health alert resolved");
            }
            return;
        };
        if self.active.get(&key).is_some_and(|(previous, sent)| {
            *previous == severity && now.saturating_sub(*sent) < REMINDER_SECONDS
        }) {
            return;
        }
        self.active.insert(key, (severity, now));
        emit_alert(alert, component, severity, observed, threshold);
    }
}

fn age(now: DateTime<Utc>, at: Option<DateTime<Utc>>) -> i64 {
    at.map_or(i64::MAX, |at| {
        now.signed_duration_since(at).num_seconds().max(0)
    })
}

fn report_age(
    state: &mut AlertState,
    now: DateTime<Utc>,
    alert: &'static str,
    component: &str,
    severity: &'static str,
    age: i64,
    threshold: i64,
) {
    state.observe(
        now.timestamp(),
        alert,
        component,
        (age >= threshold).then_some(severity),
        age,
        threshold,
    );
}

#[derive(sqlx::FromRow)]
struct OutboxHealth {
    pending_count: i64,
    pending_endpoints: i64,
    oldest_pending: Option<DateTime<Utc>>,
    oldest_due: Option<DateTime<Utc>>,
}

fn report_outbox(
    state: &mut AlertState,
    now: DateTime<Utc>,
    component: &str,
    health: &OutboxHealth,
) {
    let oldest_age = age(now, health.oldest_pending);
    let backlog = health.pending_endpoints >= 2
        && (health.pending_count >= BACKLOG_COUNT || oldest_age >= BACKLOG_AGE);
    let (observed, threshold) = if health.pending_count >= BACKLOG_COUNT {
        (health.pending_count, BACKLOG_COUNT)
    } else {
        (oldest_age, BACKLOG_AGE)
    };
    state.observe(
        now.timestamp(),
        "TopupOutboxBacklog",
        component,
        backlog.then_some("warning"),
        observed,
        threshold,
    );
    let overdue_age = health.oldest_due.map_or(0, |at| age(now, Some(at)));
    report_age(
        state,
        now,
        "TopupOutboxStalled",
        component,
        "critical",
        overdue_age,
        STALL_SECONDS,
    );
}

// last_attempt_at is written only for network attempts, never internal signer/proxy failures.
// A non-2xx or absent status with a recorded attempt is a known merchant delivery failure.
// Excluding those endpoints also excludes their unattempted siblings during endpoint cooldown.
const OUTBOX_HEALTH_SQL: &str = "SELECT count(*) AS pending_count, \
    count(DISTINCT e.id) AS pending_endpoints, min(ev.created) AS oldest_pending, \
    min(d.next_attempt_at) FILTER (WHERE d.next_attempt_at <= $2) AS oldest_due \
    FROM webhook_deliveries d JOIN webhook_endpoints e ON e.id = d.endpoint_id \
    JOIN events ev ON ev.id = d.event_id \
    WHERE e.livemode = $1 AND d.delivered_at IS NULL AND d.failed_at IS NULL \
    AND ((e.status = 'enabled' AND e.deleted_at IS NULL) OR d.url IS NOT NULL) \
    AND (e.last_attempt_at IS NULL OR e.last_attempt_status BETWEEN 200 AND 299)";

const ADDRESS_COUNTS_SQL: &str =
    "SELECT chain_id, count(*)::bigint FROM addresses GROUP BY chain_id";

fn report_address_capacity(state: &mut AlertState, now: DateTime<Utc>, chain: i64, count: i64) {
    let cap = crate::db::chain_reads::ISSUED_ADDRESS_CAP;
    let (severity, threshold) = if count >= cap * 90 / 100 {
        (Some("critical"), cap * 90 / 100)
    } else if count >= cap * 70 / 100 {
        (Some("warning"), cap * 70 / 100)
    } else {
        (None, cap * 70 / 100)
    };
    state.observe(
        now.timestamp(),
        "TopupAddressCapacity",
        &format!("chain:{chain}"),
        severity,
        count,
        threshold,
    );
}

async fn probe_database(pool: &PgPool, state: &mut AlertState) -> Result<(), sqlx::Error> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(pool).await?;
    let counts: Vec<(i64, i64)> = sqlx::query_as(ADDRESS_COUNTS_SQL).fetch_all(pool).await?;
    for (chain, count) in counts {
        report_address_capacity(state, now, chain, count);
    }
    let heartbeat = sqlx::query_scalar("SELECT max(recorded_at) FROM heartbeat")
        .fetch_one(pool)
        .await?;
    report_age(
        state,
        now,
        "TopupHeartbeatStale",
        "heartbeat",
        "critical",
        age(now, heartbeat),
        180,
    );
    for mode in [false, true] {
        let component = if mode { "live" } else { "test" };
        let health: OutboxHealth = sqlx::query_as(OUTBOX_HEALTH_SQL)
            .bind(mode)
            .bind(now)
            .fetch_one(pool)
            .await?;
        report_outbox(state, now, component, &health);
    }
    let treasury: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT min(effective_at) FROM treasuries WHERE applied_at IS NULL AND canceled_at IS NULL AND effective_at <= now()")
        .fetch_one(pool).await?;
    // An empty queue is recovery, not a missing progress timestamp.
    report_age(
        state,
        now,
        "TopupTreasuryProgressAge",
        "treasury",
        "warning",
        treasury.map_or(0, |at| age(now, Some(at))),
        PROGRESS_SECONDS,
    );
    let refund: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT min(r.paid_at) FROM refunds r JOIN deposits d ON d.id = r.deposit_id \
         WHERE r.status = 'pending' AND r.tx_hash IS NOT NULL AND d.sanctions_hit_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    report_age(
        state,
        now,
        "TopupRefundProgressAge",
        "refund",
        "warning",
        refund.map_or(0, |at| age(now, Some(at))),
        PROGRESS_SECONDS,
    );
    Ok(())
}

fn certificate_expiry(der: &[u8]) -> Result<i64, &'static str> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|_| "invalid certificate")?;
    Ok(cert.validity().not_after.timestamp())
}

fn report_certificate(state: &mut AlertState, expires: i64, now: i64) {
    let remaining = expires.saturating_sub(now);
    let (severity, threshold) = if remaining <= 3 * 86400 {
        (Some("critical"), 3 * 86400)
    } else if remaining <= 14 * 86400 {
        (Some("warning"), 14 * 86400)
    } else {
        (None, 14 * 86400)
    };
    state.observe(
        now,
        "TopupCertificateExpiry",
        "ingress",
        severity,
        remaining,
        threshold,
    );
}

async fn probe_certificate(client: &reqwest::Client, origin: &str) -> Result<i64, &'static str> {
    let response = client
        .get(format!("{origin}/healthz"))
        .send()
        .await
        .map_err(|_| "TLS request failed")?;
    let der = response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|info| info.peer_certificate())
        .ok_or("missing peer certificate")?;
    certificate_expiry(der)
}

/// Probe persisted business state and the public ingress certificate once per minute.
/// Requests are bounded and cancellation stops probes. Local HTTP origins skip TLS.
/// Alert on entry/severity change, then at most hourly; recovery is an INFO log only.
pub async fn monitor_business(pool: PgPool, origin: String, cancellation: CancellationToken) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_info(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build();
    let mut state = AlertState::default();
    loop {
        let probes = async {
            let database_ok = matches!(
                tokio::time::timeout(Duration::from_secs(20), probe_database(&pool, &mut state))
                    .await,
                Ok(Ok(()))
            );
            state.observe(
                Utc::now().timestamp(),
                "TopupBusinessProbeFailed",
                "database",
                (!database_ok).then_some("critical"),
                1,
                0,
            );
            if origin.starts_with("https://") {
                let result = match &client {
                    Ok(client) => probe_certificate(client, &origin).await,
                    Err(_) => Err("client initialization failed"),
                };
                let now = Utc::now().timestamp();
                state.observe(
                    now,
                    "TopupCertificateProbeFailed",
                    "ingress",
                    result.is_err().then_some("critical"),
                    1,
                    0,
                );
                if let Ok(expires) = result {
                    report_certificate(&mut state, expires, now);
                }
                // Failed probes leave previous certificate/business conditions unknown, not resolved.
            }
        };
        tokio::select! { () = cancellation.cancelled() => return, () = probes => {} }
        tokio::select! { () = cancellation.cancelled() => return, () = tokio::time::sleep(Duration::from_secs(60)) => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hourly_reminders_recovery_and_reentry_use_the_supplied_clock() {
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut state = AlertState::default();
                for minute in 0..1440 {
                    state.observe(
                        minute * 60,
                        "TopupHeartbeatStale",
                        "heartbeat",
                        Some("critical"),
                        180,
                        180,
                    );
                }
                assert_eq!(state.active.len(), 1);
                state.observe(86400, "TopupHeartbeatStale", "heartbeat", None, 0, 180);
                assert!(state.active.is_empty());
                state.observe(
                    86401,
                    "TopupHeartbeatStale",
                    "heartbeat",
                    Some("critical"),
                    180,
                    180,
                );
            });
        });
        assert_eq!(
            events.len(),
            25,
            "24 hourly events and one new incident, no recovery event"
        );
    }

    #[tokio::test]
    async fn capacity_counts_all_history_and_alerts_at_seventy_and_ninety_percent()
    -> Result<(), sqlx::Error> {
        use sqlx::Connection as _;
        let Ok(url) = std::env::var("DATABASE_URL") else {
            assert_ne!(
                std::env::var("CI").as_deref(),
                Ok("true"),
                "CI requires DATABASE_URL"
            );
            return Ok(());
        };
        let mut conn = sqlx::PgConnection::connect(&url).await?;
        sqlx::raw_sql("CREATE TEMP TABLE addresses (chain_id bigint, status text); INSERT INTO addresses SELECT 1,'closed' FROM generate_series(1,699)").execute(&mut conn).await?;
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut state = AlertState::default();
                report_address_capacity(&mut state, now, 1, 699);
                report_address_capacity(&mut state, now, 1, 700);
                report_address_capacity(&mut state, now, 1, 899);
                report_address_capacity(&mut state, now, 1, 900);
                report_address_capacity(&mut state, now, 2, 700);
            });
        });
        assert_eq!(events.len(), 3, "70%, 90%, and a separate chain must alert");
        assert!(
            events
                .iter()
                .all(|e| e.tags.get("alert").map(String::as_str) == Some("TopupAddressCapacity"))
        );
        assert!(events.iter().all(|e| {
            e.tags
                .get("runbook")
                .is_some_and(|r| r.ends_with("address-capacity.md"))
        }));
        assert_eq!(
            events[0].tags.get("severity").map(String::as_str),
            Some("warning")
        );
        assert_eq!(
            events[1].tags.get("severity").map(String::as_str),
            Some("critical")
        );
        sqlx::query("INSERT INTO addresses SELECT 1,'retired' FROM generate_series(1,301)")
            .execute(&mut conn)
            .await?;
        let counts: Vec<(i64, i64)> = sqlx::query_as(ADDRESS_COUNTS_SQL)
            .fetch_all(&mut conn)
            .await?;
        assert_eq!(counts, vec![(1, 1000)]);
        let preupgrade = include_str!("../../../../deploy/check-address-capacity.sql");
        sqlx::raw_sql(preupgrade).execute(&mut conn).await?;
        sqlx::query("INSERT INTO addresses VALUES (1,'active')")
            .execute(&mut conn)
            .await?;
        let error = sqlx::raw_sql(preupgrade)
            .execute(&mut conn)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("address capacity exceeded"));
        sqlx::raw_sql("ROLLBACK").execute(&mut conn).await?;
        Ok(())
    }

    #[test]
    #[tracing_test::traced_test]
    fn recovery_is_logged_once_without_an_alert_tag() {
        let mut state = AlertState::default();
        state.observe(
            0,
            "TopupHeartbeatStale",
            "heartbeat",
            Some("critical"),
            180,
            180,
        );
        state.observe(1, "TopupHeartbeatStale", "heartbeat", None, 0, 180);
        state.observe(2, "TopupHeartbeatStale", "heartbeat", None, 0, 180);
        logs_assert(|lines: &[&str]| {
            let resolved: Vec<_> = lines
                .iter()
                .filter(|line| line.contains("business health alert resolved"))
                .collect();
            if resolved.len() == 1 && !resolved[0].contains("tags.alert") {
                Ok(())
            } else {
                Err(format!("expected one untagged recovery log: {resolved:?}"))
            }
        });
    }

    #[tokio::test]
    async fn merchant_http_failures_timeouts_and_cooldown_siblings_are_excluded()
    -> Result<(), sqlx::Error> {
        use sqlx::Connection as _;
        let Ok(url) = std::env::var("DATABASE_URL") else {
            assert_ne!(
                std::env::var("CI").as_deref(),
                Ok("true"),
                "CI requires DATABASE_URL"
            );
            return Ok(());
        };
        let mut connection = sqlx::PgConnection::connect(&url).await?;
        // Session-local fixtures execute the actual production query without touching business rows.
        sqlx::raw_sql("CREATE TEMP TABLE webhook_endpoints (id uuid, livemode boolean, status text,             deleted_at timestamptz, last_attempt_at timestamptz, last_attempt_status integer);             CREATE TEMP TABLE events (id uuid, created timestamptz);             CREATE TEMP TABLE webhook_deliveries (event_id uuid, endpoint_id uuid,             next_attempt_at timestamptz, delivered_at timestamptz, failed_at timestamptz, url text)")
            .execute(&mut connection).await?;
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        for number in [1, 2] {
            let id = uuid::Uuid::from_u128(number);
            sqlx::query(
                "INSERT INTO webhook_endpoints VALUES ($1, true, 'enabled', NULL, NULL, NULL)",
            )
            .bind(id)
            .execute(&mut connection)
            .await?;
            sqlx::query("INSERT INTO events VALUES ($1, $2)")
                .bind(id)
                .bind(now - chrono::Duration::days(1))
                .execute(&mut connection)
                .await?;
            sqlx::query("INSERT INTO webhook_deliveries VALUES ($1, $1, $2, NULL, NULL, NULL)")
                .bind(id)
                .bind(now - chrono::Duration::seconds(STALL_SECONDS))
                .execute(&mut connection)
                .await?;
        }
        // An untouched sibling waits behind endpoint 1's cooldown after its other delivery fails.
        sqlx::query("INSERT INTO events VALUES ($1, $2)")
            .bind(uuid::Uuid::from_u128(3))
            .bind(now - chrono::Duration::days(1))
            .execute(&mut connection)
            .await?;
        sqlx::query("INSERT INTO webhook_deliveries VALUES ($1, $2, $3, NULL, NULL, NULL)")
            .bind(uuid::Uuid::from_u128(3))
            .bind(uuid::Uuid::from_u128(1))
            .bind(now - chrono::Duration::seconds(STALL_SECONDS))
            .execute(&mut connection)
            .await?;
        let health: OutboxHealth = sqlx::query_as(OUTBOX_HEALTH_SQL)
            .bind(true)
            .bind(now)
            .fetch_one(&mut connection)
            .await?;
        assert_eq!(health.pending_count, 3);
        assert_eq!(health.pending_endpoints, 2);
        assert_eq!(age(now, health.oldest_due), STALL_SECONDS);
        // A failed last network attempt excludes the entire endpoint, including untouched queued siblings.
        for status in [Some(400_i32), Some(500), None] {
            sqlx::query("UPDATE webhook_endpoints SET last_attempt_at = $1, last_attempt_status = $2 WHERE id = $3")
                .bind(now).bind(status).bind(uuid::Uuid::from_u128(1)).execute(&mut connection).await?;
            let health: OutboxHealth = sqlx::query_as(OUTBOX_HEALTH_SQL)
                .bind(true)
                .bind(now)
                .fetch_one(&mut connection)
                .await?;
            assert_eq!(health.pending_endpoints, 1);
            assert_eq!(health.pending_count, 1);
        }
        sqlx::query(
            "UPDATE webhook_endpoints SET last_attempt_at = $1, last_attempt_status = NULL",
        )
        .bind(now)
        .execute(&mut connection)
        .await?;
        let health: OutboxHealth = sqlx::query_as(OUTBOX_HEALTH_SQL)
            .bind(true)
            .bind(now)
            .fetch_one(&mut connection)
            .await?;
        assert_eq!(
            health.pending_count, 0,
            "all receivers timing out cannot page for pipeline stalls"
        );
        assert!(health.oldest_due.is_none());
        sqlx::query("UPDATE webhook_endpoints SET last_attempt_status = 200 WHERE id = $1")
            .bind(uuid::Uuid::from_u128(1))
            .execute(&mut connection)
            .await?;
        let health: OutboxHealth = sqlx::query_as(OUTBOX_HEALTH_SQL)
            .bind(true)
            .bind(now)
            .fetch_one(&mut connection)
            .await?;
        assert_eq!(
            health.pending_count, 2,
            "a recovered receiver and its sibling re-enter pipeline monitoring"
        );
        assert_eq!(
            age(now, health.oldest_due),
            STALL_SECONDS,
            "recent successful delivery cannot mask overdue work"
        );
        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = $1")
            .bind(now + chrono::Duration::minutes(5))
            .execute(&mut connection)
            .await?;
        let health: OutboxHealth = sqlx::query_as(OUTBOX_HEALTH_SQL)
            .bind(true)
            .bind(now)
            .fetch_one(&mut connection)
            .await?;
        assert!(
            health.oldest_due.is_none(),
            "unexpired claim leases and future retries are not stalls"
        );
        connection.close().await?;
        Ok(())
    }

    #[test]
    fn components_and_severity_transitions_are_independent() {
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut state = AlertState::default();
                state.observe(0, "TopupOutboxBacklog", "live", Some("warning"), 1000, 1000);
                state.observe(1, "TopupOutboxBacklog", "test", Some("warning"), 1000, 1000);
                state.observe(2, "TopupOutboxBacklog", "live", Some("warning"), 1000, 1000);
                state.observe(
                    3,
                    "TopupOutboxBacklog",
                    "live",
                    Some("critical"),
                    1000,
                    1000,
                );
            });
        });
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn stalled_means_overdue_work_and_backlog_requires_multiple_endpoints() {
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut state = AlertState::default();
                let mut health = OutboxHealth {
                    pending_count: 1000,
                    pending_endpoints: 1,
                    oldest_pending: Some(now),
                    oldest_due: None,
                };
                // A single endpoint's old backlog does not alert. A future lease/backoff is not a stall.
                report_outbox(&mut state, now, "live", &health);
                health.pending_endpoints = 2;
                report_outbox(&mut state, now, "live", &health);
                let age_backlog = OutboxHealth {
                    pending_count: 2,
                    pending_endpoints: 2,
                    oldest_pending: Some(now - chrono::Duration::seconds(BACKLOG_AGE)),
                    oldest_due: None,
                };
                report_outbox(&mut state, now, "test", &age_backlog);
                health.oldest_due = Some(now - chrono::Duration::seconds(899));
                report_outbox(&mut state, now, "live", &health);
                report_outbox(
                    &mut state,
                    now + chrono::Duration::seconds(1),
                    "live",
                    &health,
                );
                health.pending_count = 0;
                health.pending_endpoints = 0;
                health.oldest_due = None;
                report_outbox(
                    &mut state,
                    now + chrono::Duration::seconds(2),
                    "live",
                    &health,
                );
                report_outbox(
                    &mut state,
                    now + chrono::Duration::seconds(2),
                    "test",
                    &health,
                );
                assert!(state.active.is_empty());
            });
        });
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].tags["alert"], "TopupOutboxBacklog");
        assert_eq!(events[1].tags["alert"], "TopupOutboxBacklog");
        assert_eq!(events[2].tags["alert"], "TopupOutboxStalled");
    }

    #[test]
    fn business_thresholds_emit_sentry_events_at_boundaries() {
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut state = AlertState::default();
                for (alert, threshold) in [
                    ("TopupHeartbeatStale", 180),
                    ("TopupTreasuryProgressAge", PROGRESS_SECONDS),
                    ("TopupRefundProgressAge", PROGRESS_SECONDS),
                ] {
                    report_age(
                        &mut state,
                        now,
                        alert,
                        "test",
                        "warning",
                        threshold - 1,
                        threshold,
                    );
                    report_age(
                        &mut state, now, alert, "test", "warning", threshold, threshold,
                    );
                    report_age(&mut state, now, alert, "test", "warning", 0, threshold);
                }
                assert!(state.active.is_empty());
            });
        });
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn certificate_expiry_probe_emits_warning_and_critical_events() {
        let expires = certificate_expiry(include_bytes!("test-certificate.der")).unwrap();
        assert!(certificate_expiry(b"invalid").is_err());
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut state = AlertState::default();
                report_certificate(&mut state, expires, expires - 15 * 86400);
                report_certificate(&mut state, expires, expires - 14 * 86400);
                // Escalation is immediate even before the next hourly reminder.
                report_certificate(&mut state, expires, expires - 3 * 86400);
                report_certificate(&mut state, expires + 30 * 86400, expires);
                assert!(state.active.is_empty());
            });
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].tags["severity"], "warning");
        assert_eq!(events[1].tags["severity"], "critical");
    }
}
