//! The attested route set, validated once at startup and shared by every consumer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use alloy_primitives::Address;
use topup_adapters::chain::evm::EvmClient;
use topup_core::route::{ChainConfig, RouteFile};

use crate::rpc_provider::{ProviderUrl, environment_key, provider_label};

/// Every loaded route version with one shared RPC client per chain provider.
///
/// Construction checks everything that must agree across routes: unique versions, one finality
/// rule across a chain's current routes, one provider list per chain, one route name per chain asset, and one destination unit
/// across rate-lock routes.
#[derive(Debug, Default)]
pub struct RouteSet {
    routes: Vec<RouteFile>,
    environment: String,
    rpc: BTreeMap<u64, crate::chain_rpc::ChainRpc>,
    current: BTreeMap<(u64, Address), usize>,
    chains: BTreeMap<u64, ChainEntry>,
}

#[derive(Debug)]
struct ChainEntry {
    /// Chain settings of the first loaded route, with the confirmation floor of the chain's current
    /// routes; every route on the chain agrees on its providers.
    config: ChainConfig,
    /// One client per `rpc_providers` entry, or why the entry is unusable.
    providers: Vec<Result<Arc<EvmClient>, ProviderError>>,
}

/// A provider entry that cannot be used.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProviderError {
    /// The provider id has no URL in the configuration's `rpc_providers`.
    #[error("`{label}` is not a configured RPC provider (rpc_providers)")]
    MissingUrl {
        /// Log-safe provider label.
        label: String,
    },
    /// The provider id's key does not fit its URL (`crate::rpc_provider`).
    #[error("{environment} {problem} for `{label}`")]
    InvalidKey {
        /// Log-safe provider label.
        label: String,
        /// Environment variable that holds the key.
        environment: String,
        /// Log-safe reason, never the value.
        problem: &'static str,
    },
    /// The resolved value is not a URL.
    #[error("provider `{label}` has an invalid URL")]
    InvalidUrl {
        /// Log-safe provider label.
        label: String,
    },
    /// No loaded route configures this chain or provider position.
    #[error("chain {chain_id} has no RPC provider {index}")]
    Unconfigured {
        /// EVM chain identifier.
        chain_id: u64,
        /// Position in `chain.rpc_providers`.
        index: usize,
    },
}

impl RouteSet {
    /// Validates the loaded routes and creates one client per chain provider, for routes whose
    /// providers are inline URLs (tests). A provider id resolves to nothing here; the service
    /// resolves ids through its configuration ([`RouteSet::with_providers`]).
    pub fn new(routes: Vec<RouteFile>) -> Result<Self, String> {
        Self::from_resolver(routes, |_| None)
    }

    /// Validates the loaded routes and creates one client per chain provider, each id resolved
    /// through `providers` (the configuration's `rpc_providers`) and its sealed key from the
    /// environment (`TOPUP_RPC_<ID>_KEY`).
    ///
    /// A provider whose URL or key is missing or invalid does not fail construction; the consumer
    /// that needs it fails instead, so commands that use only provider A do not require provider B.
    /// `topup run` checks every provider at startup ([`crate::contracts::verify_routes`]).
    pub fn with_providers(
        routes: Vec<RouteFile>,
        providers: &BTreeMap<String, ProviderUrl>,
    ) -> Result<Self, String> {
        Self::from_resolver(routes, |id| {
            providers
                .get(id)
                .map(|url| (url.clone(), environment_key(id)))
        })
    }

    /// Uses the validated read/verify clients, including observation-only price chains.
    pub fn with_rpc(
        routes: Vec<RouteFile>,
        rpc: BTreeMap<u64, crate::chain_rpc::ChainRpc>,
    ) -> Result<Self, String> {
        let (current, chains) = index(&routes, |chain| match rpc.get(&chain.chain_id) {
            Some(pair) => vec![Ok(pair.read.clone()), Ok(pair.verify.clone())],
            None => Vec::new(),
        })?;
        Ok(Self {
            routes,
            environment: String::new(),
            rpc,
            current,
            chains,
        })
    }

