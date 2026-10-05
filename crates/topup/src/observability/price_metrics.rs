//! Bounded price metrics and alert events using the existing reporting integration.
use prometheus::{IntCounterVec, IntGaugeVec, Opts, core::Collector};
use std::sync::OnceLock;
use topup_core::route::RouteFile;
struct Metrics {
    health: IntGaugeVec,
    failover: IntCounterVec,
    disagreement: IntCounterVec,
    depeg: IntCounterVec,
    stuck: IntGaugeVec,
    refusals: IntCounterVec,
}
static METRICS: OnceLock<Result<Metrics, prometheus::Error>> = OnceLock::new();
fn metrics() -> Result<&'static Metrics, prometheus::Error> {
    METRICS
        .get_or_init(|| {
            let labels = &["route", "asset", "role", "source", "company"];
            Ok(Metrics {
                health: IntGaugeVec::new(
                    Opts::new("price_source_health", "Fresh source status."),
                    labels,
                )?,
                failover: IntCounterVec::new(
                    Opts::new("price_failover_total", "Ordered role failovers."),
                    labels,
                )?,
                disagreement: IntCounterVec::new(
                    Opts::new("price_disagreement_total", "Rejected price disagreement."),
                    labels,
                )?,
                depeg: IntCounterVec::new(
                    Opts::new("price_depeg_total", "Fresh source outside peg band."),
                    labels,
                )?,
                refusals: IntCounterVec::new(
                    Opts::new(
                        "price_source_refusals_total",
                        "Price safety refusals by stable error code.",
                    ),
                    &["route", "asset", "role", "source", "company", "code"],
                )?,
                stuck: IntGaugeVec::new(
                    Opts::new(
                        "valuation_stuck_seconds",
                        "Time since continuous valuation failure.",
                    ),
                    labels,
                )?,
            })
        })
        .as_ref()
        .map_err(|_| prometheus::Error::Msg("price metrics initialization failed".into()))
}
fn source_name(company: &str) -> &str {
    if company == "uniswap-v2-onchain" {
        "uniswap_v2_twap"
    } else {
        company
    }
}
/// Record an individually alertable safety refusal through the existing price telemetry.
pub fn refusal(route: &RouteFile, role: &str, company: &str, code: &'static str) {
    sentry::metrics::counter("price_source_refusals_total", 1)
        .attribute("route", route.route.clone())
        .attribute("asset", route.asset.symbol.clone())
        .attribute("role", role.to_owned())
        .attribute("source", source_name(company).to_owned())
        .attribute("company", company.to_owned())
        .attribute("code", code)
        .capture();
    match metrics() {
        Ok(m) => m
            .refusals
            .with_label_values(&[
                &route.route,
                &route.asset.symbol,
                role,
                source_name(company),
                company,
                code,
            ])
            .inc(),
        Err(_) => tracing::error!("price metrics initialization failed"),
    }
    alert(route, code);
}
/// Source labels come from the attested registry, never response bodies.
pub fn health(route: &RouteFile, role: &str, source: &str, healthy: bool) {
    sentry::metrics::gauge("price_source_health", u32::from(healthy))
        .attribute("route", route.route.clone())
        .attribute("asset", route.asset.symbol.clone())
        .attribute("role", role.to_owned())
        .attribute("source", source_name(source).to_owned())
        .attribute("company", source.to_owned())
        .capture();
    match metrics() {
        Ok(m) => m
            .health
            .with_label_values(&[
                &route.route,
                &route.asset.symbol,
                role,
                source_name(source),
                source,
            ])
            .set(i64::from(healthy)),
        Err(_) => tracing::error!("price metrics initialization failed"),
    }
}
/// Records ordered failover.
pub fn failover(route: &RouteFile, role: &str, source: &str) {
    counter("price_failover_total", route, role, source);
    match metrics() {
        Ok(m) => m
            .failover
            .with_label_values(&[
                &route.route,
                &route.asset.symbol,
                role,
                source_name(source),
                source,
            ])
            .inc(),
        Err(_) => tracing::error!("price metrics initialization failed"),
    }
}
/// Records a rejected valuation with per-source dimensions.
pub fn decision(route: &RouteFile, code: &str, evidence: &serde_json::Value) {
    if let Some(observations) = evidence["observations"].as_array() {
        for observation in observations {
            let role = observation["role"].as_str().unwrap_or("valuation");
            let source = observation["company"]
                .as_str()
                .or_else(|| observation["source"].as_str())
                .unwrap_or("policy");
            if (code == "divergent"
                && matches!(role, "primary" | "check" | "sources")
                && (observation["error"].is_null() || observation["error"] == "divergent"))
                || (code == "fx_depeg" && role == "fx" && observation["error"].is_null())
            {
                source_event(route, role, source, code);
            }
        }
    }
}
/// Emits every failed valuation through the existing sanitized alert convention.
pub fn failure(route: &RouteFile, failure: &crate::locks::pricing::PricingFailure) {
    let code = failure.code;
    let evidence = &failure.evidence;
    tracing::warn!(tags.alert = "price-outage", tags.route = route.route, tags.asset = route.asset.symbol, tags.check = code, evidence = %evidence, "price valuation halted");
}
/// Records rejected observations with the real source/company identity.
pub fn source_event(route: &RouteFile, role: &str, source: &str, code: &str) {
    let name = if code == "divergent" {
        "price_disagreement_total"
    } else {
        "price_depeg_total"
    };
    counter(name, route, role, source);
    match metrics() {
        Ok(m) => {
            let metric = if code == "divergent" {
                &m.disagreement
            } else {
                &m.depeg
            };
            metric
                .with_label_values(&[
                    &route.route,
                    &route.asset.symbol,
                    role,
                    source_name(source),
                    source,
                ])
                .inc();
        }
        Err(_) => tracing::error!("price metrics initialization failed"),
    }
}
/// Reports how long pricing has remained unavailable; successful valuation clears it.
pub fn stuck(route: &RouteFile, seconds: u64) {
    sentry::metrics::gauge(
        "valuation_stuck_seconds",
        u32::try_from(seconds).unwrap_or(u32::MAX),
    )
    .attribute("route", route.route.clone())
    .attribute("asset", route.asset.symbol.clone())
    .attribute("role", "valuation")
    .attribute("source", "policy")
    .attribute("company", "policy")
    .capture();
    match metrics() {
        Ok(m) => m
            .stuck
            .with_label_values(&[
                &route.route,
                &route.asset.symbol,
                "valuation",
                "policy",
                "policy",
            ])
            .set(i64::try_from(seconds).unwrap_or(i64::MAX)),
        Err(_) => tracing::error!("price metrics initialization failed"),
    }
    if seconds > route.alerts.stuck_after_s.detected {
        alert(route, "valuation_stuck");
    }
}
/// Existing Sentry tracing alert API; no reporting.rs modifications.
pub fn alert(route: &RouteFile, code: &str) {
    tracing::warn!(
        tags.alert = "price-outage",
        tags.route = route.route,
        tags.asset = route.asset.symbol,
        tags.check = code,
        "price valuation halted"
    );
}
/// Collects series for the admin-signed Prometheus endpoint.
pub fn collect() -> Result<Vec<prometheus::proto::MetricFamily>, prometheus::Error> {
    let m = metrics()?;
    let mut families = m.health.collect();
    families.extend(m.failover.collect());
    families.extend(m.disagreement.collect());
    families.extend(m.depeg.collect());
    families.extend(m.stuck.collect());
    families.extend(m.refusals.collect());
    Ok(families)
}

