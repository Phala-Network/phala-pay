//! Standard Prometheus exposition, with bounded HTTP labels and in-process RPC snapshots.
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, TextEncoder, core::Collector,
};
use topup_adapters::chain::evm::metrics::{counting_since, rpc_call_counts};

/// Prometheus text media type.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

struct HttpMetrics {
    requests: IntCounterVec,
    latency: HistogramVec,
}
impl HttpMetrics {
    fn new() -> Result<Self, prometheus::Error> {
        Ok(Self {
            requests: IntCounterVec::new(
                Opts::new("topup_http_requests_total", "Completed HTTP requests."),
                &["route", "method", "status_class"],
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

/// Renders counters and histograms without database or network I/O.
pub fn render() -> Result<String, prometheus::Error> {
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
    let mut families = calls.collect();
    if let Some(seconds) = counting_since().and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        let since = IntGauge::new(
            "topup_rpc_calls_since_seconds",
            "Unix time of first counted call.",
        )?;
        since.set(i64::try_from(seconds.as_secs()).unwrap_or(i64::MAX));
        families.extend(since.collect());
    }
    families.extend(topup_adapters::chain::evm::group::metrics::collect()?);
    families.extend(topup_adapters::chain::evm::group::metrics::events()?);
    families.extend(crate::db::rpc::metrics());
    families.extend(super::capacity::collect()?);
    families.extend(super::price_metrics::collect()?);
    families.extend(http.requests.collect());
    families.extend(http.latency.collect());
    // Like Registry::gather, omit families with no observed series rather than inventing zeros.
    families.retain(|family| !family.get_metric().is_empty());
    TextEncoder::new().encode_to_string(&families)
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
