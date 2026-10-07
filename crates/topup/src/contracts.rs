//! Startup verification of the deployed forwarder contracts against the attested routes (§4).

use std::collections::BTreeSet;

use alloy_primitives::{Address, B256, b256, keccak256};
use topup_adapters::chain::flush::ContractAddressGetter;
use topup_core::address::forwarder_address;
use topup_core::route::RouteFile;

use topup_adapters::chain::evm::{ChainError, EvmClient, MULTICALL3};

use crate::routes::RouteSet;

// The build fingerprints below are recorded by `deploy/contracts/check-build.sh --write` in
// `deploy/contracts/expected-codehashes.json`; a unit test keeps these copies equal to that file,
// which the service image does not contain.

/// `ForwarderFactory` runtime code hash with its immutable words zeroed.
const FACTORY_RUNTIME_TEMPLATE_HASH: B256 =
    b256!("ba68b652c46ff52b46499a742ab1805017269a13aa0fdb0928bd2a9cb11cc3b0");

/// Byte offsets of the 32-byte `implementation` words in `ForwarderFactory` runtime code.
const FACTORY_IMPLEMENTATION_OFFSETS: &[usize] = &[105, 480, 606, 887];

/// `Forwarder` runtime code hash with its immutable words zeroed.
const FORWARDER_RUNTIME_TEMPLATE_HASH: B256 =
    b256!("c34523cf7740433498fe0e3a7ef27417b28fe06207095d5a2482afb2d442a435");

/// Byte offsets of the 32-byte `factory` words in `Forwarder` runtime code.
const FORWARDER_FACTORY_OFFSETS: &[usize] = &[208, 319];

/// Runtime code hash of the canonical Multicall3, identical on Ethereum mainnet and Sepolia; a
/// unit test keeps it equal to `deploy/contracts/multicall3.json`.
const MULTICALL3_RUNTIME_CODE_HASH: B256 =
    b256!("d5c15df687b16f2ff992fc8d767b4216323184a2bbc6ee2f9c398c318e770891");

/// Salt used to compare the factory's `addressOf` with local address derivation.
#[must_use]
pub fn sample_salt() -> B256 {
    keccak256("crypto-topup-service.startup-check")
}

/// Treasury used to compare the factory's `addressOf` with local address derivation. Treasuries
/// are the accounts' (design D10), so any fixed address serves.
#[must_use]
pub fn sample_treasury() -> Address {
    Address::from_word(keccak256("crypto-topup-service.startup-check.treasury"))
}

/// Checks every route's contracts on every configured RPC provider before the service starts.
pub async fn verify_routes(routes: &RouteSet) -> Result<(), String> {
    let mut checked = BTreeSet::new();
    for route in routes.routes() {
        let contracts = &route.chain.contracts;
        for (index, provider) in route.chain.rpc_providers.iter().enumerate() {
            let key = (
                route.chain.chain_id,
                provider.as_str(),
                contracts.forwarder_factory,
                contracts.implementation,
            );
            if !checked.insert(key) {
                continue;
            }
            let client = routes
                .provider(route.chain.chain_id, index)
                .map_err(|error| error.to_string())?;
            let label = client.endpoint().provider().unwrap_or_default();
            verify_on(client, route)
                .await
                .map_err(|error| format!("route `{}` via `{label}`: {error}", route.route))?;
        }
    }
    Ok(())
}

/// Compares one provider's view of the contracts with the route. The getters come first so a
/// mismatch names the differing address; the code hashes then prove the immutables are the only
/// difference from the audited build. The treasury is not in any contract: it is each clone's
/// argument, so a sample `addressOf(treasury, salt)` proves the factory derives addresses as the
/// service does for any treasury.
pub(crate) async fn verify_on(client: &EvmClient, route: &RouteFile) -> Result<(), String> {
    let contracts = &route.chain.contracts;
    let factory = contracts.forwarder_factory;
    let implementation = contracts.implementation;
    let read = |probe: &str, error: ChainError| format!("{probe}: {error}");

    // Every balance and `addressOf` read, including the sample below, goes through Multicall3.
    let multicall = client
        .code_at(MULTICALL3)
        .await
        .map_err(|e| read("Multicall3 code", e))?;
    if multicall.is_empty() {
        return Err(format!(
            "RPC capability unavailable: Multicall3 {MULTICALL3:#x} has no code on chain {}; balance and addressOf reads \
             are aggregated through it",
            route.chain.chain_id
        ));
    }
    if keccak256(&multicall) != MULTICALL3_RUNTIME_CODE_HASH {
        return Err(format!(
            "RPC member identity invalid: Multicall3 {MULTICALL3:#x} on chain {} is not the canonical deployment (code hash \
             {:#x})",
            route.chain.chain_id,
            keccak256(&multicall)
        ));
    }

    let actual = client
        .contract_address(factory, ContractAddressGetter::Implementation)
        .await
        .map_err(|e| read("factory implementation() capability", e))?;
    if actual != implementation {
        return Err(format!(
            "RPC member identity invalid: factory implementation() is {actual:#x}, route expects {implementation:#x}"
        ));
    }
    let owner = client
        .contract_address(implementation, ContractAddressGetter::Factory)
        .await
        .map_err(|e| read("implementation factory() capability", e))?;
    if owner != factory {
        return Err(format!(
            "RPC member identity invalid: implementation factory() is {owner:#x}, route expects {factory:#x}"
        ));
    }
    let factory_code = client
        .code_at(factory)
        .await
        .map_err(|e| read("factory code", e))?;
    verify_code(
        &factory_code,
        &[(implementation, FACTORY_IMPLEMENTATION_OFFSETS)],
        FACTORY_RUNTIME_TEMPLATE_HASH,
    )
    .map_err(|error| format!("RPC member identity invalid: factory {factory:#x} {error} of the ForwarderFactory build"))?;
    let implementation_code = client
        .code_at(implementation)
        .await
        .map_err(|e| read("implementation code", e))?;
    verify_code(
        &implementation_code,
        &[(factory, FORWARDER_FACTORY_OFFSETS)],
        FORWARDER_RUNTIME_TEMPLATE_HASH,
    )
    .map_err(|error| {
        format!("RPC member identity invalid: implementation {implementation:#x} {error} of the Forwarder build")
    })?;
    let sample = client
        .factory_addresses(factory, sample_treasury(), &[sample_salt()])
        .await
        .map_err(|e| read("Multicall3 factory addressOf capability", e))?;
    let expected = forwarder_address(factory, implementation, sample_treasury(), sample_salt());
    if sample.as_slice() != [expected] {
        return Err(format!(
            "RPC member identity invalid: factory addressOf(treasury, sample) is {sample:?}, local derivation gives \
             {expected:#x}"
        ));
    }
    Ok(())
}

