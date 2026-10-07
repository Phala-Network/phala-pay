//! The finality watch on PostgreSQL with scripted providers: a backlog of deposits it keeps
//! waiting on never starves a later one.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use topup::db::{self, NewDeposit};
use topup::finality::{FinalityWatch, WATCH_PAGE, WatchStats};
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FactoryLog, FinalizedHead, ReceiptLookup, TransferLog,
};
use topup_core::deposit::DepositState;
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::route::{ChainHeads, Confirmations};
use uuid::Uuid;

use support::seed::{self, NewAccount, NewAddress};
use support::with_database;

const CHAIN_ID: u64 = 31_337;
const FINALIZED: u64 = 100;
const RECIPIENT: Address = Address::repeat_byte(0x44);
const TOKEN: Address = Address::repeat_byte(0x55);
const SENDER: Address = Address::repeat_byte(0x66);
const BLOCK_TIME: DateTime<Utc> = DateTime::UNIX_EPOCH;

fn block_hash(block_number: u64) -> B256 {
    B256::from(U256::from(block_number))
}

/// Both providers: `finalized` is [`FINALIZED`]; transactions in `included` are in their recorded
/// block, reads of those in `failing` fail, and every other one is in no block while its sender's
/// nonce is unused, so the watch keeps waiting on it.
#[derive(Clone)]
struct ScriptedChain {
    included: Arc<BTreeSet<B256>>,
    failing: Arc<BTreeSet<B256>>,
}

impl ChainReader for ScriptedChain {
    async fn header(&self, number: u64) -> Result<(B256, DateTime<Utc>), ChainError> {
        Ok((block_hash(number), BLOCK_TIME))
    }
    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: FINALIZED,
            time: BLOCK_TIME,
        })
    }

    async fn confirmation_heads(
        &self,
        _confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        panic!("the finality watch never reads confirmation heads")
    }

    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        panic!("the finality watch never reads factory events")
    }

    async fn transfer_logs_to(
        &self,
        _addresses: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        panic!("the finality watch reads receipts, not logs")
    }

    async fn receipt_transfer(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        if self.failing.contains(&tx_hash) {
            return Err(ChainError::MissingField("scripted failure"));
        }
        if !self.included.contains(&tx_hash) {
            return Ok(ReceiptLookup::Missing);
        }
        let block_number = 20;
        Ok(ReceiptLookup::Included {
            block_number,
            block_hash: block_hash(block_number),
            status: true,
            block_time: BLOCK_TIME,
            tx_from: SENDER,
            tx_nonce: 0,
            transfer: Some(Box::new(TransferLog {
                tx_hash,
                receipt_log_index,
                log_index: 0,
                block_number,
                block_hash: block_hash(block_number),
                block_time: BLOCK_TIME,
                tx_from: SENDER,
                tx_nonce: 0,
                token: TOKEN,
                from: SENDER,
                to: RECIPIENT,
                amount: AtomicAmount::new(U256::from(15)),
            })),
        })
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Ok(0)
    }
}

async fn insert(pool: &sqlx::PgPool, address_id: Uuid, index: u64, block: u64) -> Result<Uuid> {
    let tx_hash = B256::from(U256::from(index));
    ensure!(
        db::insert_deposit(
            pool,
            &NewDeposit {
                chain_id: CHAIN_ID,
                tx_hash,
                log_index: 0,
                receipt_log_index: 0,
                tx_from: SENDER,
                tx_nonce: 0,
                is_final: false,
                block_number: block,
                block_hash: block_hash(block),
                block_time: BLOCK_TIME,
                address_id,
                route: None,
                route_version: None,
                asset_contract: TOKEN,
                from_address: SENDER,
                amount_atomic: AtomicAmount::new(U256::from(15)),
                state: DepositState::Rejected,
                reason: Some(topup_core::deposit::RejectReason::UnsupportedAsset),
                next_attempt_at: Utc::now(),
            },
        )
        .await?
    );
    Ok(deposit_id(CHAIN_ID, tx_hash, 0))
}

#[tokio::test]
async fn deposits_the_watch_keeps_waiting_on_do_not_starve_later_ones() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.app_pool;
            db::chain_reads::advance_checkpoint(
                pool,
                CHAIN_ID,
                db::chain_reads::Boundary {
                    number: 100,
                    hash: block_hash(100),
                    time: BLOCK_TIME,
                },
            )
            .await?;
            let (_, customer) = seed::create_account_and_customer(
                pool,
                &NewAccount::named("finality"),
                "workspace-finality",
            )
            .await?;
            let address = NewAddress {
                id: Uuid::new_v4(),
                customer_id: customer.id,
                chain_id: CHAIN_ID,
                route: "screen".to_owned(),
                salt: B256::repeat_byte(0x33),
                address: RECIPIENT,
            };
            seed::insert_address(pool, &address).await?;

            // More stuck deposits than one page, in older blocks than two that settle, the
            // first of which cannot be read at all.
            let stuck = u64::try_from(WATCH_PAGE)? + 1;
            for index in 1..=stuck {
                insert(pool, address.id, index, 10).await?;
            }
            let unreadable = insert(pool, address.id, stuck + 1, 15).await?;
            let later = insert(pool, address.id, stuck + 2, 20).await?;
            let chain = ScriptedChain {
                included: Arc::new(BTreeSet::from([B256::from(U256::from(stuck + 2))])),
                failing: Arc::new(BTreeSet::from([B256::from(U256::from(stuck + 1))])),
            };
            let watch =
                FinalityWatch::single(pool.clone(), Arc::default(), CHAIN_ID, chain.clone(), chain);

            // One pass reads every due deposit, page after page: the later one becomes final, and
            // the unreadable one is counted without holding it back.
            let stats = watch.watch_once(CHAIN_ID).await?;
            ensure!(
                stats
                    == WatchStats {
                        watched: stuck + 2,
                        finalized: 1,
                        followed: 0,
                        reversed: 0,
                        failed: 1,
                        more: false,
                    },
                "{stats:?}"
            );
            let final_at = |id| async move {
                Ok::<_, anyhow::Error>(
                    db::get_deposit(pool, id)
                        .await?
                        .context("deposit")?
                        .final_at,
                )
            };
            ensure!(final_at(later).await?.is_some());
            ensure!(final_at(unreadable).await?.is_none());

            // The deposits it keeps waiting on have their own recheck time: an immediate pass
            // reads nothing, and a pass after it reads them all again.
            ensure!(watch.watch_once(CHAIN_ID).await?.watched == 0);
            sqlx::query("UPDATE deposits SET finality_check_at = now() - interval '1 second'")
                .execute(pool)
                .await?;
            ensure!(watch.watch_once(CHAIN_ID).await?.watched == stuck + 1);
            Ok(())
        })
    })
    .await
}
