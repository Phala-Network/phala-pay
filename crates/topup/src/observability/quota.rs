//! Configured operational stop lines and provider billing units from deploy/RPC.md.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::num::NonZeroU32;

/// Optional monitoring configuration. Omitted deployments keep their existing behavior.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaAlerts {
    /// Environment-wide newly observed deposits per UTC day (D); monitoring, not admission.
    pub deposits_per_day: Option<NonZeroU32>,
    /// Account allocations, aggregating the listed endpoint ids within this deployment.
    #[serde(default)]
    pub rpc: Vec<RpcQuota>,
}

/// An account's daily operational allocation, in its provider's billing units.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcQuota {
    /// Stable, bounded alert component, for example `ankr`.
    pub name: String,
    /// Exact configured endpoint ids sharing this account allocation.
    pub providers: Vec<String>,
    /// Unit weights from the provider's published billing schedule.
    pub unit: RpcUnit,
    /// Positive daily stop line; warning is at ceil(80% × stop_line).
    pub stop_line: NonZeroU32,
}

/// Billing schedule; unknown methods conservatively use the listed fallback rate.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcUnit {
    /// Ankr: one call per JSON-RPC method.
    Calls,
    /// BlockPI base RU (excludes log response-size surcharges).
    BlockpiRu,
    /// Infura: 255 credits for logs, 80 for other methods.
    InfuraCredits,
}

impl RpcUnit {
    pub(super) fn weight(self, method: &str) -> i64 {
        match self {
            Self::Calls => 1,
            Self::InfuraCredits if method == "eth_getLogs" => 255,
            Self::InfuraCredits => 80,
            Self::BlockpiRu => match method {
                "eth_chainId" => 5,
                "eth_blockNumber" => 10,
                "eth_getCode" | "eth_getTransactionReceipt" => 15,
                "eth_getBlockByNumber" | "eth_getBlockByHash" | "eth_call" => 20,
                "eth_getLogs" => 50,
                _ => 35,
            },
        }
    }
}

impl QuotaAlerts {
    /// Validate bounded names and references to this deployment's exact endpoint ids.
    pub fn validate(&self, endpoints: &[crate::chain_rpc::ChainRpcSpec]) -> Result<(), String> {
        if self.rpc.len() > 8 {
            return Err("quota_alerts.rpc allows at most 8 allocations".into());
        }
        let ids: BTreeSet<_> = endpoints
            .iter()
            .flat_map(|chain| [&chain.read.id, &chain.verify.id])
            .collect();
        let mut names = BTreeSet::new();
        let mut assigned = BTreeSet::new();
        for quota in &self.rpc {
            if quota.name.is_empty()
                || quota.name.len() > 40
                || !quota
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || !names.insert(&quota.name)
            {
                return Err(
                    "quota_alerts.rpc names must be unique and match [a-z0-9-]{1,40}".into(),
                );
            }
            if quota.providers.is_empty()
                || quota
                    .providers
                    .iter()
                    .any(|id| !ids.contains(id) || !assigned.insert(id))
            {
                return Err(
                    "quota_alerts.rpc providers must name configured endpoints once".into(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn allocations_round_trip_and_reject_unknown_duplicate_or_unbounded_dimensions() {
        let configured = include_str!(
            "../../../../deploy/environments/phala-network/production/topup/topup.yaml"
        );
        let config = crate::config::Config::parse(configured).unwrap();
        let again = crate::config::Config::parse(&config.resolved_json().unwrap()).unwrap();
        assert_eq!(again.quota_alerts.rpc[0].stop_line.get(), 20_442);
        assert!(
            crate::config::Config::parse(&configured.replace("stop_line: 20442", "stop_line: 0"))
                .is_err()
        );
        assert!(
            crate::config::Config::parse(&configured.replace(
                "providers: [ankr-mainnet, ankr-base-mainnet]",
                "providers: [unknown]"
            ))
            .is_err()
        );
        assert!(
            crate::config::Config::parse(&configured.replace(
                "providers: [ankr-mainnet, ankr-base-mainnet]",
                "providers: [ankr-mainnet, ankr-mainnet]"
            ))
            .is_err()
        );
        assert!(
            crate::config::Config::parse(&configured.replace("name: ankr", "name: ../secret"))
                .is_err()
        );
        assert!(
            crate::config::Config::parse(&configured.replace("unit: calls", "unit: invented"))
                .is_err()
        );
        let staging = crate::config::Config::parse(include_str!(
            "../../../../deploy/environments/phala-network/staging/topup/topup.yaml"
        ))
        .unwrap();
        assert_eq!(
            staging.quota_alerts.rpc[0].stop_line.get()
                + config.quota_alerts.rpc[0].stop_line.get(),
            25_000
        );
        assert_eq!(staging.quota_alerts.rpc[0].providers.len(), 4);
    }
}
