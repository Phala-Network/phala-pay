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
    /// Commercial use prohibited.
    Restricted,
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
            "https://docs.chain.link/data-feeds",
            "Data Feeds provide your smart contracts with access to real-world data",
            "shared RPC account/key budgets",
            Verdict::Unclear,
        ),
        "kraken" => (
            "https://docs.kraken.com/api/",
            "The Kraken API provides access to market data and trading functionality",
            "1 request/second per process",
            Verdict::Unclear,
        ),
        "binance" => (
            "https://data.binance.vision/",
            "public market data",
            "1 request/second per process",
            Verdict::Unclear,
        ),
        "coinmetrics" => (
            "https://docs.coinmetrics.io/packages/coin-metrics-community-data",
            "non-commercial use only",
            "disabled",
            Verdict::Restricted,
        ),
        _ => return None,
    };
    Some(Provider {
        company: match source {
            "chainlink" => "chainlink",
            "kraken" => "kraken",
            "binance" => "binance",
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
    /// Maximum interval between updates.
    pub heartbeat_s: u64,
    /// Clock and publication allowance.
    pub margin_s: u64,
    /// Published deviation trigger in basis points.
    pub deviation_bps: u16,
}
/// Unsupported/testnet feeds never resolve implicitly.
pub fn feed(name: &str, chain: u64) -> Option<Feed> {
    let (address, heartbeat_s, deviation_bps) = match (chain, name) {
        (1, "USDC_USD") => ("0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6", 82800, 25),
        (1, "USDT_USD") => ("0x3E7d1eAB13ad0104d2750B8863b489D65364e32D", 86400, 25),
        (8453, "USDC_USD") => ("0x7e860098F58bBFC8648a4311b374B1D669a2bc6B", 86400, 30),
        (8453, "USDT_USD") => ("0xf19d560eB8d2ADf07BD6D13ed03e1D11215721F9", 86400, 30),
        (8453, "BASE_SEQUENCER_UPTIME") => ("0xBCF85224fc0756B9Fa45aA7892530B47e10b6433", 0, 0),
        _ => return None,
    };
    Some(Feed {
        name: match name {
            "USDC_USD" => "USDC_USD",
            "USDT_USD" => "USDT_USD",
            _ => "BASE_SEQUENCER_UPTIME",
        },
        chain_id: chain,
        address,
        decimals: if heartbeat_s == 0 { 0 } else { 8 },
        heartbeat_s,
        margin_s: 60,
        deviation_bps,
    })
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
            Self::Kraken { .. } => "kraken",
            Self::Binance { .. } => "binance",
        }
    }
    /// Asset observed (USD for Chainlink and Kraken, USDT for Binance).
    pub fn asset(&self) -> &str {
        match self {
            Self::Coinmetrics { asset } => asset,
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
    /// Explicit staging-only opt-in; never legal approval.
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
                if verdict == Some(Verdict::Restricted)
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
                if verdict == Verdict::Restricted
                    || (verdict != Verdict::Allowed && !self.allow_unclear_sources)
                {
                    return Err(fail(
                        "source is not Allowed; staging must explicitly opt in",
                    ));
                }
                match source {
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
    fn licensing_gate_cannot_be_overridden_in_production() {
        let p = volatile();
        assert!(p.validate_licensing(true).is_ok());
        assert!(p.validate_licensing(false).is_err());
        let mut p = p;
        p.allow_unclear_sources = false;
        assert!(p.validate_licensing(true).is_err());
        assert_eq!(
            provider("coinmetrics").unwrap().verdict,
            Verdict::Restricted
        );
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
