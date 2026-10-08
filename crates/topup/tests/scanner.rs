//! R1–R5 and F1–F3: complete dual evidence and atomic per-address coverage.
mod support;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use support::{
    TestDatabase,
    seed::{self, NewAccount, NewAddress},
    with_database,
};
use tokio::sync::Notify;
use topup::{db, routes::RouteSet, scanner};
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FactoryLog, FactoryReceipt, FinalizedHead, ReceiptLookup, TransferLog,
};
use topup_core::{
    money::AtomicAmount,
    route::{ChainHeads, Confirmations, RouteFile},
};
use uuid::Uuid;

fn time(number: u64) -> DateTime<Utc> {
    DateTime::from_timestamp(i64::try_from(number).unwrap(), 0).unwrap()
}
fn hash(number: u64) -> B256 {
    B256::from(U256::from(number))
}
struct Reader {
    head: u64,
    live_head: Option<Arc<AtomicU64>>,
    finalized_reads: Mutex<usize>,
    logs: Vec<TransferLog>,
    receipt: Option<TransferLog>,
    error: bool,
    forged_header: bool,
    forged_at: Option<u64>,
    requests: Mutex<Vec<(Vec<Address>, u64, u64)>>,
    factory: Option<FactoryReceipt>,
    receipt_reads: Mutex<usize>,
    factory_reads: Mutex<usize>,
    header_reads: Mutex<Vec<u64>>,
    logs_gate: Option<(Arc<Notify>, Arc<Notify>)>,
    receipt_gate: Option<(Arc<Notify>, Arc<Notify>)>,
    header_gate: Option<(u64, Arc<Notify>, Arc<Notify>)>,
}
impl Reader {
    fn new(head: u64) -> Self {
        Self {
            head,
            live_head: None,
            finalized_reads: Mutex::new(0),
            logs: Vec::new(),
            receipt: None,
            error: false,
            forged_header: false,
            forged_at: None,
            requests: Mutex::new(Vec::new()),
            factory: None,
            receipt_reads: Mutex::new(0),
            factory_reads: Mutex::new(0),
            header_reads: Mutex::new(Vec::new()),
            logs_gate: None,
            receipt_gate: None,
            header_gate: None,
        }
    }
}
impl ChainReader for Reader {
    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        *self.finalized_reads.lock().unwrap() += 1;
        let head = self
            .live_head
            .as_ref()
            .map_or(self.head, |head| head.load(Ordering::SeqCst));
        Ok(FinalizedHead {
            number: head,
            time: time(head),
        })
    }
    async fn finalized_header(&self) -> Result<(FinalizedHead, B256), ChainError> {
        let head = self.finalized_head().await?;
        Ok((head, hash(head.number)))
    }
    async fn latest_header(&self) -> Result<FinalizedHead, ChainError> {
        self.finalized_head().await
    }
    async fn header(&self, number: u64) -> Result<(B256, DateTime<Utc>), ChainError> {
        if let Some((at, started, resume)) = &self.header_gate
            && *at == number
        {
            started.notify_one();
            resume.notified().await;
        }
        self.header_reads.lock().unwrap().push(number);
        Ok((
            if self.forged_header || self.forged_at == Some(number) {
                B256::ZERO
            } else {
                hash(number)
            },
            time(number),
        ))
    }
    async fn confirmation_heads(&self, _: Confirmations) -> Result<ChainHeads, ChainError> {
        Ok(ChainHeads {
            latest: Some(self.head),
            safe: Some(self.head),
            finalized: self.head,
        })
    }
    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from: u64,
        to: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        if !addresses.is_empty()
            && let Some((started, resume)) = &self.logs_gate
        {
            started.notify_one();
            resume.notified().await;
        }
        self.requests
            .lock()
            .unwrap()
            .push((addresses.to_vec(), from, to));
        if self.error {
            return Err(ChainError::Rpc("logs"));
        }
        Ok(self
            .logs
            .iter()
            .filter(|l| addresses.contains(&l.to) && (from..=to).contains(&l.block_number))
            .cloned()
            .collect())
    }
    async fn factory_logs(
        &self,
        _: Address,
        _: &[Address],
        _: u64,
        _: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        Ok(Vec::new())
    }
    async fn factory_receipt(
        &self,
        _: B256,
        _: Address,
    ) -> Result<Option<FactoryReceipt>, ChainError> {
        *self.factory_reads.lock().unwrap() += 1;
        Ok(self.factory.clone())
    }
    async fn receipt_transfer(&self, tx: B256, position: u64) -> Result<ReceiptLookup, ChainError> {
        if let Some((started, resume)) = &self.receipt_gate {
            started.notify_one();
            resume.notified().await;
        }
        *self.receipt_reads.lock().unwrap() += 1;
        Ok(match &self.receipt {
            Some(log) if log.tx_hash == tx && log.receipt_log_index == position => {
                ReceiptLookup::Included {
                    block_number: log.block_number,
                    block_hash: log.block_hash,
                    block_time: log.block_time,
                    status: true,
                    tx_from: log.tx_from,
                    tx_nonce: log.tx_nonce,
                    transfer: Some(Box::new(log.clone())),
                }
            }
            _ => ReceiptLookup::Missing,
        })
    }
    async fn nonce_at(&self, _: Address, _: u64) -> Result<u64, ChainError> {
        panic!("coverage never searches nonces")
    }
}
fn route() -> RouteFile {
    serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml")).unwrap()
}
fn routes() -> RouteSet {
    RouteSet::new(vec![route()]).unwrap()
}
fn chain() -> scanner::ChainRoutes {
    scanner::chain_routes(&routes()).remove(0)
}
async fn address(database: &TestDatabase, created: u64) -> Result<db::Address> {
    let pool = &database.app_pool;
    let account = NewAccount::named("dual coverage");
    let (_, customer) = seed::create_account_and_customer(pool, &account, "coverage").await?;
    seed::accept_routes(pool, account.id, true, &[&route()]).await?;
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: route().route,
            salt: alloy_primitives::keccak256(Uuid::new_v4().as_bytes()),
            address: Address::from_slice(
                &alloy_primitives::keccak256(Uuid::new_v4().as_bytes()).as_slice()[12..],
            ),
        },
    )
    .await?;
    sqlx::query("UPDATE addresses SET created_block=$2 WHERE id=$1")
        .bind(address.id)
        .bind(i64::try_from(created)?)
        .execute(pool)
        .await?;
    Ok(address)
}
fn transfer(address: &db::Address, number: u64) -> TransferLog {
    TransferLog {
        tx_hash: B256::repeat_byte(0xaa),
        receipt_log_index: 0,
        log_index: 2,
        block_number: number,
        block_hash: hash(number),
        block_time: time(number),
        tx_from: Address::repeat_byte(0x44),
        tx_nonce: 9,
        token: route().asset.contract,
        from: Address::repeat_byte(0x44),
        to: address.address,
        amount: AtomicAmount::new(U256::from(1000)),
    }
}
fn provisional(log: TransferLog, address: &db::ScanAddress) -> db::NewDeposit {
    db::NewDeposit {
        chain_id: 1,
        tx_hash: log.tx_hash,
        receipt_log_index: log.receipt_log_index,
        log_index: log.log_index,
        block_number: log.block_number,
        block_hash: log.block_hash,
        block_time: log.block_time,
        address_id: address.id,
        route: Some(route().route),
        route_version: Some(1),
        asset_contract: log.token,
        from_address: log.from,
        amount_atomic: log.amount,
        state: topup_core::deposit::DepositState::Detected,
        reason: None,
        next_attempt_at: Utc::now(),
        tx_from: log.tx_from,
        tx_nonce: log.tx_nonce,
        is_final: false,
    }
}
async fn marker(database: &TestDatabase, id: Uuid) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT dual_covered_through FROM addresses WHERE id=$1")
            .bind(id)
            .fetch_one(&database.app_pool)
            .await?,
    )
}

async fn compat(database: &TestDatabase) -> Result<(i64, Option<DateTime<Utc>>)> {
    Ok(
        sqlx::query_as("SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1")
            .fetch_one(&database.app_pool)
            .await?,
    )
}

