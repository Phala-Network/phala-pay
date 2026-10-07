//! Exactly two independently configured endpoints per chain.
use crate::rpc_provider::ProviderUrl;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;
use topup_adapters::chain::evm::EvmClient;
use topup_core::route::RouteFile;

/// One public endpoint template and its owner-sealed credential name.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    /// Bounded metric label, never a URL or secret.
    pub id: String,
    /// HTTPS template with one whole-segment or whole-query-value key.
    pub url: String,
    /// Explicit environment variable supplied through compose.
    pub sealed_key: String,
    /// Measured maximum inclusive log range.
    pub max_log_blocks: u32,
}
/// Public RPC configuration of a route or price chain.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChainRpcSpec {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Ankr read endpoint.
    pub read: EndpointSpec,
    /// Independent Infura verify endpoint.
    pub verify: EndpointSpec,
}
/// Shared endpoint clients, without selection, member pinning or failover.
#[derive(Clone, Debug)]
pub struct ChainRpc {
    /// Read client.
    pub read: Arc<EvmClient>,
    /// Independently decoded verification client.
    pub verify: Arc<EvmClient>,
}

/// Validate the complete public schema without reading any credential.
pub fn validate(specs: &[ChainRpcSpec], routes: &[RouteFile]) -> Result<(), String> {
    let mut chains = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for spec in specs {
        if spec.chain_id == 0
            || i64::try_from(spec.chain_id).is_err()
            || !chains.insert(spec.chain_id)
        {
            return Err("rpc.chain_id must be positive, fit bigint and be unique".into());
        }
        let mut hosts = Vec::new();
        for endpoint in [&spec.read, &spec.verify] {
            if endpoint.id.is_empty()
                || endpoint.id.len() > 40
                || !endpoint
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                || !ids.insert(&endpoint.id)
            {
                return Err("rpc endpoint ids must be unique and match [a-z0-9-]{1,40}".into());
            }
            let key = endpoint
                .sealed_key
                .strip_prefix("TOPUP_RPC_")
                .and_then(|k| k.strip_suffix("_KEY"));
            if key.is_none_or(|k| {
                k.is_empty()
                    || !k
                        .bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            }) {
                return Err(format!(
                    "endpoint {} has an invalid sealed_key name",
                    endpoint.id
                ));
            }
            if endpoint.max_log_blocks == 0 {
                return Err(format!(
                    "endpoint {} max_log_blocks must be positive",
                    endpoint.id
                ));
            }
            let template = ProviderUrl::parse(&endpoint.url)
                .map_err(|e| format!("endpoint {}: {e}", endpoint.id))?;
            if !template.is_keyed() {
                return Err(format!(
                    "endpoint {} requires a {{key}} placeholder",
                    endpoint.id
                ));
            }
            let url = url::Url::parse(&endpoint.url.replace("{key}", "template-key"))
                .map_err(|_| "invalid RPC URL")?;
            if url.scheme() != "https" {
                return Err("rpc endpoints must use https".into());
            }
            hosts.push(
                url.host_str()
                    .ok_or("rpc endpoint has no host")?
                    .trim_end_matches('.')
                    .to_owned(),
            );
        }
        if hosts.first() == hosts.get(1) {
            return Err("read and verify endpoints must have different hosts".into());
        }
    }
    let mut required = BTreeSet::new();
    for route in routes {
        required.insert(route.chain.chain_id);
        for (_, sources) in route.pricing.roles() {
            for source in sources {
                match source {
                    topup_core::price::Source::Chainlink { chain_id, .. } => {
                        required.insert(*chain_id);
                    }
                    topup_core::price::Source::UniswapV2Twap { .. } => {
                        required.insert(1);
                    }
                    _ => {}
                }
            }
        }
        if route.pricing.sequencer_uptime.is_some() {
            required.insert(8453);
        }
    }
    if !required.is_subset(&chains) {
        return Err("rpc must configure read and verify for every route and price chain".into());
    }
    Ok(())
}

impl ChainRpcSpec {
    /// Resolve only the declared sealed keys; errors never contain their values.
    pub fn clients(&self, env: impl Fn(&str) -> Option<String>) -> Result<ChainRpc, String> {
        let build = |endpoint: &EndpointSpec, verify: bool| -> Result<Arc<EvmClient>, String> {
            let template = ProviderUrl::parse(&endpoint.url).map_err(str::to_owned)?;
            let key = env(&endpoint.sealed_key).filter(|k| !k.is_empty());
            let url = template
                .resolve(key.as_deref())
                .map_err(|e| format!("{} {e} (endpoint {})", endpoint.sealed_key, endpoint.id))?;
            let mut client = EvmClient::new(&url)
                .map_err(|_| format!("endpoint {} could not be configured", endpoint.id))?
                .with_provider(&endpoint.id)
                .with_chain_id(self.chain_id)
                .with_max_log_blocks(endpoint.max_log_blocks);
            client.contract_checked(false);
            if verify {
                client = client.as_verify();
            }
            Ok(Arc::new(client))
        };
        Ok(ChainRpc {
            read: build(&self.read, false)?,
            verify: build(&self.verify, true)?,
        })
    }
}
