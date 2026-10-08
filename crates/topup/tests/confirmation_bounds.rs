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
    head_fault: Arc<Mutex<[u8; 2]>>,
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
                let fault = s.head_fault.lock().expect("head fault")[e];
                if fault == 2 {
                    return axum::Json(json!({"jsonrpc":"2.0","id":req["id"],
                        "error":{"code":-32603,"message":"fixture head failure"}}));
                }
                let mut header = block(s.heads[e].load(Ordering::SeqCst), s.head_time);
                if fault == 1 {
                    header["hash"] = json!(format!("{:#x}", B256::ZERO));
                }
                header
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
    let due = DateTime::from_timestamp(1_700_000_000, 0).context("clock")?;
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
    run_fixture_with_step(pool, f, None, body).await
}
async fn run_fixture_with_step<F>(
    pool: &PgPool,
    f: &Fixture,
    step: Option<Box<dyn Step>>,
    body: F,
) -> Result<()>
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
        head_fault: Arc::new(Mutex::new([0; 2])),
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
        let confirm = ConfirmStep::single(
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
            Arc::new(StepSet::new(
                step.unwrap_or_else(|| Box::new(confirm)),
                Box::new(Paused),
            )),
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
fn terminal_proof(log: &TransferLog) -> topup::steps::confirm::ConfirmationEvidence {
    let receipt = topup_adapters::chain::evm::ReceiptLookup::Included {
        block_number: log.block_number,
        block_hash: log.block_hash,
        block_time: log.block_time,
        status: true,
        tx_from: log.tx_from,
        tx_nonce: log.tx_nonce,
        transfer: Some(Box::new(log.clone())),
    };
    topup::steps::confirm::ConfirmationEvidence {
        policy: Confirmations::Finalized,
        terminal: true,
        heads: [topup_core::route::ChainHeads {
            finalized: 200,
            ..Default::default()
        }; 2],
        receipts: [receipt.clone(), receipt],
    }
}

async fn terminal_reference(pool: &PgPool, id: Uuid) -> Result<(Uuid, DateTime<Utc>)> {
    let (proof, marker, created): (Uuid, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT d.confirmation_terminal_transition_id,d.final_at,t.created_at FROM deposits d \
         JOIN transitions t ON t.id=d.confirmation_terminal_transition_id WHERE d.id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    ensure!(
        marker == created,
        "proof and final marker were not established together"
    );
    Ok((proof, marker))
}

async fn due(pool: &PgPool, id: Uuid) -> Result<DateTime<Utc>> {
    Ok(
        sqlx::query_scalar("SELECT finality_check_at FROM deposits WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}
async fn pump_due(pool: &PgPool, id: Uuid) -> Result<DateTime<Utc>> {
    Ok(sqlx::query_scalar(
        "SELECT GREATEST(finality_check_at,next_attempt_at) FROM deposits WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
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
                    let before=counts(&r);
                    let (a,b,c)=tokio::join!(pump.run_once_at(now),pump.run_once_at(now),watch.watch_once_at(chain,now));a?;b?;ensure!(c?.watched==0);
                    if counts(&r)!=before {ensure!(counts(&r)==before.map(|n|n+1),"duplicate L reads");first_day+=1;}
                    // SKIP LOCKED can defer an otherwise due claim without any RPC. Advance
                    // through both persisted gates rather than repeating the same fake time.
                    let next=pump_due(p,f.id).await?;ensure!(next>now,"L simulation did not advance");now=next;
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
        let admitted=confirmation::claim_read(p,f.id,Uuid::new_v4(),Reader::Watcher,after).await?;
        ensure!(admitted.is_some(),"crash permanently excluded detected row");
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
        sqlx::query("UPDATE deposits SET final_at=$2 WHERE id=$1").bind(first.id).bind(now).execute(p).await?;
        ensure!(count(now).await?==(1,2),"final detected row still occupied L stock");
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

#[tokio::test]
async fn watcher_changed_identity_never_confirms_or_values_original() -> Result<()> {
    for field in ["token", "from", "amount"] {
        with_database(|ctx| Box::pin(async move {
            let p = &ctx.app_pool;
            let f = fixture(p, Confirmations::Depth(2), 1).await?;
            sqlx::query("UPDATE deposits SET first_unresolved_at=$2,confirm_receipt_checks=1,finality_check_at=$2 WHERE id=$1")
                .bind(f.id).bind(f.due).execute(p).await?;
            run_fixture(p, &f, async |_, watch, r, _| {
                let mut changed = f.log.clone();
                match field {
                    "token" => changed.token = Address::repeat_byte(9),
                    "from" => changed.from = Address::repeat_byte(9),
                    "amount" => changed.amount = AtomicAmount::new(U256::from(2_000)),
                    _ => unreachable!(),
                }
                *r.transfer.lock().expect("fixture") = [Some(changed.clone()), Some(changed)];
                for head in r.heads.iter() {head.store(200, Ordering::SeqCst);}
                // Changed identity above the checkpoint stays in S, with no valuation.
                let first = watch.watch_once_at(1, f.due).await?;
                ensure!(first.watched == 1 && first.finalized == 0 && first.reversed == 0, "{field}");
                let original = db::get_deposit(p, f.id).await?.context("original")?;
                ensure!(original.state == DepositState::Detected && original.valuation_at.is_none(), "{field}");
                db::chain_reads::advance_checkpoint(p, 1, db::chain_reads::Boundary {
                    number: 200, hash: hash(200), time: f.due,
                }).await?;
                let next = due(p, f.id).await?;
                set_clock(&r, next);
                let final_check = watch.watch_once_at(1, next).await?;
                ensure!(final_check.reversed == 1 && final_check.finalized == 0, "changed {field} bypassed reversal");
                let original = db::get_deposit(p, f.id).await?.context("original")?;
                ensure!(original.state == DepositState::Reversed && original.valuation_at.is_none(), "{field}");
                let handed: i64 = sqlx::query_scalar("SELECT count(*) FROM transitions WHERE deposit_id=$1 AND evidence ? 'chain_confirmation'")
                    .bind(f.id).fetch_one(p).await?;
                ensure!(handed == 0, "changed {field} handed to confirm");
                Ok(())
            }).await
        })).await?;
    }
    Ok(())
}

#[tokio::test]
async fn old_terminal_proof_is_retired_on_s_and_only_fresh_watcher_evidence_is_reused() -> Result<()>
{
    for legacy in [false, true] {
        with_database(|ctx| Box::pin(async move {
        let p = &ctx.app_pool;
        let mut f = fixture(p, Confirmations::Depth(2), 1).await?;
        // The controlled scheduling clock is in the past so DB-created proofs are fresh.
        f.due = DateTime::from_timestamp(Utc::now().timestamp(), 0)
            .context("RPC second precision clock")? - Duration::hours(1);
        f.log.block_time = f.due - Duration::seconds(10);
        let proof = terminal_proof(&f.log);
        let old = Uuid::new_v4();
        sqlx::query("INSERT INTO transitions (id,deposit_id,from_state,to_state,attempt,evidence,created_at) VALUES ($1,$2,'detected','detected',0,$3,$4)")
            .bind(old).bind(f.id).bind(json!({"chain_confirmation": proof,"confirmation_proof_version":1}))
            .bind(f.due-Duration::hours(1)).execute(p).await?;
        sqlx::query("UPDATE deposits SET block_time=$2,final_at=$3,next_attempt_at=$3,finality_check_at=NULL,first_unresolved_at=CASE WHEN $5 THEN NULL ELSE $3 END,confirmation_terminal_transition_id=CASE WHEN $5 THEN NULL ELSE $4 END WHERE id=$1")
            .bind(f.id).bind(f.log.block_time).bind(f.due).bind(old).bind(legacy).execute(p).await?;
        run_fixture(p, &f, async |pump, watch, r, fail| {
            *r.transfer.lock().expect("fixture") = [None, None];
            ensure!(pump.run_once_at(f.due).await? == topup::pump::RunOnceResult::Idle, "pump reused old-final proof before due watcher");
            ensure!(counts(&r) == [0, 0]);
            // No checkpoint exists: the old final marker must still enter S.
            let first = watch.watch_once_at(1, f.due).await?;
            ensure!(first.watched == 1 && first.finalized == 0 && first.reversed == 0);
            let entry: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
                .bind(f.id).fetch_one(p).await?;
            ensure!(entry == Some(f.due), "old-final row did not enter S");
            let cached: Option<Value> = sqlx::query_scalar("SELECT confirmation_terminal_evidence($1)")
                .bind(f.id).fetch_one(p).await?;
            ensure!(cached.is_none(), "old proof survived S admission");
            let gauges = topup::observability::metrics::render_chain_reads_at(p, f.due).await?;
            ensure!(gauges.contains("topup_finality_unresolved{chain_id=\"1\"} 1"));
            ensure!(gauges.contains("topup_finality_unresolved_entries_24h{chain_id=\"1\"} 1"));
            ensure!(counts(&r) == [1,1], "first absent check methods");
            let before = counts(&r);
            ensure!(pump.run_once_at(f.due+Duration::seconds(1)).await? == topup::pump::RunOnceResult::Idle);
            ensure!(counts(&r) == before);
            let next = due(p, f.id).await?;
            ensure!(pump.run_once_at(next).await? == topup::pump::RunOnceResult::Idle, "pump stole S with stale proof");
            let mut reincluded = f.log.clone();
            reincluded.block_number = 101; reincluded.block_hash = hash(101);
            reincluded.block_time += Duration::seconds(1); reincluded.log_index = 1;
            *r.transfer.lock().expect("fixture") = [Some(reincluded.clone()), Some(reincluded.clone())];
            for head in r.heads.iter() {head.store(200, Ordering::SeqCst);}
            db::chain_reads::advance_checkpoint(p, 1, db::chain_reads::Boundary {
                number: 200, hash: hash(200), time: next,
            }).await?;
            fail.store(1, Ordering::SeqCst); set_clock(&r, next);
            ensure!(watch.watch_once_at(1, next).await?.finalized == 1);
            let current = db::get_deposit(p, f.id).await?.context("price retry")?;
            ensure!(current.state == DepositState::Detected && current.block_number == 101 && current.valuation_at.is_none());
            let fresh: Value = sqlx::query_scalar("SELECT confirmation_terminal_evidence($1)")
                .bind(f.id).fetch_one(p).await?;
            let fresh: topup::steps::confirm::ConfirmationEvidence = serde_json::from_value(fresh)?;
            ensure!(fresh.receipts[0].transfer() == Some(&reincluded));
            ensure!(counts(&r) == [4,4], "legacy/S handoff reread or exceeded methods");
            let reference = terminal_reference(p,f.id).await?;
            ensure!(reference.0 != old, "fresh proof reused old reference");
            let before = counts(&r); fail.store(0, Ordering::SeqCst);
            pump.run_once_at(current.next_attempt_at).await?;
            ensure!(counts(&r) == before, "fresh handoff re-read receipt");
            ensure!(terminal_reference(p,f.id).await? == reference, "price retry replaced its proof");
            ensure!(db::get_deposit(p, f.id).await?.context("confirmed")?.state == DepositState::Confirmed);
            ensure!(sqlx::query_scalar::<_, Option<DateTime<Utc>>>("SELECT first_unresolved_at FROM deposits WHERE id=$1").bind(f.id).fetch_one(p).await? == entry);
            Ok(())
        }).await
    })).await?;
    }
    Ok(())
}

#[tokio::test]
async fn terminal_proof_freshness_boundary_is_inclusive() -> Result<()> {
    with_database(|ctx| Box::pin(async move {
        let p = &ctx.app_pool;
        let f = fixture(p, Confirmations::Depth(2), 1).await?;
        sqlx::query("UPDATE deposits SET first_unresolved_at=$2 WHERE id=$1")
            .bind(f.id).bind(f.due).execute(p).await?;
        for (created, fresh) in [(f.due-Duration::microseconds(1), false), (f.due, true)] {
            let proof = Uuid::new_v4();
            let mut tx = p.begin().await?;
            sqlx::query("INSERT INTO transitions (id,deposit_id,from_state,to_state,attempt,evidence,created_at) VALUES ($1,$2,'detected','detected',0,$3,$4)")
                .bind(proof).bind(f.id).bind(json!({"chain_confirmation":terminal_proof(&f.log),"confirmation_proof_version":1}))
                .bind(created).execute(&mut *tx).await?;
            sqlx::query("UPDATE deposits SET final_at=now(),confirmation_terminal_transition_id=$2 WHERE id=$1")
                .bind(f.id).bind(proof).execute(&mut *tx).await?;
            tx.commit().await?;
            let present: bool = sqlx::query_scalar("SELECT confirmation_terminal_evidence($1) IS NOT NULL")
                .bind(f.id).fetch_one(p).await?;
            ensure!(present == fresh, "freshness boundary");
        }
        Ok(())
    })).await
}

#[tokio::test]
async fn terminal_price_retry_keeps_the_normal_method_ceiling() -> Result<()> {
    with_database(|ctx| {
        Box::pin(async move {
            let p = &ctx.app_pool;
            let f = fixture(p, Confirmations::Finalized, 1).await?;
            run_fixture(p, &f, async |pump, watch, r, fail| {
                fail.store(1, Ordering::SeqCst);
                let ready_at = f.due + Duration::seconds(6 * 384);
                for position in 0..7 {
                    let now = f.due + Duration::seconds(position * 384);
                    if position == 6 {
                        for h in r.heads.iter() {
                            h.store(200, Ordering::SeqCst);
                        }
                    }
                    set_clock(&r, now);
                    pump.run_once_at(now).await?;
                }
                let stored = db::get_deposit(p, f.id).await?.context("price retry")?;
                ensure!(stored.state == DepositState::Detected && stored.final_at.is_some());
                db::chain_reads::advance_checkpoint(
                    p,
                    1,
                    db::chain_reads::Boundary {
                        number: 200,
                        hash: hash(200),
                        time: f.due,
                    },
                )
                .await?;
                let before = counts(&r);
                ensure!(before == [10, 10] && before.iter().all(|n| *n <= 10));
                let reference = terminal_reference(p, f.id).await?;
                for elapsed in [60, 600, 21_600, 86_400] {
                    let now = ready_at + Duration::seconds(elapsed);
                    set_clock(&r, now);
                    ensure!(
                        watch.watch_once_at(1, now).await?.watched == 0,
                        "normal terminal proof handed to watcher"
                    );
                    pump.run_once_at(now).await?;
                    ensure!(
                        counts(&r) == before,
                        "normal terminal price retry added RPC"
                    );
                    ensure!(
                        terminal_reference(p, f.id).await? == reference,
                        "price retry replaced its proof or marker"
                    );
                }
                fail.store(0, Ordering::SeqCst);
                let retry = db::get_deposit(p, f.id)
                    .await?
                    .context("retry")?
                    .next_attempt_at;
                pump.run_once_at(retry).await?;
                ensure!(
                    counts(&r) == before
                        && db::get_deposit(p, f.id).await?.context("confirmed")?.state
                            == DepositState::Confirmed
                );
                Ok(())
            })
            .await
        })
    })
    .await
}

#[tokio::test]
async fn n_minus_one_final_marker_requires_one_fresh_watcher_proof_then_zero_retry_reads()
-> Result<()> {
    with_database(|ctx| {
        Box::pin(async move {
            let p = &ctx.app_pool;
            let f = fixture(p, Confirmations::Finalized, 1).await?;
            run_fixture(p, &f, async |pump, watch, r, fail| {
                for h in r.heads.iter() {
                    h.store(200, Ordering::SeqCst);
                }
                fail.store(1, Ordering::SeqCst);
                pump.run_once_at(f.due).await?;
                let original = terminal_reference(p, f.id).await?;
                // This is the N-1 write: it does not know or set the new reference column.
                sqlx::query(
                    "UPDATE deposits SET final_at=final_at+interval '1 second' WHERE id=$1",
                )
                .bind(f.id)
                .execute(p)
                .await?;
                let cached: Option<Value> =
                    sqlx::query_scalar("SELECT confirmation_terminal_evidence($1)")
                        .bind(f.id)
                        .fetch_one(p)
                        .await?;
                ensure!(
                    cached.is_none(),
                    "N-1 marker retained the new path's old proof"
                );
                let next = due(p, f.id).await?;
                ensure!(pump.run_once_at(next).await? == topup::pump::RunOnceResult::Idle);
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
                let before = counts(&r);
                set_clock(&r, next);
                ensure!(watch.watch_once_at(1, next).await?.finalized == 1);
                ensure!(
                    counts(&r) == before.map(|n| n + 3),
                    "legacy marker needs exactly one full watcher read"
                );
                let fresh = terminal_reference(p, f.id).await?;
                ensure!(fresh.0 != original.0);
                let before = counts(&r);
                for elapsed in [60, 600, 21_600, 86_400] {
                    let now = next + Duration::seconds(elapsed);
                    ensure!(watch.watch_once_at(1, now).await?.watched == 0);
                    pump.run_once_at(now).await?;
                    ensure!(counts(&r) == before && terminal_reference(p, f.id).await? == fresh);
                }
                fail.store(0, Ordering::SeqCst);
                let retry = db::get_deposit(p, f.id)
                    .await?
                    .context("retry")?
                    .next_attempt_at;
                pump.run_once_at(retry).await?;
                ensure!(
                    counts(&r) == before
                        && db::get_deposit(p, f.id).await?.context("confirmed")?.state
                            == DepositState::Confirmed
                );
                Ok(())
            })
            .await
        })
    })
    .await
}

#[tokio::test]
async fn terminal_reference_requires_new_schema_and_complete_terminal_identity() -> Result<()> {
    with_database(|ctx| Box::pin(async move {
        let p=&ctx.app_pool;let f=fixture(p,Confirmations::Finalized,1).await?;
        for missing in ["version","terminal","to","token","from","amount","tx_from","tx_nonce","valid"] {
            for endpoint in 0..2 {
                let mut evidence=json!({"chain_confirmation":terminal_proof(&f.log),"confirmation_proof_version":1});
                if missing == "version" {evidence.as_object_mut().context("evidence")?.remove("confirmation_proof_version");}
                else if missing == "terminal" {evidence["chain_confirmation"]["terminal"]=json!(false);}
                else if missing != "valid" {evidence["chain_confirmation"]["receipts"][endpoint]["Included"]["transfer"].as_object_mut().context("identity")?.remove(missing);}
                let proof=Uuid::new_v4();let mut tx=p.begin().await?;
                sqlx::query("INSERT INTO transitions (id,deposit_id,from_state,to_state,attempt,evidence) VALUES ($1,$2,'detected','detected',0,$3)")
                    .bind(proof).bind(f.id).bind(evidence).execute(&mut *tx).await?;
                sqlx::query("UPDATE deposits SET final_at=now(),confirmation_terminal_transition_id=$2 WHERE id=$1")
                    .bind(f.id).bind(proof).execute(&mut *tx).await?;
                tx.commit().await?;
                let present:bool=sqlx::query_scalar("SELECT confirmation_terminal_evidence($1) IS NOT NULL").bind(f.id).fetch_one(p).await?;
                ensure!(present==(missing=="valid"),"incomplete {missing} on endpoint {endpoint} was reusable");
            }
        }
        Ok(())
    })).await
}

#[tokio::test]
async fn confirmation_boundary_conflict_freezes_even_when_peer_errors() -> Result<()> {
    for coverage in [false, true] {
        for conflict_endpoint in 0..2 {
            with_database(|ctx| Box::pin(async move {
                let p = &ctx.app_pool;
                let f = fixture(p, Confirmations::Finalized, 1).await?;
                let boundary = db::chain_reads::Boundary {number:200,hash:hash(200),time:f.due};
                db::chain_reads::advance_checkpoint(p, 1, boundary).await?;
                if coverage {
                    sqlx::query("INSERT INTO chain_coverage(chain_id,through_block,through_hash,through_time) VALUES(1,200,$1,$2)")
                        .bind(format!("{:#x}",hash(200))).bind(f.due).execute(p).await?;
                    sqlx::query("UPDATE chain_checkpoints SET block_number=199,block_hash=$1 WHERE chain_id=1")
                        .bind(format!("{:#x}",hash(199))).execute(p).await?;
                }
                run_fixture(p, &f, async |pump, _, r, _| {
                    for head in r.heads.iter() {head.store(200,Ordering::SeqCst);}
                    let mut faults = [2; 2]; faults[conflict_endpoint] = 1;
                    *r.head_fault.lock().expect("faults") = faults;
                    // Persistence can be refused by the newly frozen chain; the conflict
                    // must already be durable before ordinary RPC failure is handled.
                    let _ = pump.run_once_at(f.due).await;
                    let reason: String = sqlx::query_scalar("SELECT check_name FROM reconciliation_blocks WHERE chain_id=1")
                        .fetch_one(p).await?;
                    ensure!(reason == "finalized_checkpoint_conflict");
                    let deposit = db::get_deposit(p,f.id).await?.context("deposit")?;
                    ensure!(deposit.state == DepositState::Detected && deposit.final_at.is_none() && deposit.valuation_at.is_none());
                    let receipts: i32 = sqlx::query_scalar("SELECT confirm_receipt_checks FROM deposits WHERE id=$1")
                        .bind(f.id).fetch_one(p).await?;
                    ensure!(receipts == 0, "conflict allowed full evidence reads");
                    ensure!(counts(&r)[conflict_endpoint] == 1 && counts(&r)[1-conflict_endpoint] >= 1);
                    Ok(())
                }).await
            })).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn held_terminal_evidence_survives_business_waits_without_further_chain_reads() -> Result<()>
{
    with_database(|ctx| Box::pin(async move {
        let p = &ctx.app_pool;
        let f = fixture(p,Confirmations::Finalized,1).await?;
        let restore = Uuid::new_v4();
        sqlx::query("INSERT INTO restores(id,detected_by,timeline_id,unfrozen_at,unfrozen_by,unfreeze_reason) VALUES($1,'restore_check',1,now(),'fixture','reconciled')")
            .bind(restore).execute(p).await?;
        sqlx::query("UPDATE deposits SET settings_revision_id=NULL,settings_hold_id=$2,final_at=$3 WHERE id=$1")
            .bind(f.id).bind(restore).bind(f.due).execute(p).await?;
        db::chain_reads::advance_checkpoint(p,1,db::chain_reads::Boundary {number:200,hash:hash(200),time:f.due}).await?;
        run_fixture(p,&f,async |pump,watch,r,_| {
            for head in r.heads.iter() {head.store(200,Ordering::SeqCst);}
            let first = watch.watch_once_at(1,f.due).await?;
            ensure!(first.watched == 1 && first.finalized == 1);
            let reference = terminal_reference(p,f.id).await?;
            let stored = db::get_deposit(p,f.id).await?.context("held deposit")?;
            ensure!(stored.state == DepositState::Detected
                && stored.valuation_at.is_none() && stored.credit_minor.is_none());
            let verified: bool = sqlx::query_scalar("SELECT dual_verified_at IS NOT NULL FROM deposits WHERE id=$1")
                .bind(f.id).fetch_one(p).await?;
            ensure!(verified);
            let before = counts(&r); ensure!(before == [3,3]);
            for elapsed in [60,600,21_600,86_400] {
                let now = f.due + Duration::seconds(elapsed);set_clock(&r,now);
                ensure!(watch.watch_once_at(1,now).await?.watched == 0,"held proof lost watcher eligibility");
                ensure!(matches!(pump.run_once_at(now).await?,topup::pump::RunOnceResult::Applied{..}));
                ensure!(counts(&r) == before,"business wait re-read terminal evidence");
                ensure!(terminal_reference(p,f.id).await? == reference,"business wait replaced final marker/proof");
                let proof: Option<Value> = sqlx::query_scalar("SELECT confirmation_terminal_evidence($1)")
                    .bind(f.id).fetch_one(p).await?;
                ensure!(proof.is_some());
            }
            let transitions: i64 = sqlx::query_scalar("SELECT count(*) FROM transitions WHERE deposit_id=$1 AND evidence ->> 'result'='awaiting_reconfirmation'")
                .bind(f.id).fetch_one(p).await?;
            ensure!(transitions == 5);
            let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE object_id=$1")
                .bind(f.id).fetch_one(p).await?;
            ensure!(events == 0);Ok(())
        }).await
    })).await
}

#[tokio::test]
async fn watcher_does_not_report_final_when_handoff_does_not_persist_terminal_evidence()
-> Result<()> {
    with_database(|ctx| {
        Box::pin(async move {
            let p = &ctx.app_pool;
            let f = fixture(p, Confirmations::Finalized, 1).await?;
            sqlx::query("UPDATE deposits SET final_at=$2 WHERE id=$1")
                .bind(f.id)
                .bind(f.due)
                .execute(p)
                .await?;
            db::chain_reads::advance_checkpoint(
                p,
                1,
                db::chain_reads::Boundary {
                    number: 200,
                    hash: hash(200),
                    time: f.due,
                },
            )
            .await?;
            // A handler that returns a retry without evidence models a context-load failure.
            run_fixture_with_step(p, &f, Some(Box::new(Paused)), async |pump, watch, r, _| {
                let result = watch.watch_once_at(1, f.due).await?;
                ensure!(result.watched == 1 && result.finalized == 0);
                let proof: Option<Value> =
                    sqlx::query_scalar("SELECT confirmation_terminal_evidence($1)")
                        .bind(f.id)
                        .fetch_one(p)
                        .await?;
                ensure!(proof.is_none());
                ensure!(counts(&r) == [3, 3]);
                let entry: Option<DateTime<Utc>> =
                    sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
                        .bind(f.id)
                        .fetch_one(p)
                        .await?;
                ensure!(
                    entry == Some(f.due),
                    "failed handoff did not enter S atomically"
                );
                let gauges = topup::observability::metrics::render_chain_reads_at(p, f.due).await?;
                ensure!(gauges.contains("topup_finality_unresolved{chain_id=\"1\"} 1"));
                ensure!(gauges.contains("topup_finality_unresolved_entries_24h{chain_id=\"1\"} 1"));
                let next = f.due + Duration::seconds(60);
                ensure!(due(p, f.id).await? == next && pump_due(p, f.id).await? == next);
                for elapsed in [0, 1, 30, 59] {
                    let now = f.due + Duration::seconds(elapsed);
                    set_clock(&r, now);
                    ensure!(watch.watch_once_at(1, now).await?.watched == 0);
                    ensure!(pump.run_once_at(now).await? == topup::pump::RunOnceResult::Idle);
                    ensure!(
                        counts(&r) == [3, 3],
                        "failed handoff re-read within backoff"
                    );
                }
                set_clock(&r, next);
                ensure!(watch.watch_once_at(1, next).await?.watched == 1);
                ensure!(counts(&r) == [6, 6]);
                let repeated: Option<DateTime<Utc>> =
                    sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
                        .bind(f.id)
                        .fetch_one(p)
                        .await?;
                ensure!(repeated == entry, "handoff recheck overwrote S history");
                ensure!(due(p, f.id).await? == next + Duration::seconds(60));
                // The failed handoff follows the same ten-minute phase boundary as all S.
                let later = f.due + Duration::minutes(10);
                set_clock(&r, later);
                ensure!(watch.watch_once_at(1, later).await?.watched == 1);
                ensure!(due(p, f.id).await? == later + Duration::minutes(10));
                ensure!(pump_due(p, f.id).await? == later + Duration::minutes(10));
                let before = counts(&r);
                for elapsed in [1, 60, 599] {
                    let now = later + Duration::seconds(elapsed);
                    set_clock(&r, now);
                    ensure!(watch.watch_once_at(1, now).await?.watched == 0);
                    ensure!(pump.run_once_at(now).await? == topup::pump::RunOnceResult::Idle);
                    ensure!(
                        counts(&r) == before,
                        "failed handoff bypassed ten-minute backoff"
                    );
                }

                Ok(())
            })
            .await
        })
    })
    .await
}

// Models another writer winning the version CAS while the watcher's handoff is running.
struct ConcurrentTerminalWriter(PgPool);
#[async_trait]
impl Step for ConcurrentTerminalWriter {
    async fn run(&self, deposit: &db::Deposit) -> StepResult {
        Paused.run(deposit).await
    }
    async fn run_with_final_evidence(
        &self,
        deposit: &db::Deposit,
        evidence: topup::steps::confirm::ConfirmationEvidence,
        _: tokio::time::Instant,
        now: DateTime<Utc>,
    ) -> StepResult {
        let proof = Uuid::new_v4();
        let mut tx = self.0.begin().await.expect("competing transaction");
        sqlx::query("INSERT INTO transitions(id,deposit_id,from_state,to_state,attempt,evidence) VALUES($1,$2,'detected','detected',0,$3)")
            .bind(proof).bind(deposit.id).bind(json!({"confirmation_proof_version":1,"chain_confirmation":evidence}))
            .execute(&mut *tx).await.expect("competing proof");
        sqlx::query("UPDATE deposits SET final_at=now(),confirmation_terminal_transition_id=$2,updated_at=now()+interval '1 second',lease_token=NULL,lease_until=NULL,next_attempt_at=$3,finality_check_at=$3 WHERE id=$1")
            .bind(deposit.id).bind(proof).bind(now + Duration::days(1)).execute(&mut *tx).await.expect("competing final marker");
        tx.commit().await.expect("competing commit");
        Paused.run(deposit).await
    }
}

#[tokio::test]
async fn watcher_propagates_stale_handoff_even_when_another_writer_saved_a_proof() -> Result<()> {
    with_database(|ctx| {
        Box::pin(async move {
            let p = &ctx.app_pool;
            let f = fixture(p, Confirmations::Finalized, 1).await?;
            sqlx::query("UPDATE deposits SET final_at=$2 WHERE id=$1")
                .bind(f.id)
                .bind(f.due)
                .execute(p)
                .await?;
            db::chain_reads::advance_checkpoint(
                p,
                1,
                db::chain_reads::Boundary {
                    number: 200,
                    hash: hash(200),
                    time: f.due,
                },
            )
            .await?;
            run_fixture_with_step(
                p,
                &f,
                Some(Box::new(ConcurrentTerminalWriter(p.clone()))),
                async |_, watch, r, _| {
                    let result = watch.watch_once_at(1, f.due).await?;
                    ensure!(
                        result.watched == 1 && result.finalized == 0,
                        "stale handoff reported Final"
                    );
                    ensure!(terminal_reference(p, f.id).await?.0 != Uuid::nil());
                    let entry: Option<DateTime<Utc>> =
                        sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
                            .bind(f.id)
                            .fetch_one(p)
                            .await?;
                    ensure!(entry.is_none(), "stale handoff changed newer S history");
                    ensure!(
                        due(p, f.id).await? == f.due + Duration::days(1),
                        "stale handoff rescheduled newer version"
                    );
                    ensure!(
                        db::get_deposit(p, f.id)
                            .await?
                            .context("newer deposit")?
                            .next_attempt_at
                            == f.due + Duration::days(1)
                    );

                    ensure!(counts(&r) == [3, 3]);
                    Ok(())
                },
            )
            .await
        })
    })
    .await
}

// A failed handoff that races only a row-version change must not write S or clear
// the retained token of that newer version. No terminal proof exempts it from S.
struct ConcurrentVersionWriter(PgPool);
#[async_trait]
impl Step for ConcurrentVersionWriter {
    async fn run(&self, deposit: &db::Deposit) -> StepResult {
        Paused.run(deposit).await
    }
    async fn run_with_final_evidence(
        &self,
        deposit: &db::Deposit,
        _: topup::steps::confirm::ConfirmationEvidence,
        _: tokio::time::Instant,
        now: DateTime<Utc>,
    ) -> StepResult {
        sqlx::query(
            "UPDATE deposits SET updated_at=$2,next_attempt_at=$3,finality_check_at=$3 WHERE id=$1",
        )
        .bind(deposit.id)
        .bind(deposit.updated_at + Duration::seconds(1))
        .bind(now + Duration::days(1))
        .execute(&self.0)
        .await
        .expect("newer record version");
        Paused.run(deposit).await
    }
}

#[tokio::test]
async fn stale_watcher_handoff_does_not_change_newer_version_without_a_terminal_proof() -> Result<()>
{
    with_database(|ctx| {
        Box::pin(async move {
            let p = &ctx.app_pool;
            let f = fixture(p, Confirmations::Finalized, 1).await?;
            sqlx::query("UPDATE deposits SET final_at=$2 WHERE id=$1")
                .bind(f.id)
                .bind(f.due)
                .execute(p)
                .await?;
            let version = db::get_deposit(p, f.id)
                .await?
                .context("original")?
                .updated_at;
            db::chain_reads::advance_checkpoint(
                p,
                1,
                db::chain_reads::Boundary {
                    number: 200,
                    hash: hash(200),
                    time: f.due,
                },
            )
            .await?;
            run_fixture_with_step(
                p,
                &f,
                Some(Box::new(ConcurrentVersionWriter(p.clone()))),
                async |_, watch, r, _| {
                    let result = watch.watch_once_at(1, f.due).await?;
                    ensure!(result.watched == 1 && result.finalized == 0);
                    let newer = db::get_deposit(p, f.id).await?.context("newer")?;
                    let entry: Option<DateTime<Utc>> =
                        sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
                            .bind(f.id)
                            .fetch_one(p)
                            .await?;
                    ensure!(entry.is_none(), "stale handoff wrote S on newer version");
                    ensure!(
                        newer.updated_at == version + Duration::seconds(1),
                        "stale handoff changed newer version"
                    );
                    ensure!(
                        newer.lease_token.is_some(),
                        "stale watcher cleared newer version's lease"
                    );
                    ensure!(
                        newer.next_attempt_at == f.due + Duration::days(1)
                            && due(p, f.id).await? == newer.next_attempt_at
                    );
                    ensure!(counts(&r) == [3, 3]);
                    Ok(())
                },
            )
            .await
        })
    })
    .await
}