    /// Checks everything that must agree across routes, without creating a client or reading a
    /// secret: the validation `topup config check` shares with the service.
    pub fn check(routes: &[RouteFile]) -> Result<(), String> {
        index(routes, |_| Vec::new()).map(drop)
    }

    /// Attach the attested environment for staging-only sampler cadence.
    pub fn with_environment(mut self, environment: String) -> Self {
        self.environment = environment;
        self
    }
    /// Staging alone uses five-minute PHA sampling; default routes retain one-minute sampling.
    pub fn staging(&self) -> bool {
        self.environment == "staging"
    }
    /// All explicitly configured endpoints, including price-only chains.
    pub fn rpc(&self) -> &BTreeMap<u64, crate::chain_rpc::ChainRpc> {
        &self.rpc
    }

    fn from_resolver(
        routes: Vec<RouteFile>,
        resolve: impl Fn(&str) -> Option<(ProviderUrl, Option<String>)>,
    ) -> Result<Self, String> {
        let (current, chains) = index(&routes, |chain| {
            chain
                .rpc_providers
                .iter()
                .enumerate()
                .map(|(position, provider)| {
                    resolve_provider(provider, position, chain.chain_id, &resolve)
                })
                .collect()
        })?;
        Ok(Self {
            routes,
            environment: String::new(),
            rpc: BTreeMap::new(),
            current,
            chains,
        })
    }

    /// An explicitly configured pair is usable only after both endpoints pass their checks.
    pub fn chain_ready(&self, chain: u64) -> bool {
        self.rpc
            .get(&chain)
            .is_none_or(|pair| pair.read.ready() && pair.verify.ready())
    }
    /// Returns every loaded route version, in load order.
    #[must_use]
    pub fn routes(&self) -> &[RouteFile] {
        &self.routes
    }

    /// Returns the highest loaded version of each chain asset's route, ordered by chain and asset.
    pub fn current(&self) -> impl Iterator<Item = &RouteFile> {
        self.current
            .values()
            .filter_map(|index| self.routes.get(*index))
    }

    /// Returns the configured chain ids in ascending order.
    pub fn chain_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.chains.keys().copied()
    }

    /// Returns the shared settings of a configured chain.
    #[must_use]
    pub fn chain(&self, chain_id: u64) -> Option<&ChainConfig> {
        self.chains.get(&chain_id).map(|chain| &chain.config)
    }

    /// Returns the shared client of the provider at `index` in the chain's `rpc_providers`.
    pub fn provider(&self, chain_id: u64, index: usize) -> Result<&Arc<EvmClient>, ProviderError> {
        if let Some(pair) = self.rpc.get(&chain_id) {
            return match index {
                0 => Ok(&pair.read),
                1 => Ok(&pair.verify),
                _ => Err(ProviderError::Unconfigured { chain_id, index }),
            };
        }
        self.chains
            .get(&chain_id)
            .and_then(|chain| chain.providers.get(index))
            .ok_or(ProviderError::Unconfigured { chain_id, index })?
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Returns the current routes of one mode: a test-mode key quotes only on test routes, a
    /// live-mode key only on live ones (design D9).
    pub fn current_in(&self, livemode: bool) -> impl Iterator<Item = &RouteFile> {
        self.current()
            .filter(move |route| route.livemode == livemode)
    }
}

/// Current route per chain asset, and each chain's settings with its providers.
type Index = (BTreeMap<(u64, Address), usize>, BTreeMap<u64, ChainEntry>);

