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
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    receipt_gate: Arc<AtomicBool>,
    failure: Arc<AtomicBool>,
    methods: Arc<Mutex<Vec<String>>>,
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
            receipt_gate: Arc::default(),
            failure: Arc::default(),
            methods: Arc::default(),
        }
    }
}
async fn rpc(State(state): State<Rpc>, Json(request): Json<Value>) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state
        .methods
        .lock()
        .unwrap()
        .push(request["method"].as_str().unwrap().into());
    if state.failure.load(Ordering::SeqCst) {
        return Json(
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"fixture endpoint failure"}}),
        );
    }
    if request["method"] == "eth_getTransactionReceipt" {
        while state.receipt_gate.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
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
    read_only_app: Router,
    queue: Arc<HintQueue>,
    routes: Arc<RouteSet>,
    read: Arc<EvmClient>,
    verify: Arc<EvmClient>,
    key: String,
    quote_id: String,
    secret: String,
    read_label: String,
    _nodes: Vec<Task>,
}
async fn harness(pool: &sqlx::PgPool, read_rpc: Rpc, verify_rpc: Rpc) -> Result<Harness> {
    let read_label = format!("hint-read-{}", Uuid::new_v4());
    let verify_label = format!("hint-verify-{}", Uuid::new_v4());
    let (read, read_task) = node(read_rpc, &read_label).await?;
    let (verify, verify_task) = node(verify_rpc, &verify_label).await?;
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.chain_id = CHAIN;
    route.livemode = false;
    route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
        feed: "BASE_SEQUENCER_UPTIME".into(),
        grace_s: 3600,
    });
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
    let account = seed::create_account(
        pool,
        &NewAccount {
            livemode: false,
            ..NewAccount::named("hint merchant")
        },
    )
    .await?;
    let customer = seed::create_customer(
        pool,
        &NewCustomer {
            id: Uuid::new_v4(),
            account_id: account.id,
            livemode: false,
            client_reference_id: "hint customer".into(),
            paused_scopes: Vec::new(),
        },
    )
    .await?;
    seed::accept_routes(pool, account.id, false, &[&route]).await?;
    seed::initialize_dual_chain(pool, CHAIN).await?;
    let key = seed::create_api_key(pool, account.id, false).await?;
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
    let read_only_app = topup::api::read_only_router(state.clone(), None);
    Ok(Harness {
        read_only_app,
        app: topup::api::router(state).0,
        queue,
        routes,
        read,
        verify,
        key,
        quote_id,
        secret,
        read_label,
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
        ensure!(sqlx::query_scalar::<_, i32>("SELECT used FROM daily_budgets WHERE name='hints'").fetch_one(&db.app_pool).await? == 1);
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
async fn hints_ignore_transfers_before_created_block_and_include_the_boundary() -> Result<()> {
    for before_creation in [true, false] {
        let Some(db) = TestDatabase::create().await? else {
            return Ok(());
        };
        let result = async {
            let read = Rpc::new();
            let verify = Rpc::new();
            let block = u64::from_str_radix(
                read.receipt.lock().unwrap()["blockNumber"]
                    .as_str()
                    .context("fixture block number")?
                    .trim_start_matches("0x"),
                16,
            )?;
            read.receipt_gate.store(true, Ordering::SeqCst);
            let h = harness(&db.app_pool, read.clone(), verify.clone()).await?;
            sqlx::query("UPDATE addresses SET created_block=$1")
                .bind(i64::try_from(block + u64::from(before_creation))?)
                .execute(&db.app_pool)
                .await?;
            let markers: (Option<i64>, Option<i64>, bool) = sqlx::query_as(
                "SELECT dual_covered_through,backfilled_through,backfilled FROM addresses",
            )
            .fetch_one(&db.app_pool)
            .await?;
            h.submit(
                &format!("/v1/quotes/{}/transactions", h.quote_id),
                Some(&h.key),
                json!({"transaction_hash":TX}),
            )
            .await?;
            let (cancel, mut worker) = h.worker(&db.app_pool);
            wait_until(async || {
                Ok(read.methods.lock().unwrap().iter().any(|method| {
                    method == "eth_getTransactionReceipt"
                }))
            })
            .await?;
            // A queued duplicate disappears only when the running task finishes. This
            // proves the excluded case completed, rather than merely checking before insert.
            h.submit(
                &format!("/v1/quotes/{}/transactions", h.quote_id),
                Some(&h.key),
                json!({"transaction_hash":TX}),
            )
            .await?;
            ensure!(h.queue.pending() == 1);
            read.receipt_gate.store(false, Ordering::SeqCst);
            wait_until(async || Ok(h.queue.pending() == 0)).await?;
            // Both sources finish decoding; the creation boundary alone excludes the log.
            ensure!(verify.methods.lock().unwrap().iter().any(|method| {
                method == "eth_getTransactionByHash"
            }));
            if before_creation {
                ensure!(deposits(&db.app_pool).await? == 0);
            } else {
                ensure!(deposits(&db.app_pool).await? == 1);
                ensure!(sqlx::query_scalar::<_, bool>(
                    "SELECT block_number=$1 AND dual_verified_at=created_at FROM deposits",
                )
                .bind(i64::try_from(block)?)
                .fetch_one(&db.app_pool)
                .await?);
            }
            cancel.cancel();
            (&mut worker.0).await?;
            ensure!(disagreements(&h) == 0);
            ensure!(sqlx::query_scalar::<_, bool>("SELECT through_block=0 FROM chain_coverage")
                .fetch_one(&db.app_pool)
                .await?);
            ensure!(markers == sqlx::query_as::<_, (Option<i64>, Option<i64>, bool)>(
                "SELECT dual_covered_through,backfilled_through,backfilled FROM addresses",
            )
            .fetch_one(&db.app_pool)
            .await?);
            ensure!(sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM events WHERE type IN ('quote.canceled','quote.expired','deposit.rejected')",
            )
            .fetch_one(&db.app_pool)
            .await? == 0);
            Ok(())
        }
        .await;
        db.cleanup().await?;
        result?;
    }
    Ok(())
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
        let verify = Rpc::new();
        let h = harness(&db.app_pool, Rpc::new(), verify.clone()).await?;
        ensure!(h.read.ready());
        fail_endpoint(&h.verify, &verify).await?;
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
        verify.failure.store(false, Ordering::SeqCst);
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
    for case in ["unmined", "read-behind", "verify-behind"] {
        let Some(db) = TestDatabase::create().await? else {
            return Ok(());
        };
        let result = async {
            let read = Rpc::new();
            let verify = Rpc::new();
            match case {
                "unmined" => *read.receipt.lock().unwrap() = Value::Null,
                "read-behind" => {
                    *read.head.lock().unwrap() = json!("0x2d3f773");
                    *verify.head.lock().unwrap() = json!("0x2d3f773");
                }
                "verify-behind" => *verify.head.lock().unwrap() = json!("0x2d3f773"),
                _ => unreachable!(),
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
            while if case == "verify-behind" {
                verify.calls.load(Ordering::SeqCst) < verify_start + 8
            } else {
                read.calls.load(Ordering::SeqCst) < read_start + 12
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
            if case == "verify-behind" {
                // One discovery receipt and eight read heads; either exhausted side
                // stops both transports before another read head can be sent.
                ensure!(read.calls.load(Ordering::SeqCst) == read_start + 9);
                ensure!(verify.calls.load(Ordering::SeqCst) == verify_start + 8);
            } else {
                ensure!(read.calls.load(Ordering::SeqCst) == read_start + 12);
                ensure!(verify.calls.load(Ordering::SeqCst) == verify_start);
            }
            ensure!(disagreements(&h) == 0, "budget exhaustion must not alert");
            ensure!(
                h.read.ready() && h.verify.ready(),
                "task budget exhaustion changed endpoint health"
            );
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

#[tokio::test]
async fn deposit_address_requires_an_issued_chain_and_its_secret_or_write_permission() -> Result<()>
{
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        let account: Uuid = sqlx::query_scalar("SELECT account_id FROM quotes LIMIT 1")
            .fetch_one(&db.app_pool)
            .await?;
        seed::set_treasury(&db.app_pool, account, false, CHAIN, Address::repeat_byte(9)).await?;
        let response = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/deposit_addresses")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", h.key))
                    .body(Body::from(r#"{"client_reference_id":"persistent hint"}"#))?,
            )
            .await?;
        ensure!(
            response.status().is_success(),
            "deposit address creation failed"
        );
        let address: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        let id = address["id"].as_str().context("address id")?;
        let secret = address["client_secret"]
            .as_str()
            .context("address secret")?;
        let path = format!("/v1/deposit_addresses/{id}/transactions");
        for body in [
            json!({"transaction_hash":TX}),
            json!({"transaction_hash":TX,"chain_id":1}),
            json!({"transaction_hash":TX,"chain_id":0}),
            json!({"transaction_hash":TX,"chain_id":u64::MAX}),
        ] {
            h.submit(&path, Some(&h.key), body).await?;
            ensure!(h.queue.pending() == 0);
        }
        h.submit(
            &format!("{path}?client_secret={}", h.secret),
            None,
            json!({"transaction_hash":TX,"chain_id":CHAIN}),
        )
        .await?;
        ensure!(
            h.queue.pending() == 0,
            "quote secret authenticated another object"
        );
        h.submit(
            &format!("{path}?client_secret={secret}"),
            None,
            json!({"transaction_hash":TX,"chain_id":CHAIN,"recipient":RECIPIENT}),
        )
        .await?;
        ensure!(h.queue.pending() == 1);
        let (cancel, _worker) = h.worker(&db.app_pool);
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
        ensure!(
            deposits(&db.app_pool).await? == 0,
            "caller recipient redirected the hint"
        );
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn one_transaction_can_pay_two_objects_and_each_task_gets_its_own_dedup_key() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let read = Rpc::new(); let verify = Rpc::new();
        let recipient = Address::repeat_byte(8);
        for state in [&read, &verify] {
            let mut receipt = state.receipt.lock().unwrap();
            let mut second = receipt["logs"][0].clone();
            second["topics"][2] = json!(format!("0x{:064x}", recipient.into_word()));
            second["logIndex"] = json!("0x24");
            receipt["logs"].as_array_mut().unwrap().push(second);
        }
        let h = harness(&db.app_pool, read, verify).await?;
        let customer_id: Uuid = sqlx::query_scalar("SELECT customer_id FROM quotes LIMIT 1").fetch_one(&db.app_pool).await?;
        let route: String = sqlx::query_scalar("SELECT route FROM quotes LIMIT 1").fetch_one(&db.app_pool).await?;
        let address = seed::insert_address(&db.app_pool, &NewAddress { id:Uuid::new_v4(), customer_id, chain_id:CHAIN, route, salt:B256::repeat_byte(8), address:recipient }).await?;
        let other = topup::ids::format(topup::ids::QUOTE, address.quote_id.unwrap());
        h.submit(&format!("/v1/quotes/{}/transactions", h.quote_id), Some(&h.key), json!({"transaction_hash":TX})).await?;
        let (cancel, _worker) = h.worker(&db.app_pool);
        wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?;
        // The first object's already recorded transfer must not deduplicate the second object.
        h.submit(&format!("/v1/quotes/{other}/transactions"), Some(&h.key), json!({"transaction_hash":TX})).await?;
        wait_until(async || Ok(deposits(&db.app_pool).await? == 2)).await?;
        cancel.cancel();
        ensure!(sqlx::query_scalar::<_, i64>("SELECT count(DISTINCT receipt_log_index) FROM deposits WHERE dual_verified_at IS NOT NULL").fetch_one(&db.app_pool).await? == 2);
        Ok(())
    }.await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn only_four_hint_tasks_can_be_in_flight_even_with_more_ready_objects() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let read = Rpc::new();
        let verify = Rpc::new();
        read.receipt_gate.store(true, Ordering::SeqCst);
        let h = harness(&db.app_pool, read.clone(), verify).await?;
        let (customer_id, route): (Uuid, String) =
            sqlx::query_as("SELECT customer_id,route FROM quotes LIMIT 1")
                .fetch_one(&db.app_pool)
                .await?;
        for index in 1_u8..=6 {
            let address = seed::insert_address(
                &db.app_pool,
                &NewAddress {
                    id: Uuid::new_v4(),
                    customer_id,
                    chain_id: CHAIN,
                    route: route.clone(),
                    salt: B256::repeat_byte(index),
                    address: Address::repeat_byte(index),
                },
            )
            .await?;
            let id = topup::ids::format(topup::ids::QUOTE, address.quote_id.unwrap());
            h.submit(
                &format!("/v1/quotes/{id}/transactions"),
                Some(&h.key),
                json!({"transaction_hash":TX}),
            )
            .await?;
        }
        let start = read.calls.load(Ordering::SeqCst);
        let (cancel, _worker) = h.worker(&db.app_pool);
        wait_until(async || Ok(read.calls.load(Ordering::SeqCst) == start + 4)).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        ensure!(read.calls.load(Ordering::SeqCst) == start + 4);
        ensure!(h.queue.pending() == 2);
        cancel.cancel();
        read.receipt_gate.store(false, Ordering::SeqCst);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn read_only_restricted_keys_and_other_accounts_cannot_submit_tasks() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        let response = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/api_keys")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", h.key))
                    .body(Body::from(
                        r#"{"type":"restricted","permissions":["quotes.read"]}"#,
                    ))?,
            )
            .await?;
        ensure!(response.status().is_success());
        let key: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        let other_account = seed::create_account(
            &db.app_pool,
            &NewAccount {
                livemode: false,
                ..NewAccount::named("another merchant")
            },
        )
        .await?;
        let other_key = seed::create_api_key(&db.app_pool, other_account.id, false).await?;
        let path = format!("/v1/quotes/{}/transactions", h.quote_id);
        for key in [key["secret"].as_str().unwrap(), &other_key] {
            h.submit(&path, Some(key), json!({"transaction_hash":TX}))
                .await?;
            ensure!(h.queue.pending() == 0);
        }
        h.submit(&path, Some(&h.key), json!({"transaction_hash":TX}))
            .await?;
        ensure!(h.queue.pending() == 1);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn browser_preflight_and_read_only_requests_keep_quiet_acknowledgements() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        let path = format!(
            "/v1/quotes/{}/transactions?client_secret={}",
            h.quote_id, h.secret
        );
        let preflight = h
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri(&path)
                    .header("origin", "https://merchant.example")
                    .header("access-control-request-method", "POST")
                    .header("access-control-request-headers", "content-type")
                    .body(Body::empty())?,
            )
            .await?;
        ensure!(preflight.status() == StatusCode::NO_CONTENT);
        ensure!(preflight.headers()["access-control-allow-origin"] == "*");
        ensure!(preflight.headers()["access-control-allow-methods"] == "POST, OPTIONS");
        ensure!(preflight.headers()["access-control-allow-headers"] == "Content-Type");
        ensure!(preflight.headers()["access-control-max-age"] == "600");
        ensure!(to_bytes(preflight.into_body(), 4096).await?.is_empty());
        let response = h
            .read_only_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(
                        &json!({"transaction_hash":TX}),
                    )?))?,
            )
            .await?;
        ensure!(response.status() == StatusCode::ACCEPTED);
        ensure!(h.queue.pending() == 0);
        ensure!(deposits(&db.app_pool).await? == 0);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

