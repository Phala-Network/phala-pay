//! Hint endpoints and worker against independent typed RPC fixtures and isolated Postgres.
mod support;

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{
    TestDatabase,
    seed::{self, NewAccount, NewAddress, NewCustomer},
};
use tokio_util::sync::CancellationToken;
use topup::{api::AppState, chain_rpc::ChainRpc, hints::HintQueue, routes::RouteSet};
use topup_adapters::{attestation::DstackAttestor, chain::evm::EvmClient};
use topup_core::route::{Confirmations, RouteFile};
use tower::ServiceExt;
use uuid::Uuid;

const TX: &str = "0x4b6cf1a33019535930118d535e51966a0405d78874213f5e34e5df2eb223902f";
const TOKEN: &str = "0x1a6f260377e42ead1418c7c1afdfd5de371a9284";
const RECIPIENT: &str = "0xfa810b787da3f2ca13fc13082762e78c4104ab10";
const CHAIN: u64 = 84532;
#[derive(Clone)]
struct Rpc {
    receipt: Arc<Mutex<Value>>,
    transaction: Arc<Mutex<Value>>,
    block: Arc<Mutex<Value>>,
    head: Arc<Mutex<Value>>,
    calls: Arc<AtomicUsize>,
}
impl Rpc {
    fn new() -> Self {
        Self {
            receipt: Arc::new(Mutex::new(
                serde_json::from_str(include_str!(
                    "../../adapters/tests/fixtures/base-sepolia/receipt.json"
                ))
                .unwrap(),
            )),
            transaction: Arc::new(Mutex::new(
                serde_json::from_str(include_str!(
                    "../../adapters/tests/fixtures/base-sepolia/transaction.json"
                ))
                .unwrap(),
            )),
            block: Arc::new(Mutex::new(
                serde_json::from_str(include_str!(
                    "../../adapters/tests/fixtures/base-sepolia/block-47445875.json"
                ))
                .unwrap(),
            )),
            head: Arc::new(Mutex::new(json!("0x2d3f77a"))),
            calls: Arc::default(),
        }
    }
}
async fn rpc(State(state): State<Rpc>, Json(request): Json<Value>) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let result = match request["method"].as_str().unwrap() {
        "eth_getTransactionReceipt" => {
            if request["params"][0] == TX {
                state.receipt.lock().unwrap().clone()
            } else {
                Value::Null
            }
        }
        "eth_getTransactionByHash" => state.transaction.lock().unwrap().clone(),
        "eth_getBlockByHash" => state.block.lock().unwrap().clone(),
        "eth_blockNumber" => state.head.lock().unwrap().clone(),
        method => panic!("unexpected RPC: {method}"),
    };
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
}
struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn node(state: Rpc, label: &str) -> Result<(Arc<EvmClient>, Task)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = EvmClient::new(&format!("http://{}", listener.local_addr()?))?
        .with_provider(label)
        .with_chain_id(CHAIN);
    // Readiness is normally established by startup self-tests, not the hint worker.
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", post(rpc)).with_state(state),
        )
        .await
        .unwrap();
    });
    client.latest_head().await?;
    Ok((Arc::new(client), Task(server)))
}
struct Harness {
    app: Router,
    queue: Arc<HintQueue>,
    routes: Arc<RouteSet>,
    read: Arc<EvmClient>,
    verify: Arc<EvmClient>,
    key: String,
    object: Uuid,
    quote_id: String,
    secret: String,
    _nodes: Vec<Task>,
}
async fn harness(pool: &sqlx::PgPool, read_rpc: Rpc, verify_rpc: Rpc) -> Result<Harness> {
    let (read, read_task) = node(read_rpc, "tx-hint-read").await?;
    let (verify, verify_task) = node(verify_rpc, "tx-hint-verify").await?;
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.chain_id = CHAIN;
    route.chain.name = "base-sepolia".into();
    route.chain.confirmations = Confirmations::Depth(2);
    route.asset.contract = TOKEN.parse()?;
    let routes = Arc::new(
        RouteSet::with_rpc(
            vec![route.clone()],
            BTreeMap::from([(
                CHAIN,
                ChainRpc {
                    read: read.clone(),
                    verify: verify.clone(),
                },
            )]),
        )
        .map_err(anyhow::Error::msg)?,
    );
    let account = seed::create_account(pool, &NewAccount::named("hint merchant")).await?;
    let customer = seed::create_customer(
        pool,
        &NewCustomer {
            id: Uuid::new_v4(),
            account_id: account.id,
            livemode: true,
            client_reference_id: "hint customer".into(),
        },
    )
    .await?;
    seed::accept_routes(pool, account.id, true, &[&route]).await?;
    seed::initialize_dual_chain(pool, CHAIN).await?;
    let key = seed::create_api_key(pool, account.id, true).await?;
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: CHAIN,
            route: route.route.clone(),
            salt: B256::repeat_byte(3),
            address: RECIPIENT.parse()?,
        },
    )
    .await?;
    let object = address.quote_id.context("fixture quote")?;
    let quote_id = topup::ids::format(topup::ids::QUOTE, object);
    let client_reads = Arc::new(topup::api::ClientReadLimiter::default());
    let secret = client_reads
        .key()
        .issue(&account.public_id, &quote_id)
        .map_err(anyhow::Error::msg)?;
    sqlx::query("UPDATE quotes SET client_secret_hash=$2 WHERE id=$1")
        .bind(object)
        .bind(Sha256::digest(secret.as_bytes()).as_slice())
        .execute(pool)
        .await?;
    let queue = Arc::new(HintQueue::default());
    let state = AppState {
        pool: pool.clone(),
        routes: routes.clone(),
        admin_key: topup::api::VerificationKey::from_base64(
            "hint/admin".into(),
            &support::public_key_base64(&ed25519_dalek::SigningKey::from_bytes(&[71; 32])),
        )
        .map_err(anyhow::Error::msg)?,
        maintenance_keys: vec![],
        public_origin: topup::api::PublicOrigin::parse(support::TEST_ORIGIN)?,
        attestor: Arc::new(DstackAttestor::new()),
        rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
        client_reads,
        rate_limits: Arc::default(),
        hint_limits: Arc::default(),
        transaction_hints: queue.clone(),
        screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
        contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
    };
    Ok(Harness {
        app: topup::api::router(state).0,
        queue,
        routes,
        read,
        verify,
        key,
        object,
        quote_id,
        secret,
        _nodes: vec![read_task, verify_task],
    })
}
impl Harness {
    async fn submit(&self, path: &str, key: Option<&str>, body: Value) -> Result<Value> {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(key) = key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        let mut request = request.body(Body::from(serde_json::to_vec(&body)?))?;
        request
            .extensions_mut()
            .insert(ConnectInfo("192.0.2.1:1234".parse::<SocketAddr>()?));
        let response = self.app.clone().oneshot(request).await?;
        ensure!(response.status() == StatusCode::ACCEPTED);
        ensure!(response.headers()["access-control-allow-origin"] == "*");
        let value: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(
            value
                == json!({"object":"transaction_submission","transaction_hash":body["transaction_hash"],"status":"received"})
        );
        Ok(value)
    }
    fn worker(&self, pool: &sqlx::PgPool) -> (CancellationToken, Task) {
        let cancel = CancellationToken::new();
        (
            cancel.clone(),
            Task(tokio::spawn(self.queue.clone().run(
                pool.clone(),
                self.routes.clone(),
                cancel,
            ))),
        )
    }
}
async fn wait_until(mut condition: impl AsyncFnMut() -> Result<bool>) -> Result<()> {
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition().await? {
        ensure!(
            tokio::time::Instant::now() < end,
            "hint condition timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}
async fn deposits(pool: &sqlx::PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM deposits")
        .fetch_one(pool)
        .await?)
}

#[tokio::test]
async fn browser_and_server_auth_are_object_scoped_and_every_response_is_quiet() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        let path = format!("/v1/quotes/{}/transactions", h.quote_id);
        for auth in [None, Some("invalid")] {
            h.submit(&path, auth, json!({"transaction_hash":TX}))
                .await?;
        }
        h.submit(
            &format!("{path}?client_secret=forged"),
            None,
            json!({"transaction_hash":TX}),
        )
        .await?;
        ensure!(h.queue.pending() == 0);
        h.submit(
            &format!("{path}?client_secret={}", h.secret),
            None,
            json!({"transaction_hash":TX}),
        )
        .await?;
        ensure!(h.queue.pending() == 1);
        h.submit(&path, Some(&h.key), json!({"transaction_hash":TX}))
            .await?;
        ensure!(h.queue.pending() == 1);
        h.submit(
            &path,
            Some(&h.key),
            json!({"transaction_hash":"0xforged", "recipient":RECIPIENT}),
        )
        .await?;
        ensure!(h.queue.pending() == 1);
        ensure!(deposits(&db.app_pool).await? == 0);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn hints_record_only_dual_verified_confirmed_positive_facts_and_scanner_duplicates_are_safe()
-> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        h.submit(&format!("/v1/quotes/{}/transactions", h.quote_id), Some(&h.key), json!({"transaction_hash":TX})).await?;
        let (cancel, _worker) = h.worker(&db.app_pool);
        wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?;
        cancel.cancel();
        ensure!(sqlx::query_scalar::<_, bool>("SELECT state='detected' AND final_at IS NULL AND dual_verified_at IS NOT NULL AND receipt_log_index=0 AND log_index=35 FROM deposits").fetch_one(&db.app_pool).await?);
        ensure!(sqlx::query_scalar::<_, i64>("SELECT used FROM daily_budgets WHERE name='hints'").fetch_one(&db.app_pool).await? == 1);
        let deposit: topup::db::Deposit = topup::db::get_deposit(&db.app_pool, sqlx::query_scalar("SELECT id FROM deposits").fetch_one(&db.app_pool).await?).await?.context("hint deposit")?;
        // Normal scanner insertion at the same receipt position cannot create a second deposit.
        let mut tx = db.app_pool.begin().await?;
        let duplicate = topup::db::NewDeposit { chain_id: CHAIN, tx_hash: deposit.tx_hash, receipt_log_index: 0, log_index: deposit.log_index,
            block_number: deposit.block_number, block_hash: deposit.block_hash, block_time: deposit.block_time,
            address_id: deposit.address_id, route: deposit.route, route_version: deposit.route_version,
            asset_contract: deposit.asset_contract, from_address: deposit.from_address, amount_atomic: deposit.amount_atomic,
            state: topup_core::deposit::DepositState::Detected, reason: None, next_attempt_at: chrono::Utc::now(),
            tx_from: deposit.tx_from.context("sender")?, tx_nonce: deposit.tx_nonce.context("nonce")?, is_final: false };
        ensure!(topup::db::insert_scanned_deposit_in(&mut tx, &duplicate, topup::db::Evidence::Confirmed).await?.is_none());
        tx.commit().await?;
        ensure!(deposits(&db.app_pool).await? == 1);
        Ok(())
    }.await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn unrelated_reverted_unsupported_and_disagreed_receipts_never_record_or_change_coverage()
-> Result<()> {
    for case in [
        "other-object",
        "reverted",
        "unsupported",
        "wrong-chain",
        "forged-hash",
        "verify-amount",
        "verify-time",
        "verify-nonce",
    ] {
        let Some(db) = TestDatabase::create().await? else {
            return Ok(());
        };
        let result = async {
            let read = Rpc::new(); let verify = Rpc::new();
            match case {
                "other-object" => read.receipt.lock().unwrap()["logs"][0]["topics"][2] = json!(format!("0x{}", "99".repeat(32))),
                "reverted" => read.receipt.lock().unwrap()["status"] = json!("0x0"),
                "unsupported" => read.receipt.lock().unwrap()["logs"][0]["address"] = json!(format!("0x{}", "99".repeat(20))),
                "wrong-chain" => read.transaction.lock().unwrap()["chainId"] = json!("0x1"),
                "forged-hash" => read.receipt.lock().unwrap()["transactionHash"] = json!(format!("0x{}", "99".repeat(32))),
                "verify-amount" => verify.receipt.lock().unwrap()["logs"][0]["data"] = json!(format!("0x{:064x}", 1)),
                "verify-time" => verify.block.lock().unwrap()["timestamp"] = json!("0x6abb4dc7"),
                "verify-nonce" => verify.transaction.lock().unwrap()["nonce"] = json!("0xffff"),
                _ => unreachable!(),
            }
            let h = harness(&db.app_pool, read.clone(), verify.clone()).await?;
            h.submit(&format!("/v1/quotes/{}/transactions", h.quote_id), Some(&h.key), json!({"transaction_hash":TX})).await?;
            let (cancel, _worker) = h.worker(&db.app_pool);
            wait_until(async || Ok(sqlx::query_scalar::<_, i32>("SELECT used FROM daily_budgets WHERE name='hints'").fetch_optional(&db.app_pool).await? == Some(1))).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
            ensure!(deposits(&db.app_pool).await? == 0, "case {case}");
            ensure!(sqlx::query_scalar::<_, bool>("SELECT through_block=0 FROM chain_coverage").fetch_one(&db.app_pool).await?);
            ensure!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events WHERE type IN ('quote.canceled','quote.expired','deposit.rejected')").fetch_one(&db.app_pool).await? == 0);
            Ok(())
        }.await;
        db.cleanup().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn not_ready_parks_without_spending_daily_budget_then_resumes() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        h.verify.mark_not_ready();
        h.submit(
            &format!("/v1/quotes/{}/transactions", h.quote_id),
            Some(&h.key),
            json!({"transaction_hash":TX}),
        )
        .await?;
        let (cancel, _worker) = h.worker(&db.app_pool);
        tokio::time::sleep(Duration::from_millis(100)).await;
        ensure!(h.queue.pending() == 1);
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM daily_budgets WHERE name='hints'")
                .fetch_one(&db.app_pool)
                .await?
                == 0
        );
        ensure!(deposits(&db.app_pool).await? == 0);
        h.verify.latest_head().await?;
        wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?;
        cancel.cancel();
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn whole_task_caps_include_unmined_receipt_polls_and_confirmation_head_polls() -> Result<()> {
    for unmined in [true, false] {
        let Some(db) = TestDatabase::create().await? else {
            return Ok(());
        };
        let result = async {
            let read = Rpc::new();
            let verify = Rpc::new();
            if unmined {
                *read.receipt.lock().unwrap() = Value::Null;
            } else {
                *read.head.lock().unwrap() = json!("0x2d3f773");
                *verify.head.lock().unwrap() = json!("0x2d3f773");
            }
            let h = harness(&db.app_pool, read.clone(), verify.clone()).await?;
            let read_start = read.calls.load(Ordering::SeqCst);
            let verify_start = verify.calls.load(Ordering::SeqCst);
            h.submit(
                &format!("/v1/quotes/{}/transactions", h.quote_id),
                Some(&h.key),
                json!({"transaction_hash":TX}),
            )
            .await?;
            let (cancel, _worker) = h.worker(&db.app_pool);
            let limit = tokio::time::Instant::now() + Duration::from_secs(20);
            while if unmined {
                read.calls.load(Ordering::SeqCst) < read_start + 12
            } else {
                verify.calls.load(Ordering::SeqCst) < verify_start + 8
            } {
                ensure!(
                    tokio::time::Instant::now() < limit,
                    "task did not consume bounded budget"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            ensure!(read.calls.load(Ordering::SeqCst) <= read_start + 12);
            ensure!(verify.calls.load(Ordering::SeqCst) <= verify_start + 8);
            ensure!(deposits(&db.app_pool).await? == 0);
            cancel.cancel();
            Ok(())
        }
        .await;
        db.cleanup().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn daily_budget_is_atomic_hard_counter_and_151st_task_per_utc_day_is_ignored() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
        let mut admitted = 0;
        for _ in 0..16 {
            let mut claims = tokio::task::JoinSet::new();
            for _ in 0..10 {
                let pool = db.app_pool.clone();
                claims.spawn(async move {
                    topup::db::daily_budgets::claim_on(&pool, day, "hints-concurrent-test", 150)
                        .await
                });
            }
            while let Some(claim) = claims.join_next().await {
                admitted += i32::from(claim??);
            }
        }
        ensure!(admitted == 150);
        ensure!(
            topup::db::daily_budgets::claim_on(
                &db.app_pool,
                day.succ_opt().unwrap(),
                "hints-concurrent-test",
                150
            )
            .await?
        );
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        sqlx::query("INSERT INTO daily_budgets(day,name,used) VALUES($1,'hints',150)")
            .bind(chrono::Utc::now().date_naive())
            .execute(&db.app_pool)
            .await?;
        h.submit(
            &format!("/v1/quotes/{}/transactions", h.quote_id),
            Some(&h.key),
            json!({"transaction_hash":TX}),
        )
        .await?;
        let (cancel, _worker) = h.worker(&db.app_pool);
        tokio::time::sleep(Duration::from_millis(100)).await;
        ensure!(deposits(&db.app_pool).await? == 0);
        ensure!(
            sqlx::query_scalar::<_, i32>("SELECT used FROM daily_budgets WHERE name='hints'")
                .fetch_one(&db.app_pool)
                .await?
                == 150
        );
        cancel.cancel();
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}
