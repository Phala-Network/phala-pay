//! R1–R5 and F1–F3: complete dual evidence and atomic per-address coverage.
mod support;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};
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
}
impl Reader {
    fn new(head: u64) -> Self {
        Self {
            head,
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
        }
    }
}
impl ChainReader for Reader {
    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: self.head,
            time: time(self.head),
        })
    }
    async fn finalized_header(&self) -> Result<(FinalizedHead, B256), ChainError> {
        Ok((self.finalized_head().await?, hash(self.head)))
    }
    async fn latest_header(&self) -> Result<FinalizedHead, ChainError> {
        self.finalized_head().await
    }
    async fn header(&self, number: u64) -> Result<(B256, DateTime<Utc>), ChainError> {
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
        tokio::try_join!(async {Ok::<_,anyhow::Error>(coverage.await?)},fast)?;
        let row:(i64,bool)=sqlx::query_as("SELECT block_number,dual_verified_at IS NOT NULL FROM deposits").fetch_one(&d.app_pool).await?;
        ensure!(row==(100,true), "concurrent fast insertion escaped dual verification");
        Ok(())
    })).await
}

#[tokio::test]
async fn changed_provisional_recipient_reverses_before_successor_coverage() -> Result<()> {
    for tracked in [true, false] {
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
                scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
                let id = topup_core::identity::deposit_id(1, old.tx_hash, 0);
                let provisional = db::get_deposit(&d.app_pool, id).await?.unwrap();
                ensure!(
                    provisional.state == topup_core::deposit::DepositState::Detected
                        && provisional.address_id == original.id
                );
                let watch = topup::finality::FinalityWatch::single(
                    d.app_pool.clone(),
                    Arc::new(routes()),
                    1,
                    read,
                    verify,
                );
                let stats = watch.watch_once(1).await?;
                ensure!(
                    stats.reversed == 1,
                    "provisional recipient was not handed to finality: {stats:?}; tracked={tracked}"
                );
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
