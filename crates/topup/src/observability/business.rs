//! Independent probes: retries are not evidence of business progress.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const STALL_SECONDS: i64 = 15 * 60;
const BACKLOG_COUNT: i64 = 1000;
const BACKLOG_AGE: i64 = 24 * 60 * 60;
const PROGRESS_SECONDS: i64 = 60 * 60;

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

#[derive(Default)]
struct DeliveryProgress {
    pending_since: Option<DateTime<Utc>>,
}

impl DeliveryProgress {
    // A new backlog/startup gets a grace period; retries never move it forward.
    fn stalled(&mut self, now: DateTime<Utc>, count: i64, last: Option<DateTime<Utc>>) -> bool {
        if count == 0 {
            self.pending_since = None;
            return false;
        }
        let since = *self.pending_since.get_or_insert(now);
        now.signed_duration_since(last.map_or(since, |last| last.max(since)))
            .num_seconds()
            >= STALL_SECONDS
    }
}

fn age(now: DateTime<Utc>, at: Option<DateTime<Utc>>) -> i64 {
    at.map_or(i64::MAX, |at| {
        now.signed_duration_since(at).num_seconds().max(0)
    })
}

fn report_age(alert: &str, component: &str, severity: &str, age: i64, threshold: i64) {
    if age >= threshold {
        emit_alert(alert, component, severity, age, threshold);
    }
}

fn report_outbox(
    progress: &mut DeliveryProgress,
    now: DateTime<Utc>,
    component: &str,
    count: i64,
    oldest: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
) {
    if count >= BACKLOG_COUNT || (count > 0 && age(now, oldest) >= BACKLOG_AGE) {
        tracing::warn!(
            tags.alert = "TopupOutboxBacklog",
            tags.component = component,
            pending_count = count,
            oldest_age_seconds = age(now, oldest),
            "outbox backlog count or age exceeded threshold"
        );
    }
    if progress.stalled(now, count, last) {
        emit_alert(
            "TopupOutboxStalled",
            component,
            "critical",
            age(now, last),
            STALL_SECONDS,
        );
    }
}

async fn probe_database(
    pool: &PgPool,
    progress: &mut [DeliveryProgress; 2],
) -> Result<(), sqlx::Error> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(pool).await?;
    let heartbeat = sqlx::query_scalar("SELECT max(recorded_at) FROM heartbeat")
        .fetch_one(pool)
        .await?;
    report_age(
        "TopupHeartbeatStale",
        "heartbeat",
        "critical",
        age(now, heartbeat),
        180,
    );
    for (index, mode) in [false, true].into_iter().enumerate() {
        let component = if mode { "live" } else { "test" };
        let (count, oldest, last): (i64, Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
            sqlx::query_as(
                "SELECT count(*) FILTER (WHERE d.delivered_at IS NULL AND d.failed_at IS NULL \
                 AND ((e.status = 'enabled' AND e.deleted_at IS NULL) OR d.url IS NOT NULL)), \
             min(ev.created) FILTER (WHERE d.delivered_at IS NULL AND d.failed_at IS NULL \
                 AND ((e.status = 'enabled' AND e.deleted_at IS NULL) OR d.url IS NOT NULL)), \
             max(d.delivered_at) FROM webhook_deliveries d \
             JOIN webhook_endpoints e ON e.id = d.endpoint_id JOIN events ev ON ev.id = d.event_id \
             WHERE e.livemode = $1",
            )
            .bind(mode)
            .fetch_one(pool)
            .await?;
        report_outbox(&mut progress[index], now, component, count, oldest, last);
    }
    let treasury: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT min(effective_at) FROM treasuries WHERE applied_at IS NULL AND canceled_at IS NULL AND effective_at <= now()")
        .fetch_one(pool).await?;
    if treasury.is_some() {
        report_age(
            "TopupTreasuryProgressAge",
            "treasury",
            "warning",
            age(now, treasury),
            PROGRESS_SECONDS,
        );
    }
    // paid_at is attachment time; updated_at also moves on retries.
    let refund: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT min(r.paid_at) FROM refunds r JOIN deposits d ON d.id = r.deposit_id \
         WHERE r.status = 'pending' AND r.tx_hash IS NOT NULL AND d.sanctions_hit_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    if refund.is_some() {
        report_age(
            "TopupRefundProgressAge",
            "refund",
            "warning",
            age(now, refund),
            PROGRESS_SECONDS,
        );
    }
    Ok(())
}