/// Result of a fresh independent check at one canonical finalized block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractCheck {
    /// Both endpoints report exactly the reviewed bytecode and immutable addresses.
    Pass,
    /// A complete agreed observation differs from the reviewed deployment.
    Mismatch,
    /// Endpoints disagree; no permanent conclusion is justified.
    Disagreement,
}
/// Derive each endpoint's code independently; failed requests remain errors.
pub async fn check_pair(
    read: &EvmClient,
    verify: &EvmClient,
    chain: u64,
    routes: &[RouteFile],
) -> Result<ContractCheck, ChainError> {
    use alloy::eips::{BlockId, BlockNumberOrTag};
    let (a, b) = tokio::try_join!(read.network_id(), verify.network_id())?;
    if a != chain || b != chain {
        return Ok(ContractCheck::Disagreement);
    }
    let (a, b) = tokio::try_join!(
        read.price_block(BlockNumberOrTag::Finalized),
        verify.price_block(BlockNumberOrTag::Finalized)
    )?;
    if a.0 > b.0 {
        return Ok(ContractCheck::Disagreement);
    }
    let target = verify.price_block(BlockNumberOrTag::Number(a.0)).await?;
    if a != target {
        return Ok(ContractCheck::Disagreement);
    }
    let mut addresses = BTreeSet::from([MULTICALL3]);
    for route in routes.iter().filter(|r| r.chain.chain_id == chain) {
        addresses.extend([
            route.chain.contracts.forwarder_factory,
            route.chain.contracts.implementation,
        ]);
    }
    let pin = BlockId::hash_canonical(a.1);
    let observe = async |client: &EvmClient| {
        let mut codes = std::collections::BTreeMap::new();
        for address in &addresses {
            codes.insert(*address, client.code_at_id(*address, pin).await?);
        }
        Ok::<_, ChainError>(codes)
    };
    let (a, b) = tokio::try_join!(observe(read), observe(verify))?;
    if a != b {
        return Ok(ContractCheck::Disagreement);
    }
    let reviewed = a
        .get(&MULTICALL3)
        .is_some_and(|code| keccak256(code) == MULTICALL3_RUNTIME_CODE_HASH)
        && routes
            .iter()
            .filter(|r| r.chain.chain_id == chain)
            .all(|route| {
                let c = &route.chain.contracts;
                a.get(&c.forwarder_factory).is_some_and(|code| {
                    verify_code(
                        code,
                        &[(c.implementation, FACTORY_IMPLEMENTATION_OFFSETS)],
                        FACTORY_RUNTIME_TEMPLATE_HASH,
                    )
                    .is_ok()
                }) && a.get(&c.implementation).is_some_and(|code| {
                    verify_code(
                        code,
                        &[(c.forwarder_factory, FORWARDER_FACTORY_OFFSETS)],
                        FORWARDER_RUNTIME_TEMPLATE_HASH,
                    )
                    .is_ok()
                })
            });
    Ok(if reviewed {
        ContractCheck::Pass
    } else {
        ContractCheck::Mismatch
    })
}
/// Update readiness and freeze only on a complete agreed mismatch, without stopping other chains.
pub async fn check_and_enforce(
    pool: &sqlx::PgPool,
    read: &EvmClient,
    verify: &EvmClient,
    chain: u64,
    routes: &[RouteFile],
) -> Result<bool, sqlx::Error> {
    let result = check_pair(read, verify, chain, routes).await;
    let passed = matches!(result, Ok(ContractCheck::Pass));
    read.contract_checked(passed);
    verify.contract_checked(passed);
    match result {
        Ok(ContractCheck::Pass) => {}
        Ok(ContractCheck::Mismatch) => {
            crate::db::chain_reads::freeze(pool, chain, "contract_code_mismatch").await?;
            tracing::error!(
                tags.alert = "TopupContractCodeMismatch",
                chain_id = chain,
                "both endpoints agree deployed code differs from the reviewed code; chain frozen"
            );
        }
        Ok(ContractCheck::Disagreement) => tracing::warn!(
            tags.alert = "TopupRpcDisagreement",
            chain_id = chain,
            "contract evidence disagreed; chain not-ready"
        ),
        Err(error) => {
            tracing::warn!(tags.alert="TopupRpcEndpointUnavailable",chain_id=chain,%error,"dual contract check waits; chain not-ready")
        }
    }
    Ok(passed)
}