#[tokio::test]
async fn forward_migration_repairs_existing_dual_cursors_and_n_minus_one_reissue() -> Result<()> {
    let Some(d) = TestDatabase::create_with_migrations(false).await? else {
        return Ok(());
    };
    let result=async {
        let directory=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut previous=sqlx::migrate::Migrator::new(directory.as_path()).await?;
        previous.migrations.to_mut().retain(|migration|migration.version<20261031000000);
        previous.run(&d.owner_pool).await?;
        let address=address(&d,100).await?;
        let read=Reader::new(4000);
        scanner::initialize_chain(&d.app_pool,1,&read,&read).await?;
        sqlx::query("UPDATE chain_coverage SET through_block=4000 WHERE chain_id=1").execute(&d.app_pool).await?;
        sqlx::query("UPDATE addresses SET dual_covered_through=3009 WHERE id=$1").bind(address.id).execute(&d.app_pool).await?;
        sqlx::query("UPDATE cursors SET scanned_block=9999,scanned_block_time=to_timestamp(9999) WHERE chain_id=1").execute(&d.app_pool).await?;
        sqlx::query("INSERT INTO chain_coverage(chain_id,through_block,through_hash,through_time) VALUES(2,222,$1,$2)").bind(format!("{:#x}",hash(222))).bind(time(222)).execute(&d.app_pool).await?;
        db::initialize_cursor(&d.app_pool,2,777,time(777)).await?;
        db::migrate(&d.owner_pool).await?;
        ensure!(compat(&d).await?==(3009,None), "forward repair retained a cursor above dual evidence");
        let empty:(i64,Option<DateTime<Utc>>)=sqlx::query_as("SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=2").fetch_one(&d.app_pool).await?;
        ensure!(empty==(222,None), "empty-chain cursor was not capped at its agreed boundary");
        // Exactly the UPDATE issued by N-1; it does not know any dual column.
        sqlx::query("UPDATE addresses SET created_block=10 WHERE id=$1").bind(address.id).execute(&d.app_pool).await?;
        ensure!(compat(&d).await?==(9,None));
        ensure!(marker(&d,address.id).await?.is_none());
        Ok::<_,anyhow::Error>(())
    }.await;
    result.and(d.cleanup().await)
}

#[tokio::test]
async fn compat_cursor_rebases_and_commits_only_the_common_dual_boundary() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            db::initialize_cursor(&d.app_pool, 1, 10000, time(10000)).await?;
            let read = Reader::new(4000);
            let mut verify = Reader::new(4000);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            ensure!(
                compat(d).await? == (99, Some(time(99))),
                "old N-1 cursor was not rebased"
            );
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 2).await?;
            ensure!(compat(d).await? == (4000, Some(time(4000))));
            let mut reissue = d.app_pool.begin().await?;
            sqlx::query("UPDATE addresses SET created_block=10 WHERE id=$1")
                .bind(address.id)
                .execute(&mut *reissue)
                .await?;
            let inside: (i64, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1",
            )
            .fetch_one(&mut *reissue)
            .await?;
            ensure!(
                inside == (9, None),
                "reissue did not atomically clear negative evidence"
            );
            ensure!(
                compat(d).await? == (4000, Some(time(4000))),
                "uncommitted reissue leaked"
            );
            reissue.commit().await?;
            ensure!(compat(d).await? == (9, None));
            // The partial backfill's common header must agree before any markers commit.
            verify.forged_at = Some(3009);
            ensure!(
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 3)
                    .await
                    .is_err()
            );
            ensure!(marker(d, address.id).await?.is_none());
            ensure!(compat(d).await? == (9, None));
            verify.forged_at = None;
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 3).await?;
            ensure!(marker(d, address.id).await? == Some(3009));
            ensure!(
                db::chain_reads::coverage(&d.app_pool, 1)
                    .await?
                    .unwrap()
                    .number
                    == 4000
            );
            ensure!(
                compat(d).await? == (3009, Some(time(3009))),
                "partial history authorized the distant chain boundary"
            );
            // N-1 may move its cursor while N is rolled back; startup must repair it again.
            sqlx::query(
                "UPDATE cursors SET scanned_block=9999,scanned_block_time=to_timestamp(9999)",
            )
            .execute(&d.app_pool)
            .await?;
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            ensure!(
                compat(d).await? == (3009, None),
                "existing coverage did not repair an N-1 cursor"
            );
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 4).await?;
            ensure!(compat(d).await? == (4000, Some(time(4000))));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn checkpoint_loop_advances_independently_of_failed_hourly_coverage() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            use std::time::Duration;
            use tokio_util::sync::CancellationToken;
            let address = address(d, 100).await?;
            let head = Arc::new(AtomicU64::new(40_000));
            let mut read = Reader::new(40_000);
            read.live_head = Some(head.clone());
            let mut verify = Reader::new(40_000);
            verify.live_head = Some(head.clone());
            verify.error = true;
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            let read = Arc::new(read);
            let verify = Arc::new(verify);
            let heads = scanner::FinalizedHeads::default();
            let cancel = CancellationToken::new();
            // Keep the paused runtime runnable while PostgreSQL completes real I/O; only explicit
            // advance calls below move the controlled clock.
            tokio::time::pause();
            let guard_cancel = cancel.clone();
            let guard = tokio::spawn(async move {
                while !guard_cancel.is_cancelled() {
                    tokio::task::yield_now().await;
                }
            });
            let checkpoint = {
                let (pool, read, verify, heads, cancel) = (
                    d.app_pool.clone(),
                    read.clone(),
                    verify.clone(),
                    heads.clone(),
                    cancel.clone(),
                );
                tokio::spawn(async move {
                    scanner::checkpoint_loop(
                        &pool,
                        1,
                        (&*read, &*verify),
                        &heads,
                        || true,
                        &cancel,
                    )
                    .await;
                })
            };
            let coverage = {
                let (pool, read, verify, cancel) = (
                    d.app_pool.clone(),
                    read.clone(),
                    verify.clone(),
                    cancel.clone(),
                );
                tokio::spawn(async move {
                    scanner::coverage_loop(
                        &pool,
                        &chain(),
                        (&*read, &*verify),
                        Duration::ZERO,
                        || true,
                        &cancel,
                    )
                    .await;
                })
            };
            let result = async {
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
                tokio::time::advance(Duration::from_millis(1)).await;
                wait_for_condition(|| {
                    heads.get(1).is_some_and(|h| h.number == 40_000)
                        && !read.requests.lock().unwrap().is_empty()
                })
                .await
                .with_context(|| {
                    format!(
                        "initial tick: head {:?}, requests {}, finalized reads {}",
                        heads.get(1),
                        read.requests.lock().unwrap().len(),
                        *read.finalized_reads.lock().unwrap()
                    )
                })?;
                for tick in 1..=36_u64 {
                    tokio::time::advance(Duration::from_secs(599)).await;
                    // Every prior checkpoint completed, so neither loop may run early.
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }
                    ensure!(heads.get(1).context("published head")?.number == 40_000 + tick - 1);
                    ensure!(
                        read.requests.lock().unwrap().len() == usize::try_from(1 + (tick - 1) / 6)?
                    );
                    head.store(40_000 + tick, Ordering::SeqCst);
                    tokio::time::advance(Duration::from_secs(1)).await;
                    wait_for_condition(|| heads.get(1).is_some_and(|h| h.number == 40_000 + tick))
                        .await
                        .with_context(|| {
                            format!(
                                "checkpoint tick {tick}: published {:?}, finalized reads {}",
                                heads.get(1),
                                *read.finalized_reads.lock().unwrap()
                            )
                        })?;
                    if tick.is_multiple_of(6) {
                        wait_for_condition(|| {
                            read.requests.lock().unwrap().len()
                                == usize::try_from(1 + tick / 6).unwrap()
                        })
                        .await
                        .with_context(|| {
                            format!(
                                "coverage tick {tick}: {} requests",
                                read.requests.lock().unwrap().len()
                            )
                        })?;
                    }
                }
                ensure!(
                    marker(d, address.id).await?.is_none(),
                    "failed coverage published negative evidence"
                );
                ensure!(
                    db::chain_reads::coverage(&d.app_pool, 1)
                        .await?
                        .context("coverage")?
                        .number
                        == 99
                );
                ensure!(
                    compat(d).await?.0 == 99,
                    "checkpoint alone advanced the N-1 cursor"
                );
                Ok(())
            }
            .await;
            cancel.cancel();
            checkpoint.await?;
            coverage.await?;
            guard.await?;
            tokio::time::resume();
            result
        })
    })
    .await
}

