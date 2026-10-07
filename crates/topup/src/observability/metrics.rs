//! Standard Prometheus exposition, with bounded HTTP labels and in-process RPC snapshots.
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    TextEncoder, core::Collector,
};
use sqlx::PgPool;
use topup_adapters::chain::evm::metrics::{
    counting_since, endpoint_readiness, rpc_call_counts, rpc_error_counts,
};

/// Prometheus text media type.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

struct HttpMetrics {
    requests: IntCounterVec,
    latency: HistogramVec,
    deadlines: IntCounterVec,
}
impl HttpMetrics {
    fn new() -> Result<Self, prometheus::Error> {
        Ok(Self {
            requests: IntCounterVec::new(
                Opts::new("topup_http_requests_total", "Completed HTTP requests."),
                &["route", "method", "status_class"],
            )?,
            deadlines: IntCounterVec::new(
                Opts::new(
                    "topup_api_request_deadline_exceeded_total",
                    "Requests that did not complete within their deadline.",
                ),
                &["method"],
            )?,
            latency: HistogramVec::new(
                HistogramOpts::new(
                    "topup_http_request_duration_seconds",
                    "HTTP response header latency in seconds.",
                )
                .buckets(vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
                ]),
                &["route", "method", "status_class"],
            )?,
        })
    }
    fn observe(&self, route: &str, method: &str, status: u16, elapsed: Duration) {
        let method = match method {
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" => method,
            _ => "other",
        };
        let class = match status {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };
        let labels = [route, method, class];
        self.requests.with_label_values(&labels).inc();
        self.latency
            .with_label_values(&labels)
            .observe(elapsed.as_secs_f64());
    }
}
static HTTP: OnceLock<Result<HttpMetrics, prometheus::Error>> = OnceLock::new();
fn http() -> Result<&'static HttpMetrics, &'static prometheus::Error> {
    HTTP.get_or_init(HttpMetrics::new).as_ref()
}
/// Observes a completed request. `route` must be an Axum matched template or `unmatched`.
pub(super) fn observe_http(route: &str, method: &str, status: u16, elapsed: Duration) {
    match http() {
        Ok(metrics) => metrics.observe(route, method, status, elapsed),
        Err(_) => tracing::error!("HTTP metrics initialization failed"),
    }
}
#[cfg(test)]
pub(crate) fn http_observations(route: &str, method: &str, status_class: &str) -> u64 {
    http()
        .unwrap()
        .requests
        .with_label_values(&[route, method, status_class])
        .get()
}
#[cfg(test)]
pub(crate) fn request_deadline_observations(method: &str) -> u64 {
    http().unwrap().deadlines.with_label_values(&[method]).get()
}

/// Records a request that did not complete within its deadline, with a bounded method label.
pub(crate) fn request_deadline_exceeded(method: &str) {
    let method = match method {
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" => method,
        _ => "other",
    };
    match http() {
        Ok(metrics) => metrics.deadlines.with_label_values(&[method]).inc(),
        Err(_) => tracing::error!("HTTP metrics initialization failed"),
    }
}

static POOL_TIMEOUTS: OnceLock<Result<IntCounter, prometheus::Error>> = OnceLock::new();
fn pool_timeouts() -> Result<&'static IntCounter, &'static prometheus::Error> {
    POOL_TIMEOUTS
        .get_or_init(|| {
            IntCounter::new(
                "topup_db_pool_acquire_timeouts_total",
                "Database pool connection acquisition timeouts.",
            )
        })
        .as_ref()
}

/// Records a failed database pool acquisition that timed out.
pub(crate) fn pool_acquire_timed_out() {
    match pool_timeouts() {
        Ok(counter) => counter.inc(),
        Err(_) => tracing::error!("database pool metrics initialization failed"),
    }
}