fn counter(name: &'static str, route: &RouteFile, role: &str, source: &str) {
    sentry::metrics::counter(name, 1)
        .attribute("route", route.route.clone())
        .attribute("asset", route.asset.symbol.clone())
        .attribute("role", role.to_owned())
        .attribute("source", source_name(source).to_owned())
        .attribute("company", source.to_owned())
        .capture();
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::{
        protocol::{EnvelopeItem, ItemContainer},
        test::with_captured_envelopes_options,
    };
    #[test]
    fn price_metrics_and_alerts_reach_sentry_and_prometheus() {
        let mut route: RouteFile =
            serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml"))
                .unwrap();
        route.route = "price-metrics-fixture".into();
        route.alerts.stuck_after_s.detected = 5;
        let envelopes = with_captured_envelopes_options(
            || {
                tracing::subscriber::with_default(
                    crate::observability::log_subscriber(std::io::sink),
                    || {
                        health(&route, "primary", "kraken", true);
                        failover(&route, "primary", "kraken");
                        source_event(&route, "check", "binance", "divergent");
                        source_event(&route, "sources", "chainlink", "depeg");
                        refusal(&route, "primary", "uniswap-v2-onchain", "twap_liquidity");
                        stuck(&route, 6);
                        failure(
                            &route,
                            &crate::locks::pricing::PricingFailure {
                                code: "depeg",
                                evidence: serde_json::json!({"decision":"depeg"}),
                            },
                        );
                    },
                );
                if let Some(client) = sentry::Hub::current().client() {
                    assert!(client.flush(Some(std::time::Duration::from_secs(1))));
                }
            },
            sentry::ClientOptions::default(),
        );
        let captured = envelopes
            .iter()
            .flat_map(|e| e.items())
            .filter_map(|i| match i {
                EnvelopeItem::ItemContainer(ItemContainer::Metrics(m)) => Some(m),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        for name in [
            "price_source_health",
            "price_failover_total",
            "price_disagreement_total",
            "price_depeg_total",
            "valuation_stuck_seconds",
            "price_source_refusals_total",
        ] {
            assert!(
                captured.iter().any(|m| m.name == name),
                "missing Sentry metric {name}"
            );
        }
        assert!(envelopes.iter().filter_map(|e| e.event()).any(|event| {
            event.tags.get("check").map(String::as_str) == Some("valuation_stuck")
        }));
        let text = prometheus::TextEncoder::new()
            .encode_to_string(&collect().unwrap())
            .unwrap();
        assert!(text.contains("price-metrics-fixture"));
        assert!(text.contains("company=\"chainlink\""));
        assert!(text.contains("valuation_stuck_seconds"));
        assert!(text.contains("source=\"uniswap_v2_twap\""));
        assert!(text.contains("company=\"uniswap-v2-onchain\""));
        assert!(text.contains("code=\"twap_liquidity\""));
    }
}