/// Checks everything that must agree across routes: unique versions, each route's own
/// validation, one finality rule and provider list per chain, and one route name per chain asset.
/// `providers` builds a chain's clients (none when only checking).
fn index(
    routes: &[RouteFile],
    providers: impl Fn(&ChainConfig) -> Vec<Result<Arc<EvmClient>, ProviderError>>,
) -> Result<Index, String> {
    let mut versions = BTreeSet::new();
    for route in routes {
        if !versions.insert((route.route.as_str(), route.version)) {
            return Err(format!(
                "duplicate route `{}` version {}",
                route.route, route.version
            ));
        }
    }
    let mut current = BTreeMap::<(u64, Address), usize>::new();
    let mut chains = BTreeMap::<u64, ChainEntry>::new();
    for (index, route) in routes.iter().enumerate() {
        route.validate().map_err(|error| {
            format!(
                "route `{}` version {} failed validation: {error}",
                route.route, route.version
            )
        })?;
        let chain_id = route.chain.chain_id;
        let chain = chains.entry(chain_id).or_insert_with(|| ChainEntry {
            config: route.chain.clone(),
            providers: providers(&route.chain),
        });
        if chain.config.rpc_providers != route.chain.rpc_providers {
            return Err(format!(
                "route `{}` version {} disagrees with another chain {chain_id} scanner configuration",
                route.route, route.version
            ));
        }
        let asset = (chain_id, route.asset.contract);
        match current
            .get(&asset)
            .and_then(|selected| routes.get(*selected))
        {
            Some(selected) if selected.route != route.route => {
                return Err(format!(
                    "routes `{}` and `{}` both select chain {chain_id} asset {:#x}",
                    selected.route, route.route, route.asset.contract
                ));
            }
            Some(selected) if selected.version >= route.version => {}
            Some(_) | None => {
                current.insert(asset, index);
            }
        }
    }
    // One floor per chain (design D1), its current versions': the scanner, the confirm step, and
    // the API all read the chain's, so a current route that credits at another value, written or
    // defaulted, is refused. An earlier version keeps its own value only for the terms of what it
    // governed, so a new version may raise the floor (docs/design/payment-settings.md §12).
    for (chain_id, chain) in &mut chains {
        let mut on_chain = current
            .values()
            .filter_map(|index| routes.get(*index))
            .filter(|route| route.chain.chain_id == *chain_id);
        let Some(first) = on_chain.next() else {
            continue;
        };
        if let Some(other) =
            on_chain.find(|route| route.chain.confirmations != first.chain.confirmations)
        {
            return Err(format!(
                "route `{}` version {} credits chain {chain_id} at confirmations `{}`, but route \
                 `{}` version {} at `{}`: the current routes of a chain must share one \
                 confirmation floor (write the same `chain.confirmations`, defaults included)",
                other.route,
                other.version,
                other.chain.confirmations.policy_value(),
                first.route,
                first.version,
                first.chain.confirmations.policy_value()
            ));
        }
        chain.config.confirmations = first.chain.confirmations;
    }
    Ok((current, chains))
}

