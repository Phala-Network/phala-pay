//! Attested price sources, feed metadata and valuation policy validation.
use crate::money::Bps;
use crate::route::{PricingMode, RouteError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Legal review outcome; only Allowed is usable in production.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// Commercial use reviewed and approved.
    Allowed,
    /// Permission not confirmed.
    Unclear,
    /// Prior written permission is required for commercial use.
    PermissionRequired,
    /// Commercial use prohibited under the currently available terms.
    Prohibited,
}
/// Reviewed licensing evidence compiled into the attested image.
#[derive(Debug, Serialize)]
pub struct Provider {
    /// Canonical company identity.
    pub company: &'static str,
    /// Consumer/API terms evidence.
    pub url: &'static str,
    /// Date of evidence retrieval.
    pub retrieved: &'static str,
    /// Reproducible clause; no implied commercial permission.
    pub clause: &'static str,
    /// Legal owner responsible for approval.
    pub legal_owner: &'static str,
    /// Conservative process-local request budget.
    pub rate_limit: &'static str,
    /// Current commercial-use verdict.
    pub verdict: Verdict,
}
/// No unreviewed provider is implicitly Allowed.
pub fn provider(source: &str) -> Option<Provider> {
    let (url, clause, rate_limit, verdict) = match source {
        "chainlink" => (
            "https://chain.link/terms",
            "Allowed basis: public on-chain feed data consumed through our own RPC, without an account or key. The Chainlink ToS page is client-rendered; full text could not be retrieved. This verdict covers public on-chain consumption only.",
            "shared RPC account/key budgets",
            Verdict::Allowed,
        ),
        "kraken" => (
            "https://docs-legacy.kraken.com/api/docs/guides/global-intro",
            "You must seek our prior permission for certain uses of the Kraken API's. This includes, but is not limited to, any non-personal commercial use of data from publicly accessible endpoints, such as market data … contacting marketdata@kraken.com",
            "1 request/second per process",
            Verdict::PermissionRequired,
        ),
        "binance" => (
            "https://data.binance.vision/terms-of-use.html",
            "§3.1: CC BY-NC-SA 4.0; §3.4: any commercial utilization requires a separate, written enterprise data license agreement executed with Binance. Includes data-api.binance.vision.",
            "1 request/second per process",
            Verdict::Prohibited,
        ),
        "coinbase" => (
            "https://www.coinbase.com/legal/market_data",
            "exclusively for you or your entity's personal or research purposes and may not be used to build an application intended for use by end users…; redistribution and derived works are prohibited.",
            "disabled; registry evidence only",
            Verdict::Prohibited,
        ),
        "uniswap_v2_twap" | "uniswap-v2-onchain" => (
            "https://etherscan.io/address/0x8867f20c1c63baccec7617626254a060eeb0e61e",
            "Allowed basis: public on-chain contract state consumed through our own RPC, without an API account or terms. PHA/WETH TWAP multiplied by public Chainlink ETH/USD state.",
            "one sample/minute; shared RPC budgets",
            Verdict::Allowed,
        ),
        "coinmetrics" => (
            "https://docs.coinmetrics.io/packages/coin-metrics-community-data",
            "CC BY-NC 4.0; non-commercial use only",
            "disabled",
            Verdict::Prohibited,
        ),
        _ => return None,
    };
    Some(Provider {
        company: match source {
            "chainlink" => "chainlink",
            "kraken" => "kraken",
            "binance" => "binance",
            "coinbase" => "coinbase",
            "uniswap_v2_twap" | "uniswap-v2-onchain" => "uniswap-v2-onchain",
            _ => "coinmetrics",
        },
        url,
        clause,
        rate_limit,
        verdict,
        retrieved: "2026-10-04",
        legal_owner: "Phala Legal",
    })
}
/// Pinned Chainlink proxy metadata from data.chain.link (reviewed 2026-10-04).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Feed {
    /// Feed identifier.
    pub name: &'static str,
    /// Observation network.
    pub chain_id: u64,
    /// Aggregator proxy address.
    pub address: &'static str,
    /// Aggregator decimals.
    pub decimals: u8,
    /// Published heartbeat interval; publication can arrive later.
    pub heartbeat_s: u64,
    /// Clock and publication allowance.
    pub margin_s: u64,
    /// Published deviation trigger in basis points.
    pub deviation_bps: u16,
}
/// Unsupported/testnet feeds never resolve implicitly.
/// Review of the last eight Ethereum rounds observed USDC/USD intervals of 82,812–82,836 s
/// (82,800 s heartbeat) and USDT/USD intervals of 86,412–86,436 s (86,400 s heartbeat).
/// Updates already arrive up to 36 s late in calm conditions. A 600 s publication margin
/// accommodates congestion without needless source loss; deviation-triggered updates still
/// constrain movement during normal feed operation. Completeness, A/B agreement and peg gates
/// remain mandatory, and rounds beyond heartbeat + margin remain stale.
pub fn feed(name: &str, chain: u64) -> Option<Feed> {
    let (address, heartbeat_s, deviation_bps) = match (chain, name) {
        (1, "ETH_USD") => ("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419", 3600, 50),
        (1, "USDC_USD") => ("0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6", 82800, 25),
        (1, "USDT_USD") => ("0x3E7d1eAB13ad0104d2750B8863b489D65364e32D", 86400, 25),
        (8453, "USDC_USD") => ("0x7e860098F58bBFC8648a4311b374B1D669a2bc6B", 86400, 30),
        (8453, "USDT_USD") => ("0xf19d560eB8d2ADf07BD6D13ed03e1D11215721F9", 86400, 30),
        (8453, "BASE_SEQUENCER_UPTIME") => ("0xBCF85224fc0756B9Fa45aA7892530B47e10b6433", 0, 0),
        _ => return None,
    };
    Some(Feed {
        name: match name {
            "ETH_USD" => "ETH_USD",
            "USDC_USD" => "USDC_USD",
            "USDT_USD" => "USDT_USD",
            _ => "BASE_SEQUENCER_UPTIME",
        },
        chain_id: chain,
        address,
        decimals: if heartbeat_s == 0 { 0 } else { 8 },
        heartbeat_s,
        margin_s: 600,
        deviation_bps,
    })
}
/// Safety policy for the pinned Ethereum PHA/WETH pair; no arbitrary pools or tokens.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TwapConfig {
    /// Minimum averaging window (at least thirty minutes).
    pub window_s: u64,
    /// Maximum sample age and permitted gap between samples.
    pub max_sample_age_s: u64,
    /// Minimum WETH-side USD reserve, in whole dollars.
    pub min_weth_reserve_usd: u64,
    /// Maximum spot deviation from the arithmetic TWAP.
    pub max_spot_deviation_bps: Bps,
    /// Maximum spot movement from the previous accepted sample.
    pub max_sample_jump_bps: Bps,
}
impl Default for TwapConfig {
    fn default() -> Self {
        Self {
            window_s: 1800,
            max_sample_age_s: 180,
            min_weth_reserve_usd: 100_000,
            max_spot_deviation_bps: Bps::new(1000).unwrap_or_default(),
            max_sample_jump_bps: Bps::new(500).unwrap_or_default(),
        }
    }
}
impl TwapConfig {
    /// Reject unsafe windows, unbounded history and ineffective guard rails.
    pub fn validate(&self) -> Result<(), RouteError> {
        if !(1800..=86400).contains(&self.window_s)
            || !(60..=600).contains(&self.max_sample_age_s)
            || self.min_weth_reserve_usd == 0
            || self.min_weth_reserve_usd > 1_000_000_000
            || !(1..=2000).contains(&self.max_spot_deviation_bps.value())
            || !(1..=2000).contains(&self.max_sample_jump_bps.value())
        {
            return Err(RouteError::validation(
                "price",
                "invalid TWAP window/liquidity/freshness/deviation/jump limits",
            ));
        }
        Ok(())
    }
}
/// One explicit source descriptor. Companies are canonical, never inferred from hosts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// Restricted legacy input, emitted only by the migration parser; never constructed at runtime.
    #[serde(skip_deserializing)]
    Coinmetrics {
        /// Legacy asset identity for manual migration.
        asset: String,
    },
    /// On-chain observation, independently read through A and B.
    Chainlink {
        /// Pinned feed id.
        feed: String,
        /// Feed chain.
        chain_id: u64,
        /// A group (or route role a).
        rpc_group: String,
        /// Independent B group for a cross-network feed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rpc_group_b: Option<String>,
        /// Test route chain explicitly valued from mainnet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observation_chain_id: Option<u64>,
    },
    /// Pinned mainnet PHA/WETH TWAP multiplied by Chainlink ETH/USD.
    #[serde(rename = "uniswap_v2_twap")]
    UniswapV2Twap {
        /// Ethereum mainnet A group (or route role a on Ethereum).
        rpc_group: String,
        /// Independent Ethereum mainnet B group.
        rpc_group_b: String,
        /// Explicit destination chain for cross-network/test-token valuation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observation_chain_id: Option<u64>,
        /// Averaging and guard rail policy.
        #[serde(default)]
        twap: TwapConfig,
    },
    /// Kraken USD market.
    Kraken {
        /// Reviewed symbol.
        symbol: String,
        /// Canonical company.
        company: String,
    },
    /// Binance USDT market.
    Binance {
        /// Reviewed symbol.
        symbol: String,
        /// Canonical company.
        company: String,
    },
}
impl Source {
    /// Source/company id.
    pub fn company(&self) -> &'static str {
        match self {
            Self::Coinmetrics { .. } => "coinmetrics",
            Self::Chainlink { .. } => "chainlink",
            Self::UniswapV2Twap { .. } => "uniswap-v2-onchain",
            Self::Kraken { .. } => "kraken",
            Self::Binance { .. } => "binance",
        }
    }
    /// Asset observed (USD for Chainlink and Kraken, USDT for Binance).
    pub fn asset(&self) -> &str {
        match self {
            Self::Coinmetrics { asset } => asset,
            Self::UniswapV2Twap { .. } => "pha",
            Self::Chainlink { feed, .. } => match feed.as_str() {
                "USDC_USD" => "usdc",
                "USDT_USD" => "usdt",
                _ => "unknown",
            },
            Self::Kraken { symbol, .. } => match symbol.as_str() {
                "PHAUSD" => "pha",
                "USDCUSD" => "usdc",
                "USDTUSD" => "usdt",
                _ => "unknown",
            },
            Self::Binance { symbol, .. } if symbol == "PHAUSDT" => "pha",
            _ => "unknown",
        }
    }
}
/// Sequencer gate, required for Base routes, including mainnet-valued Base test tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sequencer {
    /// Pinned uptime feed.
    pub feed: String,
    /// Recovery grace interval.
    pub grace_s: u64,
    /// Base mainnet A group for testnet routes.
    pub rpc_group: String,
    /// Base mainnet B group.
    pub rpc_group_b: String,
}
/// Canonical price schema; the old pricing shape is a migration input only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceConfig {
    /// Stablecoin or volatile policy.
    pub mode: PricingMode,
    /// Exchange receive-time freshness bound.
    #[serde(default = "age")]
    pub max_age_s: u64,
    /// Stablecoin peg band.
    #[serde(default = "band")]
    pub peg_band_bps: Bps,
    /// Primary/check disagreement limit.
    #[serde(default = "band")]
    pub max_deviation_bps: Bps,
    /// USDT FX peg band.
    #[serde(default = "fx_band")]
    pub max_fx_deviation_bps: Option<Bps>,
    /// Explicit noncommercial staging/rehearsal opt-in to non-Allowed sources; never legal approval.
    #[serde(default)]
    pub allow_unclear_sources: bool,
    /// Stablecoin source set; every fresh observation of the route asset is inspected for depeg.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source>,
    /// Ordered primary role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary: Vec<Source>,
    /// Ordered independent check role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check: Vec<Source>,
    /// Ordered independent USD normalization role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fx: Vec<Source>,
    /// Base sequencer gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequencer_uptime: Option<Sequencer>,
}
fn age() -> u64 {
    90
}
fn band() -> Bps {
    Bps::new(100).unwrap_or_default()
}
fn fx_band() -> Option<Bps> {
    Some(band())
}
impl PriceConfig {
    /// All configured roles in deterministic order.
    pub fn roles(&self) -> [(&'static str, &[Source]); 4] {
        [
            ("sources", &self.sources),
            ("primary", &self.primary),
            ("check", &self.check),
            ("fx", &self.fx),
        ]
    }
    /// Production never accepts staging opt-in as legal approval.
    pub fn validate_licensing(&self, staging: bool) -> Result<(), RouteError> {
        for (_, sources) in self.roles() {
            for source in sources {
                let verdict = provider(source.company()).map(|p| p.verdict);
                if matches!(source, Source::Coinmetrics { .. })
                    || (verdict != Some(Verdict::Allowed)
                        && !(staging && self.allow_unclear_sources))
                {
                    return Err(RouteError::validation(
                        "price",
                        "production requires Allowed licensing verdict",
                    ));
                }
            }
        }
        if self.sequencer_uptime.is_some()
            && !(staging && self.allow_unclear_sources)
            && provider("chainlink").is_none_or(|p| p.verdict != Verdict::Allowed)
        {
            return Err(RouteError::validation(
                "price",
                "sequencer source requires Allowed licensing verdict",
            ));
        }
        Ok(())
    }
    /// Validate safety and licensing at the route boundary.
    pub fn validate(&self, chain: u64, asset: &str, _live: bool) -> Result<(), RouteError> {
        let fail = |s| RouteError::validation("price", s);
        if self.max_age_s == 0 || self.max_age_s > 3600 {
            return Err(fail("max_age_s must be 1..3600"));
        }
        if self.mode == PricingMode::Stablecoin {
            if self.sources.is_empty()
                || !self.primary.is_empty()
                || !self.check.is_empty()
                || !self.fx.is_empty()
                || !matches!(asset, "usdc" | "usdt")
            {
                return Err(fail("stablecoin requires sources and forbids role lists"));
            }
        } else if !self.sources.is_empty()
            || self.primary.is_empty()
            || self.check.is_empty()
            || self.fx.is_empty()
        {
            return Err(fail(
                "volatile requires primary, check and independently checked fx",
            ));
        }
        let mut primary = BTreeSet::new();
        for (role, sources) in self.roles() {
            if sources.len() > 8 {
                return Err(fail("at most eight sources per role"));
            }
            let mut descriptors = BTreeSet::new();
            let mut companies = BTreeSet::new();
            for source in sources {
                if !descriptors.insert(source) {
                    return Err(fail("duplicate source descriptor"));
                }
                let company = source.company();
                // Stablecoin may contain both pinned feeds from the same company.
                if role != "sources" && !companies.insert(company) {
                    return Err(fail("duplicate company in role"));
                }
                if role == "primary" {
                    if matches!(source, Source::Binance { .. }) {
                        return Err(fail("primary must be quoted directly in USD"));
                    }
                    primary.insert(company);
                }
                if role == "check" && primary.contains(company) {
                    return Err(fail("primary/check companies must be disjoint"));
                }
                let market_matches = if role == "sources" {
                    matches!(source.asset(), "usdc" | "usdt")
                } else {
                    source.asset() == if role == "fx" { "usdt" } else { asset }
                };
                if source.asset() == "unknown" || !market_matches {
                    return Err(fail("source market does not match route asset/role"));
                }
                let verdict = provider(company)
                    .ok_or_else(|| fail("unknown source"))?
                    .verdict;
                if matches!(source, Source::Coinmetrics { .. })
                    || (verdict != Verdict::Allowed && !self.allow_unclear_sources)
                {
                    return Err(fail(
                        "source is not Allowed; staging must explicitly opt in",
                    ));
                }
                match source {
                    Source::UniswapV2Twap {
                        rpc_group,
                        rpc_group_b,
                        observation_chain_id,
                        twap,
                    } => {
                        twap.validate()?;
                        if rpc_group.is_empty()
                            || rpc_group_b.is_empty()
                            || rpc_group == rpc_group_b
                            || (chain != 1 && observation_chain_id != &Some(chain))
                        {
                            return Err(fail(
                                "TWAP requires independent mainnet A/B groups and explicit cross-network observation_chain_id",
                            ));
                        }
                    }
                    Source::Chainlink {
                        feed: name,
                        chain_id,
                        rpc_group,
                        rpc_group_b,
                        observation_chain_id,
                    } => {
                        if feed(name, *chain_id).is_none() || rpc_group.is_empty() {
                            return Err(fail("unsupported chain/feed or missing RPC group"));
                        }
                        if *chain_id != chain
                            && (observation_chain_id != &Some(chain)
                                || rpc_group_b.as_ref().is_none_or(String::is_empty))
                        {
                            return Err(fail(
                                "cross-network observation requires observation_chain_id and independent rpc_group_b",
                            ));
                        }
                        if matches!(chain, 11155111 | 84532) && *chain_id != 1 {
                            return Err(fail(
                                "test tokens require configured Ethereum mainnet feeds",
                            ));
                        }
                    }
                    Source::Kraken {
                        company: declared, ..
                    }
                    | Source::Binance {
                        company: declared, ..
                    } if declared != company => {
                        return Err(fail("company must match registry identity"));
                    }
                    _ => {}
                }
            }
        }
        if self.mode == PricingMode::Stablecoin {
            let symbols = self
                .sources
                .iter()
                .map(Source::asset)
                .collect::<BTreeSet<_>>();
            let mainnet_fallback = self
                .sources
                .iter()
                .any(|s| matches!(s, Source::Chainlink { chain_id: 1, .. }) && s.asset() == asset);
            if !symbols.contains(asset)
                || (!(symbols.contains("usdc") && symbols.contains("usdt")) && !mainnet_fallback)
            {
                return Err(fail(
                    "stablecoin sources must cover both symbols or configure an Ethereum-mainnet fallback for this asset",
                ));
            }
        }
        if self.mode == PricingMode::Spot
            && self
                .fx
                .iter()
                .any(|s| self.check.iter().any(|c| c.company() == s.company()))
        {
            return Err(fail("FX must be independent of check market"));
        }
        if matches!(chain, 8453 | 84532) && self.sequencer_uptime.is_none() {
            return Err(fail("Base requires sequencer uptime gate"));
        }
        if let Some(s) = &self.sequencer_uptime
            && (s.feed != "BASE_SEQUENCER_UPTIME"
                || s.grace_s < 3600
                || s.rpc_group.is_empty()
                || s.rpc_group_b.is_empty()
                || s.rpc_group == s.rpc_group_b)
        {
            return Err(fail("invalid sequencer feed/groups/grace"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    fn volatile() -> PriceConfig {
        serde_json::from_value(
            serde_json::json!({"mode":"volatile","allow_unclear_sources":true,
            "primary":[{"source":"kraken","symbol":"PHAUSD","company":"kraken"}],
            "check":[{"source":"binance","symbol":"PHAUSDT","company":"binance"}],
            "fx":[{"source":"chainlink","feed":"USDT_USD","chain_id":1,"rpc_group":"a"}]}),
        )
        .unwrap()
    }
    #[test]
    fn twap_defaults_boundaries_registry_and_licensing() {
        let mut config = volatile();
        config.primary = vec![
            serde_json::from_value(
                serde_json::json!({"source":"uniswap_v2_twap","rpc_group":"a","rpc_group_b":"b"}),
            )
            .unwrap(),
        ];
        config.check = vec![Source::Kraken {
            symbol: "PHAUSD".into(),
            company: "kraken".into(),
        }];
        config.fx = vec![serde_json::from_value(serde_json::json!({"source":"chainlink","feed":"USDT_USD","chain_id":1,"rpc_group":"a"})).unwrap()];
        assert!(config.validate(1, "pha", true).is_ok());
        assert!(config.validate_licensing(true).is_ok());
        assert!(
            config.validate_licensing(false).is_err(),
            "staging opt-in never grants Kraken permission"
        );
        assert_eq!(config.primary[0].company(), "uniswap-v2-onchain");
        assert_eq!(
            provider("uniswap_v2_twap").unwrap().verdict,
            Verdict::Allowed
        );
        assert_ne!(config.primary[0].company(), config.check[0].company());
        let Source::UniswapV2Twap {
            twap,
            observation_chain_id,
            ..
        } = &mut config.primary[0]
        else {
            panic!("TWAP source")
        };
        assert_eq!(twap.window_s, 1800);
        twap.window_s = 1799;
        assert!(twap.validate().is_err());
        twap.window_s = 86401;
        assert!(twap.validate().is_err());
        *twap = TwapConfig::default();
        *observation_chain_id = Some(11155111);
        config.fx = vec![serde_json::from_value(serde_json::json!({"source":"chainlink","feed":"USDT_USD","chain_id":1,"rpc_group":"mainnet-a","rpc_group_b":"mainnet-b","observation_chain_id":11155111})).unwrap()];
        assert!(config.validate(11155111, "pha", false).is_ok());
        let Source::UniswapV2Twap { rpc_group_b, .. } = &mut config.primary[0] else {
            panic!("TWAP source")
        };
        *rpc_group_b = "a".into();
        assert!(config.validate(1, "pha", true).is_err());
    }
    #[test]
    fn licensing_gate_cannot_be_overridden_in_production() {
        let p = volatile();
        assert!(p.validate_licensing(true).is_ok());
        assert!(p.validate_licensing(false).is_err());
        let mut p = p;
        p.allow_unclear_sources = false;
        assert!(p.validate_licensing(true).is_err());
        for (source, verdict) in [
            ("chainlink", Verdict::Allowed),
            ("uniswap-v2-onchain", Verdict::Allowed),
            ("kraken", Verdict::PermissionRequired),
            ("binance", Verdict::Prohibited),
            ("coinbase", Verdict::Prohibited),
            ("coinmetrics", Verdict::Prohibited),
        ] {
            assert_eq!(provider(source).unwrap().verdict, verdict);
        }
        for source in [volatile().primary[0].clone(), volatile().check[0].clone()] {
            let mut p = volatile();
            p.sources = vec![source];
            p.primary.clear();
            p.check.clear();
            p.fx.clear();
            assert!(p.validate_licensing(false).is_err());
        }
        let mut p = volatile();
        p.sources = vec![Source::Coinmetrics {
            asset: "pha".into(),
        }];
        p.primary.clear();
        p.check.clear();
        p.fx.clear();
        assert!(p.validate_licensing(true).is_err());
    }
    #[test]
    fn chainlink_only_stablecoins_are_production_eligible_without_opt_in() {
        for (asset, name) in [("usdc", "USDC_USD"), ("usdt", "USDT_USD")] {
            let p: PriceConfig = serde_json::from_value(serde_json::json!({
                "mode": "stablecoin",
                "sources": [{"source":"chainlink", "feed":name, "chain_id":1, "rpc_group":"a"}]
            }))
            .unwrap();
            assert!(!p.allow_unclear_sources);
            assert!(p.validate(1, asset, true).is_ok());
            assert!(p.validate_licensing(false).is_ok());
        }
    }
    #[test]
    fn testnet_feed_absence_and_cross_network_marker() {
        assert!(feed("USDT_USD", 11155111).is_none());
        let mut p = volatile();
        assert!(p.validate(11155111, "pha", false).is_err());
        if let Source::Chainlink {
            rpc_group_b,
            observation_chain_id,
            ..
        } = &mut p.fx[0]
        {
            *rpc_group_b = Some("mainnet-b".into());
            *observation_chain_id = Some(11155111);
        }
        assert!(p.validate(11155111, "pha", false).is_ok());
        assert!(p.validate(84532, "pha", false).is_err());
    }
    #[test]
    fn stablecoin_source_set_requires_coverage_or_explicit_mainnet_fallback() {
        let mut p: PriceConfig = serde_json::from_value(
            serde_json::json!({"mode":"stablecoin","allow_unclear_sources":true,
            "sources":[{"source":"kraken","symbol":"USDCUSD","company":"kraken"}]}),
        )
        .unwrap();
        assert!(p.validate(1, "usdc", true).is_err());
        p.sources.push(Source::Kraken {
            symbol: "USDTUSD".into(),
            company: "kraken".into(),
        });
        assert!(p.validate(1, "usdc", true).is_ok());
        p.sources.pop();
        p.sources.push(Source::Chainlink {
            feed: "USDC_USD".into(),
            chain_id: 1,
            rpc_group: "a".into(),
            rpc_group_b: None,
            observation_chain_id: None,
        });
        assert!(p.validate(1, "usdc", true).is_ok());
    }
    proptest! {
        #[test]
        fn never_accepts_a_single_volatile_company(remove_primary in any::<bool>(), remove_check in any::<bool>()) {
            let mut p = volatile();
            if remove_primary { p.primary.clear(); }
            if remove_check { p.check.clear(); }
            prop_assert_eq!(p.validate(1,"pha",true).is_ok(), !remove_primary && !remove_check);
        }
        #[test]
        fn company_aliases_cannot_create_independence(alias in "[a-z]{1,16}") {
            let mut p = volatile();
            p.check = vec![Source::Kraken {symbol:"PHAUSD".into(),company:alias}];
            prop_assert!(p.validate(1,"pha",true).is_err());
        }
    }
}
