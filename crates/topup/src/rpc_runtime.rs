//! Typed startup/deploy endpoint checks and bounded transport-readiness recovery.
use crate::{db, routes::RouteSet};
use alloy::sol_types::SolEvent;
use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    rpc::types::Filter,
};
use alloy_primitives::{Address, Bytes, U256};
use sqlx::PgPool;
use topup_adapters::chain::evm::{EvmClient, MULTICALL3, Transfer};

/// Both endpoints of every route and price chain pass the real production request shapes.
pub async fn preflight(routes: &RouteSet) -> Result<Vec<String>, String> {
    let mut checked = Vec::new();
    let mut errors = Vec::new();
    for (chain, pair) in routes.rpc() {
        let mut identity_ok = true;
        for client in [&pair.read, &pair.verify] {
            let id = client.endpoint().provider().unwrap_or_default();
            match client.network_id().await {
                Ok(number) if number == *chain => {}
                Ok(_) => {
                    client.mark_not_ready();
                    identity_ok = false;
                    errors.push(format!("{id}: chain id: endpoint identity mismatch"));
                }
                Err(error) => {
                    client.mark_not_ready();
                    identity_ok = false;
                    errors.push(format!("{id}: {error}"));
                }
            }
        }
        if !identity_ok {
            continue;
        }
        let boundary = async {
            let (a, b) = tokio::try_join!(
                pair.read.price_block(BlockNumberOrTag::Finalized),
                pair.verify.price_block(BlockNumberOrTag::Finalized)
            )
            .map_err(|e| e.to_string())?;
            if a.0 > b.0 {
                return Err(format!("chain {chain}: verify finalized is behind read"));
            }
            let verify = pair
                .verify
                .price_block(BlockNumberOrTag::Number(a.0))
                .await
                .map_err(|e| e.to_string())?;
            if a != verify {
                return Err(format!("chain {chain}: finalized checkpoint disagreed"));
            }
            Ok(a)
        }
        .await;
        let contract_check =
            crate::contracts::check_pair(&pair.read, &pair.verify, *chain, routes.routes()).await;
        let passed = matches!(contract_check, Ok(crate::contracts::ContractCheck::Pass));
        pair.read.contract_checked(passed);
        pair.verify.contract_checked(passed);
        if !passed {
            errors.push(format!(
                "chain {chain}: dual contract identity check failed"
            ));
            continue;
        }
        let a = match boundary {
            Ok(a) => a,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        for client in [&pair.read, &pair.verify] {
            match check_endpoint(client, *chain, routes, a.0, a.1).await {
                Ok(()) => checked.push(client.endpoint().provider().unwrap_or_default().to_owned()),
                Err(error) => {
                    client.mark_not_ready();
                    errors.push(error);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(checked)
    } else {
        Err(errors.join("\n"))
    }
}

async fn check_endpoint(
    client: &EvmClient,
    chain: u64,
    routes: &RouteSet,
    checkpoint: u64,
    hash: alloy_primitives::B256,
) -> Result<(), String> {
    let latest = client
        .price_block(BlockNumberOrTag::Latest)
        .await
        .map_err(|e| e.to_string())?;
    client
        .check_receipt_logs(checkpoint)
        .await
        .map_err(|e| e.to_string())?;
    let recipients: Vec<_> = (1..=1000_u64)
        .map(|i| alloy_primitives::B256::from(U256::from(i)))
        .collect();
    let mut signatures = topup_adapters::chain::flush::factory_event_signatures().to_vec();
    signatures.push(Transfer::SIGNATURE_HASH);
    client
        .logs(
            &Filter::new()
                .from_block(
                    checkpoint.saturating_sub(u64::from(client.max_log_blocks()).saturating_sub(1)),
                )
                .to_block(checkpoint)
                .event_signature(signatures)
                .topic2(recipients),
        )
        .await
        .map_err(|e| e.to_string())?;
    let historical = latest
        .0
        .saturating_sub(latest.0.saturating_sub(checkpoint).saturating_mul(2));
    let historical = client
        .price_block(BlockNumberOrTag::Number(historical))
        .await
        .map_err(|e| e.to_string())?;
    for pin in [hash, historical.1] {
        let pin = Some(BlockId::hash_canonical(pin));
        let code = client
            .code_at_id(MULTICALL3, pin.ok_or("missing canonical pin")?)
            .await
            .map_err(|e| e.to_string())?;
        if code.is_empty() {
            return Err(format!("chain {chain}: canonical Multicall3 code missing"));
        }
        for route in routes.routes().iter().filter(|r| r.chain.chain_id == chain) {
            let decimals = client
                .call(
                    "self-test token decimals",
                    route.asset.contract,
                    Bytes::from_static(&[0x31, 0x3c, 0xe5, 0x67]),
                    pin,
                )
                .await
                .map_err(|e| e.to_string())?;
            if decimals.len() != 32
                || U256::from_be_slice(&decimals) != U256::from(route.asset.decimals)
            {
                return Err(format!("chain {chain}: token decimals mismatch"));
            }
            let mut oracle = alloy_primitives::keccak256("isSanctioned(address)")
                .as_slice()
                .get(..4)
                .ok_or("oracle selector missing")?
                .to_vec();
            oracle.extend_from_slice(&[0; 32]);
            let result = client
                .call(
                    "self-test sanctions",
                    route.screening.sanctions_oracle,
                    oracle.into(),
                    pin,
                )
                .await
                .map_err(|e| e.to_string())?;
            if result.len() != 32 || U256::from_be_slice(&result) > U256::from(1) {
                return Err(format!("chain {chain}: sanctions response malformed"));
            }
            if client
                .code_at_id(
                    route.chain.contracts.forwarder_factory,
                    pin.ok_or("missing canonical pin")?,
                )
                .await
                .map_err(|e| e.to_string())?
                .is_empty()
            {
                return Err(format!("chain {chain}: factory code missing"));
            }
        }
        let mut feeds = std::collections::BTreeSet::new();
        for route in routes.routes() {
            for (_, sources) in route.pricing.roles() {
                for source in sources {
                    match source {
                        topup_core::price::Source::Chainlink { feed, chain_id, .. }
                            if *chain_id == chain =>
                        {
                            feeds.insert(feed.as_str());
                        }
                        topup_core::price::Source::UniswapV2Twap { .. } if chain == 1 => {
                            feeds.insert("ETH_USD");
                        }
                        _ => {}
                    }
                }
            }
            if chain == 8453 && route.pricing.sequencer_uptime.is_some() {
                feeds.insert("BASE_SEQUENCER_UPTIME");
            }
        }
        for name in feeds {
            let feed = topup_core::price::feed(name, chain).ok_or("unsupported price feed")?;
            let address: Address = feed.address.parse().map_err(|_| "invalid price address")?;
            let result = client
                .call(
                    "self-test feed decimals",
                    address,
                    Bytes::from_static(&[0x31, 0x3c, 0xe5, 0x67]),
                    pin,
                )
                .await
                .map_err(|e| e.to_string())?;
            if result.len() != 32 || U256::from_be_slice(&result) != U256::from(feed.decimals) {
                return Err(format!("chain {chain}: feed decimals mismatch"));
            }
        }
    }
    Ok(())
}
/// Establish durable checkpoints for restore/reconcile before any checkpoint-pinned check runs.
pub async fn ensure_checkpoints(pool: &PgPool, routes: &RouteSet) -> Result<(), String> {
    db::rpc::check_start(pool, &routes.chain_ids().collect::<Vec<_>>())
        .await
        .map_err(|e| e.to_string())?;
    for chain in routes.chain_ids() {
        if db::chain_reads::checkpoint(pool, chain)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
            && db::chain_reads::coverage(pool, chain)
                .await
                .map_err(|e| e.to_string())?
                .is_some()
        {
            continue;
        }
        let read = topup_adapters::chain::evm::FinalizedReader::new(
            routes
                .provider(chain, 0)
                .map_err(|e| e.to_string())?
                .clone(),
        );
        let verify = topup_adapters::chain::evm::FinalizedReader::new(
            routes
                .provider(chain, 1)
                .map_err(|e| e.to_string())?
                .clone(),
        );
        crate::scanner::initialize_chain(pool, chain, &read, &verify)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Recovery uses a cheap head read every thirty seconds only for endpoints that are not ready.
pub const RECOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// A transport success never bypasses a failed independent contract check or Infura's quota halt.
pub async fn probe_not_ready(pair: &crate::chain_rpc::ChainRpc) {
    let probe = async |client: &EvmClient| {
        if !client.ready() {
            if let Err(error) = client.latest_head().await {
                tracing::warn!(tags.alert="TopupRpcEndpointUnavailable", chain_id=?client.chain_id(),
                    provider=%client.endpoint().provider().unwrap_or_default(), %error,
                    "endpoint recovery head probe failed");
            }
        }
    };
    tokio::join!(probe(&pair.read), probe(&pair.verify));
}
/// One independently cancellable recovery cadence per configured RPC chain.
pub async fn recover(
    pair: crate::chain_rpc::ChainRpc,
    cancellation: tokio_util::sync::CancellationToken,
) {
    let mut interval = tokio::time::interval(RECOVERY_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _=cancellation.cancelled()=>return, _=interval.tick()=>{} }
        tokio::select! { _=cancellation.cancelled()=>return, _=probe_not_ready(&pair)=>{} }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn flapping_requests_stay_ready_and_failed_endpoint_recovers_on_a_head_probe() {
        let failed = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app=Router::new().route("/",post({let failed=failed.clone();let requests=requests.clone();
            move |Json(body):Json<Value>| {let failed=failed.clone();let requests=requests.clone(); async move {
                assert_eq!(body["method"],"eth_blockNumber"); requests.fetch_add(1,Ordering::SeqCst);
                Json(if failed.load(Ordering::SeqCst) {json!({"jsonrpc":"2.0","id":body["id"],"error":{"code":-32603,"message":"temporary failure"}})}
                    else {json!({"jsonrpc":"2.0","id":body["id"],"result":"0xa"})})
            }} }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let read = Arc::new(EvmClient::new(&url).unwrap());
        let verify = Arc::new(EvmClient::new(&url).unwrap());
        read.latest_head().await.unwrap();
        verify.latest_head().await.unwrap();
        let pair = crate::chain_rpc::ChainRpc { read, verify };
        for _ in 0..4 {
            failed.store(true, Ordering::SeqCst);
            assert!(pair.read.latest_head().await.is_err());
            assert!(
                pair.read.ready(),
                "a single final failure must not flap readiness"
            );
            failed.store(false, Ordering::SeqCst);
            pair.read.latest_head().await.unwrap();
        }
        failed.store(true, Ordering::SeqCst);
        for failures in 1..=3 {
            assert!(pair.read.latest_head().await.is_err());
            assert_eq!(pair.read.ready(), failures < 3);
        }
        probe_not_ready(&pair).await;
        assert!(!pair.read.ready());
        failed.store(false, Ordering::SeqCst);
        probe_not_ready(&pair).await;
        assert!(pair.read.ready());
        let count = requests.load(Ordering::SeqCst);
        probe_not_ready(&pair).await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            count,
            "healthy endpoints need no recovery traffic"
        );
        // A head probe cannot bypass the separate contract identity gate.
        pair.read.contract_checked(false);
        probe_not_ready(&pair).await;
        assert!(!pair.read.ready());
        server.abort();
        let _ = server.await;
    }
}