/// Renders counters, histograms and current pool gauges without database or network I/O.
pub fn render(pool: &PgPool) -> Result<String, prometheus::Error> {
    let http = http().map_err(|_| prometheus::Error::Msg("HTTP metrics unavailable".into()))?;
    let calls = IntCounterVec::new(
        Opts::new(
            "topup_rpc_calls_total",
            "JSON-RPC calls sent since process start.",
        ),
        &["provider", "chain_id", "method"],
    )?;
    for count in rpc_call_counts() {
        let chain = count
            .chain_id
            .map_or_else(|| "unknown".to_owned(), |id| id.to_string());
        calls
            .with_label_values(&[&count.provider, &chain, count.method])
            .inc_by(count.calls);
    }
    let connections = IntGaugeVec::new(
        Opts::new(
            "topup_db_pool_connections",
            "Current database pool connections.",
        ),
        &["state"],
    )?;
    let size = u64::from(pool.size());
    let idle = u64::try_from(pool.num_idle()).unwrap_or(u64::MAX);
    connections
        .with_label_values(&["idle"])
        .set(i64::try_from(idle).unwrap_or(i64::MAX));
    connections
        .with_label_values(&["in_use"])
        .set(i64::try_from(size.saturating_sub(idle)).unwrap_or(i64::MAX));
    let max_connections = IntGauge::new(
        "topup_db_pool_max_connections",
        "Maximum database pool connections.",
    )?;
    max_connections.set(i64::from(pool.options().get_max_connections()));
    let timeouts = pool_timeouts()
        .map_err(|_| prometheus::Error::Msg("database pool metrics unavailable".into()))?;
    let mut families = calls.collect();
    let errors = IntCounterVec::new(
        Opts::new(
            "topup_rpc_errors_total",
            "Failed endpoint requests by bounded class.",
        ),
        &["provider", "chain_id", "method", "class"],
    )?;
    for (provider, chain, method, class, count) in rpc_error_counts() {
        let chain = chain.map_or_else(|| "unknown".to_owned(), |chain| chain.to_string());
        errors
            .with_label_values(&[&provider, &chain, method, class])
            .inc_by(count);
    }
    let ready = IntGaugeVec::new(
        Opts::new(
            "topup_rpc_endpoint_ready",
            "Endpoint is ready for its next operation.",
        ),
        &["provider", "chain_id"],
    )?;
    for (provider, chain, value) in endpoint_readiness() {
        ready
            .with_label_values(&[&provider, &chain.to_string()])
            .set(i64::from(value));
    }
    families.extend(errors.collect());
    families.extend(ready.collect());
    families.extend(connections.collect());
    families.extend(max_connections.collect());
    families.extend(timeouts.collect());
    if let Some(seconds) = counting_since().and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        let since = IntGauge::new(
            "topup_rpc_calls_since_seconds",
            "Unix time of first counted call.",
        )?;
        since.set(i64::try_from(seconds.as_secs()).unwrap_or(i64::MAX));
        families.extend(since.collect());
    }

    families.extend(super::capacity::collect()?);
    families.extend(super::price_metrics::collect()?);
    families.extend(http.requests.collect());
    families.extend(http.latency.collect());
    families.extend(http.deadlines.collect());
    // Like Registry::gather, omit families with no observed series rather than inventing zeros.
    families.retain(|family| !family.get_metric().is_empty());
    TextEncoder::new().encode_to_string(&families)
}