// Use the real transport failure streak: the first two failures retain readiness.
async fn fail_endpoint(client: &EvmClient, rpc: &Rpc) -> Result<()> {
    rpc.failure.store(true, Ordering::SeqCst);
    for failure in 1..=3 {
        ensure!(client.latest_head().await.is_err());
        ensure!(client.ready() == (failure < 3));
    }
    Ok(())
}
fn disagreements(h: &Harness) -> u64 {
    topup_adapters::chain::evm::metrics::rpc_error_counts()
        .into_iter()
        .filter(|(provider, _, _, class, _)| provider == &h.read_label && *class == "disagreement")
        .map(|(_, _, _, _, count)| count)
        .sum()
}

#[tokio::test]
async fn verify_lagging_for_one_and_a_half_seconds_records_within_whole_task_budget() -> Result<()>
{
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let read = Rpc::new();
        let verify = Rpc::new();
        let receipt = verify.receipt.lock().unwrap().clone();
        *verify.receipt.lock().unwrap() = Value::Null;
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
        wait_until(async || {
            Ok(verify
                .methods
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_getTransactionReceipt"))
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        ensure!(deposits(&db.app_pool).await? == 0);
        for rpc in [&read, &verify] {
            ensure!(!rpc.methods.lock().unwrap().iter().any(|method| matches!(
                method.as_str(),
                "eth_getBlockByHash" | "eth_getTransactionByHash"
            )));
        }
        *verify.receipt.lock().unwrap() = receipt;
        wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?;
        cancel.cancel();
        ensure!(read.calls.load(Ordering::SeqCst) <= read_start + 12);
        ensure!(verify.calls.load(Ordering::SeqCst) <= verify_start + 8);
        ensure!(disagreements(&h) == 0);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn reorg_during_confirmation_wait_uses_fresh_independent_evidence() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let read = Rpc::new();
        let verify = Rpc::new();
        for rpc in [&read, &verify] {
            *rpc.head.lock().unwrap() = json!("0x2d3f773");
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
        wait_until(async || Ok(read.calls.load(Ordering::SeqCst) >= read_start + 2)).await?;
        ensure!(verify.calls.load(Ordering::SeqCst) == verify_start);
        ensure!(deposits(&db.app_pool).await? == 0);
        let new_hash = format!("0x{}", "88".repeat(32));
        for rpc in [&read, &verify] {
            ensure!(!rpc.methods.lock().unwrap().iter().any(|method| matches!(
                method.as_str(),
                "eth_getBlockByHash" | "eth_getTransactionByHash"
            )));
        }
        reinclude(&read, &new_hash);
        *read.head.lock().unwrap() = json!("0x2d3f77a");
        // Verify still sees the old inclusion and shallow head until it catches up.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        ensure!(disagreements(&h) == 0);
        reinclude(&verify, &new_hash);
        *verify.head.lock().unwrap() = json!("0x2d3f77a");
        wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?;
        cancel.cancel();
        let (hash, amount, signature): (String, String, bool) = sqlx::query_as(
            "SELECT block_hash,amount_atomic::text,dual_verified_at=created_at FROM deposits",
        )
        .fetch_one(&db.app_pool)
        .await?;
        ensure!(hash == new_hash && amount == "23" && signature);
        ensure!(disagreements(&h) == 0);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

fn reinclude(rpc: &Rpc, hash: &str) {
    let mut receipt = rpc.receipt.lock().unwrap();
    receipt["blockHash"] = json!(hash);
    receipt["logs"][0]["blockHash"] = json!(hash);
    receipt["logs"][0]["data"] = json!(format!("0x{:064x}", 23));
    rpc.transaction.lock().unwrap()["blockHash"] = json!(hash);
    rpc.block.lock().unwrap()["hash"] = json!(hash);
}

#[tokio::test]
async fn independent_evidence_disagreement_at_depth_alerts_without_recording() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let verify = Rpc::new();
        verify.transaction.lock().unwrap()["nonce"] = json!("0xffff");
        let h = harness(&db.app_pool, Rpc::new(), verify).await?;
        h.submit(
            &format!("/v1/quotes/{}/transactions", h.quote_id),
            Some(&h.key),
            json!({"transaction_hash":TX}),
        )
        .await?;
        let (cancel, _worker) = h.worker(&db.app_pool);
        wait_until(async || Ok(disagreements(&h) == 1)).await?;
        cancel.cancel();
        ensure!(deposits(&db.app_pool).await? == 0);
        ensure!(
            sqlx::query_scalar::<_, bool>("SELECT through_block=0 FROM chain_coverage")
                .fetch_one(&db.app_pool)
                .await?
        );
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn readiness_loss_after_dequeue_parks_then_resumes_without_spending_budget() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let verify = Rpc::new();
        let h = harness(&db.app_pool, Rpc::new(), verify.clone()).await?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&db.app_url)
            .await?;
        let held = pool.begin().await?;
        h.submit(
            &format!("/v1/quotes/{}/transactions", h.quote_id),
            Some(&h.key),
            json!({"transaction_hash":TX}),
        )
        .await?;
        let (cancel, _worker) = h.worker(&pool);
        wait_until(async || Ok(h.queue.pending() == 0)).await?;
        fail_endpoint(&h.verify, &verify).await?;
        held.rollback().await?;
        wait_until(async || Ok(h.queue.pending() == 1)).await?;
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM daily_budgets WHERE name='hints'")
                .fetch_one(&db.app_pool)
                .await?
                == 0
        );
        verify.failure.store(false, Ordering::SeqCst);
        h.verify.latest_head().await?;
        wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?;
        cancel.cancel();
        pool.close().await;
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn frozen_chain_defers_before_claiming_budget_or_reading_rpc() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let read = Rpc::new();
        let verify = Rpc::new();
        let h = harness(&db.app_pool, read.clone(), verify.clone()).await?;
        topup::db::chain_reads::freeze(&db.app_pool, CHAIN, "hint-test").await?;
        h.submit(
            &format!("/v1/quotes/{}/transactions", h.quote_id),
            Some(&h.key),
            json!({"transaction_hash":TX}),
        )
        .await?;
        let (cancel, _worker) = h.worker(&db.app_pool);
        wait_until(async || Ok(h.queue.pending() == 0)).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        ensure!(read.calls.load(Ordering::SeqCst) == 1 && verify.calls.load(Ordering::SeqCst) == 1);
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM daily_budgets WHERE name='hints'")
                .fetch_one(&db.app_pool)
                .await?
                == 0
        );
        ensure!(deposits(&db.app_pool).await? == 0);
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn same_ingress_peer_does_not_globally_limit_hints_but_object_credentials_do() -> Result<()> {
    let Some(db) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
        let (customer_id, route): (Uuid, String) =
            sqlx::query_as("SELECT customer_id,route FROM quotes LIMIT 1")
                .fetch_one(&db.app_pool)
                .await?;
        for index in 1_u8..=25 {
            let address = seed::insert_address(
                &db.app_pool,
                &NewAddress {
                    id: Uuid::new_v4(),
                    customer_id,
                    chain_id: CHAIN,
                    route: route.clone(),
                    salt: B256::repeat_byte(index),
                    address: Address::repeat_byte(index),
                },
            )
            .await?;
            let id = topup::ids::format(topup::ids::QUOTE, address.quote_id.unwrap());
            h.submit(
                &format!("/v1/quotes/{id}/transactions"),
                Some(&h.key),
                json!({"transaction_hash":TX}),
            )
            .await?;
        }
        ensure!(h.queue.pending() == 25);
        let path = format!("/v1/quotes/{}/transactions", h.quote_id);
        for _ in 0..25 {
            h.submit(&path, Some("invalid"), json!({"transaction_hash":TX}))
                .await?;
        }
        for byte in 30..=33 {
            h.submit(
                &path,
                Some(&h.key),
                json!({"transaction_hash":format!("{:#x}", B256::repeat_byte(byte))}),
            )
            .await?;
        }
        ensure!(
            h.queue.pending() == 28,
            "only three authenticated hints per object per minute"
        );
        Ok(())
    }
    .await;
    db.cleanup().await?;
    result
}

#[tokio::test]
async fn hinted_in_window_payment_blocks_cancel_and_cancel_requested_closure() -> Result<()> {
    for cancel_requested in [false, true] {
        let Some(db) = TestDatabase::create().await? else {
            return Ok(());
        };
        let result = async {
            let h = harness(&db.app_pool, Rpc::new(), Rpc::new()).await?;
            let (id, account): (Uuid, Uuid) = sqlx::query_as("SELECT id,account_id FROM quotes LIMIT 1").fetch_one(&db.app_pool).await?;
            sqlx::query("UPDATE quotes SET status='open',closed_at=NULL,expires_at=now()+interval '1 hour'").execute(&db.app_pool).await?;
            if cancel_requested { sqlx::query("UPDATE quotes SET cancel_requested_at=now(),expires_at=now()-interval '1 second'").execute(&db.app_pool).await?; }
            h.submit(&format!("/v1/quotes/{}/transactions", h.quote_id), Some(&h.key), json!({"transaction_hash":TX})).await?;
            let (cancel, _worker) = h.worker(&db.app_pool);
            wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?; cancel.cancel();
            if cancel_requested {
                cover_past_now(&db.app_pool).await?;
                ensure!(topup::locks::expire_once(&db.app_pool, &h.routes).await? == 0);
                ensure!(sqlx::query_scalar::<_, bool>("SELECT status='open' AND cancel_requested_at IS NOT NULL FROM quotes").fetch_one(&db.app_pool).await?);
            } else {
                ensure!(matches!(topup::locks::cancel(&db.app_pool, &h.routes, topup::tenancy::Scope::new(account, false), &topup::audit::Actor::system("hint-test"), id).await, Err(topup::locks::RateLockError::PendingPayment)));
            }
            Ok(())
        }.await;
        db.cleanup().await?;
        result?;
    }
    Ok(())
}
async fn cover_past_now(pool: &sqlx::PgPool) -> Result<()> {
    sqlx::query("UPDATE chain_coverage SET through_time=now()+interval '1 second',through_block=through_block+1").execute(pool).await?;
    sqlx::query("UPDATE addresses SET dual_covered_through=(SELECT through_block FROM chain_coverage WHERE chain_id=$1)").bind(i64::try_from(CHAIN)?).execute(pool).await?;
    Ok(())
}
#[tokio::test]
async fn late_payment_hint_records_a_positive_fact_without_reopening_or_preventing_expiry()
-> Result<()> {
    for already_expired in [false, true] {
        let Some(db) = TestDatabase::create().await? else {
            return Ok(());
        };
        let result = async {
            let rpc = Rpc::new();
            let block_time = i64::from_str_radix(rpc.block.lock().unwrap()["timestamp"].as_str().unwrap().trim_start_matches("0x"), 16)?;
            let h = harness(&db.app_pool, rpc, Rpc::new()).await?;
            sqlx::query("UPDATE quotes SET expires_at=to_timestamp($1),status=$2,closed_at=CASE WHEN $3 THEN now() ELSE NULL END")
                .bind((block_time - 1) as f64).bind(if already_expired { "expired" } else { "open" }).bind(already_expired).execute(&db.app_pool).await?;
            h.submit(&format!("/v1/quotes/{}/transactions", h.quote_id), Some(&h.key), json!({"transaction_hash":TX})).await?;
            let (cancel, _worker) = h.worker(&db.app_pool);
            wait_until(async || Ok(deposits(&db.app_pool).await? == 1)).await?; cancel.cancel();
            ensure!(sqlx::query_scalar::<_, bool>("SELECT state='detected' AND dual_verified_at=created_at FROM deposits").fetch_one(&db.app_pool).await?);
            ensure!(topup::locks::expire_once(&db.app_pool, &h.routes).await? == 0, "hint cannot supply negative coverage");
            cover_past_now(&db.app_pool).await?;
            ensure!(topup::locks::expire_once(&db.app_pool, &h.routes).await? == u64::from(!already_expired));
            ensure!(sqlx::query_scalar::<_, bool>("SELECT status='expired' FROM quotes").fetch_one(&db.app_pool).await?);
            Ok(())
        }.await;
        db.cleanup().await?;
        result?;
    }
    Ok(())
}
