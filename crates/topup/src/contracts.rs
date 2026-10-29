//! Startup verification of the deployed forwarder contracts against the attested routes (§4).

use std::collections::BTreeSet;

use alloy_primitives::{Address, B256, b256, keccak256};
use topup_core::route::RouteFile;

use topup_adapters::chain::evm::{ChainError, EvmClient, MULTICALL3};

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
    use alloy::eips::BlockId;
    let (a, b) = tokio::try_join!(read.network_id(), verify.network_id())?;
    if a != chain || b != chain {
        return Ok(ContractCheck::Disagreement);
    }
    let Some(a) = agreed_finalized_block(read, verify).await? else {
        return Ok(ContractCheck::Disagreement);
    };
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
/// Pin startup checks to the latest height finalized by both endpoints.
pub(crate) async fn agreed_finalized_block(
    read: &EvmClient,
    verify: &EvmClient,
) -> Result<Option<(u64, B256, u64)>, ChainError> {
    use alloy::eips::BlockNumberOrTag;
    let (a, b) = tokio::try_join!(
        read.price_block(BlockNumberOrTag::Finalized),
        verify.price_block(BlockNumberOrTag::Finalized)
    )?;
    let number = a.0.min(b.0);
    let target = BlockNumberOrTag::Number(number);
    let (a, b) = tokio::try_join!(read.price_block(target), verify.price_block(target))?;
    Ok((a.0 == number && a == b).then_some(a))
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
        // The business monitor owns the sustained readiness alert and its deduplication.
        Err(error) => {
            tracing::warn!(chain_id=chain,%error,"dual contract check waits; chain not-ready")
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

    #[tokio::test]
    async fn dual_contract_checks_pin_both_headers_to_the_lower_finalized_height() {
        use axum::{Json, Router, routing::post};
        use serde_json::{Value, json};
        use std::sync::{Arc, Mutex};

        let recorded: Value =
            serde_json::from_str(include_str!("../../../deploy/contracts/multicall3.json"))
                .unwrap();
        let common: Value = serde_json::from_str(include_str!(
            "../../adapters/tests/fixtures/base-sepolia/finalized-47446540.json"
        ))
        .unwrap();
        for (read_ahead, conflict) in [
            (true, "none"),
            (false, "none"),
            (true, "hash"),
            (true, "time"),
        ] {
            let mut servers = Vec::new();
            let mut clients = Vec::new();
            let requests = Arc::new(Mutex::new(Vec::new()));
            for is_read in [true, false] {
                let common = common.clone();
                let mut finalized = common.clone();
                if is_read == read_ahead {
                    finalized["number"] = json!("0x2d3fb9c");
                }
                let mut numbered = common.clone();
                if !is_read {
                    match conflict {
                        "hash" => numbered["hash"] = json!(B256::repeat_byte(1)),
                        "time" => numbered["timestamp"] = json!("0x1"),
                        _ => {}
                    }
                }
                let code = recorded["runtime_code"].clone();
                let captured = requests.clone();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let node = Router::new().route("/", post(move |Json(body): Json<Value>| {
                    let (finalized, numbered, code) = (finalized.clone(), numbered.clone(), code.clone());
                    let captured = captured.clone();
                    let common = common.clone();
                    async move {
                        captured.lock().unwrap().push((is_read, body.clone()));
                        let result = match body["method"].as_str().unwrap() {
                            "eth_chainId" => json!("0x14a34"),
                            "eth_getBlockByNumber" if body["params"][0] == "finalized" => finalized,
                            "eth_getBlockByNumber" => {
                                assert_eq!(body["params"][0], common["number"]);
                                numbered
                            }
                            "eth_getCode" => {
                                assert_eq!(body["params"][0], json!(MULTICALL3));
                                assert_eq!(body["params"][1], json!({"blockHash": common["hash"], "requireCanonical": true}));
                                code
                            }
                            other => panic!("unexpected method {other}")
                        };
                        Json(json!({"jsonrpc": "2.0", "id": body["id"], "result": result}))
                    }
                }));
                servers.push(tokio::spawn(async move {
                    axum::serve(listener, node).await.unwrap()
                }));
                clients.push(EvmClient::new(&url).unwrap());
            }
            let result = check_pair(&clients[0], &clients[1], 84532, &[]).await;
            for server in servers {
                server.abort();
                let _ = server.await;
            }
            assert_eq!(
                result.unwrap(),
                if conflict == "none" {
                    ContractCheck::Pass
                } else {
                    ContractCheck::Disagreement
                }
            );
            let requests = requests.lock().unwrap();
            for is_read in [true, false] {
                assert!(requests.iter().any(|(side, body)| *side == is_read
                    && body["method"] == "eth_getBlockByNumber"
                    && body["params"][0] == common["number"]));
            }
        }
    }

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