async fn wait_for_condition(condition: impl Fn() -> bool) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !condition() {
        ensure!(
            std::time::Instant::now() < deadline,
            "scheduled round did not finish"
        );
        tokio::task::yield_now().await;
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_coverage_uses_the_published_checkpoint_without_advancing_it() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            address(d, 100).await?;
            let mut read = Reader::new(4_000);
            let mut verify = Reader::new(4_000);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            read.head = 10_000;
            verify.head = 10_000;
            let reads = (
                *read.finalized_reads.lock().unwrap(),
                *verify.finalized_reads.lock().unwrap(),
            );
            let stats = scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 6).await?;
            ensure!(stats.finalized == 4_000 && stats.cursor == 4_000);
            ensure!(
                reads
                    == (
                        *read.finalized_reads.lock().unwrap(),
                        *verify.finalized_reads.lock().unwrap()
                    )
            );
            topup::checkpoint::advance(&d.app_pool, 1, &read, &verify).await?;
            let stats = scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 12).await?;
            ensure!(stats.finalized == 10_000 && stats.cursor == 10_000);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn first_round_caps_future_creation_at_the_agreed_checkpoint() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 10000).await?;
            let mut read = Reader::new(100);
            let mut verify = Reader::new(100);
            let boundary = scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            ensure!(boundary.number == 100, "coverage started above finality");
            for reader in [&read, &verify] {
                ensure!(reader.header_reads.lock().unwrap().contains(&100));
                ensure!(!reader.header_reads.lock().unwrap().contains(&9999));
            }
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            ensure!(marker(d, address.id).await?.is_none());
            read.head = 10100;
            verify.head = 10100;
            topup::checkpoint::advance(&d.app_pool, 1, &read, &verify).await?;
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 6).await?;
            ensure!(
                marker(d, address.id).await? == Some(10100),
                "future address coverage stalled"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn canonical_records_beyond_the_round_are_deferred_then_updated() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let mut read = Reader::new(10000);
            let mut verify = Reader::new(10000);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            let scan_address = db::list_scan_addresses(&d.app_pool, 1).await?.remove(0);
            let old = transfer(&address, 100);
            db::commit_confirmed_scan(&d.app_pool, 1, &[provisional(old, &scan_address)], 100)
                .await?;
            read.receipt = Some(transfer(&address, 4000));
            verify.receipt = read.receipt.clone();
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            let row: (i64, bool) =
                sqlx::query_as("SELECT block_number,dual_verified_at IS NOT NULL FROM deposits")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(
                row == (100, false),
                "a future canonical receipt was not deferred"
            );
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 2).await?;
            let row: (i64, bool) =
                sqlx::query_as("SELECT block_number,dual_verified_at IS NOT NULL FROM deposits")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(row == (4000, true), "existing identity kept stale evidence");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn coverage_requeries_fast_inserts_after_taking_the_chain_lock() -> Result<()> {
    with_database(|d| Box::pin(async move {
        let address=address(d,100).await?;
        let started=Arc::new(Notify::new()); let resume=Arc::new(Notify::new());
        let mut read=Reader::new(200); let mut verify=Reader::new(200);
        read.receipt=Some(transfer(&address,100)); verify.receipt=read.receipt.clone();
        scanner::initialize_chain(&d.app_pool,1,&read,&verify).await?;
        read.logs_gate=Some((started.clone(),resume.clone()));
        let scan_address=db::list_scan_addresses(&d.app_pool,1).await?.remove(0);
        let stale=provisional(transfer(&address,5000),&scan_address);
        let chain=chain();
        let coverage=scanner::coverage_once(&d.app_pool,&read,&verify,&chain,1);
        let fast=async {
            started.notified().await;
            let mut tx=d.app_pool.begin().await?;
            // The same lock the fast scanner holds for its insertion commit.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('chain:1',704202))").execute(&mut *tx).await?;
            resume.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(10),async {
                loop {
                    let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))").fetch_one(&d.owner_pool).await?;
                    if waiting {break Ok::<_,sqlx::Error>(());}
                    tokio::task::yield_now().await;
                }
            }).await??;
            db::insert_scanned_deposit_in(&mut tx,&stale,db::Evidence::Confirmed).await?;
            tx.commit().await?;
            Ok::<_,anyhow::Error>(())
        };
        let (outcome, fast) = tokio::join!(coverage, fast);
        fast?;
        ensure!(matches!(outcome, Err(scanner::ScannerError::SnapshotChanged)),
            "concurrent fast insertion did not invalidate the RPC snapshot");
        let row:(i64,bool)=sqlx::query_as("SELECT block_number,dual_verified_at IS NOT NULL FROM deposits").fetch_one(&d.app_pool).await?;
        ensure!(row==(5000,false), "invalidated round changed the concurrent fast insert");
        ensure!(marker(d,address.id).await?.is_none(), "invalidated round published coverage");
        read.logs_gate = None;
        scanner::coverage_once(&d.app_pool,&read,&verify,&chain,2).await?;
        let row:(i64,bool)=sqlx::query_as("SELECT block_number,dual_verified_at IS NOT NULL FROM deposits").fetch_one(&d.app_pool).await?;
        ensure!(row==(100,true), "concurrent fast insertion escaped dual verification");
        Ok(())
    })).await
}

#[tokio::test]
async fn reversal_during_coverage_rpc_discards_the_snapshot_without_reinserting() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            let scan_address = db::list_scan_addresses(&d.app_pool, 1).await?.remove(0);
            let old = transfer(&address, 100);
            let id = topup_core::identity::deposit_id(1, old.tx_hash, old.receipt_log_index);
            db::commit_confirmed_scan(
                &d.app_pool,
                1,
                &[provisional(old.clone(), &scan_address)],
                100,
            )
            .await?;
            // The unresolved confirmation is owned by the watcher while coverage is in flight.
            sqlx::query("UPDATE deposits SET first_unresolved_at=now() WHERE id=$1")
                .bind(id)
                .execute(&d.app_pool)
                .await?;
            let before = db::chain_reads::coverage(&d.app_pool, 1).await?;
            let compat_before: (i64, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1",
            )
            .fetch_one(&d.app_pool)
            .await?;
            // Coverage cached the old payment, but finality sees an untracked recipient.
            read.receipt = Some(old.clone());
            verify.receipt = Some(old.clone());
            let started = Arc::new(Notify::new());
            let resume = Arc::new(Notify::new());
            read.receipt_gate = Some((started.clone(), resume.clone()));
            let mut canonical = old;
            canonical.to = Address::repeat_byte(0x99);
            let mut finality_read = Reader::new(200);
            let mut finality_verify = Reader::new(200);
            finality_read.receipt = Some(canonical.clone());
            finality_verify.receipt = Some(canonical);
            let watch = topup::finality::FinalityWatch::single(
                d.app_pool.clone(),
                Arc::new(routes()),
                1,
                finality_read,
                finality_verify,
            );
            let chain = chain();
            let coverage = scanner::coverage_once(&d.app_pool, &read, &verify, &chain, 1);
            let reversal = async {
                started.notified().await;
                let result = watch.watch_once(1).await;
                resume.notify_one();
                ensure!(
                    result?.reversed == 1,
                    "concurrent finality did not reverse the payment"
                );
                Ok::<_, anyhow::Error>(())
            };
            let (outcome, reversal) = tokio::join!(coverage, reversal);
            reversal?;
            ensure!(
                matches!(outcome, Err(scanner::ScannerError::SnapshotChanged)),
                "a reversal during RPC did not discard cached evidence"
            );
            let records: i64 = sqlx::query_scalar("SELECT count(*) FROM deposits")
                .fetch_one(&d.app_pool)
                .await?;
            ensure!(
                records == 1,
                "cached evidence reinserted a reversed payment"
            );
            ensure!(
                db::get_deposit(&d.app_pool, id).await?.unwrap().state
                    == topup_core::deposit::DepositState::Reversed
            );
            ensure!(
                db::chain_reads::coverage(&d.app_pool, 1).await? == before,
                "invalidated round advanced coverage"
            );
            ensure!(marker(d, address.id).await?.is_none());
            let compat_after: (i64, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1",
            )
            .fetch_one(&d.app_pool)
            .await?;
            ensure!(
                compat_after == compat_before,
                "invalidated round advanced the compat cursor"
            );
            // The next full round must read the now-absent receipt again on both endpoints.
            read.receipt_gate = None;
            read.receipt = None;
            verify.receipt = None;
            read.logs.clear();
            read.logs.push(transfer(&address, 100));
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain, 2).await?;
            ensure!(
                *read.receipt_reads.lock().unwrap() == 2
                    && *verify.receipt_reads.lock().unwrap() == 2,
                "the next round reused invalidated receipt evidence"
            );
            let records: i64 = sqlx::query_scalar("SELECT count(*) FROM deposits")
                .fetch_one(&d.app_pool)
                .await?;
            ensure!(records == 1, "fresh absence reinserted a reversed payment");
            ensure!(marker(d, address.id).await? == Some(200));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn fast_insert_then_reversal_during_rpc_cannot_resurrect_cached_payment() -> Result<()> {
    coverage_with_transient_reversed_history(None).await
}

