//! Bounded OFAC publication and decision metrics on the signed admin metrics surface.
use prometheus::{IntCounterVec, IntGaugeVec, Opts, core::Collector};
use std::sync::OnceLock;
use topup_core::screening::SanctionsVerdict;

struct Metrics {
    age: IntGaugeVec,
    addresses: IntGaugeVec,
    publish: IntGaugeVec,
    refresh: IntCounterVec,
    screen: IntCounterVec,
    parse_errors: IntCounterVec,
}
static METRICS: OnceLock<Result<Metrics, prometheus::Error>> = OnceLock::new();
fn metrics() -> Result<&'static Metrics, prometheus::Error> {
    METRICS
        .get_or_init(|| {
            Ok(Metrics {
                age: IntGaugeVec::new(
                    Opts::new(
                        "topup_sanctions_list_verified_age_seconds",
                        "Age of latest successful official hash verification.",
                    ),
                    &["source"],
                )?,
                addresses: IntGaugeVec::new(
                    Opts::new(
                        "topup_sanctions_list_addresses",
                        "Digital currency identifiers in active snapshot.",
                    ),
                    &["source", "family"],
                )?,
                publish: IntGaugeVec::new(
                    Opts::new(
                        "topup_sanctions_list_publish_timestamp",
                        "Active OFAC publication date as UTC Unix timestamp.",
                    ),
                    &["source"],
                )?,
                refresh: IntCounterVec::new(
                    Opts::new(
                        "topup_sanctions_list_refresh_total",
                        "Publication refresh outcomes.",
                    ),
                    &["source", "result"],
                )?,
                screen: IntCounterVec::new(
                    Opts::new(
                        "topup_sanctions_screen_total",
                        "Decision-time list verdicts.",
                    ),
                    &["purpose", "verdict"],
                )?,
                parse_errors: IntCounterVec::new(
                    Opts::new(
                        "topup_sanctions_list_parse_errors_total",
                        "Malformed 0x identifiers retained without guessing.",
                    ),
                    &["source"],
                )?,
            })
        })
        .as_ref()
        .map_err(|_| prometheus::Error::Msg("sanctions metrics initialization failed".into()))
}
/// Records a publication refresh outcome.
pub fn refresh(result: &str) {
    if let Ok(m) = metrics() {
        m.refresh.with_label_values(&["ofac_sdn", result]).inc();
    }
    sentry::metrics::counter("topup_sanctions_list_refresh_total", 1)
        .attribute("source", "ofac_sdn")
        .attribute("result", result.to_owned())
        .capture();
}
/// Records a decision using fixed bounded labels.
pub fn screen(purpose: &str, verdict: SanctionsVerdict) {
    let verdict = match verdict {
        SanctionsVerdict::Sanctioned => "sanctioned",
        SanctionsVerdict::Clear => "clear",
        SanctionsVerdict::Uncertain => "uncertain",
    };
    if let Ok(m) = metrics() {
        m.screen.with_label_values(&[purpose, verdict]).inc();
    }
    sentry::metrics::counter("topup_sanctions_screen_total", 1)
        .attribute("purpose", purpose.to_owned())
        .attribute("verdict", verdict)
        .capture();
}
/// Counts malformed address-shaped identifiers.
pub fn parse_errors(count: u64) {
    sentry::metrics::counter(
        "topup_sanctions_list_parse_errors_total",
        u32::try_from(count).unwrap_or(u32::MAX),
    )
    .attribute("source", "ofac_sdn")
    .capture();
    if let Ok(m) = metrics() {
        m.parse_errors
            .with_label_values(&["ofac_sdn"])
            .inc_by(count);
    }
}
/// Samples snapshot gauges with current verification age.
pub async fn snapshot(snapshot: &crate::sanctions::Snapshot, age: i64, pool: &sqlx::PgPool) {
    if let Ok(m) = metrics() {
        m.age.with_label_values(&["ofac_sdn"]).set(age);
        sentry::metrics::gauge(
            "topup_sanctions_list_verified_age_seconds",
            u32::try_from(age.max(0)).unwrap_or(u32::MAX),
        )
        .attribute("source", "ofac_sdn")
        .capture();
        if let Some(date) = snapshot.publish_date.and_hms_opt(0, 0, 0) {
            m.publish
                .with_label_values(&["ofac_sdn"])
                .set(date.and_utc().timestamp());
            sentry::metrics::gauge(
                "topup_sanctions_list_publish_timestamp",
                u32::try_from(date.and_utc().timestamp()).unwrap_or(u32::MAX),
            )
            .attribute("source", "ofac_sdn")
            .capture();
        }
        let evm: Result<i64,_> = sqlx::query_scalar("SELECT count(*) FROM sanctions_list_addresses WHERE snapshot_id=$1 AND evm_address IS NOT NULL").bind(snapshot.id).fetch_one(pool).await;
        if let Ok(evm) = evm {
            sentry::metrics::gauge(
                "topup_sanctions_list_addresses",
                u32::try_from(evm).unwrap_or(u32::MAX),
            )
            .attribute("source", "ofac_sdn")
            .attribute("family", "evm")
            .capture();
            sentry::metrics::gauge(
                "topup_sanctions_list_addresses",
                u32::try_from(snapshot.address_count.saturating_sub(evm)).unwrap_or(u32::MAX),
            )
            .attribute("source", "ofac_sdn")
            .attribute("family", "other")
            .capture();
            m.addresses.with_label_values(&["ofac_sdn", "evm"]).set(evm);
            m.addresses
                .with_label_values(&["ofac_sdn", "other"])
                .set(snapshot.address_count.saturating_sub(evm));
        } else {
            tracing::error!("sanctions address metrics reload failed");
        }
    } else {
        tracing::error!("sanctions metrics initialization failed");
    }
}
/// Metric families for the signed admin endpoint.
pub fn collect() -> Result<Vec<prometheus::proto::MetricFamily>, prometheus::Error> {
    let m = metrics()?;
    let mut result = m.age.collect();
    result.extend(m.addresses.collect());
    result.extend(m.publish.collect());
    result.extend(m.refresh.collect());
    result.extend(m.screen.collect());
    result.extend(m.parse_errors.collect());
    Ok(result)
}
