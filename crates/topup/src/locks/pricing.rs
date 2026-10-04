//! Shared fail-closed pricing for quotes and deposit credit.
use crate::{observability::price_metrics, routes::RouteSet};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use topup_adapters::pricing::{
    Observation, PriceError, PriceSource, binance::Binance, chainlink::Chainlink, kraken::Kraken,
};
use topup_core::{
    money::ScaledPrice,
    price::{Source, feed},
    route::{PricingMode, RouteFile},
    valuation::{
        FxObservation, UnixSeconds, ValuationError, ValuationPolicy, stablecoin_price,
        validate_spot,
    },
};
struct Entry {
    source: Arc<dyn PriceSource>,
    company: &'static str,
    asset: Option<String>,
    max_age_s: Option<u64>,
    usdt_quoted: bool,
}
/// Per-role ordered sources, shared by every valuation path.
pub struct PricingRuntime {
    sources: Vec<Entry>,
    primary: Vec<Entry>,
    check: Vec<Entry>,
    fx: Vec<Entry>,
    sequencer: Option<(Chainlink, u64)>,
    stuck_since: std::sync::Mutex<Option<Instant>>,
}
impl PricingRuntime {
    /// Constructs only explicitly configured sources, never a restricted default.
    pub fn configured(route: &RouteFile, routes: &RouteSet) -> Result<Self, String> {
        route
            .pricing
            .validate(route.chain.chain_id, &route.asset.symbol, route.livemode)
            .map_err(|e| e.to_string())?;
        let entries = |list: &[Source]| -> Result<Vec<Entry>, String> {
            list.iter()
                .map(|s| {
                    let source: Arc<dyn PriceSource> = match s {
                        Source::Coinmetrics { .. } => {
                            return Err("restricted legacy source requires migration".into());
                        }
                        Source::Kraken { symbol, .. } => {
                            Arc::new(Kraken::new(symbol.clone()).map_err(|e| e.to_string())?)
                        }
                        Source::Binance { symbol, .. } => {
                            Arc::new(Binance::new(symbol.clone()).map_err(|e| e.to_string())?)
                        }
                        Source::Chainlink {
                            feed: name,
                            chain_id,
                            rpc_group,
                            rpc_group_b,
                            ..
                        } => {
                            let b = rpc_group_b.as_deref().unwrap_or(
                                if rpc_group == "b"
                                    || route.chain.rpc_providers.get(1) == Some(rpc_group)
                                {
                                    "a"
                                } else {
                                    "b"
                                },
                            );
                            Arc::new(Chainlink::new(
                                routes.price_group(route, rpc_group)?,
                                routes.price_group(route, b)?,
                                feed(name, *chain_id).ok_or("unsupported feed")?,
                            ))
                        }
                    };
                    let max_age_s = match s {
                        Source::Chainlink {
                            feed: name,
                            chain_id,
                            ..
                        } => Some({
                            let metadata = feed(name, *chain_id).ok_or("unsupported feed")?;
                            metadata.heartbeat_s.saturating_add(metadata.margin_s)
                        }),
                        _ => None,
                    };
                    Ok(Entry {
                        source,
                        company: s.company(),
                        asset: Some(s.asset().to_owned()),
                        max_age_s,
                        usdt_quoted: matches!(s, Source::Binance { .. }),
                    })
                })
                .collect()
        };
        let sequencer = route
            .pricing
            .sequencer_uptime
            .as_ref()
            .map(|s| -> Result<_, String> {
                Ok((
                    Chainlink::new(
                        routes.price_group(route, &s.rpc_group)?,
                        routes.price_group(route, &s.rpc_group_b)?,
                        feed(&s.feed, 8453).ok_or("unsupported sequencer feed")?,
                    ),
                    s.grace_s,
                ))
            })
            .transpose()?;
        Ok(Self {
            sources: entries(&route.pricing.sources)?,
            primary: entries(&route.pricing.primary)?,
            check: entries(&route.pricing.check)?,
            fx: entries(&route.pricing.fx)?,
            sequencer,
            stuck_since: std::sync::Mutex::new(None),
        })
    }
    /// Injects independent adapters for deterministic tests.
    pub fn injected(
        primary: Arc<dyn PriceSource>,
        check: Option<Arc<dyn PriceSource>>,
        fx: Option<Arc<dyn PriceSource>>,
    ) -> Self {
        let entry = |source| Entry {
            source,
            company: "injected",
            asset: None,
            max_age_s: None,
            usdt_quoted: true,
        };
        Self {
            sources: vec![],
            primary: vec![entry(primary)],
            check: check.into_iter().map(entry).collect(),
            fx: fx.into_iter().map(entry).collect(),
            sequencer: None,
            stuck_since: std::sync::Mutex::new(None),
        }
    }
    /// Fetches every stablecoin source or the first healthy source in each volatile role.
    pub async fn fetch(&self, route: &RouteFile) -> Result<ValidatedQuote, Value> {
        let result = self.fetch_inner(route).await;
        if let Err(evidence) = &result {
            price_metrics::failure(route, evidence);
        }
        match self.stuck_since.lock() {
            Ok(mut since) => {
                let age = if result.is_ok() {
                    *since = None;
                    0
                } else {
                    since.get_or_insert_with(Instant::now).elapsed().as_secs()
                };
                price_metrics::stuck(route, age);
            }
            Err(_) => tracing::error!("price stuck metric lock failed"),
        }
        result
    }
    async fn fetch_inner(&self, route: &RouteFile) -> Result<ValidatedQuote, Value> {
        let mut audit = json!({"mode":route.pricing.mode, "observations":[]});
        if let Some((sequencer, grace)) = &self.sequencer {
            match sequencer.sequencer(*grace).await {
                Ok(evidence) => audit["sequencer"] = evidence,
                Err(error) => {
                    let code = error_code(&error);
                    if let PriceError::Feed { evidence, .. } = error {
                        audit["sequencer"] = evidence;
                    }
                    if code == "divergent" {
                        price_metrics::source_event(route, "sequencer", "chainlink", code);
                    }
                    audit["decision"] = json!(code);
                    price_metrics::decision(route, code, &audit);
                    return Err(json!({"stage":"pricing", "error":code, "quote":audit}));
                }
            }
        }
        let result = if route.pricing.mode == PricingMode::Stablecoin {
            let sources = if self.sources.is_empty() {
                &self.primary
            } else {
                &self.sources
            };
            let mut healthy = false;
            let mut depeg = false;
            let mut observed = Vec::new();
            for entry in sources.iter().filter(|e| {
                e.asset
                    .as_ref()
                    .is_none_or(|asset| asset == &route.asset.symbol)
            }) {
                if let Some(o) = observe(entry, route, "sources", &mut audit).await? {
                    observed.push((o, entry.max_age_s.unwrap_or(route.pricing.max_age_s)));
                }
            }
            let now = validation_time()?;
            for (o, bound) in observed {
                if now
                    .value()
                    .checked_sub(o.observed_at.value())
                    .is_none_or(|age| age > bound)
                {
                    continue;
                }
                healthy = true;
                let mut policy = ValuationPolicy::from(&route.pricing);
                policy.max_age_s = bound;
                policy.max_deviation_bps = route.pricing.peg_band_bps;
                if stablecoin_price(&o, now, policy).is_err() {
                    price_metrics::source_event(route, "sources", o.source.as_str(), "depeg");
                    depeg = true;
                }
            }

            if depeg {
                Err("depeg")
            } else if !healthy {
                Err("source_failure")
            } else {
                ScaledPrice::new(100_000_000, 8).map_err(|_| "out_of_range")
            }
        } else {
            let mut pa = json!({"observations":[]});
            let mut ca = json!({"observations":[]});
            let mut fa = json!({"observations":[]});
            let (primary, check, fx) = tokio::join!(
                first(&self.primary, route, "primary", &mut pa),
                first(&self.check, route, "check", &mut ca),
                first(&self.fx, route, "fx", &mut fa)
            );
            for part in [pa, ca, fa] {
                if let (Some(all), Some(list)) = (
                    audit["observations"].as_array_mut(),
                    part["observations"].as_array(),
                ) {
                    all.extend(list.iter().cloned());
                }
            }
            if let Some(error) = [&primary, &check, &fx]
                .iter()
                .find_map(|r| r.as_ref().err())
            {
                audit["decision"] = error["error"].clone();
                return Err(json!({"stage":"pricing", "error":error["error"], "quote":audit}));
            }
            let primary = primary?;
            let check = check?;
            let fx = fx?;
            match (primary, check, fx) {
                (Some(p), Some(c), Some(f)) if p.source != c.source && f.source != c.source => {
                    let now = validation_time()?;
                    for (o, entries) in [(&p, &self.primary), (&c, &self.check), (&f, &self.fx)] {
                        let bound = entries
                            .iter()
                            .find(|e| e.company == o.source.as_str())
                            .and_then(|e| e.max_age_s)
                            .unwrap_or(route.pricing.max_age_s);
                        if now
                            .value()
                            .checked_sub(o.observed_at.value())
                            .is_none_or(|age| age > bound)
                        {
                            return Err(json!({"stage":"pricing","error":"stale","quote":audit}));
                        }
                    }
                    let mut policy = ValuationPolicy::from(&route.pricing);
                    policy.max_age_s = u64::MAX;
                    let usdt_quoted = self
                        .check
                        .iter()
                        .find(|e| e.company == c.source.as_str())
                        .is_none_or(|e| e.usdt_quoted);
                    let mut fx_policy = policy;
                    fx_policy.max_deviation_bps = policy.max_fx_deviation_bps;
                    stablecoin_price(&f, now, fx_policy)
                        .map_err(|_| "fx_depeg")
                        .and_then(|dollar| {
                            validate_spot(
                                &p,
                                &c,
                                Some(&FxObservation {
                                    source: f.source,
                                    rate: if usdt_quoted { f.price } else { dollar },
                                    observed_at: f.observed_at,
                                }),
                                now,
                                policy,
                            )
                            .map_err(|e| valuation_error_code(&e))
                        })
                }
                (Some(_), Some(_), Some(_)) => Err("company_overlap"),
                _ => Err(
                    if audit["observations"]
                        .as_array()
                        .is_some_and(|list| list.iter().any(|o| o["error"] == "stale"))
                    {
                        "stale"
                    } else {
                        "source_failure"
                    },
                ),
            }
        };
        match result {
            Ok(price) => {
                audit["decision"] = json!("accepted");
                Ok(ValidatedQuote {
                    price,
                    evidence: audit,
                })
            }
            Err(code) => {
                audit["decision"] = json!(code);
                price_metrics::decision(route, code, &audit);
                Err(json!({"stage":"pricing", "error":code, "quote":audit}))
            }
        }
    }
}
async fn first(
    entries: &[Entry],
    route: &RouteFile,
    role: &str,
    audit: &mut Value,
) -> Result<Option<Observation>, Value> {
    for (index, entry) in entries.iter().enumerate() {
        if let Some(observation) = observe(entry, route, role, audit).await? {
            if index > 0 {
                price_metrics::failover(route, role, entry.company);
            }
            return Ok(Some(observation));
        }
    }
    Ok(None)
}
async fn observe(
    entry: &Entry,
    route: &RouteFile,
    role: &str,
    audit: &mut Value,
) -> Result<Option<Observation>, Value> {
    let result = tokio::time::timeout(Duration::from_secs(30), entry.source.evidence()).await;
    let result = match result {
        Ok(r) => r,
        Err(_) => Err(PriceError::Timeout),
    };
    let now = validation_time()?;
    let result = result.and_then(|(o, evidence)| {
        if now
            .value()
            .checked_sub(o.observed_at.value())
            .is_none_or(|age| age > entry.max_age_s.unwrap_or(route.pricing.max_age_s))
        {
            Err(PriceError::Stale)
        } else {
            Ok((o, evidence))
        }
    });
    let (evidence, observation) = match result {
        Ok((o, data)) => (
            json!({"role":role, "company":entry.company, "source":o.source.as_str(), "price_scaled":o.price.value().to_string(), "observed_at":o.observed_at.value(), "age_s":now.value().saturating_sub(o.observed_at.value()), "data":data}),
            Some(o),
        ),
        Err(error) => {
            let code = error_code(&error);
            if let Some(list) = audit["observations"].as_array_mut() {
                list.push(json!({"role":role,"company":entry.company,"source":entry.company,"error":code,"data": match &error {PriceError::Feed {evidence,..} => evidence.clone(), _ => Value::Null}}));
            }
            price_metrics::health(route, role, entry.company, false);
            if code == "divergent" {
                audit["decision"] = json!("divergent");
                price_metrics::source_event(route, role, entry.company, "divergent");
                return Err(json!({"stage":"pricing", "error":"divergent", "quote":audit}));
            }
            return Ok(None);
        }
    };
    if let Some(list) = audit["observations"].as_array_mut() {
        list.push(evidence);
    }
    price_metrics::health(route, role, entry.company, true);
    Ok(observation)
}
fn error_code(error: &PriceError) -> &'static str {
    match error {
        PriceError::Feed { class, .. } => class,
        PriceError::Timeout => "timeout",
        PriceError::RpcUnavailable => "outage",
        PriceError::Stale => "stale",
        PriceError::Disagreement => "divergent",
        PriceError::SequencerUnavailable => "sequencer_down",
        PriceError::HttpStatus(_) | PriceError::Request(_) => "outage",
        _ => "malformed",
    }
}