#[tokio::test]
async fn fast_insert_then_reversal_during_rpc_links_changed_evidence_as_successor() -> Result<()> {
    for field in [
        "recipient",
        "block_number",
        "block_hash",
        "block_time",
        "log_index",
        "token",
        "from",
        "amount",
        "tx_from",
        "tx_nonce",
    ] {
        coverage_with_transient_reversed_history(Some(field)).await?;
    }
    Ok(())
}

async fn coverage_with_transient_reversed_history(changed: Option<&'static str>) -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let old = transfer(&address, 100);
            let id = topup_core::identity::deposit_id(1, old.tx_hash, old.receipt_log_index);
            let mut cached = old.clone();
            let recipient = if changed == Some("recipient") {
                let customer_id: Uuid = sqlx::query_scalar("SELECT customer_id FROM quotes WHERE id=$1")
                    .bind(address.quote_id).fetch_one(&d.app_pool).await?;
                let recipient = seed::insert_address(&d.app_pool, &NewAddress {
                    id: Uuid::new_v4(), customer_id, chain_id: 1, route: route().route,
                    salt: B256::repeat_byte(0x88), address: Address::repeat_byte(0x88),
                }).await?;
                sqlx::query("UPDATE addresses SET created_block=100 WHERE id=$1")
                    .bind(recipient.id).execute(&d.app_pool).await?;
                recipient
            } else {
                address.clone()
            };
            match changed {
                Some("recipient") => cached.to = recipient.address,
                Some("block_number") => {
                    cached.block_number = 101;
                    cached.block_hash = hash(101);
                    cached.block_time = time(101);
                }
                Some("block_hash") => cached.block_hash = B256::repeat_byte(0xbb),
                Some("block_time") => cached.block_time = time(101),
                Some("log_index") => cached.log_index = 3,
                Some("token") => cached.token = Address::repeat_byte(0x55),
                Some("from") => cached.from = Address::repeat_byte(0x55),
                Some("amount") => cached.amount = AtomicAmount::new(U256::from(1001)),
                Some("tx_from") => cached.tx_from = Address::repeat_byte(0x55),
                Some("tx_nonce") => cached.tx_nonce = 10,
                None => {}
                _ => unreachable!("unknown evidence fixture"),
            }
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            // No row exists in the pre-RPC active-id snapshot.
            read.logs.push(cached.clone());
            read.receipt = Some(cached.clone());
            verify.receipt = Some(cached.clone());
            let started = Arc::new(Notify::new());
            let resume = Arc::new(Notify::new());
            read.receipt_gate = Some((started.clone(), resume.clone()));
            let mut fast_read = Reader::new(200);
            fast_read.logs.push(old.clone());
            let mut fast_chain = chain();
            fast_chain.chain.confirmations = Confirmations::Depth(2);
            let mut removed = old;
            removed.to = Address::repeat_byte(0x99);
            let mut finality_read = Reader::new(200);
            let mut finality_verify = Reader::new(200);
            finality_read.receipt = Some(removed.clone());
            finality_verify.receipt = Some(removed);
            let watch = topup::finality::FinalityWatch::single(
                d.app_pool.clone(), Arc::new(routes()), 1, finality_read, finality_verify,
            );
            let chain = chain();
            let coverage = scanner::coverage_once(&d.app_pool, &read, &verify, &chain, 1);
            let concurrent = async {
                started.notified().await;
                let result = async {
                    let fast = scanner::fast_once(&d.app_pool, &fast_read, &fast_chain).await?;
                    ensure!(fast.inserted == 1, "fast scanner did not insert during RPC");
                    // A confirmation anomaly makes this transient detected row watcher-owned S.
                    sqlx::query("UPDATE deposits SET first_unresolved_at=now() WHERE id=$1")
                        .bind(id).execute(&d.app_pool).await?;
                    ensure!(watch.watch_once(1).await?.reversed == 1,
                        "finality did not reverse the transient fast insert");
                    Ok::<_, anyhow::Error>(())
                }.await;
                resume.notify_one();
                result
            };
            let (outcome, concurrent) = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                async { tokio::join!(coverage, concurrent) },
            ).await.context("coverage/fast/finality concurrency timed out")?;
            concurrent?;
            let stats = outcome?;
            ensure!(stats.cursor == 200 && marker(d, address.id).await? == Some(200));
            let history: Vec<(Uuid, String, i64, Option<Uuid>, bool)> = sqlx::query_as(
                "SELECT id,state,revision,replaces,dual_verified_at IS NOT NULL FROM deposits ORDER BY revision",
            ).fetch_all(&d.app_pool).await?;
            ensure!(history[0] == (id, "reversed".into(), 0, None, true));
            if changed.is_none() {
                ensure!(history.len() == 1 && stats.inserted == 0,
                    "cached evidence resurrected the already reversed payment");
                ensure!(db::claim_deposit(&d.app_pool, Uuid::new_v4()).await?.is_none(),
                    "reversed evidence re-entered the credit flow");
            } else {
                ensure!(history.len() == 2 && stats.inserted == 1);
                let successor = topup_core::identity::deposit_revision_id(1, cached.tx_hash, 0, 1);
                ensure!(history[1].0 == successor && history[1].2 == 1
                    && history[1].3 == Some(id) && history[1].4,
                    "changed {changed:?} evidence was inserted without the reversal link");
                let recorded = db::get_deposit(&d.app_pool, successor).await?.unwrap();
                ensure!(recorded.address_id == recipient.id
                    && recorded.block_number == cached.block_number
                    && recorded.block_hash == cached.block_hash
                    && recorded.block_time == cached.block_time
                    && recorded.log_index == cached.log_index
                    && recorded.asset_contract == cached.token
                    && recorded.from_address == cached.from
                    && recorded.amount_atomic == cached.amount
                    && recorded.tx_from == Some(cached.tx_from)
                    && recorded.tx_nonce == Some(cached.tx_nonce));
            }
            ensure!(db::chain_reads::coverage(&d.app_pool, 1).await?.unwrap().number == 200);
            ensure!(compat(d).await? == (200, Some(time(200))));
            Ok(())
        })
    }).await
}

