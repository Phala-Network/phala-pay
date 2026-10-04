//! Regression tests through production scanner, reconciliation and finality entry points.
mod support;
use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use topup::{
    db,
    routes::RouteSet,
    scanner::{self, chain_routes},
};
use topup_adapters::{
    chain::evm::{
        EvmClient, FinalizedReader,
        group::{
            GroupPolicy, HeadAnchor, Member, RpcGroup, Selection, WatermarkStore,
            budget::{BudgetSpec, Budgets},
        },
    },
    redaction::Redacted,
};
use topup_core::route::RouteFile;
use uuid::Uuid;
const HEIGHT: u64 = 100;
const CHAIN: u64 = 84532;
const RECIPIENT: &str = "0x3c0fe91b38c2f708d360f5724208fa7ecaa6ed34";
fn fixture(name: &str) -> Value {
    let raw = match name {
        "block" => include_str!("../../adapters/tests/fixtures/base-sepolia/block-47297199.json"),
        "logs" => include_str!("../../adapters/tests/fixtures/base-sepolia/bridge-mint-logs.json"),
        "receipt" => {
            include_str!("../../adapters/tests/fixtures/base-sepolia/bridge-mint-receipt.json")
        }
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(raw).unwrap()
}
fn header(height: u64) -> Value {
    let mut block = fixture("block");
    block["number"] = json!(format!("0x{height:x}"));
    if height != HEIGHT {
        block["hash"] = json!(format!("{:#x}", B256::from(U256::from(height))));
    }
    block
}
fn transfer(index: usize) -> Value {
    let mut log = fixture("logs")[0].clone();
    log["blockNumber"] = json!(format!("0x{HEIGHT:x}"));
    if index > 0 {
        log["transactionHash"] = json!(format!("{:#x}", B256::from(U256::from(index))));
    }
    log
}
fn factory_event(height: u64) -> Value {
    use alloy::sol_types::SolEvent;
    let event = topup_adapters::chain::flush::Flushed {
        salt: B256::ZERO,
        forwarder: RECIPIENT.parse().unwrap(),
        token: transfer(0)["address"].as_str().unwrap().parse().unwrap(),
        treasury: support::seed::FIXTURE_TREASURY,
        amount: U256::from(1),
    };
    let data = event.encode_log_data();
    let mut log = transfer(0);
    log["topics"] = json!(data.topics());
    log["data"] = json!(data.data);
    log["blockNumber"] = json!(format!("0x{height:x}"));
    log["blockHash"] = header(height)["hash"].clone();
    log["logIndex"] = json!(format!("0x{height:x}"));
    log
}
struct Node {
    url: String,
    mode: Arc<AtomicUsize>,
    sends: Arc<AtomicUsize>,
    hanging: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Node {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Node {
    // Modes: 0 complete, 1 omission, 2 lag/null receipt, 3 last batch/receipt fails,
    // 4 hanging batch/receipt, 5 alternate nonfinal hash.
    async fn start(count: usize) -> Result<Self> {
        let mode = Arc::new(AtomicUsize::new(0));
        let sends = Arc::new(AtomicUsize::new(0));
        let hanging = Arc::new(tokio::sync::Notify::new());
        let signal = hanging.clone();
        let m = mode.clone();
        let hits = sends.clone();
        let router=Router::new().route("/",post(move |Json(request):Json<Value>|{
            let mode=m.clone();let sends=hits.clone();let signal=signal.clone();async move {
                let m=mode.load(Ordering::SeqCst);
                if m==6 || m==12 {tokio::time::sleep(Duration::from_millis(30)).await;}
                let result=match request["method"].as_str().unwrap(){
                    "eth_getBlockByNumber"|"eth_getBlockByHash"=>{
                        let arg=request["params"][0].as_str().unwrap();
                        let height=if ["latest","safe","finalized"].contains(&arg){if m==13 {5000}else if m==2 || ([5,7,8,9,10,11].contains(&m) && arg=="finalized") {HEIGHT-1}else if [10,11].contains(&m) && arg=="latest" {HEIGHT+1}else{HEIGHT}}else{u64::from_str_radix(arg.trim_start_matches("0x"),16).unwrap_or(HEIGHT)};
                        let mut h=header(height);
                        if ([5,9,11].contains(&m) && height==HEIGHT && arg!="finalized") || (m==7 && ["latest","safe"].contains(&arg)){h["hash"]=json!(format!("0x{}","33".repeat(32)));}
                        h
                    },
                    "eth_getLogs"=>{
                        let filter=&request["params"][0];
                        let batched=filter.pointer("/topics/2").is_some_and(|v|!v.is_null());
                        let batch=sends.fetch_add(1,Ordering::SeqCst);
                        if batched && m==3 && batch>0 {return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"last batch failed"}}));}
                        if batched && m==4 && batch>0 {signal.notify_one();std::future::pending::<()>().await;}
                        let from=u64::from_str_radix(filter["fromBlock"].as_str().unwrap().trim_start_matches("0x"),16).unwrap();
                        let to=u64::from_str_radix(filter["toBlock"].as_str().unwrap().trim_start_matches("0x"),16).unwrap();
                        if m==12 && from<to {return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32005,"message":"block range too wide"}}));}
                        let is_transfer=filter.pointer("/topics/0").and_then(Value::as_str)==Some("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
                        if m==12 && !is_transfer {
                            json!((from..=to).map(factory_event).collect::<Vec<_>>())
                        } else if [1,2,8,10].contains(&m) || !is_transfer || from>HEIGHT || to<HEIGHT {json!([])} else {
                            json!((0..count).map(|i| {let mut log=transfer(i);if [9,11].contains(&m){log["blockHash"]=json!(format!("0x{}","33".repeat(32)));}log}).collect::<Vec<_>>())
                        }
                    },
                    "eth_getTransactionReceipt"=>{
                        if m==2 {Value::Null} else {
                            let hash=request["params"][0].as_str().unwrap();let original=transfer(0);
                            let index=if hash==original["transactionHash"].as_str().unwrap(){0}else{usize::from_str_radix(hash.trim_start_matches("0x"),16).unwrap()};
                            if m==3 && index>0 {return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"last receipt failed"}}));}
                            if m==4 && index>0 {signal.notify_one();std::future::pending::<()>().await;}

                            let mut receipt=fixture("receipt");receipt["transactionHash"]=json!(hash);receipt["blockNumber"]=json!(format!("0x{HEIGHT:x}"));receipt["logs"]=json!([transfer(index)]);if [9,11].contains(&m){receipt["blockHash"]=json!(format!("0x{}","33".repeat(32)));receipt["logs"][0]["blockHash"]=receipt["blockHash"].clone();}receipt
                        }
                    },
                    "eth_getTransactionCount"=>json!("0xffffffff"),
                    other=>panic!("unexpected RPC method {other}"),
                };
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Ok(Self {
            url,
            mode,
            sends,
            hanging,
            task,
        })
    }
}
fn group(id: &str, urls: &[&str], rate: u32, policy: GroupPolicy) -> Result<Arc<RpcGroup>> {
    let budgets = Arc::new(
        Budgets::new(&BTreeMap::from([
            (
                "account".into(),
                BudgetSpec {
                    requests_per_second: rate,
                    burst: rate,
                },
            ),
            (
                "key".into(),
                BudgetSpec {
                    requests_per_second: rate,
                    burst: rate,
                },
            ),
        ]))
        .map_err(anyhow::Error::msg)?,
    );
    let members = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            Ok(Member {
                id: format!("{id}-{i}"),
                company: id.into(),
                endpoint: Redacted::parse(url)?,
                account: "account".into(),
                key: "key".into(),
                priority: u32::try_from(i)?,
                weight: 1,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let group = RpcGroup::new(id.into(), CHAIN, policy, members, budgets)?;
    for i in 0..urls.len() {
        group.verified(i, true);
    }
    Ok(group)
}
fn routes(a: Arc<RpcGroup>, b: Arc<RpcGroup>, address_mode: bool) -> Result<Arc<RouteSet>> {
    routes_confirmations(
        a,
        b,
        address_mode,
        topup_core::route::Confirmations::Finalized,
    )
}
fn routes_confirmations(
    a: Arc<RpcGroup>,
    b: Arc<RpcGroup>,
    address_mode: bool,
    confirmations: topup_core::route::Confirmations,
) -> Result<Arc<RouteSet>> {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.chain_id = CHAIN;
    route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
        feed: "BASE_SEQUENCER_UPTIME".into(),
        grace_s: 3600,
        rpc_group: "a".into(),
        rpc_group_b: "b".into(),
    });
    route.livemode = false;
    route.chain.confirmations = confirmations;
    route.chain.rpc_providers = vec![a.id.clone(), b.id.clone()];
    route.asset.contract = transfer(0)["address"].as_str().unwrap().parse()?;
    if address_mode {
        route.asset.backstop = topup_core::route::Backstop::Addresses;
    }
    Ok(Arc::new(
        RouteSet::with_groups(
            vec![route],
            BTreeMap::from([
                (a.id.clone(), Arc::new(EvmClient::from_group(a, None)?)),
                (b.id.clone(), Arc::new(EvmClient::from_group(b, None)?)),
            ]),
        )
        .map_err(anyhow::Error::msg)?,
    ))
}
async fn seed(pool: &PgPool, count: usize) -> Result<Uuid> {
    let (_, customer) = support::seed::create_account_and_customer(
        pool,
        &support::seed::NewAccount::named("RPC entry regression"),
        "customer",
    )
    .await?;
    let mut first = None;
    for i in 0..count {
        let id = Uuid::new_v4();
        let address = if i == 0 {
            RECIPIENT.parse()?
        } else {
            Address::from_slice(&B256::from(U256::from(i)).as_slice()[12..])
        };
        support::seed::insert_address(
            pool,
            &support::seed::NewAddress {
                id,
                customer_id: customer.id,
                chain_id: CHAIN,
                route: "phala-cloud-ethereum-pha-usd".into(),
                salt: B256::from(U256::from(i)),
                address,
            },
        )
        .await?;
        first.get_or_insert(id);
    }
    sqlx::query("UPDATE addresses SET created_block=$1,backfilled=true WHERE chain_id=$2")
        .bind(i64::try_from(HEIGHT)?)
        .bind(i64::try_from(CHAIN)?)
        .execute(pool)
        .await?;
    db::initialize_cursor(pool, CHAIN, HEIGHT - 1, chrono::Utc::now()).await?;
    first.context("seed address")
}
async fn install(pool: &PgPool, groups: &[Arc<RpcGroup>]) -> Result<()> {
    let state = db::rpc::state(pool, "entrypoints".into());
    let anchor: HeadAnchor = serde_json::from_value(
        json!({"number":HEIGHT-1,"hash":header(HEIGHT-1)["hash"],"parent_hash":header(HEIGHT-1)["parentHash"]}),
    )?;
    for group in groups {
        group.set_store(state.clone());
        state
            .accept(CHAIN, &group.id, "cursor", &group.members[0].id, &anchor)
            .await?;
    }
    Ok(())
}
async fn scan(pool: &PgPool, routes: &RouteSet) -> Result<scanner::ScanStats> {
    let reader = FinalizedReader::new(routes.provider(CHAIN, 0)?.clone());
    Ok(scanner::scan_once(pool, &reader, &chain_routes(routes)[0]).await?)
}
#[tokio::test]
async fn dense_window_completes_at_ten_rps_through_the_scanner() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let node = Node::start(50).await?;
        seed(&database.app_pool, 1).await?;
        let a = group("a", &[&node.url], 10, GroupPolicy::default())?;
        let b = group("b", &[&node.url], 1000, GroupPolicy::default())?;
        install(&database.app_pool, &[a.clone(), b.clone()]).await?;
        let routes = routes(a, b, false)?;
        let stats =
            tokio::time::timeout(Duration::from_secs(60), scan(&database.app_pool, &routes))
                .await??;
        ensure!(
            stats.inserted == 50,
            "dense window must finish, not retry forever: {stats:?}"
        );
        ensure!(db::get_cursor(&database.app_pool, CHAIN).await? == Some(HEIGHT));
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn singleton_replays_old_omission_after_restart_without_claiming_independent_review()
-> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result=async{
        let node=Node::start(1).await?;node.mode.store(1,Ordering::SeqCst);seed(&database.app_pool,1).await?;
        for restart in 0..2 {
            let a=group("a",&[&node.url],1000,GroupPolicy::default())?;let b=group("b",&[&node.url],1000,GroupPolicy::default())?;
            install(&database.app_pool,&[a.clone(),b.clone()]).await?;let routes=routes(a,b,false)?;
            if restart==1 {node.mode.store(0,Ordering::SeqCst);}
            scan(&database.app_pool,&routes).await?;
        }
        let deposits:i64=sqlx::query_scalar("SELECT count(*) FROM deposits").fetch_one(&database.app_pool).await?;
        ensure!(deposits==1,"singleton must replay historical omissions");
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM rpc_window_reviews WHERE reviewed_at IS NULL AND replayed_at IS NOT NULL").fetch_one(&database.app_pool).await?;
        ensure!(pending>0,"self replay must preserve independent backlog");Ok(())
    }.await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn mixed_member_null_receipt_cannot_reverse_a_real_deposit() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let good = Node::start(1).await?;
        let lag = Node::start(1).await?;
        lag.mode.store(2, Ordering::SeqCst);
        seed(&database.app_pool, 1).await?;
        let a = group("a", &[&good.url], 1000, GroupPolicy::default())?;
        let b = group("b", &[&good.url], 1000, GroupPolicy::default())?;
        install(&database.app_pool, &[a.clone(), b.clone()]).await?;
        scan(&database.app_pool, routes(a, b, false)?.as_ref()).await?;
        sqlx::query("UPDATE deposits SET state='credited',credit_minor=1")
            .execute(&database.app_pool)
            .await?;
        // Use round robin: previously each receipt could come from the lagging member while
        // finalized heads and consumed nonces came from the fresh member.
        let policy = GroupPolicy {
            selection: Selection::WeightedRoundRobin,
            ..Default::default()
        };
        let a = group("a", &[&good.url, &lag.url], 1000, policy.clone())?;
        let b = group("b", &[&good.url, &lag.url], 1000, policy)?;
        install(&database.app_pool, &[a.clone(), b.clone()]).await?;
        let routes = routes(a, b, false)?;
        let watch = topup::finality::FinalityWatch::single(
            database.app_pool.clone(),
            routes.clone(),
            CHAIN,
            FinalizedReader::new(routes.provider(CHAIN, 0)?.clone()),
            FinalizedReader::new(routes.provider(CHAIN, 1)?.clone()),
        );
        let stats = watch.watch_once(CHAIN).await?;
        ensure!(stats.reversed == 0 && stats.finalized == 1, "{stats:?}");
        let reversed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM deposits WHERE state='reversed'")
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(reversed == 0);
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}

