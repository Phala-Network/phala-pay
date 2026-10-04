//! Disk and unarchived WAL capacity, sampled by the read-only deployment probe.
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{GaugeVec, IntGauge, Opts, core::Collector, proto::MetricFamily};
use serde::Deserialize;

use super::business::emit_alert;

#[derive(Deserialize)]
struct Sample {
    timestamp: u64,
    pgdata_total: u64,
    pgdata_available: u64,
    observability_total: u64,
    observability_available: u64,
    wal_bytes: u64,
    wal_age_seconds: u64,
}
struct Metrics {
    used: GaugeVec,
    wal_bytes: IntGauge,
    wal_age: IntGauge,
    timestamp: IntGauge,
}
static METRICS: OnceLock<Result<Metrics, prometheus::Error>> = OnceLock::new();
fn metrics() -> Result<&'static Metrics, &'static prometheus::Error> {
    METRICS
        .get_or_init(|| {
            Ok(Metrics {
                used: GaugeVec::new(
                    Opts::new(
                        "topup_disk_used_ratio",
                        "Used filesystem capacity including reserved blocks.",
                    ),
                    &["volume"],
                )?,
                wal_bytes: IntGauge::new(
                    "topup_unarchived_wal_bytes",
                    "Completed WAL awaiting successful archiving.",
                )?,
                wal_age: IntGauge::new(
                    "topup_unarchived_wal_age_seconds",
                    "Age of oldest completed unarchived WAL.",
                )?,
                timestamp: IntGauge::new(
                    "topup_capacity_sample_timestamp_seconds",
                    "Last valid capacity probe sample.",
                )?,
            })
        })
        .as_ref()
}
fn used(total: u64, available: u64) -> Option<f64> {
    (total > 0 && available <= total).then(|| 1.0 - available as f64 / total as f64)
}

/// Updates gauges and emits bounded Sentry-compatible tracing alerts every backup monitor poll.
pub(super) async fn observe() {
    let result = async {
        let bytes = tokio::fs::read("/run/topup-observability/capacity.json").await?;
        let sample: Sample = serde_json::from_slice(&bytes)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        record(sample, now)
    }
    .await;
    if result.is_err() {
        emit_alert("TopupCapacityProbeFailed", "capacity", "critical", 1, 0);
    }
}

fn record(sample: Sample, now: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        sample.timestamp <= now && now - sample.timestamp <= 120,
        "stale capacity sample"
    );
    let metrics = metrics().map_err(|_| anyhow::anyhow!("capacity gauges unavailable"))?;
    for (volume, total, available) in [
        ("pgdata", sample.pgdata_total, sample.pgdata_available),
        (
            "observability",
            sample.observability_total,
            sample.observability_available,
        ),
    ] {
        let ratio = used(total, available).ok_or_else(|| anyhow::anyhow!("invalid capacity"))?;
        metrics.used.with_label_values(&[volume]).set(ratio);
        let observed = (ratio * 100.0).round() as i64;
        if ratio >= 0.90 {
            emit_alert("TopupDiskCritical", volume, "critical", observed, 90);
        } else if ratio >= 0.75 {
            emit_alert("TopupDiskWarning", volume, "warning", observed, 75);
        }
    }
    metrics.wal_bytes.set(i64::try_from(sample.wal_bytes)?);
    metrics.wal_age.set(i64::try_from(sample.wal_age_seconds)?);
    metrics.timestamp.set(i64::try_from(sample.timestamp)?);
    if sample.wal_bytes >= 1_073_741_824 {
        emit_alert(
            "TopupUnarchivedWalPressure",
            "wal-bytes",
            "critical",
            i64::try_from(sample.wal_bytes)?,
            1_073_741_824,
        );
    }
    if sample.wal_age_seconds >= 120 {
        emit_alert(
            "TopupUnarchivedWalPressure",
            "wal-age",
            "critical",
            i64::try_from(sample.wal_age_seconds)?,
            120,
        );
    }
    Ok(())
}

/// Collects the last observed sample without disk or network I/O.
pub(super) fn collect() -> Result<Vec<MetricFamily>, prometheus::Error> {
    let m = metrics().map_err(|_| prometheus::Error::Msg("capacity metrics unavailable".into()))?;
    let mut families = m.used.collect();
    families.extend(m.wal_bytes.collect());
    families.extend(m.wal_age.collect());
    families.extend(m.timestamp.collect());
    Ok(families)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_includes_reserved_blocks_and_rejects_invalid_samples() {
        assert_eq!(used(100, 25), Some(0.75));
        assert_eq!(used(100, 0), Some(1.0));
        assert_eq!(used(0, 0), None);
        assert_eq!(used(100, 101), None);
    }
    #[test]
    fn thresholds_deliver_sentry_events_with_bounded_volume_tags() {
        let events = sentry::test::with_captured_events(|| {
            tracing::subscriber::with_default(super::super::log_subscriber(std::io::sink), || {
                record(
                    Sample {
                        timestamp: 100,
                        pgdata_total: 100,
                        pgdata_available: 25,
                        observability_total: 100,
                        observability_available: 10,
                        wal_bytes: 0,
                        wal_age_seconds: 120,
                    },
                    100,
                )
                .unwrap();
            });
        });
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0].tags.get("alert").map(String::as_str),
            Some("TopupDiskWarning")
        );
        assert_eq!(
            events[0].tags.get("component").map(String::as_str),
            Some("pgdata")
        );
        assert_eq!(
            events[1].tags.get("alert").map(String::as_str),
            Some("TopupDiskCritical")
        );
        assert_eq!(
            events[2].tags.get("alert").map(String::as_str),
            Some("TopupUnarchivedWalPressure")
        );
        assert!(
            record(
                Sample {
                    timestamp: 100,
                    pgdata_total: 100,
                    pgdata_available: 26,
                    observability_total: 100,
                    observability_available: 26,
                    wal_bytes: 0,
                    wal_age_seconds: 0,
                },
                221
            )
            .is_err()
        );
    }
}
