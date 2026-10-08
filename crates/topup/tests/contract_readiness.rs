//! A complete dual contract mismatch freezes only its chain; lifting requires a fresh check.
mod support;
use alloy_primitives::Address;
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    http::{Method, StatusCode},
    routing::post,
};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use topup::{
    api::{AppState, PublicOrigin, VerificationKey},
    chain_rpc::ChainRpc,
    contracts, db,
    routes::RouteSet,
};
use topup_adapters::{
    attestation::DstackAttestor,
    chain::evm::{EvmClient, MULTICALL3},
};
use topup_core::route::RouteFile;
use tower::ServiceExt;

struct Node {
    client: Arc<EvmClient>,
    mode: Arc<AtomicU8>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Node {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn node(route: &RouteFile) -> Result<Node> {
    let templates: Value =
        serde_json::from_str(include_str!("fixtures/rpc-probe-contract-code.json"))?;
    let multicall: Value =
        serde_json::from_str(include_str!("../../../deploy/contracts/multicall3.json"))?;
    let c = &route.chain.contracts;
    let mut codes = BTreeMap::new();
    for (address, name, immutable, offsets) in [
        (
            c.forwarder_factory,
            "ForwarderFactory",
            c.implementation,
            vec![105, 480, 606, 887],
        ),
        (
            c.implementation,
            "Forwarder",
            c.forwarder_factory,
            vec![208, 319],
        ),
    ] {
        let mut code = hex::decode(templates[name].as_str().unwrap().trim_start_matches("0x"))?;
        for offset in offsets {
            code[offset..offset + 32].copy_from_slice(immutable.into_word().as_slice());
        }
        codes.insert(address, format!("0x{}", hex::encode(code)));
    }
    codes.insert(
        MULTICALL3,
        multicall["runtime_code"].as_str().unwrap().to_owned(),
    );
    let factory = c.forwarder_factory;
    let mode = Arc::new(AtomicU8::new(0));
    let observed = mode.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client =
        Arc::new(EvmClient::new(&format!("http://{}", listener.local_addr()?))?.with_chain_id(1));
    client.contract_checked(false);
    let app=Router::new().route("/",post(move |Json(request):Json<Value>| {
        let mode=observed.load(Ordering::SeqCst);
        let result=match request["method"].as_str().unwrap() {
            "eth_chainId"=>json!("0x1"),
            "eth_getBlockByNumber"=>{
                let mut block=serde_json::to_value(alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default()).unwrap();
                block["number"]=json!("0x64");block["timestamp"]=json!("0x64");block["hash"]=json!(format!("0x{}","11".repeat(32)));block
            },
            "eth_getCode"=>{
                assert_eq!(request["params"][1],json!({"blockHash":format!("0x{}","11".repeat(32)),"requireCanonical":true}));
                let address:Address=serde_json::from_value(request["params"][0].clone()).unwrap();
                if (mode==1 && address==factory)||(mode==2 && address==MULTICALL3) {json!("0x00")} else {json!(codes[&address])}
            },
            method=>panic!("unexpected RPC {method}"),
        };
        async move {Json(if mode==3 {json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"unavailable"}})} else {json!({"jsonrpc":"2.0","id":request["id"],"result":result})})}
    }));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok(Node { client, mode, task })
}
fn route() -> RouteFile {
    serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml")).unwrap()
}