async fn cancel_last_batch<F: std::future::Future>(node: &Node, operation: F) -> Result<()> {
    tokio::pin!(operation);
    tokio::select! {
        _ = node.hanging.notified() => {
            ensure!(node.sends.load(Ordering::SeqCst)>=1,"must read transfers before cancelling the last receipt");
            Ok(())
        }
        _ = &mut operation => anyhow::bail!("window completed before receipt cancellation"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => anyhow::bail!("window never reached the hanging last batch"),
    }
}

async fn batch_scenario(switch: bool, cancel: bool) -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let first = Node::start(2).await?;
        let backup = Node::start(1).await?;
        first
            .mode
            .store(if cancel { 4 } else { 3 }, Ordering::SeqCst);
        // One bounded address page still needs all transfer/receipt/factory evidence from
        // one member before it can commit. The scale test separately covers page boundaries.
        seed(&database.app_pool, 1000).await?;
        let urls = if switch {
            vec![first.url.as_str(), backup.url.as_str()]
        } else {
            vec![first.url.as_str()]
        };
        let policy = GroupPolicy {
            max_attempts: if switch { 2 } else { 1 },
            ..Default::default()
        };
        let a = group("a", &urls, 10000, policy.clone())?;
        let b = group("b", &[&backup.url], 10000, GroupPolicy::default())?;
        install(&database.app_pool, &[a.clone(), b.clone()]).await?;
        let configured = routes(a, b, true)?;
        if cancel {
            cancel_last_batch(&first, scan(&database.app_pool, &configured)).await?;
        } else {
            let outcome = scan(&database.app_pool, &configured).await;
            ensure!(
                outcome.is_ok() == switch,
                "scanner whole-window outcome: {outcome:?}"
            );
        }
        ensure!(
            db::get_cursor(&database.app_pool, CHAIN).await?
                == Some(if switch { HEIGHT } else { HEIGHT - 1 })
        );
        let deposits: i64 = sqlx::query_scalar("SELECT count(*) FROM deposits")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(
            deposits == if switch { 1 } else { 0 },
            "partial scanner evidence must never commit"
        );
        if switch {
            ensure!(
                backup.sends.load(Ordering::SeqCst) >= 2,
                "backup must read the entire window"
            );
        }
        // Exercise the real missing-deposit reconciliation check, starting at the same range.
        sqlx::query("UPDATE cursors SET scanned_block=$1 WHERE chain_id=$2")
            .bind(i64::try_from(HEIGHT)?)
            .bind(i64::try_from(CHAIN)?)
            .execute(&database.app_pool)
            .await?;
        sqlx::query(
            "INSERT INTO reconciliation_deposit_cursors(chain_id,next_block) VALUES($1,$2)",
        )
        .bind(i64::try_from(CHAIN)?)
        .bind(i64::try_from(HEIGHT)?)
        .execute(&database.app_pool)
        .await?;
        first.sends.store(0, Ordering::SeqCst);
        // Fresh clients make the reconciler actually read the failing receipts, rather than
        // merely inheriting the scanner's cooldown and rejecting an empty pool.
        let a = group("a", &urls, 10000, policy)?;
        let b = group("b", &[&backup.url], 10000, GroupPolicy::default())?;
        install(&database.app_pool, &[a.clone(), b.clone()]).await?;
        let configured = routes(a, b, true)?;
        let reconciler =
            topup::reconciler::Reconciler::from_routes(database.app_pool.clone(), configured)?;
        if cancel {
            cancel_last_batch(
                &first,
                reconciler.check(topup::reconciler::CheckName::MissingDeposit),
            )
            .await?;
        } else {
            let outcome = reconciler
                .check(topup::reconciler::CheckName::MissingDeposit)
                .await;
            ensure!(
                outcome.is_ok() == switch,
                "reconciler whole-window outcome: {outcome:?}"
            );
        }
        ensure!(
            first.sends.load(Ordering::SeqCst) >= 1,
            "reconciler must read transfers before verifying the failing receipt"
        );
        let deposits_after: i64 = sqlx::query_scalar("SELECT count(*) FROM deposits")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(
            deposits_after == if switch { 1 } else { 0 },
            "reconciliation must commit zero partial deposits and only backup evidence"
        );
        let next: i64 = sqlx::query_scalar(
            "SELECT next_block FROM reconciliation_deposit_cursors WHERE chain_id=$1",
        )
        .bind(i64::try_from(CHAIN)?)
        .fetch_one(&database.app_pool)
        .await?;
        ensure!(
            next == i64::try_from(if switch { HEIGHT + 1 } else { HEIGHT })?,
            "partial reconciliation progress must never commit"
        );
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn scanner_and_reconciler_do_not_commit_a_failed_last_receipt() -> Result<()> {
    batch_scenario(false, false).await
}
#[tokio::test]
async fn scanner_and_reconciler_switch_by_retrying_the_whole_address_page() -> Result<()> {
    batch_scenario(true, false).await
}
#[tokio::test]
async fn scanner_and_reconciler_cancel_mid_window_without_progress() -> Result<()> {
    batch_scenario(false, true).await
}

#[tokio::test]
async fn changed_latest_hash_keeps_numeric_floor_and_replays_derived_progress() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result=async {
        let node=Node::start(0).await?;seed(&database.app_pool,1).await?;
        let a=group("a",&[&node.url],10000,GroupPolicy::default())?;let b=group("b",&[&node.url],10000,GroupPolicy::default())?;
        install(&database.app_pool,&[a.clone(),b.clone()]).await?;
        let deadline=tokio::time::Instant::now()+Duration::from_secs(5);
        a.head(0,"latest",deadline).await?;
        node.mode.store(5,Ordering::SeqCst);
        a.head(0,"latest",deadline).await?;
        let state=db::rpc::state(&database.app_pool,"test".into());
        ensure!(state.load(CHAIN,"a","latest").await?.context("latest")?.number==HEIGHT);
        let routes=routes(a,b,false)?;
        let reader=FinalizedReader::new(routes.provider(CHAIN,0)?.clone());
        let scanned=scanner::head_scan_once(&database.app_pool,&reader,&chain_routes(&routes)[0]).await?.context("reorg replay")?;
        ensure!(scanned.from_block==1,"must replay affected nonfinal progress");
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM rpc_reorg_ranges WHERE COALESCE(replayed_through,from_block-1)<to_block").fetch_one(&database.app_pool).await?;
        ensure!(pending==0);Ok(())
    }.await;
    database.cleanup().await?;
    result
}

