//! R1–R5 and F1–F3: complete dual evidence and atomic per-address coverage.
mod support;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use std::sync::Mutex;
use support::{
    TestDatabase,
    seed::{self, NewAccount, NewAddress},
    with_database,
};
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
    requests: Mutex<Vec<(Vec<Address>, u64, u64)>>,
}
impl Reader {
    fn new(head: u64) -> Self {
        Self {
            head,
            logs: Vec::new(),
            receipt: None,
            error: false,
            forged_header: false,
            requests: Mutex::new(Vec::new()),
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
        Ok((
            if self.forged_header {
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
        Ok(None)
    }
    async fn receipt_transfer(&self, tx: B256, position: u64) -> Result<ReceiptLookup, ChainError> {
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
async fn marker(database: &TestDatabase, id: Uuid) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT dual_covered_through FROM addresses WHERE id=$1")
            .bind(id)
            .fetch_one(&database.app_pool)
            .await?,
    )
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
            ensure!(!db::chain_reads::admit_address(&mut tx, 1).await?);
            tx.rollback().await?;
            let reader = Reader::new(10000);
            scanner::initialize_chain(&d.app_pool, 1, &reader, &reader).await?;
            let mut tx = d.app_pool.begin().await?;
            ensure!(db::chain_reads::admit_address(&mut tx, 1).await?);
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
            scanner::fast_once(&d.app_pool, &read, &chain()).await?;
            let marked: bool =
                sqlx::query_scalar("SELECT dual_verified_at IS NOT NULL FROM deposits")
                    .fetch_one(&d.app_pool)
                    .await?;
            ensure!(!marked);
            // N-1 writes no new marker and may have provisional fields to correct.
            sqlx::query("UPDATE deposits SET block_time=to_timestamp(101),tx_nonce=10")
                .execute(&d.owner_pool)
                .await?;
            read.logs.clear();
            scanner::coverage_once(&d.app_pool, &read, &verify, &chain(), 1).await?;
            let row: (DateTime<Utc>, String, bool) = sqlx::query_as(
                "SELECT block_time,tx_nonce::text,dual_verified_at IS NOT NULL FROM deposits",
            )
            .fetch_one(&d.app_pool)
            .await?;
            ensure!(row == (time(100), "9".into(), true));
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
        scanner::fast_once(&d.app_pool,&read,&chain()).await?;
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