fn certificate_expiry(der: &[u8]) -> Result<i64, &'static str> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|_| "invalid certificate")?;
    Ok(cert.validity().not_after.timestamp())
}

fn report_certificate(expires: i64, now: i64) {
    let remaining = expires.saturating_sub(now);
    let (severity, threshold) = if remaining <= 3 * 86400 {
        ("critical", 3 * 86400)
    } else if remaining <= 14 * 86400 {
        ("warning", 14 * 86400)
    } else {
        return;
    };
    emit_alert(
        "TopupCertificateExpiry",
        "ingress",
        severity,
        remaining,
        threshold,
    );
}

async fn probe_certificate(client: &reqwest::Client, origin: &str) -> Result<(), &'static str> {
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
    report_certificate(certificate_expiry(der)?, Utc::now().timestamp());
    Ok(())
}

/// Probe persisted business state and the public ingress certificate once per minute.
/// Requests are bounded and cancellation stops probes. Local HTTP origins skip TLS.
pub async fn monitor_business(pool: PgPool, origin: String, cancellation: CancellationToken) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_info(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build();
    let mut progress = [DeliveryProgress::default(), DeliveryProgress::default()];
    loop {
        let probes = async {
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(20),
                    probe_database(&pool, &mut progress)
                )
                .await,
                Ok(Ok(()))
            ) {
                emit_alert("TopupBusinessProbeFailed", "database", "critical", 1, 0);
            }
            if origin.starts_with("https://") {
                let result = match &client {
                    Ok(client) => probe_certificate(client, &origin).await,
                    Err(_) => Err("client initialization failed"),
                };
                if result.is_err() {
                    emit_alert("TopupCertificateProbeFailed", "ingress", "critical", 1, 0);
                }
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
    fn retries_do_not_hide_stalls_and_success_or_idle_resets_clock() {
        let start = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let mut progress = DeliveryProgress::default();
        assert!(!progress.stalled(start, 1, None));
        assert!(!progress.stalled(start + chrono::Duration::seconds(899), 1, None));
        assert!(progress.stalled(start + chrono::Duration::seconds(900), 1, None));
        let success = start + chrono::Duration::seconds(901);
        assert!(!progress.stalled(success, 1, Some(success)));
        assert!(progress.stalled(success + chrono::Duration::seconds(900), 1, Some(success)));
        assert!(!progress.stalled(success, 0, None));
        assert!(!progress.stalled(success, 1, None));
    }

    #[test]
    fn business_thresholds_emit_sentry_events_at_boundaries() {
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                let mut progress = DeliveryProgress::default();
                report_outbox(&mut progress, now, "live", 999, Some(now), None);
                report_outbox(&mut progress, now, "live", 1000, Some(now), None);
                report_outbox(
                    &mut progress,
                    now,
                    "test",
                    1,
                    Some(now - chrono::Duration::seconds(BACKLOG_AGE)),
                    None,
                );
                report_outbox(
                    &mut progress,
                    now + chrono::Duration::seconds(STALL_SECONDS),
                    "live",
                    1,
                    Some(now),
                    None,
                );
                for (alert, threshold) in [
                    ("TopupHeartbeatStale", 180),
                    ("TopupTreasuryProgressAge", PROGRESS_SECONDS),
                    ("TopupRefundProgressAge", PROGRESS_SECONDS),
                ] {
                    report_age(alert, "test", "warning", threshold - 1, threshold);
                    report_age(alert, "test", "warning", threshold, threshold);
                }
            });
        });
        assert_eq!(events.len(), 6, "{events:?}");
        for alert in [
            "TopupOutboxBacklog",
            "TopupOutboxStalled",
            "TopupHeartbeatStale",
            "TopupTreasuryProgressAge",
            "TopupRefundProgressAge",
        ] {
            assert!(
                events.iter().any(|event| event.tags["alert"] == alert),
                "{alert}"
            );
        }
    }

    #[test]
    fn certificate_expiry_probe_emits_warning_and_critical_events() {
        let expires = certificate_expiry(include_bytes!("test-certificate.der")).unwrap();
        assert!(certificate_expiry(b"invalid").is_err());
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                report_certificate(expires, expires - 15 * 86400);
                report_certificate(expires, expires - 14 * 86400);
                report_certificate(expires, expires - 3 * 86400);
            });
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].tags["severity"], "warning");
        assert_eq!(events[1].tags["severity"], "critical");
        assert!(
            events
                .iter()
                .all(|event| event.tags["alert"] == "TopupCertificateExpiry")
        );
    }
}