/// A validated current price plus provider evidence safe to persist.
pub struct ValidatedQuote {
    /// Validated eight-decimal USD price.
    pub price: ScaledPrice,
    /// Sanitized source observations and validation mode.
    pub evidence: Value,
}

fn validation_time() -> Result<UnixSeconds, Value> {
    u64::try_from(Utc::now().timestamp())
        .map(UnixSeconds::new)
        .map_err(|_| json!({"stage": "pricing", "error": "invalid_clock"}))
}

/// Stable validation error code shared with transition evidence.
pub(crate) const fn valuation_error_code(error: &ValuationError) -> &'static str {
    match error {
        ValuationError::Stale { .. } => "stale",
        ValuationError::Divergent { .. } => "divergent",
        ValuationError::FxDepeg => "fx_depeg",
        ValuationError::FxMissing => "fx_missing",
        ValuationError::Depeg => "depeg",
        ValuationError::BelowMinimum => "below_minimum",
        ValuationError::ArithmeticOutOfRange | ValuationError::Credit(_) => "out_of_range",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use topup_core::valuation::SourceId;
    struct Fixture {
        id: &'static str,
        price: u64,
        age: u64,
        error: Option<PriceError>,
    }
    #[async_trait]
    impl PriceSource for Fixture {
        async fn observe(&self) -> Result<Observation, PriceError> {
            if let Some(e) = &self.error {
                return Err(e.clone());
            }
            Ok(Observation {
                source: SourceId::new(self.id),
                price: ScaledPrice::new(self.price, 8).unwrap(),
                observed_at: UnixSeconds::new(
                    validation_time().unwrap().value().saturating_sub(self.age),
                ),
            })
        }
    }
    fn entry(id: &'static str, price: u64, age: u64, error: Option<PriceError>) -> Entry {
        Entry {
            source: Arc::new(Fixture {
                id,
                price,
                age,
                error,
            }),
            company: id,
            asset: None,
            max_age_s: None,
            usdt_quoted: true,
        }
    }
    fn runtime() -> PricingRuntime {
        PricingRuntime {
            sources: vec![],
            primary: vec![entry("kraken", 10_000_000, 0, None)],
            check: vec![entry("binance", 10_000_000, 0, None)],
            fx: vec![entry("chainlink", 100_000_000, 0, None)],
            sequencer: None,
            stuck_since: std::sync::Mutex::new(None),
        }
    }
    fn route() -> RouteFile {
        serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml")).unwrap()
    }
    #[tokio::test]
    async fn each_source_outage_stale_and_malformed_halts_volatile() {
        for role in ["primary", "check", "fx"] {
            for fault in [
                Some(PriceError::HttpStatus(503)),
                Some(PriceError::MalformedResponse("body")),
                Some(PriceError::Stale),
                None,
            ] {
                let mut r = runtime();
                let broken = entry(role, 10_000_000, 1000, fault);
                match role {
                    "primary" => r.primary = vec![broken],
                    "check" => r.check = vec![broken],
                    _ => r.fx = vec![broken],
                }
                assert!(r.fetch(&route()).await.is_err(), "{role}");
            }
        }
        assert!(runtime().fetch(&route()).await.is_ok());
    }
    #[tokio::test]
    async fn failover_only_on_unavailability_not_disagreement_or_depeg() {
        let mut r = runtime();
        r.primary.insert(
            0,
            entry("offline", 10_000_000, 0, Some(PriceError::HttpStatus(503))),
        );
        assert!(r.fetch(&route()).await.is_ok());
        r.primary[0] = entry("offline", 10_000_000, 0, Some(PriceError::Disagreement));
        assert_eq!(r.fetch(&route()).await.err().unwrap()["error"], "divergent");
        r.primary[0] = entry("offline", 20_000_000, 0, None);
        assert_eq!(r.fetch(&route()).await.err().unwrap()["error"], "divergent");
        r.fx.insert(0, entry("depeg", 90_000_000, 0, None));
        assert_eq!(r.fetch(&route()).await.err().unwrap()["error"], "fx_depeg");
    }
    #[tokio::test]
    async fn usd_check_is_not_multiplied_by_usdt_fx() {
        let mut r = runtime();
        r.check[0].usdt_quoted = false;
        r.fx[0] = entry("chainlink", 100_500_000, 0, None);
        let mut route = route();
        route.pricing.max_deviation_bps = topup_core::money::Bps::new(0).unwrap();
        assert!(r.fetch(&route).await.is_ok());
        r.check[0].usdt_quoted = true;
        assert_eq!(r.fetch(&route).await.err().unwrap()["error"], "divergent");
        r.check[0].usdt_quoted = false;
        r.fx[0] = entry("chainlink", 90_000_000, 0, None);
        assert_eq!(r.fetch(&route).await.err().unwrap()["error"], "fx_depeg");
    }
    #[tokio::test]
    async fn stablecoin_any_fresh_depeg_overrides_all_healthy_sources() {
        let mut route = route();
        route.pricing.mode = PricingMode::Stablecoin;
        for id in ["chainlink", "kraken"] {
            let mut r = runtime();
            r.sources = vec![
                entry("healthy", 100_000_000, 0, None),
                entry(id, 98_000_000, 0, None),
            ];
            assert_eq!(r.fetch(&route).await.err().unwrap()["error"], "depeg");
            r.sources[1] = entry(id, 98_000_000, 1000, None);
            assert_eq!(
                r.fetch(&route).await.ok().unwrap().price.value(),
                100_000_000
            );
            r.sources[0] = entry("healthy", 100_000_000, 1000, None);
            assert!(r.fetch(&route).await.is_err());
        }
    }
    #[tokio::test]
    async fn heartbeat_fresh_chainlink_is_not_subject_to_exchange_age() {
        let mut r = runtime();
        let mut route = route();
        route.pricing.mode = PricingMode::Stablecoin;
        let mut cl = entry("chainlink", 100_000_000, 3600, None);
        cl.max_age_s = Some(82860);
        r.sources = vec![cl];
        assert!(r.fetch(&route).await.is_ok());
    }
    #[tokio::test]
    async fn another_stablecoin_cannot_authorize_credit_for_a_missing_asset_price() {
        let mut route = route();
        route.pricing.mode = PricingMode::Stablecoin;
        route.asset.symbol = "usdc".into();
        let mut r = runtime();
        let mut usdc = entry("kraken", 100_000_000, 1000, None);
        usdc.asset = Some("usdc".into());
        let mut usdt = entry("chainlink", 100_000_000, 0, None);
        usdt.asset = Some("usdt".into());
        r.sources = vec![usdc, usdt];
        assert!(r.fetch(&route).await.is_err());
    }
}
