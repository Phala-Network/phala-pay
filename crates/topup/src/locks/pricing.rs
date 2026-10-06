//! Shared fail-closed pricing for quotes and deposit credit.
use crate::{
    observability::price_metrics,
    routes::RouteSet,
    rpc_groups::{BASE_CHAIN_ID, price_group, price_pair},
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
    pub(crate) fn new(code: &'static str) -> Self {
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
const SOURCE_REUSE: Duration = Duration::from_secs(12);

struct SharedSource {
    inner: Arc<dyn PriceSource>,
    reuse: Duration,
    slot: tokio::sync::Mutex<Option<Cached>>,
    source_id: &'static str,
    company: &'static str,
}
struct Cached {
    quote: PriceQuote,
    completed_at: tokio::time::Instant,
}
impl SharedSource {
    fn new(
        inner: Arc<dyn PriceSource>,
        reuse: Duration,
        source_id: &'static str,
        company: &'static str,
    ) -> Self {
        Self {
            inner,
            reuse,
            slot: tokio::sync::Mutex::new(None),
            source_id,
            company,
        }
    }
    fn get_or_build(
        sources: &mut BTreeMap<String, Arc<Self>>,
        key: String,
        reuse: Duration,
        source_id: &'static str,
        company: &'static str,
        build: impl FnOnce() -> Result<Arc<dyn PriceSource>, String>,
    ) -> Result<Arc<Self>, String> {
        if let Some(source) = sources.get(&key) {
            return Ok(source.clone());
        }
        let source = Arc::new(Self::new(build()?, reuse, source_id, company));
        sources.insert(key, source.clone());
        Ok(source)
    }
    async fn quote_shared(&self) -> Result<(PriceQuote, Option<u64>), PriceError> {
        self.quote_with_reuse(self.reuse, tokio::time::Instant::now())
            .await
    }
    async fn quote_shared_fresh(&self) -> Result<(PriceQuote, Option<u64>), PriceError> {
        self.quote_shared_since(tokio::time::Instant::now()).await
    }
    async fn quote_shared_since(
        &self,
        arrived: tokio::time::Instant,
    ) -> Result<(PriceQuote, Option<u64>), PriceError> {
        self.quote_with_reuse(Duration::ZERO, arrived).await
    }
    async fn quote_with_reuse(
        &self,
        reuse: Duration,
        arrived: tokio::time::Instant,
    ) -> Result<(PriceQuote, Option<u64>), PriceError> {
        let mut slot = self.slot.lock().await;
        if let Some(cached) = slot.as_ref() {
            let coalesced = cached.completed_at >= arrived;
            if (coalesced || cached.completed_at.elapsed() <= reuse)
                && cached.quote.reuse_until.is_none_or(|until| {
                    validation_time().is_ok_and(|now| now.value() <= until.value())
                })
            {
                price_metrics::cache(
                    self.source_id,
                    self.company,
                    if coalesced { "coalesced" } else { "hit" },
                );
                let age_ms =
                    u64::try_from(cached.completed_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                return Ok((cached.quote.clone(), Some(age_ms)));
            }
        }
        price_metrics::cache(self.source_id, self.company, "miss");
        // Clear before awaiting so a cancelled leader leaves no reusable result behind.
        *slot = None;
        let quote = tokio::time::timeout(Duration::from_secs(30), self.inner.quote())
            .await
            .map_err(|_| PriceError::Timeout)??;
        *slot = Some(Cached {
            quote: quote.clone(),
            completed_at: tokio::time::Instant::now(),
        });
        Ok((quote, None))
    }
}
struct SharedSequencer {
    inner: Chainlink,
    grace_s: u64,
    slot: tokio::sync::Mutex<Option<(Value, tokio::time::Instant)>>,
}
impl SharedSequencer {
    async fn evidence(&self) -> Result<Value, PriceError> {
        let arrived = tokio::time::Instant::now();
        let mut slot = self.slot.lock().await;
        if let Some((evidence, completed_at)) = slot.as_ref()
            && *completed_at >= arrived
        {
            price_metrics::cache("sequencer", "chainlink", "coalesced");
            return Ok(evidence.clone());
        }
        price_metrics::cache("sequencer", "chainlink", "miss");
        *slot = None;
        let evidence = self.inner.sequencer(self.grace_s).await?;
        *slot = Some((evidence.clone(), tokio::time::Instant::now()));
        Ok(evidence)
    }
}
struct Entry {
    source: Arc<SharedSource>,
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
    sequencer: Option<Arc<SharedSequencer>>,
    stuck_since: std::sync::Mutex<Option<Instant>>,
}
/// Shared pricing runtimes indexed by attested route name and version.
pub type PricingRuntimes = Arc<BTreeMap<(String, u64), Arc<PricingRuntime>>>;

impl PricingRuntime {
    /// Builds one shared pricing runtime for every attested route version.
    pub fn build_all(routes: &RouteSet, pool: sqlx::PgPool) -> Result<PricingRuntimes, String> {
        let mut runtimes = BTreeMap::new();
        let mut sources = BTreeMap::new();
        let mut sequencers = BTreeMap::new();
        for route in routes.routes() {
            let key = (route.route.clone(), route.version);
            let runtime =
                Self::configured(route, routes, pool.clone(), &mut sources, &mut sequencers)?;
            if runtimes.insert(key.clone(), Arc::new(runtime)).is_some() {
                return Err(format!(
                    "duplicate pricing runtime for route `{}` version {}",
                    key.0, key.1
                ));
            }
        }
        Ok(Arc::new(runtimes))
    }
    /// Constructs only explicitly configured sources, never a restricted default.
    fn configured(
        route: &RouteFile,
        routes: &RouteSet,
        pool: sqlx::PgPool,
        sources: &mut BTreeMap<String, Arc<SharedSource>>,
        sequencers: &mut BTreeMap<String, Arc<SharedSequencer>>,
    ) -> Result<Self, String> {
        route
            .pricing
            .validate(route.chain.chain_id, &route.asset.symbol, route.livemode)
            .map_err(|e| e.to_string())?;
        let mut entries = |list: &[Source]| -> Result<Vec<Entry>, String> {
            list.iter()
                .map(|s| {
                    let source_id = match s {
                        Source::UniswapV2Twap { .. } => "uniswap_v2_twap",
                        _ => s.company(),
                    };
                    let source = match s {
                        Source::Coinmetrics { .. } => {
                            return Err("restricted legacy source requires migration".into());
                        }
                        Source::Kraken { symbol, .. } => SharedSource::get_or_build(
                            sources,
                            format!("kraken|{symbol}"),
                            Duration::ZERO,
                            source_id,
                            s.company(),
                            || {
                                Ok(Arc::new(
                                    Kraken::new(symbol.clone()).map_err(|e| e.to_string())?,
                                ))
                            },
                        )?,
                        Source::Binance { symbol, .. } => SharedSource::get_or_build(
                            sources,
                            format!("binance|{symbol}"),
                            Duration::ZERO,
                            source_id,
                            s.company(),
                            || {
                                Ok(Arc::new(
                                    Binance::new(symbol.clone()).map_err(|e| e.to_string())?,
                                ))
                            },
                        )?,
                        Source::UniswapV2Twap { twap, .. } => {
                            let (a, b, _) = price_pair(route, s)?;
                            let policy = serde_json::to_string(twap)
                                .map_err(|_| "price source encoding failed")?;
                            SharedSource::get_or_build(
                                sources,
                                format!("uniswap_v2_twap|{a}|{b}|{policy}"),
                                SOURCE_REUSE,
                                source_id,
                                s.company(),
                                || {
                                    Ok(Arc::new(
                                        UniswapV2::new(
                                            routes.resolved_price_group(&a)?,
                                            routes.resolved_price_group(&b)?,
                                            twap.clone(),
                                            Arc::new(crate::db::pricing::TwapStore(pool.clone())),
                                        )
                                        .map_err(|e| e.to_string())?,
                                    ))
                                },
                            )?
                        }
                        Source::Chainlink { feed: name, .. } => {
                            let (a, b, chain) = price_pair(route, s)?;
                            SharedSource::get_or_build(
                                sources,
                                format!("chainlink|{name}|{chain}|{a}|{b}"),
                                SOURCE_REUSE,
                                source_id,
                                s.company(),
                                || {
                                    Ok(Arc::new(Chainlink::new(
                                        routes.resolved_price_group(&a)?,
                                        routes.resolved_price_group(&b)?,
                                        feed(name, chain).ok_or("unsupported feed")?,
                                    )))
                                },
                            )?
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
                        source_id,
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
                let alias_chain = matches!(s.rpc_group.as_str(), "a" | "b")
                    .then_some(route.chain.chain_id)
                    .or_else(|| {
                        matches!(s.rpc_group_b.as_str(), "a" | "b").then_some(route.chain.chain_id)
                    });
                let key = format!(
                    "{}|{BASE_CHAIN_ID}|{}|{}|{}|{alias_chain:?}",
                    s.feed,
                    price_group(route, &s.rpc_group)?,
                    price_group(route, &s.rpc_group_b)?,
                    s.grace_s
                );
                if let Some(sequencer) = sequencers.get(&key) {
                    return Ok(sequencer.clone());
                }
                let sequencer = Arc::new(SharedSequencer {
                    inner: Chainlink::new(
                        routes.price_group(route, &s.rpc_group)?,
                        routes.price_group(route, &s.rpc_group_b)?,
                        feed(&s.feed, BASE_CHAIN_ID).ok_or("unsupported sequencer feed")?,
                    ),
                    grace_s: s.grace_s,
                    slot: tokio::sync::Mutex::new(None),
                });
                sequencers.insert(key, sequencer.clone());
                Ok(sequencer)
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
    /// Accumulate TWAP history even when no merchant requests a quote.
    pub async fn sample_twaps(&self, route: &RouteFile, arrived: tokio::time::Instant) {
        for (role, entries) in [("primary", &self.primary), ("check", &self.check)] {
            for entry in entries.iter().filter(|e| e.company == "uniswap-v2-onchain") {
                let mut audit = Audit::default();
                if let Err(failure) =
                    observe(entry, route, role, &mut audit, true, Some(arrived)).await
                {
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
            source: Arc::new(SharedSource::new(
                source,
                Duration::ZERO,
                "injected",
                "injected",
            )),
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
    #[cfg(test)]
    pub(crate) fn injected_onchain(
        primary: Arc<dyn PriceSource>,
        check: Option<Arc<dyn PriceSource>>,
        fx: Option<Arc<dyn PriceSource>>,
    ) -> Self {
        let mut runtime = Self::injected(primary, check, fx);
        for entry in runtime
            .primary
            .iter_mut()
            .chain(&mut runtime.check)
            .chain(&mut runtime.fx)
        {
            entry.source = Arc::new(SharedSource::new(
                entry.source.inner.clone(),
                SOURCE_REUSE,
                entry.source_id,
                entry.company,
            ));
        }
        runtime
    }
    /// Fetches every stablecoin source or the first healthy source in each volatile role.
    pub async fn fetch(&self, route: &RouteFile) -> Result<ValidatedQuote, PricingFailure> {
        self.fetch_with_reuse(route, false).await
    }
    /// Crediting fetches fresh evidence, sharing only fetches completed after arrival.
    pub async fn fetch_fresh(&self, route: &RouteFile) -> Result<ValidatedQuote, PricingFailure> {
        self.fetch_with_reuse(route, true).await
    }
    async fn fetch_with_reuse(
        &self,
        route: &RouteFile,
        fresh_only: bool,
    ) -> Result<ValidatedQuote, PricingFailure> {
        let result = self.fetch_inner(route, fresh_only).await;
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
    async fn fetch_inner(
        &self,
        route: &RouteFile,
        fresh_only: bool,
    ) -> Result<ValidatedQuote, PricingFailure> {
        let mut audit = Audit {
            mode: Some(route.pricing.mode),
            ..Audit::default()
        };
        if let Some(sequencer) = &self.sequencer {
            match sequencer.evidence().await {
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
                    price_metrics::decision(route, code, &failure.evidence);
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
                let quote = observe(entry, route, "sources", &mut audit, fresh_only, None).await?;
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
                first(&self.primary, route, "primary", &mut pa, fresh_only),
                first(&self.check, route, "check", &mut ca, fresh_only),
                first(&self.fx, route, "fx", &mut fa, fresh_only)
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
                price_metrics::decision(route, code, &failure.evidence);
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
    fresh_only: bool,
) -> Result<Option<(&'a Entry, PriceQuote)>, PricingFailure> {
    for (index, entry) in entries.iter().enumerate() {
        if let Some(observation) = observe(entry, route, role, audit, fresh_only, None).await? {
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
    fresh_only: bool,
    arrived: Option<tokio::time::Instant>,
) -> Result<Option<PriceQuote>, PricingFailure> {
    let result = if let Some(arrived) = arrived {
        entry.source.quote_shared_since(arrived).await
    } else if fresh_only {
        entry.source.quote_shared_fresh().await
    } else {
        entry.source.quote_shared().await
    };
    let cached_age_ms = result.as_ref().ok().and_then(|(_, age_ms)| *age_ms);
    let now = validation_time()?;
    let result = result.and_then(|(quote, age_ms)| {
        let o = &quote.valuation;
        if !fresh(
            entry.max_age_s.unwrap_or(route.pricing.max_age_s),
            o.observed_at,
            now,
        ) {
            Err(PriceError::Stale)
        } else {
            Ok((quote, age_ms))
        }
    });
    let (evidence, observation) = match result {
        Ok((quote, age_ms)) => {
            let o = &quote.valuation;
            let mut evidence = json!({"role":role, "company":entry.company, "source":o.source.as_str(), "descriptor":entry.descriptor, "price_scaled":o.price.value().to_string(), "agreement_price_scaled":quote.agreement_price.value().to_string(), "observed_at":o.observed_at.value(), "age_s":now.value().saturating_sub(o.observed_at.value()), "data":quote.evidence});
            if let Some(age_ms) = age_ms {
                evidence["cached"] = json!({"age_ms": age_ms});
            }
            (evidence, Some(quote))
        }
        Err(error) => {
            let code = error_code(&error);
            let mut evidence = json!({"role":role,"company":entry.company,"source":entry.source_id,"descriptor":entry.descriptor,"error":code,"data": match &error {PriceError::Feed {evidence,..} => evidence.clone(), _ => Value::Null}});
            if let Some(age_ms) = cached_age_ms {
                evidence["cached"] = json!({"age_ms": age_ms});
            }
            audit.observations.push(evidence);
            price_metrics::health(route, role, entry.source_id, entry.company, false);
            if code.starts_with("twap_") {
                price_metrics::refusal(route, role, entry.source_id, entry.company, code);
                audit.decision = Some(code);
                return Err(PricingFailure::with_audit(code, audit));
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
                return Err(PricingFailure::with_audit("divergent", audit));
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

    #[tokio::test]
    async fn runtime_reads_the_reversed_pair_validation_approved() {
        let route = reversed_group_route();
        let now = validation_time().unwrap().value();
        let a = topup_adapters::pricing::test_rpc::chainlink("a", 101_000_000, 20, 20, now, false)
            .await;
        let b = topup_adapters::pricing::test_rpc::chainlink("b", 100_000_000, 20, 20, now, false)
            .await;
        let expected = ("b".into(), "a".into(), 1);
        assert_eq!(price_pair(&route, &route.pricing.fx[0]).unwrap(), expected);
        assert_eq!(
            crate::rpc_groups::price_pairs(&route).unwrap(),
            vec![expected]
        );
        let clients = BTreeMap::from([
            ("a".into(), a.client.clone()),
            ("b".into(), b.client.clone()),
        ]);
        let routes = RouteSet::with_groups(vec![route.clone()], clients).unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/pricing-regression")
            .unwrap();
        let runtime = PricingRuntime::configured(
            &route,
            &routes,
            pool,
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
        )
        .unwrap();
        let error = runtime.fx[0].source.quote_shared().await.unwrap_err();
        let PriceError::Feed { class, evidence } = error else {
            panic!("expected A/B disagreement, got {error:?}");
        };
        assert_eq!(class, "divergent");
        // Distinct answers pin both the identity and the order of the groups actually read.
        assert_eq!(evidence["a"]["answer"], "100000000");
        assert_eq!(evidence["b"]["answer"], "101000000");

        let mut route = route;
        route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
            feed: "BASE_SEQUENCER_UPTIME".into(),
            grace_s: 3600,
            rpc_group: "a".into(),
            rpc_group_b: "b".into(),
        });
        let (a, b, chain) = crate::rpc_groups::price_pairs(&route)
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            chain == BASE_CHAIN_ID
                && Arc::ptr_eq(
                    &routes.resolved_price_group(&a).unwrap(),
                    &routes.price_group(&route, "a").unwrap()
                )
                && Arc::ptr_eq(
                    &routes.resolved_price_group(&b).unwrap(),
                    &routes.price_group(&route, "b").unwrap()
                )
        );
    }

    #[test]
    fn validation_rejects_a_price_pair_resolving_to_one_group() {
        let mut route = reversed_group_route();
        let Source::Chainlink { rpc_group_b, .. } = &mut route.pricing.fx[0] else {
            unreachable!();
        };
        *rpc_group_b = Some("a".into());
        let fixture: Value = serde_saphyr::from_str(
            &include_str!("../../tests/fixtures/rpc-groups.yaml")
                .replace("alchemy", "a")
                .replace("quicknode", "b"),
        )
        .unwrap();
        let groups = serde_json::from_value(fixture["rpc_groups"].clone()).unwrap();
        let companies = serde_json::from_value(fixture["rpc_companies"].clone()).unwrap();
        let budgets = serde_json::from_value(fixture["rpc_budgets"].clone()).unwrap();
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
            source: Arc::new(SharedSource::new(
                Arc::new(Fixture {
                    id,
                    price,
                    age,
                    error,
                }),
                Duration::ZERO,
                id,
                id,
            )),
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
                reuse_until: None,
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
                source: Arc::new(SharedSource::new(
                    source,
                    Duration::ZERO,
                    "uniswap_v2_twap",
                    "uniswap-v2-onchain",
                )),
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
    struct CountingSource {
        calls: std::sync::atomic::AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
        delay: Duration,
        reuse_until: Option<UnixSeconds>,
        observed_at: UnixSeconds,
        price: u64,
    }
    impl CountingSource {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: std::sync::atomic::AtomicBool::new(false),
                delay,
                reuse_until: None,
                observed_at: validation_time().unwrap(),
                price: 100_000_000,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl PriceSource for CountingSource {
        async fn observe(&self) -> Result<Observation, PriceError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(PriceError::Timeout);
            }
            Ok(Observation {
                source: SourceId::new("counted"),
                price: ScaledPrice::new(self.price, 8).unwrap(),
                observed_at: self.observed_at,
            })
        }
        async fn quote(&self) -> Result<PriceQuote, PriceError> {
            let valuation = self.observe().await?;
            Ok(PriceQuote {
                agreement_price: valuation.price,
                valuation,
                evidence: Value::Null,
                reuse_until: self.reuse_until,
            })
        }
    }
    fn shared(inner: Arc<dyn PriceSource>) -> SharedSource {
        SharedSource::new(inner, SOURCE_REUSE, "counted", "counted")
    }
    #[tokio::test(start_paused = true)]
    async fn fetch_fresh_does_not_reuse_a_warmed_quote() {
        let inner = Arc::new(CountingSource {
            price: 10_000_000,
            ..Arc::try_unwrap(CountingSource::new(Duration::from_millis(1)))
                .ok()
                .unwrap()
        });
        let mut runtime = runtime();
        runtime.primary[0].source = Arc::new(shared(inner.clone()));
        runtime.fetch(&route()).await.unwrap();
        assert_eq!(inner.calls(), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        let quote = runtime.fetch_fresh(&route()).await.unwrap();
        assert_eq!(
            inner.calls(),
            2,
            "fresh valuation must bypass the quote TTL"
        );
        assert!(
            quote.evidence["observations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o.get("cached").is_none())
        );
    }
    #[tokio::test(start_paused = true)]
    async fn shared_source_coalesces_concurrent_quotes() {
        let inner = CountingSource::new(Duration::from_millis(10));
        let source = shared(inner.clone());
        let results =
            futures_util::future::join_all((0..10).map(|_| source.quote_shared_fresh())).await;
        assert_eq!(inner.calls(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| r.as_ref().unwrap().1.is_none())
                .count(),
            1
        );
        assert!(results.iter().all(Result::is_ok));
    }
    #[tokio::test(start_paused = true)]
    async fn shared_source_reuses_within_ttl_and_refetches_after() {
        let inner = CountingSource::new(Duration::from_millis(1));
        let source = shared(inner.clone());
        assert!(source.quote_shared().await.unwrap().1.is_none());
        tokio::time::advance(Duration::from_secs(12)).await;
        assert_eq!(source.quote_shared().await.unwrap().1, Some(12_000));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(source.quote_shared().await.unwrap().1.is_none());
        assert_eq!(inner.calls(), 2);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(source.quote_shared_fresh().await.unwrap().1.is_none());
        assert_eq!(
            inner.calls(),
            3,
            "confirm cannot reuse a completed quote fetch"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn shared_source_never_caches_errors() {
        let inner = CountingSource::new(Duration::from_millis(1));
        let source = shared(inner.clone());
        source.quote_shared().await.unwrap();
        tokio::time::advance(Duration::from_secs(13)).await;
        inner.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(source.quote_shared().await.is_err());
        assert!(source.slot.lock().await.is_none());
        assert!(source.quote_shared().await.is_err());
        inner.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(source.quote_shared().await.unwrap().1.is_none());
        assert_eq!(inner.calls(), 4);
    }
    #[tokio::test(start_paused = true)]
    async fn shared_source_respects_reuse_until() {
        GOLDEN_NOW
            .scope(1000, async {
                let inner = Arc::new(CountingSource {
                    reuse_until: Some(UnixSeconds::new(1001)),
                    ..Arc::try_unwrap(CountingSource::new(Duration::from_millis(1)))
                        .ok()
                        .unwrap()
                });
                let source = shared(inner.clone());
                source.quote_shared().await.unwrap();
                tokio::time::advance(Duration::from_millis(1)).await;
                GOLDEN_NOW
                    .scope(1001, async {
                        assert!(source.quote_shared().await.unwrap().1.is_some());
                    })
                    .await;
                GOLDEN_NOW
                    .scope(1002, async {
                        assert!(source.quote_shared().await.unwrap().1.is_none());
                    })
                    .await;
                assert_eq!(inner.calls(), 2);
            })
            .await;
    }
    #[tokio::test(start_paused = true)]
    async fn cache_hit_marks_observation_age_and_rechecks_route_freshness() {
        GOLDEN_NOW
            .scope(1000, async {
                let inner = CountingSource::new(Duration::from_millis(1));
                let mut entry = entry("counted", 100_000_000, 0, None);
                entry.source = Arc::new(shared(inner.clone()));
                entry.max_age_s = Some(1);
                let mut audit = Audit::default();
                observe(&entry, &route(), "primary", &mut audit, false, None)
                    .await
                    .unwrap();
                assert!(audit.observations[0].get("cached").is_none());
                tokio::time::advance(Duration::from_millis(500)).await;
                observe(&entry, &route(), "primary", &mut audit, false, None)
                    .await
                    .unwrap();
                assert_eq!(audit.observations[1]["cached"], json!({"age_ms":500}));
                GOLDEN_NOW
                    .scope(1002, async {
                        assert!(
                            observe(&entry, &route(), "primary", &mut audit, false, None)
                                .await
                                .unwrap()
                                .is_none()
                        );
                        assert_eq!(audit.observations[2]["error"], "stale");
                        assert_eq!(audit.observations[2]["cached"], json!({"age_ms": 500}));
                    })
                    .await;
                assert_eq!(inner.calls(), 1);
            })
            .await;
    }
    #[tokio::test(start_paused = true)]
    async fn cancelled_leader_releases_source_for_next_fetch() {
        let inner = CountingSource::new(Duration::from_secs(20));
        let source = shared(inner.clone());
        assert!(
            tokio::time::timeout(Duration::from_secs(15), source.quote_shared())
                .await
                .is_err()
        );
        assert!(source.quote_shared_fresh().await.unwrap().1.is_none());
        assert_eq!(inner.calls(), 2);
    }
    #[tokio::test(start_paused = true)]
    async fn observe_timeout_excludes_waiting_for_the_source_lock() {
        struct SlowLeader(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl PriceSource for SlowLeader {
            async fn observe(&self) -> Result<Observation, PriceError> {
                let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(if call == 0 { 31 } else { 5 })).await;
                Ok(Observation {
                    source: SourceId::new("slow"),
                    price: ScaledPrice::new(10_000_000, 8).unwrap(),
                    observed_at: validation_time().unwrap(),
                })
            }
        }
        let inner = Arc::new(SlowLeader(std::sync::atomic::AtomicUsize::new(0)));
        let mut entry = entry("slow", 10_000_000, 0, None);
        entry.source = Arc::new(shared(inner.clone()));
        let started = tokio::time::Instant::now();
        let mut leader_audit = Audit::default();
        let mut waiter_audit = Audit::default();
        let route = route();
        let leader = observe(&entry, &route, "primary", &mut leader_audit, true, None);
        let waiter = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            observe(&entry, &route, "primary", &mut waiter_audit, true, None).await
        };
        let (leader, waiter) = tokio::join!(leader, waiter);
        assert!(leader.unwrap().is_none());
        assert_eq!(leader_audit.observations[0]["error"], "timeout");
        assert!(waiter.unwrap().is_some());
        assert_eq!(inner.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(started.elapsed(), Duration::from_secs(35));
    }
    fn pha_routes() -> [RouteFile; 2] {
        let mut a = route();
        a.chain.rpc_providers = vec!["a".into(), "b".into()];
        a.pricing.primary = vec![Source::UniswapV2Twap {
            rpc_group: "a".into(),
            rpc_group_b: "b".into(),
            observation_chain_id: None,
            twap: topup_core::price::TwapConfig::default(),
        }];
        a.pricing.check = vec![Source::Kraken {
            symbol: "PHAUSD".into(),
            company: "kraken".into(),
        }];
        a.pricing.fx = vec![Source::Chainlink {
            feed: "USDT_USD".into(),
            chain_id: 1,
            rpc_group: "a".into(),
            rpc_group_b: Some("b".into()),
            observation_chain_id: None,
        }];
        let mut b = a.clone();
        b.route = "phala-cloud-sepolia-pha-usd".into();
        b.chain.chain_id = 11155111;
        b.livemode = false;
        if let Source::UniswapV2Twap {
            observation_chain_id,
            ..
        } = &mut b.pricing.primary[0]
        {
            *observation_chain_id = Some(b.chain.chain_id);
        }
        if let Source::Chainlink {
            observation_chain_id,
            ..
        } = &mut b.pricing.fx[0]
        {
            *observation_chain_id = Some(b.chain.chain_id);
        }
        [a, b]
    }
    #[tokio::test]
    async fn routes_share_one_source_per_adapter_identity() {
        let [a, b] = pha_routes();
        let now = validation_time().unwrap().value();
        let ra = topup_adapters::pricing::test_rpc::chainlink("a", 100_000_000, 20, 20, now, false)
            .await;
        let rb = topup_adapters::pricing::test_rpc::chainlink("b", 100_000_000, 20, 20, now, false)
            .await;
        let routes = RouteSet::with_groups(
            vec![a.clone(), b.clone()],
            BTreeMap::from([
                ("a".into(), ra.client.clone()),
                ("b".into(), rb.client.clone()),
            ]),
        )
        .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/shared-pricing")
            .unwrap();
        let runtimes = PricingRuntime::build_all(&routes, pool).unwrap();
        let a = &runtimes[&(a.route.clone(), a.version)];
        let b = &runtimes[&(b.route.clone(), b.version)];
        for (a, b) in [
            (&a.primary[0], &b.primary[0]),
            (&a.check[0], &b.check[0]),
            (&a.fx[0], &b.fx[0]),
        ] {
            assert!(Arc::ptr_eq(&a.source, &b.source));
        }
        assert_ne!(a.primary[0].descriptor, b.primary[0].descriptor);
    }
    #[tokio::test]
    async fn different_adapter_identities_do_not_share_sources() {
        let now = validation_time().unwrap().value();
        let a_rpc =
            topup_adapters::pricing::test_rpc::chainlink("a", 100_000_000, 20, 20, now, false)
                .await;
        let b_rpc =
            topup_adapters::pricing::test_rpc::chainlink("b", 100_000_000, 20, 20, now, false)
                .await;
        let c_rpc =
            topup_adapters::pricing::test_rpc::chainlink("c", 100_000_000, 20, 20, now, false)
                .await;
        let clients = BTreeMap::from([
            ("a".into(), a_rpc.client.clone()),
            ("b".into(), b_rpc.client.clone()),
            ("c".into(), c_rpc.client.clone()),
        ]);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/shared-pricing")
            .unwrap();
        for case in ["policy", "group_a", "group_b", "swapped"] {
            let [a, mut b] = pha_routes();
            if let Source::UniswapV2Twap {
                rpc_group,
                rpc_group_b,
                twap,
                ..
            } = &mut b.pricing.primary[0]
            {
                match case {
                    "policy" => twap.window_s = 3600,
                    "group_a" => *rpc_group = "c".into(),
                    "group_b" => *rpc_group_b = "c".into(),
                    _ => std::mem::swap(rpc_group, rpc_group_b),
                }
            }
            if case != "policy"
                && let Source::Chainlink {
                    rpc_group,
                    rpc_group_b,
                    ..
                } = &mut b.pricing.fx[0]
            {
                match case {
                    "group_a" => *rpc_group = "c".into(),
                    "group_b" => *rpc_group_b = Some("c".into()),
                    _ => {
                        *rpc_group = "b".into();
                        *rpc_group_b = Some("a".into());
                    }
                }
            }
            let routes =
                RouteSet::with_groups(vec![a.clone(), b.clone()], clients.clone()).unwrap();
            let runtimes = PricingRuntime::build_all(&routes, pool.clone()).unwrap();
            let a = &runtimes[&(a.route.clone(), a.version)];
            let b = &runtimes[&(b.route.clone(), b.version)];
            assert!(
                !Arc::ptr_eq(&a.primary[0].source, &b.primary[0].source),
                "{case}"
            );
            if case != "policy" {
                assert!(!Arc::ptr_eq(&a.fx[0].source, &b.fx[0].source), "{case}");
            }
        }
        let mut a = route();
        a.chain.chain_id = 1;
        a.chain.rpc_providers = vec!["a".into(), "b".into()];
        a.asset.symbol = "usdt".into();
        a.pricing.mode = PricingMode::Stablecoin;
        a.pricing.primary.clear();
        a.pricing.check.clear();
        a.pricing.fx.clear();
        a.pricing.sources = ["USDC_USD", "USDT_USD"]
            .map(|name| Source::Chainlink {
                feed: name.into(),
                chain_id: 1,
                rpc_group: "a".into(),
                rpc_group_b: Some("b".into()),
                observation_chain_id: None,
            })
            .to_vec();
        let mut b = a.clone();
        b.version += 1;
        if let Source::Chainlink {
            chain_id,
            observation_chain_id,
            ..
        } = &mut b.pricing.sources[1]
        {
            *chain_id = 8453;
            *observation_chain_id = Some(1);
        }
        let routes = RouteSet::with_groups(vec![a.clone(), b.clone()], clients).unwrap();
        let runtimes = PricingRuntime::build_all(&routes, pool).unwrap();
        let a = &runtimes[&(a.route.clone(), a.version)];
        let b = &runtimes[&(b.route.clone(), b.version)];
        assert!(
            !Arc::ptr_eq(&a.sources[0].source, &a.sources[1].source),
            "different feed"
        );
        assert!(
            !Arc::ptr_eq(&a.sources[1].source, &b.sources[1].source),
            "different feed chain"
        );
    }
    #[tokio::test]
    async fn sequencer_aliases_do_not_share_clients_across_chains() {
        let [mut a, mut b] = pha_routes();
        a.chain.chain_id = 8453;
        b.chain.chain_id = 84532;
        for route in [&mut a, &mut b] {
            route.chain.rpc_providers = vec!["provider-a".into(), "provider-b".into()];
            route.pricing.primary = vec![Source::Kraken {
                symbol: "PHAUSD".into(),
                company: "kraken".into(),
            }];
            route.pricing.check = vec![Source::Binance {
                symbol: "PHAUSDT".into(),
                company: "binance".into(),
            }];
            route.pricing.fx = vec![Source::Kraken {
                symbol: "USDTUSD".into(),
                company: "kraken".into(),
            }];
            route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
                feed: "BASE_SEQUENCER_UPTIME".into(),
                rpc_group: "a".into(),
                rpc_group_b: "b".into(),
                grace_s: 3600,
            });
        }
        let routes = RouteSet::with_providers(
            vec![a.clone(), b.clone()],
            &BTreeMap::from([
                (
                    "provider-a".into(),
                    crate::rpc_provider::ProviderUrl::parse("http://127.0.0.1:1").unwrap(),
                ),
                (
                    "provider-b".into(),
                    crate::rpc_provider::ProviderUrl::parse("http://127.0.0.1:2").unwrap(),
                ),
            ]),
        )
        .unwrap();
        assert!(!Arc::ptr_eq(
            &routes.price_group(&a, "a").unwrap(),
            &routes.price_group(&b, "a").unwrap()
        ));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/shared-pricing")
            .unwrap();
        let runtimes = PricingRuntime::build_all(&routes, pool).unwrap();
        let a = runtimes[&(a.route.clone(), a.version)]
            .sequencer
            .as_ref()
            .unwrap();
        let b = runtimes[&(b.route.clone(), b.version)]
            .sequencer
            .as_ref()
            .unwrap();
        assert!(
            !Arc::ptr_eq(a, b),
            "per-chain alias clients cannot share a sequencer"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn sampler_fetches_pair_once_per_interval_across_routes() {
        let inner = CountingSource::new(Duration::from_millis(1));
        let source = Arc::new(shared(inner.clone()));
        let make_runtime = || {
            let mut r = runtime();
            r.primary[0].source = source.clone();
            r.primary[0].company = "uniswap-v2-onchain";
            r
        };
        let a = make_runtime();
        let b = make_runtime();
        let routes = pha_routes();
        for expected in 1..=3 {
            let arrived = tokio::time::Instant::now();
            a.sample_twaps(&routes[0], arrived).await;
            b.sample_twaps(&routes[1], arrived).await;
            assert_eq!(inner.calls(), expected);
            tokio::time::advance(Duration::from_secs(60)).await;
        }
    }
    #[tokio::test(start_paused = true)]
    async fn sampler_does_not_reuse_a_quote_completed_before_the_tick() {
        struct SampledSource {
            last_recorded: tokio::sync::Mutex<tokio::time::Instant>,
            calls: std::sync::atomic::AtomicUsize,
            records: std::sync::atomic::AtomicUsize,
        }
        #[async_trait]
        impl PriceSource for SampledSource {
            async fn observe(&self) -> Result<Observation, PriceError> {
                use std::sync::atomic::Ordering;
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut recorded = self.last_recorded.lock().await;
                if recorded.elapsed() >= Duration::from_secs(60) {
                    *recorded = tokio::time::Instant::now();
                    self.records.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Observation {
                    source: SourceId::new("sampled"),
                    price: ScaledPrice::new(10_000_000, 8).unwrap(),
                    observed_at: validation_time().unwrap(),
                })
            }
        }
        let inner = Arc::new(SampledSource {
            last_recorded: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            records: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut runtime = runtime();
        runtime.primary[0].source = Arc::new(shared(inner.clone()));
        runtime.primary[0].company = "uniswap-v2-onchain";
        tokio::time::advance(Duration::from_secs(59)).await;
        runtime.fetch(&route()).await.unwrap();
        assert_eq!(inner.records.load(std::sync::atomic::Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(1)).await;
        runtime
            .sample_twaps(&route(), tokio::time::Instant::now())
            .await;
        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(inner.records.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn quote_pricing_budget_maps_to_pricing_unavailable() {
        struct Stalled;
        #[async_trait]
        impl crate::locks::QuoteProvider for Stalled {
            async fn quote(&self, _: &RouteFile) -> Result<ValidatedQuote, Value> {
                std::future::pending().await
            }
        }
        let provider: Arc<dyn crate::locks::QuoteProvider> = Arc::new(Stalled);
        let started = tokio::time::Instant::now();
        assert!(matches!(
            crate::locks::quote_with_budget(&provider, &route()).await,
            Err(crate::locks::RateLockError::PricingUnavailable)
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(15));
    }

    struct CountingStore {
        history: tokio::sync::Mutex<Vec<topup_adapters::pricing::uniswap_v2::Sample>>,
        records: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl topup_adapters::pricing::uniswap_v2::ObservationStore for CountingStore {
        async fn latest(
            &self,
            _: &topup_core::price::TwapConfig,
        ) -> Result<Option<topup_adapters::pricing::uniswap_v2::Sample>, PriceError> {
            Ok(self.history.lock().await.last().cloned())
        }
        async fn record(
            &self,
            sample: &topup_adapters::pricing::uniswap_v2::Sample,
            _: &topup_core::price::TwapConfig,
        ) -> Result<Vec<topup_adapters::pricing::uniswap_v2::Sample>, PriceError> {
            self.records
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut history = self.history.lock().await;
            if history
                .last()
                .is_none_or(|previous| previous.timestamp < sample.timestamp)
            {
                history.push(sample.clone());
            }
            Ok(history.clone())
        }
    }
    #[tokio::test]
    async fn concurrent_pha_quotes_send_only_one_cold_fetch_and_cached_quote_sends_zero() {
        use alloy_primitives::{B256, U256};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use topup_adapters::pricing::uniswap_v2::{PHA, PairState, Sample, WETH, counterfactual};
        let now = validation_time().unwrap().value();
        let hash = B256::repeat_byte(1);
        let mut state = PairState {
            token0: PHA,
            token1: WETH,
            reserve0: U256::from(100_000_000_000_000_000_000_000_u128),
            reserve1: U256::from(100_000_000_000_000_000_000_u128),
            timestamp_last: u32::try_from(now).unwrap(),
            cumulative0: U256::ZERO,
            cumulative1: U256::ZERO,
        };
        let spot = counterfactual(
            &state,
            topup_adapters::pricing::chainlink::PriceBlock {
                number: 98,
                hash,
                timestamp: now,
            },
        )
        .unwrap()
        .0
        .spot;
        state.cumulative0 = spot * U256::from(1800);
        let history = (0..30_u64)
            .map(|i| Sample {
                block: 67 + i,
                hash,
                timestamp: now - 1800 + i * 60,
                spot,
                cumulative: spot * U256::from(i * 60),
            })
            .collect();
        let store = Arc::new(CountingStore {
            history: tokio::sync::Mutex::new(history),
            records: AtomicUsize::new(0),
        });
        let sends = Arc::new(AtomicUsize::new(0));
        let a = topup_adapters::pricing::test_rpc::uniswap_v2(
            "a",
            (100, hash),
            state.clone(),
            200_000_000_000,
            now,
            sends.clone(),
        )
        .await;
        let b = topup_adapters::pricing::test_rpc::uniswap_v2(
            "b",
            (100, hash),
            state,
            200_000_000_000,
            now,
            sends.clone(),
        )
        .await;
        let primary = Arc::new(SharedSource::new(
            Arc::new(
                UniswapV2::new(
                    a.client.clone(),
                    b.client.clone(),
                    topup_core::price::TwapConfig::default(),
                    store.clone(),
                )
                .unwrap(),
            ),
            SOURCE_REUSE,
            "uniswap_v2_twap",
            "uniswap-v2-onchain",
        ));
        let fx = Arc::new(SharedSource::new(
            Arc::new(Chainlink::new(
                a.client.clone(),
                b.client.clone(),
                feed("USDT_USD", 1).unwrap(),
            )),
            SOURCE_REUSE,
            "chainlink",
            "chainlink",
        ));
        let make_runtime = || {
            let mut r = runtime();
            r.primary[0].source = primary.clone();
            r.primary[0].company = "uniswap-v2-onchain";
            r.primary[0].max_age_s = Some(180);
            r.check = vec![entry("kraken", 1, 0, None)];
            r.check[0].usdt_quoted = false;
            r.fx[0].source = fx.clone();
            r
        };
        let price = topup_adapters::pricing::uniswap_v2::usd_price(
            spot,
            ScaledPrice::new(200_000_000_000, 8).unwrap(),
        )
        .unwrap()
        .value();
        let mut cold = make_runtime();
        cold.check[0] = entry("kraken", price, 0, None);
        cold.check[0].usdt_quoted = false;
        sends.store(0, Ordering::SeqCst);
        store.records.store(0, Ordering::SeqCst);
        cold.fetch(&route()).await.unwrap();
        let cold_sends = sends.load(Ordering::SeqCst);
        assert!(
            cold_sends > 0,
            "the cold quote must actually fetch RPC evidence"
        );
        *primary.slot.lock().await = None;
        *fx.slot.lock().await = None;
        sends.store(0, Ordering::SeqCst);
        store.records.store(0, Ordering::SeqCst);
        let market = Arc::new(CountingSource {
            price,
            ..Arc::try_unwrap(CountingSource::new(Duration::from_millis(10)))
                .ok()
                .unwrap()
        });
        let check = Arc::new(SharedSource::new(
            market.clone(),
            Duration::ZERO,
            "kraken",
            "kraken",
        ));
        let make_runtime = || {
            let mut r = make_runtime();
            r.check = vec![entry("kraken", price, 0, None)];
            r.check[0].source = check.clone();
            r.check[0].usdt_quoted = false;
            r
        };
        let runtimes = [make_runtime(), make_runtime()];
        let routes = pha_routes();
        let results =
            futures_util::future::join_all((0..10).map(|i| runtimes[i % 2].fetch(&routes[i % 2])))
                .await;
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(store.records.load(Ordering::SeqCst), 1);
        assert_eq!(market.calls(), 1, "CEX only coalesces concurrent fetches");
        assert_eq!(sends.load(Ordering::SeqCst), cold_sends);
        let quote = runtimes[1].fetch(&routes[1]).await.unwrap();
        assert_eq!(
            sends.load(Ordering::SeqCst),
            cold_sends,
            "cached quote sends zero RPCs"
        );
        assert!(quote.evidence["observations"][0].get("cached").is_some());
        println!(
            "PR-3 PHA acceptance: cold={cold_sends} sends; 10 concurrent quotes={cold_sends} sends / 1 TWAP fetch; next quote=0 additional sends"
        );
    }
    #[tokio::test]
    async fn sequencer_coalesces_only_and_never_caches_errors() {
        use alloy::sol_types::SolCall;
        use alloy_primitives::{B256, I256, U256};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use topup_adapters::pricing::{
            chainlink::{decimalsCall, latestRoundDataCall, latestRoundDataReturn},
            test_rpc,
        };
        let now = validation_time().unwrap().value();
        let sends = Arc::new(AtomicUsize::new(0));
        let down = Arc::new(AtomicBool::new(false));
        let fixture = |id: &'static str| {
            let sends = sends.clone();
            let down = down.clone();
            test_rpc::rpc(id, move |request| {
                sends.fetch_add(1, Ordering::SeqCst);
                if request["method"] == "eth_getBlockByNumber" {
                    let mut head = serde_json::to_value(alloy::rpc::types::Block::<
                        alloy::rpc::types::Transaction,
                    >::default())
                    .unwrap();
                    head["number"] = if request["params"][0] == "latest" {
                        json!("0x64")
                    } else {
                        request["params"][0].clone()
                    };
                    head["hash"] = json!(B256::repeat_byte(1));
                    head["parentHash"] = json!(B256::repeat_byte(2));
                    return head;
                }
                let input = request["params"][0]["input"]
                    .as_str()
                    .or_else(|| request["params"][0]["data"].as_str())
                    .unwrap();
                let bytes = if input.starts_with("0x313ce567") {
                    decimalsCall::abi_encode_returns(&0)
                } else {
                    latestRoundDataCall::abi_encode_returns(&latestRoundDataReturn {
                        roundId: alloy_primitives::Uint::<80, 2>::from(20),
                        answer: I256::try_from(if down.load(Ordering::SeqCst) { 1 } else { 0 })
                            .unwrap(),
                        startedAt: U256::from(now - 7200),
                        updatedAt: U256::from(now),
                        answeredInRound: alloy_primitives::Uint::<80, 2>::from(20),
                    })
                };
                json!(format!("0x{}", hex::encode(bytes)))
            })
        };
        let a = fixture("sequencer-a").await;
        let b = fixture("sequencer-b").await;
        let sequencer = SharedSequencer {
            inner: Chainlink::new(
                a.client.clone(),
                b.client.clone(),
                feed("BASE_SEQUENCER_UPTIME", BASE_CHAIN_ID).unwrap(),
            ),
            grace_s: 3600,
            slot: tokio::sync::Mutex::new(None),
        };
        let results = futures_util::future::join_all((0..10).map(|_| sequencer.evidence())).await;
        assert!(results.iter().all(Result::is_ok));
        let one_fetch = sends.load(Ordering::SeqCst);
        assert!(one_fetch > 0, "sequencer evidence must actually be fetched");
        sequencer.evidence().await.unwrap();
        assert_eq!(
            sends.load(Ordering::SeqCst),
            2 * one_fetch,
            "sequencer must fetch again after completion"
        );
        down.store(true, Ordering::SeqCst);
        assert!(sequencer.evidence().await.is_err());
        assert!(sequencer.slot.lock().await.is_none());
        assert!(sequencer.evidence().await.is_err());
        down.store(false, Ordering::SeqCst);
        sequencer.evidence().await.unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 5 * one_fetch);
    }
}