/// One provider entry's client: an inline URL (tests) as is, or a configured id's URL with its
/// sealed key in place of `{key}`.
fn resolve_provider(
    provider: &str,
    index: usize,
    chain_id: u64,
    resolve: &impl Fn(&str) -> Option<(ProviderUrl, Option<String>)>,
) -> Result<Arc<EvmClient>, ProviderError> {
    let label = provider_label(provider, index);
    let url = if provider.contains("://") {
        provider.to_owned()
    } else {
        let (url, key) = resolve(provider).ok_or_else(|| ProviderError::MissingUrl {
            label: label.clone(),
        })?;
        url.resolve(key.as_deref())
            .map_err(|problem| ProviderError::InvalidKey {
                label: label.clone(),
                environment: crate::rpc_provider::key_environment(provider),
                problem,
            })?
    };
    EvmClient::new(&url)
        .map(|client| Arc::new(client.with_provider(label.clone()).with_chain_id(chain_id)))
        .map_err(|_| ProviderError::InvalidUrl { label })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> RouteFile {
        serde_saphyr::from_str(include_str!("../tests/fixtures/phala-cloud-pha.yaml"))
            .expect("route fixture parses")
    }

    #[test]
    fn route_loading_accepts_versions_and_rejects_exact_duplicates() {
        let route = fixture();
        let mut newer = route.clone();
        newer.version = route.version + 1;

        let set = RouteSet::new(vec![route.clone(), newer]).expect("versions load");
        assert_eq!(
            set.current().map(|route| route.version).collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(
            RouteSet::new(vec![route.clone(), route]).map(|_| ()),
            Err("duplicate route `phala-cloud-ethereum-pha-usd` version 1".to_owned())
        );
    }

    #[test]
    fn each_mode_sees_only_its_own_routes() {
        let live = fixture();
        let mut test = live.clone();
        test.route = "sepolia-route".to_owned();
        test.livemode = false;
        test.chain.chain_id = 11_155_111;
        let set = RouteSet::new(vec![live, test]).expect("a live and a test route load");
        let names = |livemode| {
            set.current_in(livemode)
                .map(|route| route.route.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(true), ["phala-cloud-ethereum-pha-usd"]);
        assert_eq!(names(false), ["sepolia-route"]);
    }

    /// A Base route of the fixture, with `confirmations` written or, for `None`, left to the
    /// OP-stack default.
    fn base_route(route: &str, token: u8, confirmations: Option<&str>) -> RouteFile {
        let written =
            confirmations.map_or_else(String::new, |value| format!("  confirmations: {value}\n"));
        serde_saphyr::from_str(
            &include_str!("../tests/fixtures/phala-cloud-pha.yaml")
                .replace(
                    "price:\n",
                    "price:\n  sequencer_uptime: { feed: BASE_SEQUENCER_UPTIME, grace_s: 3600 }\n",
                )
                .replace("chain_id: 1\n", "chain_id: 8453\n")
                .replace("phala-cloud-ethereum-pha-usd", route)
                .replace("  confirmations: finalized\n", &written)
                .replace(
                    "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
                    &format!("{:#x}", Address::repeat_byte(token)),
                ),
        )
        .expect("Base route parses")
    }

    #[test]
    fn one_chain_has_one_confirmation_floor_whatever_the_route_order() {
        let default = base_route("base-pha", 0x41, None);
        assert_eq!(
            default.chain.confirmations,
            topup_core::route::Confirmations::Depth(3)
        );
        for stricter in ["safe", "finalized"] {
            let explicit = base_route("base-usdc", 0x42, Some(stricter));
            for routes in [
                vec![default.clone(), explicit.clone()],
                vec![explicit.clone(), default.clone()],
            ] {
                let error = RouteSet::new(routes.clone())
                    .map(drop)
                    .expect_err("one chain has one confirmation floor");
                assert!(
                    error.contains("must share one confirmation floor")
                        && error.contains("`3`")
                        && error.contains(&format!("`{stricter}`")),
                    "{error}"
                );
                assert_eq!(RouteSet::check(&routes), Err(error));
            }
        }
        // The default written out is the same floor.
        RouteSet::new(vec![default, base_route("base-usdc", 0x42, Some("3"))])
            .expect("routes that agree on the floor load");
    }

    #[test]
    fn one_chain_has_one_provider_list_and_route_per_asset() {
        let route = fixture();

        let mut providers = route.clone();
        providers.route = "other-route".to_owned();
        providers.asset.contract = Address::repeat_byte(0x42);
        providers.chain.rpc_providers = vec!["a".to_owned(), "b".to_owned()];
        assert!(
            RouteSet::new(vec![route.clone(), providers])
                .expect_err("one chain has one provider list")
                .contains("disagrees with another chain 1")
        );

        let mut same_asset = route.clone();
        same_asset.route = "other-route".to_owned();
        assert!(
            RouteSet::new(vec![route, same_asset])
                .expect_err("one asset has one route name")
                .contains("both select chain 1 asset")
        );
    }

    #[test]
    fn providers_resolve_lazily_with_log_safe_labels() {
        let mut route = fixture();
        route.chain.rpc_providers = vec![
            "https://user:secret@rpc.example/v1?key=secret".to_owned(),
            "r1-routeset-unset-provider".to_owned(),
        ];
        let set = RouteSet::new(vec![route]).expect("missing provider B does not fail loading");

        let primary = set.provider(1, 0).expect("inline URL resolves");
        assert_eq!(
            primary.endpoint().to_string(),
            "provider `rpc_providers[0]`"
        );
        assert_eq!(
            set.provider(1, 1).map(|_| ()),
            Err(ProviderError::MissingUrl {
                label: "r1-routeset-unset-provider".to_owned(),
            })
        );
        assert_eq!(
            set.provider(2, 0).map(|_| ()),
            Err(ProviderError::Unconfigured {
                chain_id: 2,
                index: 0
            })
        );
    }
}
