//! Attested group, company and quota validation; secret resolution stays in-process.
use crate::rpc_provider::ProviderUrl;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use topup_adapters::chain::evm::{
    EvmClient,
    group::{
        GroupPolicy, Member, RpcGroup,
        budget::{BudgetSpec, Budgets},
    },
};
use topup_core::{price::Source, route::RouteFile};

/// Base mainnet chain identifier for price and sequencer feeds.
pub const BASE_CHAIN_ID: u64 = 8453;

/// Reviewed company identity with explicit domain evidence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Company {
    /// Domain suffixes reviewed as belonging to this company.
    pub domains: Vec<String>,
}
/// One URL/credential pair; templates may repeat with distinct sealed names.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberSpec {
    /// Stable metrics id.
    pub id: String,
    /// Reviewed company id.
    pub company: String,
    /// Attested URL template.
    pub url: String,
    /// Explicit sealed key name, absent for a public endpoint.
    #[serde(default)]
    pub sealed_key: Option<String>,
    /// Shared account quota.
    pub account_budget: String,
    /// Shared credential scope (synthetic for keyless members).
    pub key_budget: String,
    /// Priority order.
    #[serde(default)]
    pub priority: u32,
    /// Weighted round robin share.
    #[serde(default = "one")]
    pub weight: u32,
}
fn one() -> u32 {
    1
}
/// Members and policy for one chain and independent role.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupSpec {
    /// Chain served by every member.
    pub chain_id: u64,
    /// Complete member list, including unavailable backups.
    pub members: Vec<MemberSpec>,
    /// Resolved policy defaults.
    #[serde(default)]
    pub policy: GroupPolicy,
}
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
/// Validates all public configuration without secret or network access.
pub fn validate(
    routes: &[RouteFile],
    groups: &BTreeMap<String, GroupSpec>,
    companies: &BTreeMap<String, Company>,
    budgets: &BTreeMap<String, BudgetSpec>,
) -> Result<(), String> {
    Budgets::new(budgets).map_err(str::to_owned)?;
    let mut used = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut evidence = BTreeMap::<String, String>::new();
    let mut credentials = BTreeMap::new();
    for (company, spec) in companies {
        if !name(company) || spec.domains.is_empty() {
            return Err("invalid reviewed RPC company".to_owned());
        }
        for domain in &spec.domains {
            if domain.is_empty()
                || domain != &domain.to_ascii_lowercase()
                || domain.contains('/')
                || (psl::domain_str(domain).is_none()
                    && domain != "localhost"
                    && domain.parse::<std::net::IpAddr>().is_err())
            {
                return Err("invalid RPC company domain".to_owned());
            }
            for (other, owner) in &evidence {
                if owner != company
                    && (psl::domain_str(domain).unwrap_or(domain)
                        == psl::domain_str(other).unwrap_or(other)
                        || domain == other
                        || domain.ends_with(&format!(".{other}"))
                        || other.ends_with(&format!(".{domain}")))
                {
                    return Err("RPC company domain ownership overlaps".to_owned());
                }
            }
            evidence.insert(domain.clone(), company.clone());
        }
    }
    for (id, group) in groups {
        if !name(id) || !(1..=8).contains(&group.members.len()) {
            return Err("invalid RPC group id/member count".to_owned());
        }
        group.policy.validate().map_err(str::to_owned)?;
        if group
            .policy
            .rpc_error_rules
            .iter()
            .any(|rule| !group.members.iter().any(|m| m.company == rule.company))
        {
            return Err("RPC error rule company has no member in its group".into());
        }
        let mut identities = BTreeSet::new();
        for member in &group.members {
            if !name(&member.id)
                || !ids.insert(member.id.clone())
                || !(1..=1_000).contains(&member.weight)
            {
                return Err("invalid/duplicate RPC member id or weight".to_owned());
            }
            let url = ProviderUrl::parse(&member.url)
                .map_err(|e| format!("RPC member {} URL {e}", member.id))?;
            let parsed = url::Url::parse(&member.url.replace("{key}", "placeholder"))
                .map_err(|_| "invalid RPC URL")?;
            let host = parsed.host_str().ok_or("RPC member host required")?;
            let company = companies
                .get(&member.company)
                .ok_or("RPC company is not reviewed")?;
            if !company
                .domains
                .iter()
                .any(|d| host == d || host.ends_with(&format!(".{d}")))
            {
                return Err(format!(
                    "RPC member {} host is outside its reviewed company",
                    member.id
                ));
            }
            if !identities.insert((member.url.clone(), member.sealed_key.clone())) {
                return Err("duplicate RPC URL/credential identity".to_owned());
            }
            if url.is_keyed() != member.sealed_key.is_some() {
                return Err("RPC sealed_key must match URL placeholder".to_owned());
            }
            if let Some(key) = &member.sealed_key {
                if !key.starts_with("TOPUP_RPC_")
                    || !key.ends_with("_KEY")
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                {
                    return Err("invalid RPC sealed key name".to_owned());
                }
                let scopes = (member.account_budget.clone(), member.key_budget.clone());
                if credentials
                    .insert(key.clone(), scopes.clone())
                    .is_some_and(|previous| previous != scopes)
                {
                    return Err("one RPC credential must share account/key budgets".to_owned());
                }
            }
            if member.account_budget == member.key_budget
                || !budgets.contains_key(&member.account_budget)
                || !budgets.contains_key(&member.key_budget)
            {
                return Err("RPC member needs distinct configured account/key budgets".to_owned());
            }
        }
    }
    for route in routes {
        let mut role_companies = BTreeSet::new();
        for id in &route.chain.rpc_providers {
            let group = groups
                .get(id)
                .ok_or_else(|| format!("RPC group {id} is not configured"))?;
            if group.chain_id != route.chain.chain_id {
                return Err("RPC group chain mismatch".to_owned());
            }
            used.insert(id.clone());
        }
        let a = route
            .chain
            .rpc_providers
            .first()
            .and_then(|id| groups.get(id))
            .ok_or("RPC A group missing")?;
        for member in &a.members {
            role_companies.insert(&member.company);
        }
        let b = route
            .chain
            .rpc_providers
            .get(1)
            .and_then(|id| groups.get(id))
            .ok_or("RPC B group missing")?;
        if b.members
            .iter()
            .any(|m| role_companies.contains(&m.company))
        {
            return Err("RPC A/B groups share a reviewed provider company".to_owned());
        }
    }
    for route in routes {
        for (a, b, chain) in price_pairs(route)? {
            let ga = groups.get(&a).ok_or("price A group missing")?;
            let gb = groups.get(&b).ok_or("price B group missing")?;
            if a == b
                || ga.chain_id != chain
                || gb.chain_id != chain
                || ga
                    .members
                    .iter()
                    .any(|m| gb.members.iter().any(|n| n.company == m.company))
            {
                return Err("price RPC A/B must have matching chain and disjoint companies".into());
            }
            used.insert(a);
            used.insert(b);
        }
    }
    if used.len() != groups.len() {
        return Err("unused RPC group".to_owned());
    }
    Ok(())
}
/// Resolves sealed keys, constructs shared quota scopes and initializes typed group clients.
pub fn clients(
    groups: &BTreeMap<String, GroupSpec>,
    budgets: &BTreeMap<String, BudgetSpec>,
    key: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, Arc<EvmClient>>, String> {
    let budgets = Arc::new(Budgets::new(budgets).map_err(str::to_owned)?);
    let mut clients = BTreeMap::new();
    let mut credentials = BTreeMap::new();
    for (id, spec) in groups {
        let mut members = Vec::new();
        for m in &spec.members {
            let template = ProviderUrl::parse(&m.url).map_err(str::to_owned)?;
            let secret = m
                .sealed_key
                .as_deref()
                .and_then(&key)
                .filter(|v| !v.is_empty());
            if let Some(secret) = &secret {
                let identity = (m.company.clone(), secret.clone());
                let scopes = (m.account_budget.clone(), m.key_budget.clone());
                if credentials
                    .insert(identity, scopes.clone())
                    .is_some_and(|previous| previous != scopes)
                {
                    return Err(
                        "resolved RPC credential aliases must share account/key budgets".into(),
                    );
                }
            }
            let url = template
                .resolve(secret.as_deref())
                .map_err(|e| format!("RPC member {} key {e}", m.id))?;
            let endpoint = topup_adapters::redaction::Redacted::parse(&url)
                .map_err(|_| "invalid RPC endpoint")?
                .with_provider(m.id.clone());
            members.push(Member {
                id: m.id.clone(),
                company: m.company.clone(),
                endpoint,
                account: m.account_budget.clone(),
                key: m.key_budget.clone(),
                priority: m.priority,
                weight: m.weight,
            });
        }
        let group = RpcGroup::new(
            id.clone(),
            spec.chain_id,
            spec.policy.clone(),
            members,
            budgets.clone(),
        )
        .map_err(|e| e.to_string())?;
        clients.insert(
            id.clone(),
            Arc::new(EvmClient::from_group(group, None).map_err(|e| e.to_string())?),
        );
    }
    Ok(clients)
}

/// Resolves one on-chain source to the exact group identities validation approves.
pub fn price_pair(route: &RouteFile, source: &Source) -> Result<(String, String, u64), String> {
    let resolve = |id: &str| -> Result<String, String> {
        match id {
            "a" => route
                .chain
                .rpc_providers
                .first()
                .cloned()
                .ok_or("RPC A missing".into()),
            "b" => route
                .chain
                .rpc_providers
                .get(1)
                .cloned()
                .ok_or("RPC B missing".into()),
            _ => Ok(id.to_owned()),
        }
    };
    match source {
        Source::UniswapV2Twap {
            rpc_group,
            rpc_group_b,
            ..
        } => Ok((resolve(rpc_group)?, resolve(rpc_group_b)?, 1)),
        Source::Chainlink {
            chain_id,
            rpc_group,
            rpc_group_b,
            ..
        } => {
            let a = resolve(rpc_group)?;
            let b = match rpc_group_b {
                Some(b) => resolve(b)?,
                None => {
                    let ra = resolve("a")?;
                    let rb = resolve("b")?;
                    if a == ra {
                        rb
                    } else if a == rb {
                        ra
                    } else {
                        return Err("explicit price group requires rpc_group_b".into());
                    }
                }
            };
            Ok((a, b, *chain_id))
        }
        _ => Err("price source has no RPC pair".into()),
    }
}

/// Observation-only group pairs, including sequencer checks, validated like route groups.
pub fn price_pairs(route: &RouteFile) -> Result<Vec<(String, String, u64)>, String> {
    let mut pairs = Vec::new();
    for (_, sources) in route.pricing.roles() {
        for source in sources {
            if matches!(
                source,
                Source::UniswapV2Twap { .. } | Source::Chainlink { .. }
            ) {
                pairs.push(price_pair(route, source)?);
            }
        }
    }
    if let Some(s) = &route.pricing.sequencer_uptime {
        pairs.push((s.rpc_group.clone(), s.rpc_group_b.clone(), BASE_CHAIN_ID));
    }
    Ok(pairs)
}