/// Checks runtime code against a build whose immutable words are zeroed.
///
/// Each immutable word must hold the route's expected address at exactly the recorded offsets;
/// after zeroing those words, the code must hash to the recorded template hash.
fn verify_code(
    code: &[u8],
    immutables: &[(Address, &[usize])],
    template_hash: B256,
) -> Result<(), &'static str> {
    let mut template = code.to_vec();
    for (expected, offsets) in immutables {
        let word = expected.into_word();
        for offset in *offsets {
            let slot = offset
                .checked_add(32)
                .and_then(|end| template.get_mut(*offset..end))
                .ok_or("runtime code is shorter than the immutable references")?;
            if slot != word.as_slice() {
                return Err("does not hold the route's addresses in the immutable words");
            }
            slot.fill(0);
        }
    }
    if keccak256(&template) != template_hash {
        return Err("runtime code differs from the recorded code hash");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicall3_code_hash_matches_the_recorded_deployment() {
        let recorded: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/contracts/multicall3.json"))
                .expect("recorded Multicall3 parses");
        assert_eq!(recorded["address"], MULTICALL3.to_checksum(None));
        assert_eq!(
            recorded["runtime_code_hash"],
            MULTICALL3_RUNTIME_CODE_HASH.to_string()
        );
        let code = recorded["runtime_code"]
            .as_str()
            .and_then(|code| code.parse::<alloy_primitives::Bytes>().ok())
            .expect("recorded runtime code is hex");
        assert_eq!(keccak256(&code), MULTICALL3_RUNTIME_CODE_HASH);
    }

    #[test]
    fn template_hashes_match_the_recorded_contract_build() {
        let recorded: serde_json::Value = serde_json::from_str(include_str!(
            "../../../deploy/contracts/expected-codehashes.json"
        ))
        .expect("recorded code hashes parse");
        let artifacts = &recorded["artifacts"];
        assert_eq!(
            artifacts["ForwarderFactory"]["runtime_template_code_hash"],
            FACTORY_RUNTIME_TEMPLATE_HASH.to_string()
        );
        assert_eq!(
            artifacts["Forwarder"]["runtime_template_code_hash"],
            FORWARDER_RUNTIME_TEMPLATE_HASH.to_string()
        );
    }

    #[test]
    fn immutable_offsets_match_the_recorded_contract_build() {
        let recorded: serde_json::Value = serde_json::from_str(include_str!(
            "../../../deploy/contracts/expected-codehashes.json"
        ))
        .expect("recorded code hashes parse");
        let offsets = |contract: &str, immutable: &str| -> Vec<usize> {
            serde_json::from_value(
                recorded["artifacts"][contract]["immutable_offsets"][immutable].clone(),
            )
            .expect("recorded offsets are a list")
        };
        assert_eq!(
            offsets("ForwarderFactory", "implementation"),
            FACTORY_IMPLEMENTATION_OFFSETS
        );
        assert_eq!(offsets("Forwarder", "factory"), FORWARDER_FACTORY_OFFSETS);
    }

    #[test]
    fn code_check_uses_exact_immutable_offsets() {
        let immutable = Address::repeat_byte(0xab);
        let mut template = vec![0x60; 70];
        template[2..34].fill(0);
        let hash = keccak256(&template);
        let mut code = template.clone();
        code[2..34].copy_from_slice(immutable.into_word().as_slice());
        let offsets: &[usize] = &[2];

        assert_eq!(verify_code(&code, &[(immutable, offsets)], hash), Ok(()));
        // The same address at another offset, a different address, or trailing code all fail.
        let mut moved = template.clone();
        moved[36..68].copy_from_slice(immutable.into_word().as_slice());
        assert!(verify_code(&moved, &[(immutable, offsets)], hash).is_err());
        assert!(verify_code(&code, &[(Address::repeat_byte(0xcd), offsets)], hash).is_err());
        let mut extended = code.clone();
        extended.push(0);
        assert!(verify_code(&extended, &[(immutable, offsets)], hash).is_err());
        assert!(verify_code(&code[..20], &[(immutable, offsets)], hash).is_err());
    }
}