#[tokio::test]
async fn dense_window_deadline_accounts_for_verification_latency_too() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let node = Node::start(50).await?;
        node.mode.store(6, Ordering::SeqCst);
        seed(&database.app_pool, 1).await?;
        let policy = GroupPolicy {
            total_deadline_ms: 1000,
            attempt_timeout_ms: 1000,
            max_attempts: 1,
            ..Default::default()
        };
        let a = group("a", &[&node.url], 1000, policy)?;
        let b = group("b", &[&node.url], 1000, GroupPolicy::default())?;
        install(&database.app_pool, &[a.clone(), b.clone()]).await?;
        let stats = scan(&database.app_pool, routes(a, b, false)?.as_ref()).await?;
        ensure!(
            stats.inserted == 50,
            "network verification must have a work-scaled deadline"
        );
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}

#[tokio::test]
async fn inconsistent_head_window_waits_and_preserves_reorg_replay() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result=async {
        let node=Node::start(0).await?;seed(&database.app_pool,1).await?;
        let a=group("a",&[&node.url],10000,GroupPolicy::default())?;let b=group("b",&[&node.url],10000,GroupPolicy::default())?;
        install(&database.app_pool,&[a.clone(),b.clone()]).await?;
        a.head(0,"latest",tokio::time::Instant::now()+Duration::from_secs(5)).await?;
        node.mode.store(7,Ordering::SeqCst);
        let configured=routes(a,b,false)?;
        let reader=FinalizedReader::new(configured.provider(CHAIN,0)?.clone());
        ensure!(scanner::head_scan_once(&database.app_pool,&reader,&chain_routes(&configured)[0]).await.is_err(),"a stale numeric header must not answer a new-branch window");
        ensure!(db::get_confirmed_cursor(&database.app_pool,CHAIN).await?.is_none());
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM rpc_reorg_ranges WHERE COALESCE(replayed_through,from_block-1)<to_block").fetch_one(&database.app_pool).await?;
        ensure!(pending>0,"failed replay must retain its queued range");
        node.mode.store(5,Ordering::SeqCst);
        let a=group("a",&[&node.url],10000,GroupPolicy::default())?;let b=group("b",&[&node.url],10000,GroupPolicy::default())?;
        install(&database.app_pool,&[a.clone(),b.clone()]).await?;
        let configured=routes(a,b,false)?;
        let reader=FinalizedReader::new(configured.provider(CHAIN,0)?.clone());
        ensure!(scanner::head_scan_once(&database.app_pool,&reader,&chain_routes(&configured)[0]).await?.is_some());
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM rpc_reorg_ranges WHERE COALESCE(replayed_through,from_block-1)<to_block").fetch_one(&database.app_pool).await?;
        ensure!(pending==0);Ok(())
    }.await;
    database.cleanup().await?;
    result
}