#[tokio::test]
async fn agreement_freezes_and_disagreement_or_unavailability_only_blocks_readiness() -> Result<()>
{
    support::with_database(|d| {
        Box::pin(async move {
            let route = route();
            let a = node(&route).await?;
            let b = node(&route).await?;
            a.mode.store(1, Ordering::SeqCst);
            ensure!(
                !contracts::check_and_enforce(
                    &d.app_pool,
                    &a.client,
                    &b.client,
                    1,
                    std::slice::from_ref(&route)
                )
                .await?
            );
            ensure!(!topup::reconciler::chain_is_blocked(&d.app_pool, 1).await?);
            ensure!(!a.client.ready() && !b.client.ready());
            b.mode.store(3, Ordering::SeqCst);
            ensure!(
                !contracts::check_and_enforce(
                    &d.app_pool,
                    &a.client,
                    &b.client,
                    1,
                    std::slice::from_ref(&route)
                )
                .await?
            );
            ensure!(!topup::reconciler::chain_is_blocked(&d.app_pool, 1).await?);
            b.mode.store(1, Ordering::SeqCst);
            ensure!(
                !contracts::check_and_enforce(
                    &d.app_pool,
                    &a.client,
                    &b.client,
                    1,
                    std::slice::from_ref(&route)
                )
                .await?
            );
            let row: (String, String) = sqlx::query_as(
                "SELECT scope,check_name FROM reconciliation_blocks WHERE chain_id=1",
            )
            .fetch_one(&d.app_pool)
            .await?;
            ensure!(row == ("chain".into(), "contract_code_mismatch".into()));
            ensure!(topup::reconciler::chain_is_blocked(&d.app_pool, 1).await?);
            ensure!(!topup::reconciler::chain_is_blocked(&d.app_pool, 8453).await?);
            let mut tx = d.app_pool.begin().await?;
            ensure!(db::chain_reads::admit_address(&mut tx, 1).await.is_err());
            tx.rollback().await?;
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn audited_lift_refuses_before_a_passing_fresh_dual_check() -> Result<()> {
    support::with_database(|d| {
        Box::pin(async move {
            let route = route();
            let a = node(&route).await?;
            let b = node(&route).await?;
            a.mode.store(2, Ordering::SeqCst);
            b.mode.store(2, Ordering::SeqCst);
            contracts::check_and_enforce(
                &d.app_pool,
                &a.client,
                &b.client,
                1,
                std::slice::from_ref(&route),
            )
            .await?;
            let routes = Arc::new(
                RouteSet::with_rpc(
                    vec![route],
                    BTreeMap::from([(
                        1,
                        ChainRpc {
                            read: a.client.clone(),
                            verify: b.client.clone(),
                        },
                    )]),
                )
                .map_err(anyhow::Error::msg)?,
            );
            let key = SigningKey::from_bytes(&[71; 32]);
            let (app, _) = topup::api::router(AppState {
                pool: d.app_pool.clone(),
                max_attached_pending_refunds: std::num::NonZeroU32::new(2)
                    .expect("positive refund limit"),
                routes: routes.clone(),
                admin_key: VerificationKey::from_base64(
                    "admin/test".into(),
                    &support::public_key_base64(&key),
                )
                .unwrap(),
                maintenance_keys: Vec::new(),
                public_origin: PublicOrigin::parse(support::TEST_ORIGIN).unwrap(),
                attestor: Arc::new(DstackAttestor::new()),
                rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
                client_reads: Arc::default(),
                rate_limits: Arc::default(),
                hint_limits: Arc::default(),
                transaction_hints: Arc::default(),
                screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
                sanctions_rescreen: Arc::default(),
                contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
            });
            let path = "/v1/admin/reconciliation_blocks/chain:1/lift";
            let request = |reason: &str| {
                support::signed_request(
                    Method::POST,
                    path,
                    serde_json::to_vec(&json!({"reason":reason})).unwrap(),
                    "admin/test",
                    &key,
                    Utc::now().timestamp(),
                )
            };
            ensure!(
                app.clone()
                    .oneshot(request("recheck still mismatched"))
                    .await?
                    .status()
                    == StatusCode::BAD_REQUEST
            );
            ensure!(topup::reconciler::chain_is_blocked(&d.app_pool, 1).await?);
            ensure!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM audit WHERE action='reconciliation_block.lift'"
                )
                .fetch_one(&d.app_pool)
                .await?
                    == 0
            );
            a.mode.store(0, Ordering::SeqCst);
            b.mode.store(0, Ordering::SeqCst);
            ensure!(
                app.oneshot(request("reviewed deployment repaired"))
                    .await?
                    .status()
                    == StatusCode::OK
            );
            ensure!(!topup::reconciler::chain_is_blocked(&d.app_pool, 1).await?);
            ensure!(routes.chain_ready(1));
            ensure!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM audit WHERE action='reconciliation_block.lift'"
                )
                .fetch_one(&d.app_pool)
                .await?
                    == 1
            );
            Ok(())
        })
    })
    .await
}
