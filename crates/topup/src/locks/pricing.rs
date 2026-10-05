//! Shared fail-closed pricing for quotes and deposit credit.
use crate::{
    observability::price_metrics,
    routes::RouteSet,
    rpc_groups::{BASE_CHAIN_ID, price_pair},
};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use topup_adapters::pricing::{
    Observation, PriceError, PriceQuote, PriceSource, binance::Binance, chainlink::Chainlink,
    kraken::Kraken, uniswap_v2::UniswapV2,
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
/// A pricing refusal with sanitized evidence, converted to JSON at persistence boundaries.
#[derive(Debug)]
pub struct PricingFailure {
    /// Stable failure classification.
    pub code: &'static str,
    /// Sanitized quote audit, or null when no quote audit exists.
    pub evidence: Value,
}
impl PricingFailure {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            evidence: Value::Null,
        }
    }
    fn with_audit(code: &'static str, audit: &Audit) -> Self {
        Self {
            code,
            evidence: json!(audit),
        }
    }
}
#[derive(Default, Serialize)]
struct Audit {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<PricingMode>,
    observations: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequencer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'static str>,
}
fn fresh(bound: u64, observed_at: UnixSeconds, now: UnixSeconds) -> bool {
    now.value()
        .checked_sub(observed_at.value())
        .is_some_and(|age| age <= bound)
}
struct Entry {
    source: Arc<dyn PriceSource>,
    company: &'static str,
    source_id: &'static str,
    asset: Option<String>,
    max_age_s: Option<u64>,
    usdt_quoted: bool,
    descriptor: Option<Value>,
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
/// Shared pricing runtimes indexed by attested route name and version.
pub type PricingRuntimes = Arc<BTreeMap<(String, u64), Arc<PricingRuntime>>>;

impl PricingRuntime {
    /// Constructs only explicitly configured sources, never a restricted default.
    pub fn configured(
        route: &RouteFile,
        routes: &RouteSet,
        pool: sqlx::PgPool,
    ) -> Result<Self, String> {
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
                        Source::UniswapV2Twap { twap, .. } => {
                            let (a, b, _) = price_pair(route, s)?;
                            Arc::new(
                                UniswapV2::new(
                                    routes.resolved_price_group(&a)?,
                                    routes.resolved_price_group(&b)?,
                                    twap.clone(),
                                    Arc::new(crate::db::pricing::TwapStore(pool.clone())),
                                )
                                .map_err(|e| e.to_string())?,
                            )
                        }
                        Source::Chainlink { feed: name, .. } => {
                            let (a, b, chain) = price_pair(route, s)?;
                            Arc::new(Chainlink::new(
                                routes.resolved_price_group(&a)?,
                                routes.resolved_price_group(&b)?,
                                feed(name, chain).ok_or("unsupported feed")?,
                            ))
                        }
                    };
                    let max_age_s = match s {
                        Source::UniswapV2Twap { twap, .. } => Some(twap.max_sample_age_s),
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
                        source_id: match s {
                            Source::UniswapV2Twap { .. } => "uniswap_v2_twap",
                            _ => s.company(),
                        },
                        asset: Some(s.asset().to_owned()),
                        max_age_s,
                        usdt_quoted: matches!(s, Source::Binance { .. }),
                        descriptor: Some(
                            serde_json::to_value(s).map_err(|_| "price source encoding failed")?,
                        ),
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
                        feed(&s.feed, BASE_CHAIN_ID).ok_or("unsupported sequencer feed")?,
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
    fn source_ids(&self) -> impl Iterator<Item = (&'static str, &'static str)> + Clone {
        self.sources
            .iter()
            .chain(&self.primary)
            .chain(&self.check)
            .chain(&self.fx)
            .map(|entry| (entry.company, entry.source_id))
    }
    /// Accumulate TWAP history even when no merchant requests a quote.
    pub async fn sample_twaps(&self, route: &RouteFile) {
        for (role, entries) in [("primary", &self.primary), ("check", &self.check)] {
            for entry in entries.iter().filter(|e| e.company == "uniswap-v2-onchain") {
                let mut audit = Audit::default();
                if let Err(mut failure) = observe(entry, route, role, &mut audit).await {
                    if audit.decision.is_some() {
                        failure.evidence = json!(audit);
                    }
                    price_metrics::failure(route, &failure);
                }
            }
        }
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
            source_id: "injected",
            asset: None,
            max_age_s: None,
            usdt_quoted: true,
            descriptor: None,
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
    pub async fn fetch(&self, route: &RouteFile) -> Result<ValidatedQuote, PricingFailure> {
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
    async fn fetch_inner(&self, route: &RouteFile) -> Result<ValidatedQuote, PricingFailure> {
        let mut audit = Audit {
            mode: Some(route.pricing.mode),
            ..Audit::default()
        };
        if let Some((sequencer, grace)) = &self.sequencer {
            match sequencer.sequencer(*grace).await {
                Ok(evidence) => audit.sequencer = Some(evidence),
                Err(error) => {
                    let code = error_code(&error);
                    if let PriceError::Feed { evidence, .. } = error {
                        audit.sequencer = Some(evidence);
                    }
                    if code == "divergent" {
                        price_metrics::source_event(
                            route,
                            "sequencer",
                            "chainlink",
                            "chainlink",
                            code,
                        );
                    }
                    audit.decision = Some(code);
                    let failure = PricingFailure::with_audit(code, &audit);
                    price_metrics::decision(route, code, &failure.evidence, self.source_ids());
                    return Err(failure);
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
                let quote =
                    observe(entry, route, "sources", &mut audit)
                        .await
                        .map_err(|mut failure| {
                            if audit.decision.is_some() {
                                failure.evidence = json!(audit);
                            }
                            failure
                        })?;
                if let Some(quote) = quote {
                    observed.push((
                        quote.valuation,
                        entry.max_age_s.unwrap_or(route.pricing.max_age_s),
                    ));
                }
            }
            let now = validation_time()?;
            for (o, bound) in observed {
                if !fresh(bound, o.observed_at, now) {
                    continue;
                }
                healthy = true;
                let mut policy = ValuationPolicy::from(&route.pricing);
                policy.max_age_s = bound;
                policy.max_deviation_bps = route.pricing.peg_band_bps;
                if stablecoin_price(&o, now, policy).is_err() {
                    price_metrics::source_event(
                        route,
                        "sources",
                        o.source.as_str(),
                        o.source.as_str(),
                        "depeg",
                    );
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
            let mut pa = Audit::default();
            let mut ca = Audit::default();
            let mut fa = Audit::default();
            let (primary, check, fx) = tokio::join!(
                first(&self.primary, route, "primary", &mut pa),
                first(&self.check, route, "check", &mut ca),
                first(&self.fx, route, "fx", &mut fa)
            );
            audit.observations.extend(
                pa.observations
                    .into_iter()
                    .chain(ca.observations)
                    .chain(fa.observations),
            );
            if let Some(error) = [&primary, &check, &fx]
                .iter()
                .find_map(|r| r.as_ref().err())
            {
                audit.decision = Some(error.code);
                return Err(PricingFailure::with_audit(error.code, &audit));
            }
            let primary = primary?;
            let check = check?;
            let fx = fx?;
            match (primary, check, fx) {
                (Some((pe, p)), Some((ce, c)), Some((fe, f)))
                    if p.valuation.source != c.valuation.source
                        && f.valuation.source != c.valuation.source =>
                {
                    // Compare current markets, then retain the primary's conservative valuation.
                    let p_agreement = Observation {
                        price: p.agreement_price,
                        ..p.valuation.clone()
                    };
                    let c_agreement = Observation {
                        price: c.agreement_price,
                        ..c.valuation.clone()
                    };
                    let (p, c, f) = (p.valuation, c.valuation, f.valuation);
                    let now = validation_time()?;
                    for (o, entry) in [(&p, pe), (&c, ce), (&f, fe)] {
                        let bound = entry.max_age_s.unwrap_or(route.pricing.max_age_s);
                        if !fresh(bound, o.observed_at, now) {
                            return Err(PricingFailure::with_audit("stale", &audit));
                        }
                    }
                    let mut policy = ValuationPolicy::from(&route.pricing);
                    policy.max_age_s = u64::MAX;
                    let usdt_quoted = ce.usdt_quoted;
                    let mut fx_policy = policy;
                    fx_policy.max_deviation_bps = policy.max_fx_deviation_bps;
                    stablecoin_price(&f, now, fx_policy)
                        .map_err(|_| "fx_depeg")
                        .and_then(|dollar| {
                            validate_spot(
                                &p_agreement,
                                &c_agreement,
                                Some(&FxObservation {
                                    source: f.source,
                                    rate: if usdt_quoted { f.price } else { dollar },
                                    observed_at: f.observed_at,
                                }),
                                now,
                                policy,
                            )
                            .map(|_| p.price)
                            .map_err(|e| valuation_error_code(&e))
                        })
                }
                (Some(_), Some(_), Some(_)) => Err("company_overlap"),
                _ => Err(
                    if audit.observations.iter().any(|o| o["error"] == "stale") {
                        "stale"
                    } else {
                        "source_failure"
                    },
                ),
            }
        };
        match result {
            Ok(price) => {
                audit.decision = Some("accepted");
                Ok(ValidatedQuote {
                    price,
                    evidence: json!(audit),
                })
            }
            Err(code) => {
                audit.decision = Some(code);
                let failure = PricingFailure::with_audit(code, &audit);
                price_metrics::decision(route, code, &failure.evidence, self.source_ids());
                Err(failure)
            }
        }
    }
}
async fn first<'a>(
    entries: &'a [Entry],
    route: &RouteFile,
    role: &str,
    audit: &mut Audit,
) -> Result<Option<(&'a Entry, PriceQuote)>, PricingFailure> {
    for (index, entry) in entries.iter().enumerate() {
        if let Some(observation) = observe(entry, route, role, audit).await? {
            if index > 0 {
                price_metrics::failover(route, role, entry.source_id, entry.company);
            }
            return Ok(Some((entry, observation)));
        }
    }
    Ok(None)
}
async fn observe(
    entry: &Entry,
    route: &RouteFile,
    role: &str,
    audit: &mut Audit,
) -> Result<Option<PriceQuote>, PricingFailure> {
    let result = tokio::time::timeout(Duration::from_secs(30), entry.source.quote()).await;
    let result = match result {
        Ok(r) => r,
        Err(_) => Err(PriceError::Timeout),
    };
    let now = validation_time()?;
    let result = result.and_then(|quote| {
        let o = &quote.valuation;
        if !fresh(
            entry.max_age_s.unwrap_or(route.pricing.max_age_s),
            o.observed_at,
            now,
        ) {
            Err(PriceError::Stale)
        } else {
            Ok(quote)
        }
    });
    let (evidence, observation) = match result {
        Ok(quote) => {
            let o = &quote.valuation;
            (
                json!({"role":role, "company":entry.company, "source":o.source.as_str(), "descriptor":entry.descriptor, "price_scaled":o.price.value().to_string(), "agreement_price_scaled":quote.agreement_price.value().to_string(), "observed_at":o.observed_at.value(), "age_s":now.value().saturating_sub(o.observed_at.value()), "data":quote.evidence}),
                Some(quote),
            )
        }
        Err(error) => {
            let code = error_code(&error);
            audit.observations.push(json!({"role":role,"company":entry.company,"source":entry.source_id,"descriptor":entry.descriptor,"error":code,"data": match &error {PriceError::Feed {evidence,..} => evidence.clone(), _ => Value::Null}}));
            price_metrics::health(route, role, entry.source_id, entry.company, false);
            if code.starts_with("twap_") {
                price_metrics::refusal(route, role, entry.source_id, entry.company, code);
                audit.decision = Some(code);
                return Err(PricingFailure::new(code));
            }
            if code == "divergent" {
                audit.decision = Some("divergent");
                price_metrics::source_event(
                    route,
                    role,
                    entry.source_id,
                    entry.company,
                    "divergent",
                );
                return Err(PricingFailure::new("divergent"));
            }
            return Ok(None);
        }
    };
    audit.observations.push(evidence);
    price_metrics::health(route, role, entry.source_id, entry.company, true);
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

fn validation_time() -> Result<UnixSeconds, PricingFailure> {
    #[cfg(test)]
    if let Ok(now) = tests::GOLDEN_NOW.try_with(|now| *now) {
        return Ok(UnixSeconds::new(now));
    }
    u64::try_from(Utc::now().timestamp())
        .map(UnixSeconds::new)
        .map_err(|_| PricingFailure::new("invalid_clock"))
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
    tokio::task_local! {
        pub(super) static GOLDEN_NOW: u64;
    }

    // Literal serialized snapshots pin persisted field names, nulls, ordering, and types.
    fn golden_observation(role: &str, company: &str, price: &str) -> String {
        format!(
            r#"{{"age_s":0,"agreement_price_scaled":"{price}","company":"{company}","data":null,"descriptor":null,"observed_at":1700000000,"price_scaled":"{price}","role":"{role}","source":"{company}"}}"#
        )
    }

    fn golden_audit(mode: &str, decision: &str, observations: &[String]) -> String {
        format!(
            r#"{{"decision":"{decision}","mode":"{mode}","observations":[{}]}}"#,
            observations.join(",")
        )
    }

    fn golden_failure(code: &str, audit: &str) -> String {
        format!(r#"{{"error":"{code}","quote":{audit},"stage":"pricing"}}"#)
    }

    #[tokio::test]
    async fn golden_persisted_pricing_json() {
        GOLDEN_NOW.scope(1_700_000_000, golden_cases()).await;
    }

    async fn golden_cases() {
        let route = route();
        let volatile = [
            golden_observation("primary", "kraken", "10000000"),
            golden_observation("check", "binance", "10000000"),
            golden_observation("fx", "chainlink", "100000000"),
        ];
        let quote = runtime().fetch(&route).await.ok().unwrap();
        assert_eq!(
            serde_json::to_vec(&quote.evidence).unwrap(),
            golden_audit("volatile", "accepted", &volatile).as_bytes()
        );

        let mut r = runtime();
        r.primary[0] = entry("kraken", 20_000_000, 0, None);
        let mut divergent = volatile.clone();
        divergent[0] = golden_observation("primary", "kraken", "20000000");
        assert_eq!(
            serde_json::to_vec(&Value::from(r.fetch(&route).await.err().unwrap())).unwrap(),
            golden_failure(
                "divergent",
                &golden_audit("volatile", "divergent", &divergent)
            )
            .as_bytes()
        );

        r.primary[0] = entry("kraken", 10_000_000, 1000, None);
        let mut stale = volatile.clone();
        stale[0] = r#"{"company":"kraken","data":null,"descriptor":null,"error":"stale","role":"primary","source":"kraken"}"#.into();
        assert_eq!(
            serde_json::to_vec(&Value::from(r.fetch(&route).await.err().unwrap())).unwrap(),
            golden_failure("stale", &golden_audit("volatile", "stale", &stale)).as_bytes()
        );

        for code in [
            "twap_history",
            "twap_liquidity",
            "twap_stale_sample",
            "twap_spot_divergence",
            "twap_sample_jump",
            "twap_storage",
            "twap_reorg",
            "twap_sample_order",
            "twap_token_order",
        ] {
            r.primary = vec![
                entry(
                    "uniswap-v2-onchain",
                    10_000_000,
                    0,
                    Some(PriceError::Feed {
                        class: code,
                        evidence: json!({"window_s":1800}),
                    }),
                ),
                entry("fallback", 10_000_000, 0, None),
            ];
            let mut refused = volatile.clone();
            refused[0] = format!(
                r#"{{"company":"uniswap-v2-onchain","data":{{"window_s":1800}},"descriptor":null,"error":"{code}","role":"primary","source":"uniswap_v2_twap"}}"#
            );
            assert_eq!(
                serde_json::to_vec(&Value::from(r.fetch(&route).await.err().unwrap())).unwrap(),
                golden_failure(code, &golden_audit("volatile", code, &refused)).as_bytes(),
                "{code}"
            );
        }

        let mut route = route;
        route.pricing.mode = PricingMode::Stablecoin;
        r.sources = vec![
            entry("healthy", 100_000_000, 0, None),
            entry("chainlink", 100_000_000, 0, None),
        ];
        let mut stable = [
            golden_observation("sources", "healthy", "100000000"),
            golden_observation("sources", "chainlink", "100000000"),
        ];
        let quote = r.fetch(&route).await.ok().unwrap();
        assert_eq!(
            serde_json::to_vec(&quote.evidence).unwrap(),
            golden_audit("stablecoin", "accepted", &stable).as_bytes()
        );
        r.sources[1] = entry("chainlink", 98_000_000, 0, None);
        stable[1] = golden_observation("sources", "chainlink", "98000000");
        assert_eq!(
            serde_json::to_vec(&Value::from(r.fetch(&route).await.err().unwrap())).unwrap(),
            golden_failure("depeg", &golden_audit("stablecoin", "depeg", &stable)).as_bytes()
        );
    }

    fn reversed_group_route() -> RouteFile {
        let mut route = route();
        route.chain.rpc_providers = vec!["b".into(), "a".into()];
        route.pricing.fx = vec![Source::Chainlink {
            feed: "USDT_USD".into(),
            chain_id: 1,
            rpc_group: "a".into(),
            rpc_group_b: None,
            observation_chain_id: None,
        }];
        route
    }

    struct RegressionConfig {
        groups: BTreeMap<String, crate::rpc_groups::GroupSpec>,
        companies: BTreeMap<String, crate::rpc_groups::Company>,
        budgets: BTreeMap<String, topup_adapters::chain::evm::group::budget::BudgetSpec>,
    }

    fn regression_config(a: &str, b: &str) -> RegressionConfig {
        use crate::rpc_groups::{Company, GroupSpec, MemberSpec};
        use std::collections::BTreeMap;
        use topup_adapters::chain::evm::group::{GroupPolicy, budget::BudgetSpec};
        let groups = [("a", a), ("b", b)]
            .into_iter()
            .map(|(id, url)| {
                (
                    id.into(),
                    GroupSpec {
                        chain_id: 1,
                        policy: GroupPolicy::default(),
                        members: vec![MemberSpec {
                            id: format!("price-{id}"),
                            company: format!("provider-{id}"),
                            url: url.into(),
                            sealed_key: None,
                            account_budget: "account".into(),
                            key_budget: "key".into(),
                            priority: 0,
                            weight: 1,
                        }],
                    },
                )
            })
            .collect();
        let companies = BTreeMap::from([
            (
                "provider-a".into(),
                Company {
                    domains: vec!["127.0.0.1".into()],
                },
            ),
            (
                "provider-b".into(),
                Company {
                    domains: vec!["localhost".into()],
                },
            ),
        ]);
        let budgets = ["account", "key"]
            .into_iter()
            .map(|id| {
                (
                    id.into(),
                    BudgetSpec {
                        requests_per_second: 100,
                        burst: 100,
                    },
                )
            })
            .collect();
        RegressionConfig {
            groups,
            companies,
            budgets,
        }
    }

    struct RegressionRpc {
        url: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for RegressionRpc {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn regression_rpc(answer: i64) -> RegressionRpc {
        use alloy::sol_types::SolCall;
        use alloy_primitives::{B256, I256, U256, Uint};
        use axum::{Json, Router, routing::post};
        use topup_adapters::pricing::chainlink::{
            decimalsCall, latestRoundDataCall, latestRoundDataReturn,
        };
        let now = validation_time().unwrap().value();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handler = move |Json(request): Json<Value>| async move {
            let result = match request["method"].as_str().unwrap() {
                "eth_blockNumber" => json!("0x64"),
                "eth_getBlockByNumber" => {
                    let mut block = serde_json::to_value(alloy::rpc::types::Block::<
                        alloy::rpc::types::Transaction,
                    >::default())
                    .unwrap();
                    block["number"] = if request["params"][0] == "latest" {
                        json!("0x64")
                    } else {
                        request["params"][0].clone()
                    };
                    block["hash"] = json!(B256::repeat_byte(1));
                    block["parentHash"] = json!(B256::repeat_byte(2));
                    block["timestamp"] = json!(format!("0x{now:x}"));
                    block
                }
                "eth_call" => {
                    assert_eq!(request["params"][1], "0x62");
                    let call = &request["params"][0];
                    let input = call["input"]
                        .as_str()
                        .or_else(|| call["data"].as_str())
                        .unwrap();
                    let data = if input.starts_with("0x313ce567") {
                        decimalsCall::abi_encode_returns(&8)
                    } else {
                        latestRoundDataCall::abi_encode_returns(&latestRoundDataReturn {
                            roundId: Uint::<80, 2>::from(20),
                            answer: I256::try_from(answer).unwrap(),
                            startedAt: U256::from(now),
                            updatedAt: U256::from(now),
                            answeredInRound: Uint::<80, 2>::from(20),
                        })
                    };
                    json!(format!("0x{}", hex::encode(data)))
                }
                _ => panic!("unexpected RPC"),
            };
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
        };
        let app = Router::new().route("/", post(handler));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        RegressionRpc { url, task }
    }

    #[tokio::test]
    async fn runtime_reads_the_reversed_pair_validation_approved() {
        let route = reversed_group_route();
        let a = regression_rpc(101_000_000).await;
        let b = regression_rpc(100_000_000).await;
        let RegressionConfig {
            groups,
            companies,
            budgets,
        } = regression_config(&a.url, &b.url.replace("127.0.0.1", "localhost"));
        crate::rpc_groups::validate(std::slice::from_ref(&route), &groups, &companies, &budgets)
            .unwrap();
        let expected = ("b".into(), "a".into(), 1);
        assert_eq!(price_pair(&route, &route.pricing.fx[0]).unwrap(), expected);
        assert_eq!(
            crate::rpc_groups::price_pairs(&route).unwrap(),
            vec![expected]
        );
        let clients = crate::rpc_groups::clients(&groups, &budgets, |_| None).unwrap();
        for client in clients.values() {
            client.group().unwrap().verified(0, true);
        }
        let routes = RouteSet::with_groups(vec![route.clone()], clients).unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/pricing-regression")
            .unwrap();
        let runtime = PricingRuntime::configured(&route, &routes, pool).unwrap();
        let error = runtime.fx[0].source.quote().await.unwrap_err();
        let PriceError::Feed { class, evidence } = error else {
            panic!("expected A/B disagreement, got {error:?}");
        };
        assert_eq!(class, "divergent");
        // Distinct answers pin both the identity and the order of the groups actually read.
        assert_eq!(evidence["a"]["answer"], "100000000");
        assert_eq!(evidence["b"]["answer"], "101000000");
    }

    #[test]
    fn validation_rejects_a_price_pair_resolving_to_one_group() {
        let mut route = reversed_group_route();
        let Source::Chainlink { rpc_group_b, .. } = &mut route.pricing.fx[0] else {
            unreachable!();
        };
        *rpc_group_b = Some("a".into());
        let RegressionConfig {
            groups,
            companies,
            budgets,
        } = regression_config("http://127.0.0.1:1", "http://localhost:2");
        assert_eq!(
            crate::rpc_groups::price_pairs(&route).unwrap(),
            vec![("b".into(), "b".into(), 1)]
        );
        assert_eq!(
            crate::rpc_groups::validate(&[route], &groups, &companies, &budgets).unwrap_err(),
            "price RPC A/B must have matching chain and disjoint companies"
        );
    }

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
            source_id: if id == "uniswap-v2-onchain" {
                "uniswap_v2_twap"
            } else {
                id
            },
            asset: None,
            max_age_s: None,
            usdt_quoted: true,
            descriptor: None,
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
    struct TwapFixture {
        spot_units: u64,
    }
    #[async_trait]
    impl PriceSource for TwapFixture {
        async fn observe(&self) -> Result<Observation, PriceError> {
            self.quote().await.map(|q| q.valuation)
        }
        async fn quote(&self) -> Result<PriceQuote, PriceError> {
            use alloy_primitives::{B256, U256};
            use topup_adapters::pricing::uniswap_v2::{
                Sample, average, usd_price, valuation_price,
            };
            let now = validation_time().unwrap().value();
            let twap = U256::from(10_000) << 112_usize;
            let spot = U256::from(self.spot_units) << 112_usize;
            let mut history: Vec<_> = (0..=30_u64)
                .map(|i| Sample {
                    block: i,
                    hash: B256::ZERO,
                    timestamp: now - 1800 + i * 60,
                    cumulative: twap * U256::from(i * 60),
                    spot: twap,
                })
                .collect();
            history.last_mut().unwrap().spot = spot;
            let current = history.last().unwrap();
            let ratio = average(
                &history,
                current,
                now,
                &topup_core::price::TwapConfig::default(),
            )?
            .0;
            let eth = ScaledPrice::new(100_000_000, 8).unwrap();
            Ok(PriceQuote {
                valuation: Observation {
                    source: SourceId::new("uniswap_v2_twap"),
                    price: valuation_price(ratio, spot, eth)?,
                    observed_at: UnixSeconds::new(now),
                },
                agreement_price: usd_price(spot, eth)?,
                evidence: json!({"twap_usd_scaled":usd_price(ratio, eth)?.value().to_string()}),
            })
        }
    }
    #[tokio::test]
    async fn conservative_twap_valuation_checks_current_markets_and_pauses_fast_moves() {
        let mut route = route();
        route.pricing.max_deviation_bps = topup_core::money::Bps::new(100).unwrap();
        // Units are hundredths of the baseline: TWAP=10000, spot=9800 means a 2% drop.
        for (scenario, spot, kraken, expected, refusal) in [
            ("drop", 9800, 9800, 9800, None),
            ("pump", 10200, 10000, 10000, Some("divergent")),
            ("normal_rise", 10200, 10200, 10000, None),
            ("normal_drop", 9800, 9850, 9800, None),
            ("drop_boundary", 9700, 9700, 9700, None),
            ("rise_boundary", 10300, 10300, 10000, None),
            ("fast_drop", 9699, 9699, 9699, Some("twap_spot_divergence")),
            (
                "fast_rise",
                10301,
                10301,
                10000,
                Some("twap_spot_divergence"),
            ),
        ] {
            let source = Arc::new(TwapFixture { spot_units: spot });
            if scenario == "pump" {
                assert_eq!(
                    source.quote().await.unwrap().valuation.price.value(),
                    expected * 100_000_000
                );
            }
            let mut r = runtime();
            r.primary[0] = Entry {
                source,
                company: "uniswap-v2-onchain",
                ..entry("uniswap_v2_twap", 0, 0, None)
            };
            r.check = vec![entry("kraken", kraken * 100_000_000, 0, None)];
            r.check[0].usdt_quoted = false;
            let result = r.fetch(&route).await;
            if let Some(code) = refusal {
                assert_eq!(result.err().unwrap().code, code, "{scenario}");
            } else {
                let quote = result.ok().unwrap();
                assert_eq!(quote.price.value(), expected * 100_000_000, "{scenario}");
                let primary = quote.evidence["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|o| o["role"] == "primary")
                    .unwrap();
                assert_eq!(
                    primary["agreement_price_scaled"],
                    (spot * 100_000_000).to_string()
                );
            }
        }
    }
    #[tokio::test]
    async fn twap_uses_sample_age_and_safety_refusals_cannot_fail_over() {
        let mut r = runtime();
        let mut twap = entry("uniswap_v2_twap", 10_000_000, 120, None);
        twap.company = "uniswap-v2-onchain";
        twap.max_age_s = Some(180);
        r.primary = vec![twap];
        r.check = vec![entry("kraken", 10_000_000, 0, None)];
        r.check[0].usdt_quoted = false;
        assert!(
            r.fetch(&route()).await.is_ok(),
            "TWAP sample age is distinct from the 90-second ticker age"
        );
        for code in [
            "twap_history",
            "twap_liquidity",
            "twap_stale_sample",
            "twap_spot_divergence",
            "twap_sample_jump",
            "twap_storage",
            "twap_reorg",
        ] {
            r.primary[0] = entry(
                "uniswap-v2-onchain",
                10_000_000,
                0,
                Some(PriceError::Feed {
                    class: code,
                    evidence: json!({"window_s":1800}),
                }),
            );
            r.primary.push(entry("fallback", 10_000_000, 0, None));
            let failure = r.fetch(&route()).await.err().unwrap();
            assert_eq!(failure.code, code);
            assert!(
                failure.evidence["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|o| o["source"] == "uniswap_v2_twap"
                        && o["company"] == "uniswap-v2-onchain")
            );
            r.primary.pop();
        }
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
        assert_eq!(r.fetch(&route()).await.err().unwrap().code, "divergent");
        r.primary[0] = entry("offline", 20_000_000, 0, None);
        assert_eq!(r.fetch(&route()).await.err().unwrap().code, "divergent");
        r.fx.insert(0, entry("depeg", 90_000_000, 0, None));
        assert_eq!(r.fetch(&route()).await.err().unwrap().code, "fx_depeg");
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
        assert_eq!(r.fetch(&route).await.err().unwrap().code, "divergent");
        r.check[0].usdt_quoted = false;
        r.fx[0] = entry("chainlink", 90_000_000, 0, None);
        assert_eq!(r.fetch(&route).await.err().unwrap().code, "fx_depeg");
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
            assert_eq!(r.fetch(&route).await.err().unwrap().code, "depeg");
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