#[tokio::test]
async fn changed_address_snapshot_retries_once_immediately_with_fresh_logs() -> Result<()> {
    for changes in [1, 2] {
        with_database(|d| {
            Box::pin(async move {
                let original = address(d, 100).await?;
                let started = Arc::new(Notify::new());
                let resume = Arc::new(Notify::new());
                let mut read = Reader::new(200);
                let verify = Reader::new(200);
                scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
                read.logs_gate = Some((started.clone(), resume.clone()));
                let chain = chain();
                let coverage = scanner::coverage_round(&d.app_pool, &read, &verify, &chain, 1);
                let issuance = async {
                    for attempt in 0..2 {
                        started.notified().await;
                        let changed = if attempt < changes {
                            address(d, 100).await.map(|_| ())
                        } else {
                            Ok(())
                        };
                        resume.notify_one();
                        changed?;
                    }
                    Ok::<_, anyhow::Error>(())
                };
                let (outcome, issuance) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        tokio::join!(coverage, issuance)
                    })
                    .await
                    .context("immediate snapshot retry did not finish")?;
                issuance?;
                ensure!(
                    read.requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(addresses, _, _)| !addresses.is_empty())
                        .count()
                        == 2
                        && verify
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|(addresses, _, _)| !addresses.is_empty())
                            .count()
                            == 2,
                    "snapshot retry must repeat both endpoints' recipient log RPCs exactly once"
                );
                if changes == 1 {
                    ensure!(
                        outcome?.cursor == 200,
                        "fresh retry did not commit immediately"
                    );
                    ensure!(marker(d, original.id).await? == Some(200));
                } else {
                    ensure!(
                        matches!(outcome, Err(scanner::ScannerError::SnapshotChanged)),
                        "coverage retries must stop after one fresh retry"
                    );
                    ensure!(marker(d, original.id).await?.is_none());
                }
                Ok(())
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn slow_rpc_does_not_hold_chain_lock_during_initialization_or_coverage() -> Result<()> {
    for initializing in [true, false] {
        with_database(|d| {
            Box::pin(async move {
                let address = address(d, 100).await?;
                let started = Arc::new(Notify::new());
                let resume = Arc::new(Notify::new());
                let mut read = Reader::new(200);
                let mut verify = Reader::new(200);
                if initializing {
                    read.header_gate = Some((99, started.clone(), resume.clone()));
                } else {
                    scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
                    read.logs.push(transfer(&address, 100));
                    read.receipt = Some(transfer(&address, 100));
                    verify.receipt = read.receipt.clone();
                    read.receipt_gate = Some((started.clone(), resume.clone()));
                }
                let chain = chain();
                let scanner = async {
                    if initializing {
                        scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
                    } else {
                        scanner::coverage_once(&d.app_pool, &read, &verify, &chain, 1).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                };
                let issuance = async {
                    started.notified().await;
                    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                        let mut tx = d.app_pool.begin().await?;
                        let admitted = db::chain_reads::admit_address(&mut tx, 1).await?;
                        tx.rollback().await?;
                        Ok::<_, sqlx::Error>(admitted)
                    })
                    .await;
                    // Release even when a mutation causes a timeout, so DB cleanup can finish.
                    resume.notify_one();
                    outcome
                };
                let (scan, admission) = tokio::join!(scanner, issuance);
                scan?;
                let admitted = admission.context("slow RPC blocked the chain issuance lock")??;
                ensure!(
                    admitted
                        == if initializing {
                            db::chain_reads::AddressAdmission::NotReady
                        } else {
                            db::chain_reads::AddressAdmission::Admitted
                        }
                );
                Ok(())
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn changed_provisional_recipient_reverses_before_successor_coverage() -> Result<()> {
    for (tracked, permanent) in [(true, false), (false, false), (true, true), (false, true)] {
        with_database(|d| {
            Box::pin(async move {
                let original = address(d, 100).await?;
                let other = address(d, 100).await?;
                let mut read = Reader::new(200);
                let mut verify = Reader::new(200);
                scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
                let scan_address = db::list_scan_addresses(&d.app_pool, 1)
                    .await?
                    .into_iter()
                    .find(|a| a.id == original.id)
                    .unwrap();
                let mut old = transfer(&original, 100);
                old.tx_hash = original.salt;
                db::commit_confirmed_scan(
                    &d.app_pool,
                    1,
                    &[provisional(old.clone(), &scan_address)],
                    100,
                )
                .await?;
                let mut canonical = old.clone();
                canonical.to = if tracked {
                    other.address
                } else {
                    Address::repeat_byte(0x99)
                };
                canonical.block_number = 101;
                canonical.block_hash = hash(101);
                canonical.block_time = time(101);
                read.logs.push(canonical.clone());
                read.receipt = Some(canonical.clone());
                verify.receipt = Some(canonical.clone());
                if permanent {
                    sqlx::query("UPDATE deposits SET state='rejected',reason='out_of_bounds'")
                        .execute(&d.app_pool)
                        .await?;
                    ensure!(
                        scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1)
                            .await
                            .is_err()
                    );
                    let check: String = sqlx::query_scalar(
                        "SELECT check_name FROM reconciliation_blocks WHERE chain_id=1",
                    )
                    .fetch_one(&d.app_pool)
                    .await?;
                    ensure!(
                        check == "unverified_evidence_mismatch",
                        "permanent recipient contradiction did not freeze"
                    );
                    return Ok(());
                }
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
                let id = topup_core::identity::deposit_id(1, old.tx_hash, 0);
                let original_row = db::get_deposit(&d.app_pool, id).await?.unwrap();
                ensure!(original_row.state == topup_core::deposit::DepositState::Reversed);
                ensure!(marker(d, other.id).await? == Some(200));
                let watch = topup::finality::FinalityWatch::single(
                    d.app_pool.clone(),
                    Arc::new(routes()),
                    1,
                    read,
                    verify,
                );
                ensure!(watch.watch_once(1).await?.reversed == 0);
                let mut read = Reader::new(201);
                let mut verify = Reader::new(201);
                read.receipt = Some(canonical.clone());
                verify.receipt = Some(canonical);
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 2).await?;
                let marked: bool = sqlx::query_scalar(
                    "SELECT dual_verified_at IS NOT NULL FROM deposits WHERE id=$1",
                )
                .bind(id)
                .fetch_one(&d.app_pool)
                .await?;
                ensure!(marked, "dual reversal left an unverified old revision");
                let count: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM deposits WHERE tx_hash=$1")
                        .bind(format!("{:#x}", old.tx_hash))
                        .fetch_one(&d.app_pool)
                        .await?;
                ensure!(count == if tracked { 2 } else { 1 });
                let frozen: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM reconciliation_blocks)")
                        .fetch_one(&d.app_pool)
                        .await?;
                ensure!(!frozen, "successor falsely froze its reversed predecessor");
                Ok(())
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn recipient_successor_and_coverage_commit_atomically_before_expiry_or_cancel() -> Result<()>
{
    for cancelled in [false, true] {
        with_database(|d| {
            Box::pin(async move {
                let original = address(d, 100).await?;
                let recipient = address(d, 100).await?;
                sqlx::query("UPDATE quotes SET status='open',exposure_reserved=true,closed_at=NULL,created_at=$2,expires_at=$3,cancel_requested_at=CASE WHEN $4 THEN $3 END WHERE id=$1")
                    .bind(recipient.quote_id).bind(time(90)).bind(time(150)).bind(cancelled)
                    .execute(&d.app_pool).await?;
                let mut read = Reader::new(200);
                let mut verify = Reader::new(200);
                scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
                let scan_address = db::list_scan_addresses(&d.app_pool, 1)
                    .await?.into_iter().find(|a| a.id == original.id).unwrap();
                let old = transfer(&original, 100);
                db::commit_confirmed_scan(&d.app_pool, 1, &[provisional(old.clone(), &scan_address)], 100).await?;
                let mut canonical = old.clone();
                canonical.to = recipient.address;
                canonical.block_number = 101;
                canonical.block_hash = hash(101);
                canonical.block_time = time(101);
                read.logs.push(canonical.clone());
                read.receipt = Some(canonical.clone());
                verify.receipt = Some(canonical.clone());

                let started = Arc::new(Notify::new());
                let resume = Arc::new(Notify::new());
                read.receipt_gate = Some((started.clone(),resume.clone()));
                let chain = chain();
                let coverage = scanner::coverage_once(&d.app_pool,&read,&verify,&chain,1);
                let during_rpc = async {
                    started.notified().await;
                    let closed = topup::locks::expire_once(&d.app_pool,&routes()).await;
                    resume.notify_one();
                    ensure!(closed? == 0, "pending RPC allowed a recipient quote to close");
                    Ok::<_,anyhow::Error>(())
                };
                tokio::try_join!(async { Ok::<_,anyhow::Error>(coverage.await?) },during_rpc)?;
                // Run expiry immediately after coverage, before the finality worker can run.
                // B's successor must already protect both expiry and cancellation here.
                ensure!(marker(d,original.id).await? == Some(200));
                ensure!(marker(d,recipient.id).await? == Some(200));
                ensure!(compat(d).await? == (200,Some(time(200))));
                ensure!(topup::locks::expire_once(&d.app_pool, &routes()).await? == 0);
                let successor: (String, DateTime<Utc>, bool) = sqlx::query_as(
                    "SELECT state,block_time,dual_verified_at IS NOT NULL FROM deposits WHERE address_id=$1 AND state <> 'reversed'",
                ).bind(recipient.id).fetch_one(&d.app_pool).await?;
                ensure!(successor == ("detected".into(), time(101), true));
                let held: bool = sqlx::query_scalar("SELECT status='open' AND exposure_reserved FROM quotes WHERE id=$1")
                    .bind(recipient.quote_id).fetch_one(&d.app_pool).await?;
                ensure!(held, "recorded in-window successor lost locked-price eligibility");
                Ok(())
            })
        }).await?;
    }
    Ok(())
}

#[tokio::test]
async fn n_minus_one_unmarked_reversed_revision_does_not_freeze_its_successor() -> Result<()> {
    for marked_successor in [false, true] {
        with_database(|d| {
            Box::pin(async move {
                let original = address(d, 100).await?;
                let recipient = address(d, 100).await?;
                let mut read = Reader::new(200);
                let mut verify = Reader::new(200);
                scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
                let index = db::list_scan_addresses(&d.app_pool, 1).await?;
                let old = transfer(&original, 100);
                let source = index.iter().find(|a| a.id == original.id).unwrap();
                db::commit_confirmed_scan(&d.app_pool, 1, &[provisional(old.clone(), source)], 100).await?;
                // N-1 reverses without knowing the new dual marker column.
                sqlx::query("UPDATE deposits SET state='reversed',reason=NULL WHERE tx_hash=$1")
                    .bind(format!("{:#x}", old.tx_hash)).execute(&d.app_pool).await?;
                let mut canonical = old.clone();
                canonical.to = recipient.address;
                canonical.block_number = 101;
                canonical.block_hash = hash(101);
                canonical.block_time = time(101);
                let target = index.iter().find(|a| a.id == recipient.id).unwrap();
                let mut tx = d.app_pool.begin().await?;
                let successor = db::insert_scanned_deposit_in(
                    &mut tx, &provisional(canonical.clone(), target), db::Evidence::Finalized,
                ).await?.unwrap();
                if marked_successor {
                    sqlx::query("UPDATE deposits SET dual_verified_at=now() WHERE id=$1")
                        .bind(successor).execute(&mut *tx).await?;
                }
                tx.commit().await?;
                read.logs.push(canonical.clone());
                read.receipt = Some(canonical.clone());
                verify.receipt = Some(canonical);
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
                let frozen: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM reconciliation_blocks)")
                    .fetch_one(&d.app_pool).await?;
                ensure!(!frozen, "N-1 reversed history froze the current successor");
                let old_untouched: bool = sqlx::query_scalar("SELECT state='reversed' AND dual_verified_at IS NULL FROM deposits WHERE id=$1")
                    .bind(topup_core::identity::deposit_id(1, old.tx_hash, 0)).fetch_one(&d.app_pool).await?;
                ensure!(old_untouched, "coverage rewrote historical N-1 evidence");
                let current: (String, bool) = sqlx::query_as("SELECT state,dual_verified_at IS NOT NULL FROM deposits WHERE id=$1")
                    .bind(successor).fetch_one(&d.app_pool).await?;
                ensure!(current == ("detected".into(), true));
                ensure!(db::chain_reads::coverage(&d.app_pool, 1).await?.unwrap().number == 200);
                Ok(())
            })
        }).await?;
    }
    Ok(())
}

#[tokio::test]
async fn first_scan_includes_creation_and_all_boundaries_end_at_scanned_e() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let read = Reader::new(10000);
            let verify = Reader::new(10000);
            let start = scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            ensure!(start.number == 99);
            let pending: Vec<_> = [3099, 3100]
                .into_iter()
                .map(|number| {
                    let log = transfer(&address, number);
                    db::NewPendingTransfer {
                        chain_id: 1,
                        tx_hash: hash(number),
                        receipt_log_index: 0,
                        log_index: log.log_index,
                        block_number: number,
                        block_hash: log.block_hash,
                        block_time: log.block_time,
                        address_id: address.id,
                        asset_contract: log.token,
                        from_address: log.from,
                        amount_atomic: log.amount,
                    }
                })
                .collect();
            db::commit_head_scan(&d.app_pool, 1, 100, 10000, &pending).await?;
            let result = scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            ensure!(result.cursor == 3099 && result.finalized == 10000);
            let coverage = db::chain_reads::coverage(&d.app_pool, 1).await?.unwrap();
            ensure!(
                coverage.number == 3099
                    && coverage.hash == hash(3099)
                    && coverage.time == time(3099)
            );
            let compat: (i64, DateTime<Utc>) = sqlx::query_as(
                "SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1",
            )
            .fetch_one(&d.app_pool)
            .await?;
            ensure!(compat == (3099, time(3099)));
            ensure!(marker(d, address.id).await? == Some(3099));
            let pending_blocks: Vec<i64> = sqlx::query_scalar(
                "SELECT block_number FROM pending_transfers ORDER BY block_number",
            )
            .fetch_all(&d.app_pool)
            .await?;
            ensure!(pending_blocks == vec![3100]);
            for reader in [&read, &verify] {
                ensure!(
                    reader
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(_, from, to)| *from == 100 && *to == 3099)
                );
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn empty_chain_is_initialized_before_issuance_and_legacy_state_refuses_start() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let mut tx = d.app_pool.begin().await?;
            ensure!(
                db::chain_reads::admit_address(&mut tx, 1).await?
                    == db::chain_reads::AddressAdmission::NotReady
            );
            tx.rollback().await?;
            let reader = Reader::new(10000);
            scanner::initialize_chain(&d.app_pool, 1, &reader, &reader).await?;
            let mut tx = d.app_pool.begin().await?;
            ensure!(
                db::chain_reads::admit_address(&mut tx, 1).await?
                    == db::chain_reads::AddressAdmission::Admitted
            );
            tx.rollback().await?;
            ensure!(
                db::chain_reads::coverage(&d.app_pool, 1)
                    .await?
                    .unwrap()
                    .number
                    == 10000
            );
            // N-1's recovery metadata is only read, never silently repaired by N.
            sqlx::query("INSERT INTO rpc_chain_state(chain_id,awaiting_anchor) VALUES(1,true)")
                .execute(&d.owner_pool)
                .await?;
            ensure!(
                scanner::initialize_cursors(&d.app_pool, &routes())
                    .await
                    .is_err()
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn union_records_an_omitted_log_once_and_marks_complete_evidence() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let log = transfer(&address, 100);
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            read.receipt = Some(log.clone());
            verify.receipt = Some(log.clone());
            verify.logs.push(log);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            ensure!(
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1)
                    .await?
                    .inserted
                    == 1
            );
            ensure!(
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 2)
                    .await?
                    .inserted
                    == 0
            );
            let state: (String, bool) =
                sqlx::query_as("SELECT state,dual_verified_at IS NOT NULL FROM deposits")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(state == ("detected".into(), true));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn request_error_or_evidence_disagreement_never_commits_coverage() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let log = transfer(&address, 100);
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            read.logs.push(log.clone());
            read.receipt = Some(log.clone());
            verify.receipt = Some(log);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            for fault in ["request", "nonce", "time"] {
                verify.error = fault == "request";
                verify.receipt.as_mut().unwrap().tx_nonce = if fault == "nonce" { 10 } else { 9 };
                verify.receipt.as_mut().unwrap().block_time = if fault == "time" {
                    time(101)
                } else {
                    time(100)
                };
                ensure!(
                    scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1)
                        .await
                        .is_err()
                );
                ensure!(
                    db::chain_reads::coverage(&d.app_pool, 1)
                        .await?
                        .unwrap()
                        .number
                        == 99
                );
                ensure!(marker(d, address.id).await?.is_none());
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deposits")
                    .fetch_one(&d.app_pool)
                    .await?;
                ensure!(count == 0);
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn fast_and_n_minus_one_rows_are_reverified_even_if_both_logs_omit_them() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let log = transfer(&address, 100);
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            read.logs.push(log.clone());
            read.receipt = Some(log.clone());
            verify.receipt = Some(log);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            let mut fast_chain = chain();
            fast_chain.chain.confirmations = Confirmations::Depth(2);
            scanner::fast_once(&d.app_pool, &read, &fast_chain).await?;
            let marked: bool =
                sqlx::query_scalar("SELECT dual_verified_at IS NOT NULL FROM deposits")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(!marked);
            // N-1 writes no new marker and may have provisional fields to correct.
            sqlx::query("UPDATE deposits SET block_number=5000,block_time=to_timestamp(101),tx_nonce=10")
                .execute(&d.owner_pool)
                .await?;
            read.logs.clear();
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            let row: (i64, DateTime<Utc>, String, bool) = sqlx::query_as(
                "SELECT block_number,block_time,tx_nonce::text,dual_verified_at IS NOT NULL FROM deposits",
            )
            .fetch_one(&d.app_pool)
            .await?;
            ensure!(row == (100, time(100), "9".into(), true));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn progressed_unverified_mismatch_freezes_and_prevents_coverage_advance() -> Result<()> {
    with_database(|d|Box::pin(async move {
        let address=address(d,100).await?;let log=transfer(&address,100);
        let mut read=Reader::new(200);let mut verify=Reader::new(200);
        read.logs.push(log.clone());read.receipt=Some(log.clone());verify.receipt=Some(log);
        scanner::initialize_chain(&d.app_pool,1,&read,&verify).await?;
        let mut fast_chain=chain();fast_chain.chain.confirmations=topup_core::route::Confirmations::Depth(2);
        scanner::fast_once(&d.app_pool,&read,&fast_chain).await?;
        sqlx::query("UPDATE deposits SET state='rejected',reason='out_of_bounds',block_time=to_timestamp(101)").execute(&d.owner_pool).await?;
        read.logs.clear();
        ensure!(scanner::coverage_once(&d.app_pool,&read,&verify,&chain(),1).await.is_err());
        let check:String=sqlx::query_scalar("SELECT check_name FROM reconciliation_blocks WHERE chain_id=1").fetch_one(&d.app_pool).await?;
        ensure!(check=="unverified_evidence_mismatch");
        ensure!(db::chain_reads::coverage(&d.app_pool,1).await?.unwrap().number==99);
        Ok(())
    })).await
}

#[tokio::test]
async fn marked_reversed_revision_cannot_hide_an_unverified_current_revision() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let log = transfer(&address, 100);
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            read.logs.push(log.clone());
            read.receipt = Some(log.clone());
            verify.receipt = Some(log.clone());
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            let mut fast_chain = chain();
            fast_chain.chain.confirmations = Confirmations::Depth(2);
            scanner::fast_once(&d.app_pool, &read, &fast_chain).await?;
            sqlx::query("UPDATE deposits SET state='reversed',reason=NULL,dual_verified_at=now()")
                .execute(&d.owner_pool)
                .await?;
            let mut tx = d.app_pool.begin().await?;
            let successor = db::insert_scanned_deposit_in(
                &mut tx,
                &db::NewDeposit {
                    chain_id: 1,
                    tx_hash: log.tx_hash,
                    receipt_log_index: log.receipt_log_index,
                    log_index: log.log_index,
                    block_number: log.block_number,
                    block_hash: log.block_hash,
                    block_time: time(101),
                    address_id: address.id,
                    route: Some(route().route),
                    route_version: Some(1),
                    asset_contract: log.token,
                    from_address: log.from,
                    amount_atomic: log.amount,
                    state: topup_core::deposit::DepositState::Detected,
                    reason: None,
                    next_attempt_at: Utc::now(),
                    tx_from: log.tx_from,
                    tx_nonce: log.tx_nonce,
                    is_final: true,
                },
                db::Evidence::Finalized,
            )
            .await?;
            ensure!(successor.is_some());
            tx.commit().await?;
            sqlx::query("UPDATE deposits SET state='rejected',reason='out_of_bounds' WHERE id=$1")
                .bind(successor.unwrap())
                .execute(&d.owner_pool)
                .await?;
            ensure!(
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1)
                    .await
                    .is_err()
            );
            let check: String =
                sqlx::query_scalar("SELECT check_name FROM reconciliation_blocks WHERE chain_id=1")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(check == "unverified_evidence_mismatch");
            ensure!(
                db::chain_reads::coverage(&d.app_pool, 1)
                    .await?
                    .unwrap()
                    .number
                    == 99
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn lowered_creation_clears_marker_and_backfills_before_quote_expiry() -> Result<()> {
    with_database(|d|Box::pin(async move {
        let address=address(d,100).await?;let read=Reader::new(4000);let verify=Reader::new(4000);
        scanner::initialize_chain(&d.app_pool,1,&read,&verify).await?;
        scanner::coverage_once(&d.app_pool,&read,&verify,&chain(),1).await?;
        scanner::coverage_once(&d.app_pool,&read,&verify,&chain(),2).await?;
        sqlx::query("UPDATE addresses SET created_block=10 WHERE id=$1").bind(address.id).execute(&d.app_pool).await?;
        ensure!(marker(d,address.id).await?.is_none());
        sqlx::query("UPDATE quotes SET status='open',closed_at=NULL,exposure_reserved=true,expires_at=to_timestamp(200) WHERE id=(SELECT quote_id FROM addresses WHERE id=$1)").bind(address.id).execute(&d.app_pool).await?;
        ensure!(topup::locks::expire_once(&d.app_pool,&routes()).await?==0);
        let partial=scanner::coverage_once(&d.app_pool,&read,&verify,&chain(),3).await?;
        ensure!(partial.backfilled_addresses==0 && marker(d,address.id).await?==Some(3009));
        ensure!(topup::locks::expire_once(&d.app_pool,&routes()).await?==0);
        scanner::coverage_once(&d.app_pool,&read,&verify,&chain(),4).await?;
        ensure!(marker(d,address.id).await?==Some(4000));
        ensure!(topup::locks::expire_once(&d.app_pool,&routes()).await?==1);
        ensure!(read.requests.lock().unwrap().iter().any(|(_,from,to)|*from==10 && *to==3009));
        ensure!(verify.requests.lock().unwrap().iter().any(|(_,from,to)|*from==10 && *to==3009));
        Ok(())
    })).await
}

async fn assert_coverage_boundary_conflict(
    stored_coverage_conflict: bool,
    forged_read: bool,
    forged_verify: bool,
) -> Result<()> {
    with_database(|d| Box::pin(async move {
        let mut read = Reader::new(100);
        let mut verify = Reader::new(100);
        scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
        let address = address(d, 100).await?;
        if stored_coverage_conflict {
            // Keep distinct checkpoint and coverage hashes at the same height. When both
            // endpoints return the forged hash, only the coverage guard can catch it.
            sqlx::query("UPDATE chain_coverage SET through_block=100,through_hash=$1,through_time=$2 WHERE chain_id=1")
                .bind(format!("{:#x}", hash(100))).bind(time(100)).execute(&d.app_pool).await?;
            sqlx::query("UPDATE chain_checkpoints SET block_hash=$1 WHERE chain_id=1")
                .bind(format!("{:#x}", B256::ZERO)).execute(&d.app_pool).await?;
        }
        if !stored_coverage_conflict {
            // Keep coverage below the checkpoint, so this fixture independently proves
            // the checkpoint guard rather than also triggering the coverage guard.
            sqlx::query("UPDATE chain_coverage SET through_block=99,through_hash=$1,through_time=$2 WHERE chain_id=1")
                .bind(format!("{:#x}", hash(99))).bind(time(99)).execute(&d.app_pool).await?;
            sqlx::query("UPDATE cursors SET scanned_block=99,scanned_block_time=$1 WHERE chain_id=1")
                .bind(time(99)).execute(&d.app_pool).await?;
        }
        let before = db::chain_reads::coverage(&d.app_pool, 1).await?;
        let cursor: (i64, Option<DateTime<Utc>>) = sqlx::query_as("SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1").fetch_one(&d.app_pool).await?;
        let marker_before = marker(d, address.id).await?;
        sqlx::query("UPDATE quotes SET status='open',closed_at=NULL,exposure_reserved=true,expires_at=to_timestamp(100) WHERE id=(SELECT quote_id FROM addresses WHERE id=$1)").bind(address.id).execute(&d.app_pool).await?;
        read.forged_at = forged_read.then_some(100);
        verify.forged_at = forged_verify.then_some(100);
        ensure!(matches!(scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await, Err(scanner::ScannerError::Disagreement)));
        let reason: String = sqlx::query_scalar("SELECT check_name FROM reconciliation_blocks WHERE chain_id=1").fetch_one(&d.app_pool).await?;
        ensure!(reason == "finalized_checkpoint_conflict");
        ensure!(db::chain_reads::coverage(&d.app_pool, 1).await? == before);
        let after: (i64, Option<DateTime<Utc>>) = sqlx::query_as("SELECT scanned_block,scanned_block_time FROM cursors WHERE chain_id=1").fetch_one(&d.app_pool).await?;
        ensure!(after == cursor && marker(d, address.id).await? == marker_before);
        ensure!(read.requests.lock().unwrap().is_empty() && verify.requests.lock().unwrap().is_empty(), "conflict must freeze before reading logs");
        ensure!(topup::locks::expire_once(&d.app_pool, &routes()).await? == 0);
        let reservation: (String, bool) = sqlx::query_as("SELECT status,exposure_reserved FROM quotes WHERE id=(SELECT quote_id FROM addresses WHERE id=$1)").bind(address.id).fetch_one(&d.app_pool).await?;
        ensure!(reservation == ("open".to_owned(), true));
        Ok(())
    })).await
}

#[tokio::test]
async fn dual_agreed_coverage_boundary_conflict_freezes_before_any_publication() -> Result<()> {
    for stored_coverage_conflict in [false, true] {
        assert_coverage_boundary_conflict(stored_coverage_conflict, true, true).await?;
    }
    Ok(())
}

#[tokio::test]
async fn single_endpoint_coverage_boundary_conflict_freezes_before_disagreement() -> Result<()> {
    for stored_coverage_conflict in [false, true] {
        for (forged_read, forged_verify) in [(true, false), (false, true)] {
            assert_coverage_boundary_conflict(stored_coverage_conflict, forged_read, forged_verify)
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn checkpoint_conflict_freezes_instead_of_replacing_durable_hash() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let read = Reader::new(100);
            let mut verify = Reader::new(100);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            verify.forged_header = true;
            ensure!(
                topup::checkpoint::advance(&d.app_pool, 1, &read, &verify)
                    .await
                    .is_err()
            );
            ensure!(
                db::chain_reads::checkpoint(&d.app_pool, 1)
                    .await?
                    .unwrap()
                    .hash
                    == hash(100)
            );
            let check: String =
                sqlx::query_scalar("SELECT check_name FROM reconciliation_blocks WHERE chain_id=1")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(check == "finalized_checkpoint_conflict");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn lagging_chunk_is_bounded_and_never_bulk_marks_unscanned_addresses() -> Result<()> {
    with_database(|d|Box::pin(async move {
        let first=address(d,100).await?;
        let customer:Uuid=sqlx::query_scalar("SELECT customer_id FROM quotes WHERE id=(SELECT quote_id FROM addresses WHERE id=$1)").bind(first.id).fetch_one(&d.app_pool).await?;
        for n in 1..=1000_u64 {
            seed::insert_address(&d.app_pool,&NewAddress {id:Uuid::new_v4(),customer_id:customer,chain_id:1,route:route().route,salt:hash(n),address:Address::from_word(hash(n))}).await?;
        }
        let read=Reader::new(4000);let verify=Reader::new(4000);
        scanner::initialize_chain(&d.app_pool,1,&read,&verify).await?;
        scanner::coverage_once(&d.app_pool,&read,&verify,&chain(),1).await?;
        let marked:i64=sqlx::query_scalar("SELECT count(*) FROM addresses WHERE dual_covered_through IS NOT NULL").fetch_one(&d.app_pool).await?;
        ensure!(marked==1000);
        ensure!(read.requests.lock().unwrap().iter().all(|(addresses,_,_)|addresses.len()<=1000));
        ensure!(verify.requests.lock().unwrap().iter().all(|(addresses,_,_)|addresses.len()<=1000));
        let mut tx=d.app_pool.begin().await?;ensure!(db::chain_reads::admit_address(&mut tx,1).await? == db::chain_reads::AddressAdmission::CapacityReached);tx.rollback().await?;
        Ok(())
    })).await
}

#[tokio::test]
async fn marked_deposits_are_skipped_but_existing_factory_records_are_always_reverified()
-> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            let address = address(d, 100).await?;
            let log = transfer(&address, 100);
            let mut read = Reader::new(200);
            let mut verify = Reader::new(200);
            read.logs.push(log.clone());
            read.receipt = Some(log.clone());
            verify.receipt = Some(log);
            scanner::initialize_chain(&d.app_pool, 1, &read, &verify).await?;
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            let event = FactoryLog {
                tx_hash: B256::repeat_byte(0xbb),
                log_index: 3,
                block_number: 100,
                block_hash: hash(100),
                event: topup_adapters::chain::flush::FactoryEvent::Flushed(
                    topup_adapters::chain::flush::DecodedFlushed {
                        salt: address.salt,
                        forwarder: address.address,
                        treasury: address.treasury,
                        token: route().asset.contract,
                        amount: U256::from(1000),
                    },
                ),
            };
            db::commit_factory_logs(&d.app_pool, 1, std::slice::from_ref(&event)).await?;
            let receipt = FactoryReceipt {
                status: true,
                block_number: 100,
                block_hash: hash(100),
                block_time: time(100),
                origin: (Address::repeat_byte(4), 9),
                logs: vec![event],
            };
            read.factory = Some(receipt.clone());
            verify.factory = Some(receipt);
            // Reissue resets the per-address marker; a verified deposit still needs no receipt call.
            sqlx::query("UPDATE addresses SET created_block=99 WHERE id=$1")
                .bind(address.id)
                .execute(&d.app_pool)
                .await?;
            read.receipt = None;
            verify.receipt = None;
            let before = *read.receipt_reads.lock().unwrap();
            verify.factory.as_mut().unwrap().origin.1 += 1;
            ensure!(
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 2)
                    .await
                    .is_err()
            );
            ensure!(marker(d, address.id).await?.is_none());
            ensure!(
                *read.factory_reads.lock().unwrap() == 1
                    && *verify.factory_reads.lock().unwrap() == 1
            );
            verify.factory = read.factory.clone();
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 3).await?;
            ensure!(*read.receipt_reads.lock().unwrap() == before);
            ensure!(
                *read.factory_reads.lock().unwrap() == 2
                    && *verify.factory_reads.lock().unwrap() == 2
            );
            ensure!(marker(d, address.id).await? == Some(200));
            // Both endpoints agreeing on a contradiction of insert-only evidence freezes the chain.
            sqlx::query("UPDATE addresses SET created_block=98 WHERE id=$1")
                .bind(address.id)
                .execute(&d.app_pool)
                .await?;
            read.factory.as_mut().unwrap().logs.clear();
            verify.factory = read.factory.clone();
            ensure!(
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 4)
                    .await
                    .is_err()
            );
            ensure!(topup::reconciler::chain_is_blocked(&d.app_pool, 1).await?);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn previous_checkpoint_is_rechecked_on_each_endpoint_before_a_new_checkpoint() -> Result<()> {
    with_database(|d| {
        Box::pin(async move {
            for (chain, forge_read) in [(1, true), (8453, false)] {
                let initial = Reader::new(100);
                topup::checkpoint::advance(&d.app_pool, chain, &initial, &initial).await?;
                let mut read = Reader::new(200);
                let mut verify = Reader::new(200);
                if forge_read {
                    read.forged_at = Some(100);
                } else {
                    verify.forged_at = Some(100);
                }
                // Both agree on the new boundary: only the previous hash re-check can catch this.
                ensure!(matches!(
                    topup::checkpoint::advance(&d.app_pool, chain, &read, &verify).await,
                    Err(scanner::ScannerError::Disagreement)
                ));
                ensure!(
                    db::chain_reads::checkpoint(&d.app_pool, chain)
                        .await?
                        .unwrap()
                        .number
                        == 100
                );
                let check: String = sqlx::query_scalar(
                    "SELECT check_name FROM reconciliation_blocks WHERE chain_id=$1",
                )
                .bind(i64::try_from(chain)?)
                .fetch_one(&d.app_pool)
                .await?;
                ensure!(check == "finalized_checkpoint_conflict");
            }
            Ok(())
        })
    })
    .await
}
