//! The finality watch on PostgreSQL with scripted providers: a backlog of deposits it keeps
//! waiting on never starves a later one.

mod support;

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use topup::db::{self, NewDeposit};
use topup::finality::{FinalityWatch, WATCH_PAGE, WatchStats};
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FactoryLog, FinalizedHead, ReceiptLookup, TransferLog,
};
use topup_core::deposit::DepositState;
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::route::{ChainHeads, Confirmations, RouteFile};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::fmt::MakeWriter;
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
    reads: Arc<Mutex<Vec<B256>>>,
    nonce: Option<u64>,
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
        self.reads.lock().expect("read recorder").push(tx_hash);
        if self.failing.contains(&tx_hash) {
            return Err(ChainError::MissingField("scripted failure"));
        }
        if !self.included.contains(&tx_hash) {
            return Ok(ReceiptLookup::Missing);
        }
        let block_number = 20;
        let nonce: u64 = U256::from_be_bytes(tx_hash.0)
            .try_into()
            .expect("fixture nonce");
        let nonce = self.nonce.unwrap_or(nonce);
        Ok(ReceiptLookup::Included {
            block_number,
            block_hash: block_hash(block_number),
            status: true,
            block_time: BLOCK_TIME,
            tx_from: SENDER,
            tx_nonce: nonce,
            transfer: Some(Box::new(TransferLog {
                tx_hash,
                receipt_log_index,
                log_index: 0,
                block_number,
                block_hash: block_hash(block_number),
                block_time: BLOCK_TIME,
                tx_from: SENDER,
                tx_nonce: nonce,
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
                tx_nonce: index,
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
                reads: Arc::default(),
                nonce: None,
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
            ensure!(
                first_unresolved(pool, unreadable).await?.is_some(),
                "RPC failure leaves a counted unresolved entry"
            );

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

async fn setup(pool: &sqlx::PgPool) -> Result<Uuid> {
    db::chain_reads::advance_checkpoint(
        pool,
        CHAIN_ID,
        db::chain_reads::Boundary {
            number: FINALIZED,
            hash: block_hash(FINALIZED),
            time: BLOCK_TIME,
        },
    )
    .await?;
    let (_, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount::named("finality-bounds"),
        "finality-bounds",
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
    Ok(address.id)
}

fn scripted(included: BTreeSet<B256>) -> ScriptedChain {
    ScriptedChain {
        included: Arc::new(included),
        failing: Arc::default(),
        reads: Arc::default(),
        nonce: None,
    }
}

async fn schedule(pool: &sqlx::PgPool, id: Uuid) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    Ok(
        sqlx::query_as("SELECT finality_due_at, finality_check_at FROM deposits WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn unresolved_finality_cadence_keeps_first_due_time_and_exact_boundaries() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.app_pool;
            let address = setup(pool).await?;
            let id = insert(pool, address, 1, 10).await?;
            let chain = scripted(BTreeSet::new());
            let watch = FinalityWatch::single(
                pool.clone(),
                Arc::default(),
                CHAIN_ID,
                chain.clone(),
                chain.clone(),
            );
            let due = DateTime::from_timestamp(1_700_000_000, 0).context("clock")?;
            ensure!(watch.watch_once_at(CHAIN_ID, due).await?.watched == 1);
            ensure!(schedule(pool, id).await? == (due, due + TimeDelta::seconds(60)));
            // No wall-clock sleeps: run every due check through both boundaries and one slow phase.
            let mut elapsed = 60;
            while elapsed <= 25_200 {
                let now = due + TimeDelta::seconds(elapsed);
                ensure!(
                    watch
                        .watch_once_at(CHAIN_ID, now - TimeDelta::seconds(1))
                        .await?
                        .watched
                        == 0
                );
                ensure!(watch.watch_once_at(CHAIN_ID, now).await?.watched == 1);
                let interval = if elapsed < 600 {
                    60
                } else if elapsed < 21_600 {
                    600
                } else {
                    3600
                };
                ensure!(
                    schedule(pool, id).await? == (due, now + TimeDelta::seconds(interval)),
                    "elapsed {elapsed}"
                );
                elapsed += interval;
            }
            // A delayed claim immediately before a boundary still uses the earlier phase.
            for (elapsed, interval) in [(599, 60), (600, 600), (21_599, 600), (21_600, 3600)] {
                let now = due + TimeDelta::seconds(elapsed);
                sqlx::query("UPDATE deposits SET finality_check_at=$2 WHERE id=$1")
                    .bind(id)
                    .bind(now)
                    .execute(pool)
                    .await?;
                let restarted = FinalityWatch::single(
                    pool.clone(),
                    Arc::default(),
                    CHAIN_ID,
                    chain.clone(),
                    chain.clone(),
                );
                ensure!(restarted.watch_once_at(CHAIN_ID, now).await?.watched == 1);
                ensure!(
                    schedule(pool, id).await? == (due, now + TimeDelta::seconds(interval)),
                    "boundary {elapsed}"
                );
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn newly_due_deposits_finalize_immediately_beside_backed_off_deposits() -> Result<()> {
    with_database(|context| Box::pin(async move {
        let pool = &context.app_pool;
        let address = setup(pool).await?;
        let old = insert(pool, address, 1, 10).await?;
        let now = DateTime::from_timestamp(1_700_000_000, 0).context("clock")?;
        sqlx::query("UPDATE deposits SET finality_due_at=$2 - interval '7 hours', finality_check_at=$2 + interval '1 hour' WHERE id=$1")
            .bind(old).bind(now).execute(pool).await?;
        let fresh = insert(pool, address, 2, 20).await?;
        let not_due = insert(pool, address, 3, FINALIZED + 1).await?;
        let chain = scripted(BTreeSet::from([B256::from(U256::from(2))]));
        let watch = FinalityWatch::single(pool.clone(), Arc::default(), CHAIN_ID, chain.clone(), chain.clone());
        let stats = watch.watch_once_at(CHAIN_ID, now).await?;
        ensure!(stats.watched == 1 && stats.finalized == 1);
        ensure!(db::get_deposit(pool, fresh).await?.context("fresh")?.final_at.is_some());
        ensure!(schedule(pool, fresh).await? == (now, now + TimeDelta::seconds(60)));
        ensure!(first_unresolved(pool, fresh).await?.is_none(), "normal finalization is not an unresolved entry");
        let first_due: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT finality_due_at FROM deposits WHERE id=$1").bind(not_due).fetch_one(pool).await?;
        ensure!(first_due.is_none());
        ensure!(chain.reads.lock().expect("read recorder").as_slice() == [B256::from(U256::from(2)); 2]);
        Ok(())
    })).await
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl<'writer> MakeWriter<'writer> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'writer self) -> Self {
        self.clone()
    }
}

async fn replacement_case(pool: &sqlx::PgPool, candidates: u64) -> Result<()> {
    replacement_case_with_state(pool, candidates, DepositState::Rejected, false).await
}

async fn replacement_case_with_state(
    pool: &sqlx::PgPool,
    candidates: u64,
    state: DepositState,
    old_final: bool,
) -> Result<()> {
    let address = setup(pool).await?;
    let original = insert(pool, address, 1, 10).await?;
    if state == DepositState::Detected {
        sqlx::query("UPDATE deposits SET state='detected',reason=NULL,first_unresolved_at=now() WHERE id=$1").bind(original).execute(pool).await?;
        if old_final {
            sqlx::query("UPDATE deposits SET final_at=now() WHERE id=$1")
                .bind(original)
                .execute(pool)
                .await?;
        }
        ensure!(
            db::claim_deposit(pool, Uuid::new_v4()).await?.is_none(),
            "pump must leave unresolved due finality to the watcher"
        );
    }
    let mut included = BTreeSet::new();
    for index in 2..=candidates + 1 {
        let id = insert(pool, address, index, 20).await?;
        // Already-final candidates are service-known but do not themselves need checking.
        sqlx::query("UPDATE deposits SET tx_nonce=1, final_at=now() WHERE id=$1")
            .bind(id)
            .execute(pool)
            .await?;
        included.insert(B256::from(U256::from(index)));
    }
    let mut chain = scripted(included.clone());
    chain.nonce = Some(1);
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.chain_id = CHAIN_ID;
    route.livemode = false;
    let routes = Arc::new(topup::routes::RouteSet::new(vec![route]).map_err(anyhow::Error::msg)?);
    let watch = FinalityWatch::single(pool.clone(), routes, CHAIN_ID, chain.clone(), chain.clone());
    let metric = format!("topup_finality_replacement_ambiguous_total{{chain_id=\"{CHAIN_ID}\"}} ");
    let count = |text: String| -> Result<u64> {
        Ok(text
            .lines()
            .find_map(|line| line.strip_prefix(&metric))
            .unwrap_or("0")
            .parse()?)
    };
    let before = count(topup::observability::metrics::render(pool)?)?;
    let buffer = LogBuffer::default();
    let stats = watch
        .watch_once(CHAIN_ID)
        .with_subscriber(topup::observability::log_subscriber(buffer.clone()))
        .await?;
    let logs = String::from_utf8(buffer.0.lock().expect("log buffer").clone())?;
    let anomaly: Vec<serde_json::Value> = logs
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let anomaly: Vec<_> = anomaly
        .iter()
        .filter(|event| event["fields"]["tags.alert"] == "TopupDepositReplacementAmbiguous")
        .collect();
    let reads = chain.reads.lock().expect("read recorder").clone();
    ensure!(
        reads
            .iter()
            .filter(|hash| **hash == B256::from(U256::from(1)))
            .count()
            == 2,
        "original evidence stays dual-source"
    );
    if candidates == 1 {
        ensure!(stats.reversed == 1);
        ensure!(
            reads.iter().filter(|hash| included.contains(*hash)).count() == 2,
            "one candidate on each endpoint"
        );
        ensure!(anomaly.is_empty());
    } else {
        ensure!(stats.reversed == 0 && stats.finalized == 0);
        ensure!(
            reads.len() == 2,
            "no candidate chain reads on ambiguity: {reads:?}"
        );
        let deposit = db::get_deposit(pool, original).await?.context("original")?;
        ensure!(deposit.state == state && deposit.final_at.is_none());
        ensure!(anomaly.len() == 1);
        let event = anomaly[0];
        ensure!(event["level"] == "ERROR");
        ensure!(event["fields"]["tags.chain_id"] == CHAIN_ID);
        ensure!(event["fields"]["deposit_id"] == topup::ids::format(topup::ids::DEPOSIT, original));
        let fields = event["fields"].as_object().context("fields")?;
        ensure!(
            fields.len() == 4,
            "only message, alert, chain and deposit id: {fields:?}"
        );
        ensure!(count(topup::observability::metrics::render(pool)?)? == before + 1);
    }
    Ok(())
}

#[tokio::test]
async fn unresolved_detected_deposit_reverses_only_on_watcher_replacement_proof() -> Result<()> {
    for old_final in [false, true] {
        with_database(|context| {
            Box::pin(async move {
                replacement_case_with_state(&context.app_pool, 1, DepositState::Detected, old_final)
                    .await
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn one_replacement_candidate_is_read_on_both_endpoints() -> Result<()> {
    with_database(|context| Box::pin(async move { replacement_case(&context.app_pool, 1).await }))
        .await
}

#[tokio::test]
async fn ambiguous_replacements_read_no_candidates_and_alert_without_reversing() -> Result<()> {
    for candidates in [2, 3] {
        with_database(|context| {
            Box::pin(async move { replacement_case(&context.app_pool, candidates).await })
        })
        .await?;
    }
    Ok(())
}

async fn unresolved_counts(pool: &sqlx::PgPool, now: DateTime<Utc>) -> Result<(u64, u64)> {
    let text = topup::observability::metrics::render_chain_reads_at(pool, now).await?;
    let count = |name: &str| -> Result<u64> {
        let series = format!("{name}{{chain_id=\"{CHAIN_ID}\"}} ");
        Ok(text
            .lines()
            .find_map(|line| line.strip_prefix(&series))
            .context("unresolved gauge")?
            .parse()?)
    };
    Ok((
        count("topup_finality_unresolved")?,
        count("topup_finality_unresolved_entries_24h")?,
    ))
}

async fn first_unresolved(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<DateTime<Utc>>> {
    Ok(
        sqlx::query_scalar("SELECT first_unresolved_at FROM deposits WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn unresolved_stock_and_entries_alert_at_two_and_survive_rechecks_and_restart() -> Result<()>
{
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.app_pool;
            let address = setup(pool).await?;
            let now = DateTime::from_timestamp(1_700_000_000, 0).context("clock")?;
            let first = insert(pool, address, 1, 10).await?;
            ensure!(
                unresolved_counts(pool, now).await? == (0, 0),
                "not counted before a due check"
            );
            let chain = scripted(BTreeSet::new());
            let watch = FinalityWatch::single(
                pool.clone(),
                Arc::default(),
                CHAIN_ID,
                chain.clone(),
                chain.clone(),
            );
            ensure!(watch.watch_once_at(CHAIN_ID, now).await?.watched == 1);
            ensure!(first_unresolved(pool, first).await? == Some(now));
            let (stock, entries) = unresolved_counts(pool, now).await?;
            ensure!((stock, entries) == (1, 1), "stock has no one-hour delay");
            ensure!(stock <= 1 && entries <= 1, "one entry does not alert");
            // Rebuilding the watch and refreshing from a fresh pool cannot depend on process counts.
            drop(watch);
            let restarted_pool = topup::db::connect(&context.app_url, "test", 2).await?;
            let restarted = FinalityWatch::single(
                restarted_pool.clone(),
                Arc::default(),
                CHAIN_ID,
                chain.clone(),
                chain.clone(),
            );
            ensure!(
                restarted
                    .watch_once_at(CHAIN_ID, now + TimeDelta::seconds(60))
                    .await?
                    .watched
                    == 1
            );
            ensure!(
                unresolved_counts(&restarted_pool, now + TimeDelta::seconds(60)).await? == (1, 1)
            );
            ensure!(
                first_unresolved(pool, first).await? == Some(now),
                "recheck cannot overwrite first entry time"
            );
            let second = insert(pool, address, 2, 20).await?;
            ensure!(
                restarted
                    .watch_once_at(CHAIN_ID, now + TimeDelta::seconds(61))
                    .await?
                    .watched
                    == 1
            );
            ensure!(first_unresolved(pool, second).await? == Some(now + TimeDelta::seconds(61)));
            let (stock, entries) =
                unresolved_counts(&restarted_pool, now + TimeDelta::seconds(61)).await?;
            ensure!((stock, entries) == (2, 2));
            ensure!(stock > 1 && entries > 1, "two entries alert");
            let rules: serde_json::Value =
                serde_saphyr::from_str(include_str!("../../../deploy/rpc-alerts.yaml"))?;
            for (name, expression) in [
                (
                    "RpcUnresolvedDepositStock",
                    "sum(topup_finality_unresolved) > 1",
                ),
                (
                    "RpcUnresolvedDepositEntries",
                    "sum(topup_finality_unresolved_entries_24h) > 1",
                ),
            ] {
                let rule = rules["groups"][0]["rules"]
                    .as_array()
                    .context("rules")?
                    .iter()
                    .find(|rule| rule["alert"] == name)
                    .context("unresolved alert")?;
                ensure!(rule["expr"] == expression);
                ensure!(rule["labels"]["severity"] == "critical");
            }
            restarted_pool.close().await;
            // Persisted rows from a previous process must appear without watch-side increments.
            let resolved_history = insert(pool, address, 3, 20).await?;
            let carried_history = insert(pool, address, 4, 20).await?;
            sqlx::query("UPDATE deposits SET first_unresolved_at=$2, final_at=now() WHERE id=$1")
                .bind(resolved_history)
                .bind(now)
                .execute(pool)
                .await?;
            sqlx::query("UPDATE deposits SET first_unresolved_at=$2 WHERE id=$1")
                .bind(carried_history)
                .bind(now - TimeDelta::hours(25))
                .execute(pool)
                .await?;
            let refreshed_pool = topup::db::connect(&context.app_url, "test", 2).await?;
            ensure!(
                unresolved_counts(&refreshed_pool, now + TimeDelta::seconds(61)).await? == (3, 3),
                "DB reload includes resolved recent history and old unresolved stock"
            );
            refreshed_pool.close().await;
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn resolved_unresolved_entries_remain_in_the_rolling_window_until_twenty_four_hours()
-> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.app_pool;
            let address = setup(pool).await?;
            let id = insert(pool, address, 1, 20).await?;
            let now = DateTime::from_timestamp(1_700_000_000, 0).context("clock")?;
            let chain = scripted(BTreeSet::new());
            let watch =
                FinalityWatch::single(pool.clone(), Arc::default(), CHAIN_ID, chain.clone(), chain);
            ensure!(watch.watch_once_at(CHAIN_ID, now).await?.watched == 1);
            let resolved = scripted(BTreeSet::from([B256::from(U256::from(1))]));
            let watch = FinalityWatch::single(
                pool.clone(),
                Arc::default(),
                CHAIN_ID,
                resolved.clone(),
                resolved,
            );
            ensure!(
                watch
                    .watch_once_at(CHAIN_ID, now + TimeDelta::seconds(60))
                    .await?
                    .finalized
                    == 1
            );
            ensure!(
                first_unresolved(pool, id).await? == Some(now),
                "resolution retains the entry"
            );
            ensure!(unresolved_counts(pool, now + TimeDelta::seconds(60)).await? == (0, 1));
            ensure!(
                unresolved_counts(pool, now + TimeDelta::hours(24) - TimeDelta::seconds(1)).await?
                    == (0, 1)
            );
            ensure!(unresolved_counts(pool, now + TimeDelta::hours(24)).await? == (0, 0));
            ensure!(unresolved_counts(pool, now + TimeDelta::hours(25)).await? == (0, 0));
            ensure!(
                first_unresolved(pool, id).await? == Some(now),
                "expiry does not clear durable history"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn old_final_detected_absence_keeps_s_until_fresh_terminal_proof() -> Result<()> {
    with_database(|context|Box::pin(async move {
        let p=&context.app_pool;let address=setup(p).await?;let id=insert(p,address,1,20).await?;
        let now=DateTime::from_timestamp(1_700_000_000,0).context("clock")?;
        sqlx::query("UPDATE deposits SET state='detected',reason=NULL,final_at=$2,first_unresolved_at=$2,next_attempt_at=$2,finality_check_at=$2 WHERE id=$1").bind(id).bind(now).execute(p).await?;
        let chain=scripted(BTreeSet::new());
        let watch=FinalityWatch::single(p.clone(),Arc::default(),CHAIN_ID,chain.clone(),chain.clone());
        let buffer=LogBuffer::default();
        let stats=watch.watch_once_at(CHAIN_ID,now).with_subscriber(topup::observability::log_subscriber(buffer.clone())).await?;
        ensure!(stats.watched==1 && stats.finalized==0 && stats.reversed==0,"old final marker bypassed fresh proof");
        ensure!(db::claim_deposit_at(p,Uuid::new_v4(),now+TimeDelta::minutes(10)).await?.is_none(),"pump stole S");
        ensure!(unresolved_counts(p,now).await?==(1,1),"old final S not counted");
        let logs=String::from_utf8(buffer.0.lock().expect("logs").clone())?;
        ensure!(logs.contains("log_absent_at_finality"));
        ensure!(watch.watch_once_at(CHAIN_ID,now).await?.watched==0);
        ensure!(chain.reads.lock().expect("reads").len()==2);
        let included=scripted(BTreeSet::from([hash_for_index(1)]));
        let watch=FinalityWatch::single(p.clone(),Arc::default(),CHAIN_ID,included.clone(),included.clone());
        let stats=watch.watch_once_at(CHAIN_ID,now+TimeDelta::seconds(60)).await?;
        ensure!(stats.finalized==1 && included.reads.lock().expect("reads").len()==2);
        ensure!(unresolved_counts(p,now+TimeDelta::seconds(60)).await?==(0,1));
        ensure!(first_unresolved(p,id).await?==Some(now));Ok(())
    })).await
}
fn hash_for_index(n: u64) -> B256 {
    B256::from(U256::from(n))
}
