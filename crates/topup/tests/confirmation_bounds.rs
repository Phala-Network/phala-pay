//! Controlled-clock budgets through the production EVM readers, pump and watcher.
mod support;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use support::{
    seed::{self, NewAccount, NewAddress},
    with_database,
};
use tokio_util::sync::CancellationToken;
use topup::{
    db::{self, NewDeposit},
    finality::FinalityWatch,
    pump::{Pump, PumpConfig, Step, StepResult, StepSet},
    routes::RouteSet,
    steps::confirm::ConfirmStep,
};
use topup_adapters::{
    chain::evm::{EvmClient, FinalizedReader, TransferLog},
    pricing::{Observation, PriceError, PriceSource},
};
use topup_core::{
    deposit::{DepositState, StepOutcome, WaitReason},
    identity::deposit_id,
    money::{AtomicAmount, PRICE_SCALE, ScaledPrice},
    route::{Confirmations, RouteFile},
    valuation::{SourceId, UnixSeconds},
};
use uuid::Uuid;

fn hash(n: u64) -> B256 {
    B256::from(U256::from(n))
}
fn block(number: u64, timestamp: u64) -> Value {
    json!({"hash":format!("{:#x}",hash(number)),"parentHash":format!("{:#x}",B256::ZERO),"sha3Uncles":format!("{:#x}",B256::ZERO),"miner":format!("{:#x}",Address::ZERO),"stateRoot":format!("{:#x}",B256::ZERO),"transactionsRoot":format!("{:#x}",B256::ZERO),"receiptsRoot":format!("{:#x}",B256::ZERO),"logsBloom":format!("0x{}","00".repeat(256)),"difficulty":"0x0","number":format!("0x{number:x}"),"gasLimit":"0x1c9c380","gasUsed":"0x0","timestamp":format!("0x{timestamp:x}"),"extraData":"0x","mixHash":format!("{:#x}",B256::ZERO),"nonce":"0x0000000000000000","transactions":[],"uncles":[]})
}
#[derive(Clone)]
struct Rpc {
    chain: u64,
    methods: Arc<[AtomicU64; 2]>,
    heads: Arc<[AtomicU64; 2]>,
    checks: Arc<Mutex<[Vec<u64>; 2]>>,
    clock: Arc<AtomicU64>,
    head_time: u64,
    transfer: Arc<Mutex<[Option<TransferLog>; 2]>>,
}
async fn rpc(
    axum::extract::State(s): axum::extract::State<Rpc>,
    axum::extract::Path(e): axum::extract::Path<usize>,
    axum::Json(req): axum::Json<Value>,
) -> axum::Json<Value> {
    s.methods[e].fetch_add(1, Ordering::SeqCst);
    let log = s.transfer.lock().expect("fixture")[e].clone();
    let result = match req["method"].as_str().expect("method") {
        "eth_getBlockByNumber" => {
            let tag = req["params"][0].as_str().expect("tag");
            if let Some(number) = tag.strip_prefix("0x") {
                let n = u64::from_str_radix(number, 16).expect("number");
                block(
                    n,
                    log.as_ref()
                        .map_or(0, |l| l.block_time.timestamp().try_into().expect("time")),
                )
            } else {
                s.checks.lock().expect("checks")[e].push(s.clock.load(Ordering::SeqCst));
                block(s.heads[e].load(Ordering::SeqCst), s.head_time)
            }
        }
        "eth_getBlockByHash" => {
            let l = log.expect("receipt block");
            block(
                l.block_number,
                l.block_time.timestamp().try_into().expect("time"),
            )
        }
        "eth_getTransactionReceipt" => match log {
            None => Value::Null,
            Some(l) => {
                let topic = alloy_primitives::keccak256("Transfer(address,address,uint256)");
                let topics = [
                    format!("{topic:#x}"),
                    format!("{:#x}", B256::from(l.from.into_word())),
                    format!("{:#x}", B256::from(l.to.into_word())),
                ];
                json!({"transactionHash":format!("{:#x}",l.tx_hash),"transactionIndex":"0x0","blockHash":format!("{:#x}",l.block_hash),"blockNumber":format!("0x{:x}",l.block_number),"from":format!("{:#x}",l.tx_from),"to":format!("{:#x}",l.token),"cumulativeGasUsed":"0x5208","gasUsed":"0x5208","effectiveGasPrice":"0x1","contractAddress":null,"logsBloom":format!("0x{}","00".repeat(256)),"status":"0x1","type":"0x0","logs":[{"address":format!("{:#x}",l.token),"topics":topics,"data":format!("0x{:064x}",l.amount.value()),"blockNumber":format!("0x{:x}",l.block_number),"transactionHash":format!("{:#x}",l.tx_hash),"transactionIndex":"0x0","blockHash":format!("{:#x}",l.block_hash),"logIndex":format!("0x{:x}",l.log_index),"removed":false}]})
            }
        },
        "eth_getTransactionByHash" => {
            let l = log.expect("transaction");
            json!({"hash":format!("{:#x}",l.tx_hash),"nonce":"0x0","blockHash":format!("{:#x}",l.block_hash),"blockNumber":format!("0x{:x}",l.block_number),"transactionIndex":"0x0","from":format!("{:#x}",l.tx_from),"to":format!("{:#x}",l.token),"value":"0x0","gas":"0x5208","gasPrice":"0x1","input":"0x","v":format!("0x{:x}",s.chain*2+35),"r":"0x1","s":"0x1","type":"0x0","chainId":format!("0x{:x}",s.chain)})
        }
        method => panic!("unexpected {method}"),
    };
    axum::Json(json!({"jsonrpc":"2.0","id":req["id"],"result":result}))
}
struct Price {
    source: &'static str,
    fail: Arc<AtomicU64>,
}
#[async_trait]
impl PriceSource for Price {
    async fn observe(&self) -> Result<Observation, PriceError> {
        if self.fail.load(Ordering::SeqCst) > 0 {
            return Err(PriceError::RpcUnavailable);
        }
        Ok(Observation {
            source: SourceId::new(self.source),
            price: ScaledPrice::new(100_000_000, PRICE_SCALE).expect("price"),
            observed_at: UnixSeconds::new(Utc::now().timestamp().try_into().expect("now")),
        })
    }
}
struct Paused;
#[async_trait]
impl Step for Paused {
    async fn run(&self, _: &db::Deposit) -> StepResult {
        StepResult::new(
            StepOutcome::Wait {
                reason: WaitReason::Paused,
            },
            json!({"paused":true}),
        )
    }
}
struct Fixture {
    id: Uuid,
    route: RouteFile,
    log: TransferLog,
    due: DateTime<Utc>,
}
async fn fixture(pool: &PgPool, policy: Confirmations, chain: u64) -> Result<Fixture> {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.chain_id = chain;
    route.chain.confirmations = policy;
    if chain == 8453 {
        route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
            feed: "BASE_SEQUENCER_UPTIME".into(),
            grace_s: 3600,
        });
    }
    route.asset.decimals = 2;
    route.asset.quote_amount_decimals = 2;
    route.merchant.min_amount = topup_core::route::Bounded::at(1);
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount::named("bounded confirmation"),
        "confirmation",
    )
    .await?;
    seed::accept_routes(pool, account.id, true, &[&route]).await?;
    let address = NewAddress {
        id: Uuid::new_v4(),
        customer_id: customer.id,
        chain_id: chain,
        route: route.route.clone(),
        salt: hash(1),
        address: Address::repeat_byte(1),
    };
    seed::insert_address(pool, &address).await?;
    let due = DateTime::from_timestamp(2_000_000_000, 0).context("clock")?;
    let log = TransferLog {
        tx_hash: hash(8),
        receipt_log_index: 0,
        log_index: 0,
        block_number: 100,
        block_hash: hash(100),
        block_time: due - Duration::seconds(10),
        tx_from: Address::ZERO,
        tx_nonce: 0,
        token: route.asset.contract,
        from: Address::repeat_byte(2),
        to: address.address,
        amount: AtomicAmount::new(U256::from(1000)),
    };
    let id = deposit_id(chain, log.tx_hash, 0);
    db::insert_deposit(
        pool,
        &NewDeposit {
            chain_id: chain,
            tx_hash: log.tx_hash,
            receipt_log_index: 0,
            log_index: 0,
            tx_from: log.tx_from,
            tx_nonce: 0,
            is_final: false,
            block_number: 100,
            block_hash: log.block_hash,
            block_time: log.block_time,
            address_id: address.id,
            route: Some(route.route.clone()),
            route_version: Some(route.version),
            asset_contract: log.token,
            from_address: log.from,
            amount_atomic: log.amount,
            state: DepositState::Detected,
            reason: None,
            next_attempt_at: due,
        },
    )
    .await?;
    Ok(Fixture {
        id,
        route,
        log,
        due,
    })
}
async fn run_fixture<F>(pool: &PgPool, f: &Fixture, body: F) -> Result<()>
where
    F: AsyncFnOnce(Arc<Pump>, FinalityWatch, Rpc, Arc<AtomicU64>) -> Result<()>,
{
    let state = Rpc {
        chain: f.route.chain.chain_id,
        methods: Arc::new([AtomicU64::new(0), AtomicU64::new(0)]),
        heads: Arc::new([AtomicU64::new(99), AtomicU64::new(97)]),
        checks: Arc::new(Mutex::new([Vec::new(), Vec::new()])),
        clock: Arc::new(AtomicU64::new(f.due.timestamp().try_into()?)),
        head_time: (f.due - Duration::seconds(24)).timestamp().try_into()?,
        transfer: Arc::new(Mutex::new([Some(f.log.clone()), Some(f.log.clone())])),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let app = axum::Router::new()
        .route("/{endpoint}", axum::routing::post(rpc))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    let result = async {
        let reader = |e| -> Result<_> {
            Ok(FinalizedReader::new(Arc::new(EvmClient::new(&format!(
                "http://{addr}/{e}"
            ))?)))
        };
        let fail = Arc::new(AtomicU64::new(0));
        let price = |source| {
            Arc::new(Price {
                source,
                fail: fail.clone(),
            }) as Arc<dyn PriceSource>
        };
        let step = ConfirmStep::single(
            pool.clone(),
            f.route.clone(),
            reader(0)?,
            reader(1)?,
            price("primary"),
            Some(price("check")),
            Some(price("fx")),
        );
        let routes = Arc::new(RouteSet::new(vec![f.route.clone()]).map_err(anyhow::Error::msg)?);
        let pump = Arc::new(Pump::new(
            pool.clone(),
            routes.clone(),
            Arc::new(StepSet::new(Box::new(step), Box::new(Paused))),
            PumpConfig::default(),
        )?);
        let watch = FinalityWatch::single(
            pool.clone(),
            routes,
            f.route.chain.chain_id,
            reader(0)?,
            reader(1)?,
        )
        .with_pump(pump.clone());
        body(pump, watch, state, fail).await
    }
    .await;
    cancel.cancel();
    server.await??;
    result
}
async fn due(pool: &PgPool, id: Uuid) -> Result<DateTime<Utc>> {
    Ok(
        sqlx::query_scalar("SELECT finality_check_at FROM deposits WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}
fn set_clock(r: &Rpc, now: DateTime<Utc>) {
    r.clock
        .store(now.timestamp().try_into().expect("time"), Ordering::SeqCst);
}
fn counts(r: &Rpc) -> [u64; 2] {
    r.methods.each_ref().map(|n| n.load(Ordering::SeqCst))
}

#[tokio::test]
async fn normal_modes_and_slow_lane_have_hard_method_bounds() -> Result<()> {
    for (policy, normal, ceiling) in [
        (Confirmations::Depth(2), 6, 13),
        (Confirmations::Safe, 3, 10),
        (Confirmations::Finalized, 7, 10),
    ] {
        with_database(|ctx|Box::pin(async move {
            let p=&ctx.app_pool;let chain=if policy==Confirmations::Safe {8453} else {1};let f=fixture(p,policy,chain).await?;
            run_fixture(p,&f,async |pump,watch,r,_| {
                let mut now=f.due;
                let positions = match policy {Confirmations::Depth(_)=>vec![0,4,12,28,60,124],Confirmations::Safe=>vec![0,384,768],Confirmations::Finalized=>vec![0,384,768,1152,1536,1920,2304]};
                for index in 0..normal {
                    ensure!(now==f.due+Duration::seconds(positions[usize::try_from(index)?]),"probe position drift");
                    set_clock(&r,now);
                    pump.run_once_at(now).await?;
                    ensure!(watch.watch_once_at(chain,now).await?.watched==0,"watcher claimed normal/L");
                    let history:(i32,Option<DateTime<Utc>>,Option<DateTime<Utc>>)=sqlx::query_as("SELECT confirm_head_checks,first_slow_at,first_unresolved_at FROM deposits WHERE id=$1").bind(f.id).fetch_one(p).await?;
                    ensure!(history.0==index+1 && history.2.is_none(),"height lag entered S: {history:?}");
                    if index==normal-1 {ensure!(history.1==Some(now),"exhaustion did not enter L");}
                    now=due(p,f.id).await?;
                }
                ensure!(counts(&r)==[u64::try_from(normal)?,u64::try_from(normal)?]);
                let entry:DateTime<Utc>=sqlx::query_scalar("SELECT first_slow_at FROM deposits WHERE id=$1").bind(f.id).fetch_one(p).await?;
                ensure!(now==entry+Duration::seconds(60),"L performed an immediate extra check");
                let mut first_day=0;
                while now<entry+Duration::days(1) {
                    set_clock(&r,now);
                    // Both pump calls race with watcher; only one owns this due, including restart.
                    let (a,b,c)=tokio::join!(pump.run_once_at(now),pump.run_once_at(now),watch.watch_once_at(chain,now));a?;b?;ensure!(c?.watched==0);
                    first_day+=1;now=due(p,f.id).await?;
                }
                ensure!(first_day==62,"first L day has {first_day} checks");
                ensure!(counts(&r)==[u64::try_from(normal+62)?;2],"duplicate L reads");
                let before=counts(&r);let start=now;let mut carried=0;
                while now<start+Duration::days(1) {set_clock(&r,now);pump.run_once_at(now).await?;carried+=1;now=due(p,f.id).await?;}
                ensure!(carried==24 && counts(&r)==before.map(|n|n+24));
                for head in r.heads.iter() {head.store(200,Ordering::SeqCst);}
                set_clock(&r,now);pump.run_once_at(now).await?;
                let stored=db::get_deposit(p,f.id).await?.context("deposit")?;
                ensure!(stored.state==DepositState::Confirmed,"L waited for checkpoint or lost proof: {stored:?}");
                let terminal_before=counts(&r);
                db::chain_reads::advance_checkpoint(p,chain,db::chain_reads::Boundary{number:200,hash:hash(200),time:now}).await?;
                let final_check = due(p,f.id).await?;
                watch.watch_once_at(chain,final_check).await?;
                let terminal=counts(&r).into_iter().zip(terminal_before).map(|(a,b)|a-b).max().unwrap_or(0);
                ensure!(if policy==Confirmations::Finalized {terminal==0} else {(1..=4).contains(&terminal)},"first terminal check missing or over budget: {terminal}");
                let total=counts(&r);for n in total {ensure!(n<=ceiling+62+24+1,"method ceiling: {n}");}
                let checks=r.checks.lock().expect("checks");
                for timeline in checks.iter() {let mut heads=timeline.clone(); // receipt shares the ready claim, not a second scheduling probe
                    heads.dedup();ensure!(heads.len()==usize::try_from(normal+62+24+1)?);}
                println!("{policy:?}: normal {normal}, first L day {first_day}, hourly day {carried}; actual methods {total:?}, first terminal {terminal}");
                Ok(())
            }).await
        })).await?;
    }
    Ok(())
}

#[tokio::test]
async fn base_lag_recovers_in_normal_window_without_s_and_normal_method_bounds() -> Result<()> {
    for (policy, probes, limit) in [
        (Confirmations::Depth(2), 6, 13),
        (Confirmations::Safe, 3, 10),
        (Confirmations::Finalized, 7, 10),
    ] {
        with_database(|ctx| {
            Box::pin(async move {
                let p = &ctx.app_pool;
                let f = fixture(p, policy, 8453).await?;
                run_fixture(p, &f, async |pump, watch, r, _| {
                    let mut now = f.due;
                    for index in 0..probes {
                        if index == probes - 1 {
                            for h in r.heads.iter() {
                                h.store(200, Ordering::SeqCst);
                            }
                        }
                        set_clock(&r, now);
                        pump.run_once_at(now).await?;
                        if index < probes - 1 {
                            now = due(p, f.id).await?;
                        }
                    }
                    let stored = db::get_deposit(p, f.id).await?.context("deposit")?;
                    ensure!(
                        stored.state == DepositState::Confirmed,
                        "normal did not confirm: {stored:?}"
                    );
                    let entries: (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
                        "SELECT first_unresolved_at,first_slow_at FROM deposits WHERE id=$1",
                    )
                    .bind(f.id)
                    .fetch_one(p)
                    .await?;
                    ensure!(entries == (None, None));
                    db::chain_reads::advance_checkpoint(
                        p,
                        8453,
                        db::chain_reads::Boundary {
                            number: 200,
                            hash: hash(200),
                            time: now,
                        },
                    )
                    .await?;
                    watch
                        .watch_once_at(8453, now + Duration::seconds(60))
                        .await?;
                    let actual = counts(&r);
                    ensure!(actual.into_iter().all(|n| n <= limit));
                    println!("normal {policy:?}: actual methods {actual:?}, limit {limit}");
                    Ok(())
                })
                .await
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn receipt_anomalies_and_price_failure_keep_watcher_ownership_and_one_quota() -> Result<()> {
    for anomaly in ["missing", "disagreement", "changed", "price"] {
        with_database(|ctx| {
            Box::pin(async move {
                let p = &ctx.app_pool;
                let f = fixture(p, Confirmations::Depth(2), 1).await?;
                run_fixture(p, &f, async |pump, watch, r, fail| {
                    for head in r.heads.iter() {
                        head.store(200, Ordering::SeqCst);
                    }
                    match anomaly {
                        "missing" => *r.transfer.lock().expect("fixture") = [None, None],
                        "disagreement" => {
                            r.transfer.lock().expect("fixture")[1]
                                .as_mut()
                                .expect("transfer")
                                .amount = AtomicAmount::new(U256::from(2000))
                        }
                        "changed" => {
                            for l in r.transfer.lock().expect("fixture").iter_mut().flatten() {
                                l.amount = AtomicAmount::new(U256::from(2000));
                            }
                        }
                        "price" => fail.store(1, Ordering::SeqCst),
                        _ => unreachable!(),
                    }
                    pump.run_once_at(f.due).await?;
                    let entry: Option<DateTime<Utc>> =
                        sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
                            .bind(f.id)
                            .fetch_one(p)
                            .await?;
                    ensure!(entry == Some(f.due), "{anomaly} did not enter S");
                    ensure!(
                        pump.run_once_at(f.due + Duration::minutes(10)).await?
                            == topup::pump::RunOnceResult::Idle
                    );
                    // Reappearance above the checkpoint remains S; it never hands back to the pump.
                    *r.transfer.lock().expect("fixture") =
                        [Some(f.log.clone()), Some(f.log.clone())];
                    db::chain_reads::advance_checkpoint(
                        p,
                        1,
                        db::chain_reads::Boundary {
                            number: 90,
                            hash: hash(90),
                            time: f.due,
                        },
                    )
                    .await?;
                    let next = due(p, f.id).await?;
                    set_clock(&r, next);
                    ensure!(watch.watch_once_at(1, next).await?.watched == 1);
                    ensure!(
                        pump.run_once_at(next + Duration::seconds(2)).await?
                            == topup::pump::RunOnceResult::Idle
                    );
                    fail.store(1, Ordering::SeqCst);
                    db::chain_reads::advance_checkpoint(
                        p,
                        1,
                        db::chain_reads::Boundary {
                            number: 200,
                            hash: hash(200),
                            time: next,
                        },
                    )
                    .await?;
                    let next = due(p, f.id).await?;
                    set_clock(&r, next);
                    watch.watch_once_at(1, next).await?;
                    let stored = db::get_deposit(p, f.id).await?.context("price retry")?;
                    ensure!(stored.final_at.is_some() && stored.state == DepositState::Detected);
                    let before = counts(&r);
                    fail.store(0, Ordering::SeqCst);
                    set_clock(&r, stored.next_attempt_at);
                    pump.run_once_at(stored.next_attempt_at).await?;
                    ensure!(counts(&r) == before, "price retry re-read receipt");
                    ensure!(
                        db::get_deposit(p, f.id).await?.context("confirmed")?.state
                            == DepositState::Confirmed
                    );
                    let quota: i32 = sqlx::query_scalar(
                        "SELECT confirm_receipt_checks FROM deposits WHERE id=$1",
                    )
                    .bind(f.id)
                    .fetch_one(p)
                    .await?;
                    ensure!(quota == 1);
                    Ok(())
                })
                .await
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn missed_positions_restart_crash_and_stale_claims_never_reset_budgets() -> Result<()> {
    with_database(|ctx|Box::pin(async move {
        use db::confirmation::{self, Reader};
        let p=&ctx.app_pool;let f=fixture(p,Confirmations::Depth(2),1).await?;
        let token=Uuid::new_v4();
        let first=confirmation::claim_read(p,f.id,token,Reader::Confirm(Confirmations::Depth(2)),f.due).await?.context("first claim")?;
        ensure!(first.head_checks==1);
        confirmation::finish_probe(p,f.id,token,first,Confirmations::Depth(2),f.due,false,f.due).await?;
        ensure!(due(p,f.id).await?==f.due+Duration::seconds(4));
        ensure!(confirmation::claim_read(p,f.id,Uuid::new_v4(),Reader::Confirm(Confirmations::Depth(2)),f.due+Duration::seconds(4)).await?.is_none(),"live lease ignored");
        db::release_deposit_lease(p,f.id,token).await?;
        let token=Uuid::new_v4();let now=f.due+Duration::seconds(60);
        let resumed=confirmation::claim_read(p,f.id,token,Reader::Confirm(Confirmations::Depth(2)),now).await?.context("restart")?;
        ensure!(resumed.head_checks==5,"restart replayed missed positions");
        ensure!(resumed.deadline==Some(f.due+Duration::seconds(124)),"deadline extended");
        ensure!(confirmation::finish_probe(p,f.id,token,resumed,Confirmations::Depth(2),now,false,now).await?);
        ensure!(due(p,f.id).await?==f.due+Duration::seconds(124));
        ensure!(!confirmation::reserve_receipt(p,f.id,Uuid::new_v4(),resumed.version).await?);
        // A different record version invalidates writes even if the token was retained.
        sqlx::query("UPDATE deposits SET updated_at=updated_at+interval '1 second' WHERE id=$1").bind(f.id).execute(p).await?;
        ensure!(!confirmation::reserve_receipt(p,f.id,token,resumed.version).await?);
        db::release_deposit_lease(p,f.id,token).await?;
        let token=Uuid::new_v4();let now=f.due+Duration::seconds(124);
        let last=confirmation::claim_read(p,f.id,token,Reader::Confirm(Confirmations::Depth(2)),now).await?.context("last")?;
        ensure!(last.head_checks==6);
        // Crash after receipt reservation: a new process cannot consume it again or open L.
        ensure!(confirmation::reserve_receipt(p,f.id,token,last.version).await?);
        ensure!(!confirmation::reserve_receipt(p,f.id,token,last.version).await?);
        let after=now+Duration::minutes(5);
        ensure!(confirmation::claim_read(p,f.id,Uuid::new_v4(),Reader::Confirm(Confirmations::Depth(2)),after).await?.is_none());
        ensure!(confirmation::claim_read(p,f.id,Uuid::new_v4(),Reader::Watcher,after).await?.is_some(),"crash permanently excluded detected row");
        let history:(i32,i32,DateTime<Utc>)=sqlx::query_as("SELECT confirm_head_checks,confirm_receipt_checks,confirm_deadline_at FROM deposits WHERE id=$1").bind(f.id).fetch_one(p).await?;
        ensure!(history==(6,1,f.due+Duration::seconds(124)));Ok(())
    })).await
}

#[tokio::test]
async fn slow_gauges_are_db_derived_once_only_and_keep_resolved_entries() -> Result<()> {
    with_database(|ctx|Box::pin(async move {
        let p=&ctx.app_pool;let first=fixture(p,Confirmations::Depth(2),1).await?;
        sqlx::query("UPDATE deposits SET first_slow_at=$2,confirm_head_checks=6 WHERE id=$1").bind(first.id).bind(first.due).execute(p).await?;
        let count=async |now| -> Result<(u64,u64)> {
            let text=topup::observability::metrics::render_chain_reads_at(p,now).await?;
            let value=|name|->Result<u64>{text.lines().find_map(|l|l.strip_prefix(&format!("{name}{{chain_id=\"1\"}} "))).context("gauge")?.parse().map_err(Into::into)};
            Ok((value("topup_confirmation_slow")?,value("topup_confirmation_slow_entries_24h")?))
        };
        ensure!(count(first.due).await?==(1,1));
        let second_id=Uuid::new_v4();
        // A second received payment remains represented; the cap is an operational stop rule.
        sqlx::query("INSERT INTO deposits SELECT (jsonb_populate_record(NULL::deposits,to_jsonb(d)||jsonb_build_object('id',$2::text,'tx_hash',$3))).* FROM deposits d WHERE id=$1")
            .bind(first.id).bind(second_id).bind(format!("{:#x}",hash(9))).execute(p).await?;
        ensure!(count(first.due).await?==(2,2));
        let rules=include_str!("../../../deploy/rpc-alerts.yaml");
        for name in ["topup_confirmation_slow","topup_confirmation_slow_entries_24h"] {ensure!(rules.contains(&format!("expr: sum({name}) > 1")));}
        ensure!(rules.contains("expr: sum(topup_confirmation_slow) > 0\n        for: 10m"));
        let token=Uuid::new_v4();let now=first.due+Duration::seconds(60);
        let claim=db::confirmation::claim_read(p,first.id,token,db::confirmation::Reader::Confirm(Confirmations::Depth(2)),now).await?.context("L recheck")?;
        db::confirmation::finish_probe(p,first.id,token,claim,Confirmations::Depth(2),now,false,now).await?;
        ensure!(count(now).await?==(2,2),"recheck double-counted or overwrote entry");
        sqlx::query("UPDATE deposits SET state='confirmed' WHERE id=$1").bind(first.id).execute(p).await?;
        ensure!(count(now).await?==(1,2),"resolved entry lost");
        // Fresh metric collectors read the same DB after all worker objects have gone.
        ensure!(count(now).await?==(1,2));
        ensure!(count(first.due+Duration::days(1)).await?==(1,0),"24h boundary not strict");Ok(())
    })).await
}

#[tokio::test]
async fn nonterminal_price_failure_then_reorg_reverses_with_atomic_successor() -> Result<()> {
    with_database(|ctx|Box::pin(async move {
        let p=&ctx.app_pool;let f=fixture(p,Confirmations::Depth(2),1).await?;
        sqlx::query("UPDATE quotes SET exposure_reserved=true WHERE id=(SELECT quote_id FROM addresses a JOIN deposits d ON d.address_id=a.id WHERE d.id=$1)").bind(f.id).execute(p).await?;
        let customer:Uuid=sqlx::query_scalar("SELECT customer_id FROM deposits WHERE id=$1").bind(f.id).fetch_one(p).await?;
        let successor_address=NewAddress {id:Uuid::new_v4(),customer_id:customer,chain_id:1,route:f.route.route.clone(),salt:hash(3),address:Address::repeat_byte(3)};
        seed::insert_address(p,&successor_address).await?;
        run_fixture(p,&f,async |pump,watch,r,fail| {
            for h in r.heads.iter(){h.store(200,Ordering::SeqCst);}
            fail.store(1,Ordering::SeqCst);pump.run_once_at(f.due).await?;
            ensure!(db::get_deposit(p,f.id).await?.context("deposit")?.final_at.is_none());
            let mut changed=f.log.clone();changed.to=successor_address.address;
            *r.transfer.lock().expect("fixture")=[Some(changed.clone()),Some(changed)];
            db::chain_reads::advance_checkpoint(p,1,db::chain_reads::Boundary {number:200,hash:hash(200),time:f.due}).await?;
            let next=due(p,f.id).await?;set_clock(&r,next);ensure!(watch.watch_once_at(1,next).await?.reversed==1);
            ensure!(db::get_deposit(p,f.id).await?.context("reversed")?.state==DepositState::Reversed);
            let successor:Uuid=sqlx::query_scalar("SELECT id FROM deposits WHERE replaces=$1").bind(f.id).fetch_one(p).await?;
            let new=db::get_deposit(p,successor).await?.context("successor")?;
            ensure!(new.address_id==successor_address.id && new.final_at.is_some());
            let receipt_checks:i32=sqlx::query_scalar("SELECT confirm_receipt_checks FROM deposits WHERE id=$1").bind(successor).fetch_one(p).await?;
            ensure!(receipt_checks==1,"successor reopened its complete receipt allowance");
            let reserved:bool=sqlx::query_scalar("SELECT q.exposure_reserved FROM quotes q JOIN addresses a ON a.quote_id=q.id JOIN deposits d ON d.address_id=a.id WHERE d.id=$1").bind(f.id).fetch_one(p).await?;
            ensure!(reserved,"reversal released reservation");
            let before=counts(&r);fail.store(0,Ordering::SeqCst);
            let retry=db::get_deposit(p,successor).await?.context("retry")?.next_attempt_at;
            pump.run_once_at(retry).await?;
            ensure!(counts(&r)==before,"successor reread terminal receipt");
            ensure!(db::get_deposit(p,successor).await?.context("confirmed successor")?.state==DepositState::Confirmed);
            Ok(())
        }).await
    })).await
}