/// Durable coverage and daily budget gauges are reloaded on authenticated scrapes, including
/// after restart; no RPC request or address-wide backfill is issued here.
pub async fn render_chain_reads(pool: &PgPool) -> Result<String, anyhow::Error> {
    let (coverage,budgets,issued)=tokio::try_join!(
        sqlx::query_as::<_,(i64,i64,i64)>("SELECT c.chain_id,GREATEST(0,extract(epoch FROM now()-c.through_time)::bigint),(SELECT count(*) FROM addresses a WHERE a.chain_id=c.chain_id AND (a.dual_covered_through IS NULL OR a.dual_covered_through < c.through_block)) FROM chain_coverage c").fetch_all(pool),
        sqlx::query_as::<_,(String,i32)>("SELECT name,used FROM daily_budgets WHERE day=(now() AT TIME ZONE 'UTC')::date").fetch_all(pool),
        sqlx::query_as::<_,(i64,i64)>("SELECT chain_id,count(*) FROM addresses GROUP BY chain_id UNION ALL SELECT chain_id,0 FROM chain_coverage c WHERE NOT EXISTS(SELECT 1 FROM addresses a WHERE a.chain_id=c.chain_id)").fetch_all(pool)
    )?;
    let lag = IntGaugeVec::new(
        Opts::new(
            "topup_coverage_lag_seconds",
            "Time since the dual covered boundary.",
        ),
        &["chain_id"],
    )?;
    let addresses = IntGaugeVec::new(
        Opts::new(
            "topup_addresses_lagging",
            "Issued addresses awaiting dual backfill.",
        ),
        &["chain_id"],
    )?;
    let used = IntGaugeVec::new(
        Opts::new(
            "topup_daily_budget_used",
            "Units claimed during the current UTC day.",
        ),
        &["name"],
    )?;
    for (chain, seconds, count) in coverage {
        let chain = chain.to_string();
        lag.with_label_values(&[&chain]).set(seconds);
        addresses.with_label_values(&[&chain]).set(count);
    }
    for (name, count) in budgets {
        used.with_label_values(&[&name]).set(i64::from(count));
    }
    let issued_gauge = IntGaugeVec::new(
        Opts::new(
            "topup_issued_addresses",
            "All historical issued addresses; retirement and expiry do not free capacity.",
        ),
        &["chain_id"],
    )?;
    let cap_gauge = IntGaugeVec::new(
        Opts::new(
            "topup_issued_address_cap",
            "Permanent per-chain issued-address pilot cap.",
        ),
        &["chain_id"],
    )?;
    for (chain, count) in issued {
        let chain = chain.to_string();
        issued_gauge.with_label_values(&[&chain]).set(count);
        cap_gauge
            .with_label_values(&[&chain])
            .set(crate::db::chain_reads::ISSUED_ADDRESS_CAP);
    }
    let mut families = lag.collect();
    families.extend(issued_gauge.collect());
    families.extend(cap_gauge.collect());
    families.extend(addresses.collect());
    families.extend(used.collect());
    families.retain(|family| !family.get_metric().is_empty());
    Ok(TextEncoder::new().encode_to_string(&families)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latency_buckets_and_labels_are_bounded() {
        let metrics = HttpMetrics::new().unwrap();
        for _ in 0..1000 {
            metrics.observe("/v1/quotes/{id}", "GET", 200, Duration::from_millis(80));
            metrics.observe("unmatched", "ATTACKER", 404, Duration::from_millis(300));
        }
        let requests = metrics.requests.collect();
        assert_eq!(requests[0].get_metric().len(), 2);
        let latency = metrics.latency.collect();
        assert_eq!(latency[0].get_metric().len(), 2);
        let sample = latency[0]
            .get_metric()
            .iter()
            .find(|sample| {
                sample
                    .get_label()
                    .iter()
                    .any(|label| label.value() == "/v1/quotes/{id}")
            })
            .unwrap();
        let histogram = sample.get_histogram();
        assert_eq!(histogram.get_sample_count(), 1000);
        assert!((histogram.get_sample_sum() - 80.0).abs() < 0.001);
        assert_eq!(
            histogram
                .get_bucket()
                .iter()
                .find(|b| b.upper_bound() == 0.1)
                .unwrap()
                .cumulative_count(),
            1000
        );
        let text = TextEncoder::new().encode_to_string(&latency).unwrap();
        assert!(text.contains("_bucket{"));
        assert!(!text.contains("ATTACKER"));
    }
}