async fn production_reorg(safe: bool) -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result=async {
        let node=Node::start(1).await?;node.mode.store(if safe {10}else{8},Ordering::SeqCst);
        seed(&database.app_pool,1).await?;
        let a=group("a",&[&node.url],10000,GroupPolicy::default())?;let b=group("b",&[&node.url],10000,GroupPolicy::default())?;
        install(&database.app_pool,&[a.clone(),b.clone()]).await?;
        let configured=routes_confirmations(a,b,false,if safe {topup_core::route::Confirmations::Safe}else{topup_core::route::Confirmations::Depth(1)})?;
        let reader=FinalizedReader::new(configured.provider(CHAIN,0)?.clone());
        let cancel=tokio_util::sync::CancellationToken::new();
        let task=tokio::spawn(scanner::run_chain(database.app_pool.clone(),reader,chain_routes(&configured)[0].clone(),scanner::ScanConfig {head_poll_interval:Some(Duration::from_millis(10)),finalized_poll_interval:Duration::from_secs(60)},scanner::FinalizedHeads::default(),cancel.clone()));
        let outcome=tokio::time::timeout(Duration::from_secs(10),async {
            loop {if db::get_confirmed_cursor(&database.app_pool,CHAIN).await?==Some(HEIGHT){break;}tokio::time::sleep(Duration::from_millis(10)).await;}
            ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM deposits").fetch_one(&database.app_pool).await?==0);
            node.mode.store(if safe {11}else{9},Ordering::SeqCst);
            loop {
                let count:i64=sqlx::query_scalar("SELECT count(*) FROM deposits").fetch_one(&database.app_pool).await?;
                let pending:i64=sqlx::query_scalar("SELECT count(*) FROM rpc_reorg_ranges WHERE COALESCE(replayed_through,from_block-1)<to_block").fetch_one(&database.app_pool).await?;
                if count==1 && pending==0 {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            ensure!(db::get_confirmed_cursor(&database.app_pool,CHAIN).await?==Some(HEIGHT));
            Ok::<_,anyhow::Error>(())
        }).await;
        cancel.cancel();task.await??;outcome??;Ok(())
    }.await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn production_poll_replays_same_latest_height_and_records_deposit() -> Result<()> {
    production_reorg(false).await
}
#[tokio::test]
async fn production_poll_checks_safe_at_same_latest_height_and_records_deposit() -> Result<()> {
    production_reorg(true).await
}

#[tokio::test]
async fn head_commit_does_not_skip_an_unread_reorg_prefix() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        seed(&database.app_pool, 1).await?;
        sqlx::query(
            "INSERT INTO rpc_reorg_ranges(chain_id,group_id,epoch,from_block,to_block) VALUES($1,'a',0,91,101)",
        )
        .bind(i64::try_from(CHAIN)?)
        .execute(&database.app_pool)
        .await?;
        db::rpc::commit_head_window(&database.app_pool, CHAIN, (100, 100), &[], None, &[], None)
            .await?;
        let through: Option<i64> =
            sqlx::query_scalar("SELECT replayed_through FROM rpc_reorg_ranges")
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(
            through.is_none(),
            "100 cannot cover the unread prefix 91..99"
        );
        db::rpc::commit_head_window(&database.app_pool, CHAIN, (91, 95), &[], None, &[], None)
            .await?;
        db::rpc::commit_head_window(&database.app_pool, CHAIN, (96, 100), &[], None, &[], None)
            .await?;
        let through: i64 = sqlx::query_scalar("SELECT replayed_through FROM rpc_reorg_ranges")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(through == 100);
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn replay_confirmation_cursor_is_capped_at_the_read_window() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let node = Node::start(0).await?;
        seed(&database.app_pool, 1).await?;
        sqlx::query("UPDATE cursors SET confirmed_block=100 WHERE chain_id=$1")
            .bind(i64::try_from(CHAIN)?)
            .execute(&database.app_pool)
            .await?;
        sqlx::query(
            "INSERT INTO rpc_reorg_ranges(chain_id,group_id,epoch,from_block,to_block) VALUES($1,'a',0,1,5000)",
        )
        .bind(i64::try_from(CHAIN)?)
        .execute(&database.app_pool)
        .await?;
        let a = group("a", &[&node.url], 10000, GroupPolicy::default())?;
        let b = group("b", &[&node.url], 10000, GroupPolicy::default())?;
        let configured =
            routes_confirmations(a, b, false, topup_core::route::Confirmations::Depth(1))?;
        let reader = FinalizedReader::new(configured.provider(CHAIN, 0)?.clone());
        // Supply the production scan entry point a distant horizon; the pinned reader
        // rejects a real too-low head, so use a deterministic high node mode below.
        node.mode.store(13, Ordering::SeqCst);
        let scan = scanner::scan_new_blocks(
            &database.app_pool,
            &reader,
            &chain_routes(&configured)[0],
            topup_core::route::ChainHeads {
                latest: Some(5000),
                safe: None,
                finalized: 99,
            },
        )
        .await?
        .context("bounded replay")?;
        ensure!(scan.latest == scanner::MAX_SCAN_WINDOW);
        ensure!(
            db::get_confirmed_cursor(&database.app_pool, CHAIN).await?
                == Some(scanner::MAX_SCAN_WINDOW)
        );
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
#[tokio::test]
async fn dense_factory_window_with_recursive_log_splits_completes_in_scanner() -> Result<()> {
    let Some(database) = support::TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let node = Node::start(0).await?;
        node.mode.store(12, Ordering::SeqCst);
        seed(&database.app_pool, 1).await?;
        sqlx::query("UPDATE cursors SET scanned_block=68 WHERE chain_id=$1")
            .bind(i64::try_from(CHAIN)?)
            .execute(&database.app_pool)
            .await?;
        sqlx::query("UPDATE addresses SET created_block=68 WHERE chain_id=$1")
            .bind(i64::try_from(CHAIN)?)
            .execute(&database.app_pool)
            .await?;
        let policy = GroupPolicy {
            total_deadline_ms: 1000,
            attempt_timeout_ms: 1000,
            max_attempts: 1,
            ..Default::default()
        };
        let a = group("a", &[&node.url], 10, policy)?;
        let b = group("b", &[&node.url], 10000, GroupPolicy::default())?;
        let configured = routes(a, b, false)?;
        tokio::time::timeout(
            Duration::from_secs(60),
            scan(&database.app_pool, &configured),
        )
        .await??;
        ensure!(
            node.sends.load(Ordering::SeqCst) >= 126,
            "both transfer and factory filters must split every block"
        );
        ensure!(db::get_cursor(&database.app_pool, CHAIN).await? == Some(HEIGHT));
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM flushed")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(events == 32, "dense factory events must commit atomically");
        Ok(())
    }
    .await;
    database.cleanup().await?;
    result
}
