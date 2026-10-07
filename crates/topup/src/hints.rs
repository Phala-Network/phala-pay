//! Bounded, process-local transaction hints. Only dual-verified positive transfers are written.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use alloy_primitives::B256;
use prometheus::{IntCounterVec, Opts, core::Collector};
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{ChainError, ChainReader, FinalizedReader};
use uuid::Uuid;

use crate::{db, routes::RouteSet, scanner};

const QUEUE_CAPACITY: usize = 256;
const QUEUE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_IN_FLIGHT: usize = 4;
fn base_family(chain: u64) -> bool {
    matches!(chain, 8453 | 84532)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Recorded,
    Exhausted,
    Deferred,
    Parked,
}
impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Exhausted => "exhausted",
            Self::Deferred => "deferred",
            Self::Parked => "parked",
        }
    }
}
static METRICS: OnceLock<Result<IntCounterVec, prometheus::Error>> = OnceLock::new();
fn metrics() -> Result<&'static IntCounterVec, &'static prometheus::Error> {
    METRICS
        .get_or_init(|| {
            IntCounterVec::new(
                Opts::new(
                    "topup_hint_total",
                    "Hint budget claims and bounded task outcomes.",
                ),
                &["result"],
            )
        })
        .as_ref()
}
fn count(result: &'static str) {
    if let Ok(metrics) = metrics() {
        metrics.with_label_values(&[result]).inc();
    }
}
pub(crate) fn collect_metrics() -> Result<Vec<prometheus::proto::MetricFamily>, prometheus::Error> {
    metrics()
        .map(Collector::collect)
        .map_err(|_| prometheus::Error::Msg("hint metrics initialization failed".into()))
}
/// Hard number of hint tasks per environment and UTC day.
pub const HINTS_PER_DAY: i32 = 150;

/// A server-derived object network. No caller-supplied recipient is used.
#[derive(Clone, Debug)]
pub(crate) struct Hint {
    pub chain: u64,
    pub tx: B256,
    pub object: Uuid,
    pub address: db::ScanAddress,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pending,
    Running,
    Done,
}
struct Entry {
    hint: Hint,
    expires: Instant,
    status: Status,
}

/// Bounded queue and deduplication state, shared by the API and its supervised worker.
#[derive(Default)]
pub struct HintQueue {
    entries: Mutex<VecDeque<Entry>>,
}
impl HintQueue {
    pub(crate) fn enqueue(&self, hint: Hint) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|entry| entry.status == Status::Running || entry.expires > now);
        if entries.len() >= QUEUE_CAPACITY
            || entries
                .iter()
                .any(|entry| entry.status != Status::Running && same(&entry.hint, &hint))
        {
            return;
        }
        entries.push_back(Entry {
            hint,
            expires: now + QUEUE_TTL,
            status: Status::Pending,
        });
    }

    /// Pending tasks, including hints parked until both endpoints are ready.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|entry| entry.status == Status::Pending && entry.expires > Instant::now())
            .count()
    }

    fn take_ready(&self, routes: &RouteSet) -> Option<Hint> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|entry| entry.status == Status::Running || entry.expires > now);
        let index = entries.iter().position(|entry| {
            entry.status == Status::Pending
                && !entries.iter().any(|running| {
                    running.status == Status::Running && same(&running.hint, &entry.hint)
                })
                && [0, 1].into_iter().all(|side| {
                    routes
                        .provider(entry.hint.chain, side)
                        .is_ok_and(|client| client.ready())
                })
        })?;
        entries[index].status = Status::Running;
        Some(entries[index].hint.clone())
    }

    fn finish(&self, hint: &Hint, status: Status) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut kept = false;
        entries.retain_mut(|entry| {
            if !same(&entry.hint, hint) {
                return true;
            }
            if kept {
                return false;
            }
            kept = true;
            entry.status = status;
            true
        });
    }

    /// Runs at most four tasks, stops all of them on shutdown, and never persists pending work.
    pub async fn run(
        self: Arc<Self>,
        pool: PgPool,
        routes: Arc<RouteSet>,
        cancellation: CancellationToken,
    ) {
        let mut tasks = JoinSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                result = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = result {
                        tracing::error!(%error, "hint worker task stopped");
                    }
                }
                _ = tick.tick() => {}
            }
            while tasks.len() < MAX_IN_FLIGHT {
                let Some(hint) = self.take_ready(&routes) else {
                    break;
                };
                let pool = pool.clone();
                let routes = routes.clone();
                let lease = RunningLease {
                    queue: self.clone(),
                    hint: hint.clone(),
                    status: Status::Pending,
                };
                tasks.spawn(async move {
                    let outcome = match process(&pool, &routes, &hint).await {
                        Ok(outcome) => outcome,
                        Err(scanner::ScannerError::Chain(ChainError::HintCallLimit)) => {
                            Outcome::Exhausted
                        }
                        Err(scanner::ScannerError::Disagreement) => {
                            tracing::warn!(chain = hint.chain, "confirmed hint evidence disagreed");
                            Outcome::Deferred
                        }
                        Err(_) => Outcome::Deferred,
                    };
                    count(outcome.label());
                    // Routine pending/exhausted work is scanner fallback, not an incident.
                    tracing::debug!(
                        chain = hint.chain,
                        outcome = outcome.label(),
                        "hint task finished"
                    );
                    lease.finish(if outcome == Outcome::Parked {
                        Status::Pending
                    } else {
                        Status::Done
                    });
                });
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

fn same(left: &Hint, right: &Hint) -> bool {
    left.chain == right.chain && left.tx == right.tx && left.object == right.object
}
/// Cancellation or panic releases the running entry. Only parked/done entries deduplicate
/// admission; a queued duplicate cannot run while its original task is still active.
struct RunningLease {
    queue: Arc<HintQueue>,
    hint: Hint,
    status: Status,
}
impl RunningLease {
    fn finish(mut self, status: Status) {
        self.status = status;
    }
}
impl Drop for RunningLease {
    fn drop(&mut self) {
        self.queue.finish(&self.hint, self.status);
    }
}

async fn process(
    pool: &PgPool,
    routes: &RouteSet,
    hint: &Hint,
) -> Result<Outcome, scanner::ScannerError> {
    let read = routes.provider(hint.chain, 0).map_err(|_| {
        scanner::ScannerError::Configuration("hint read endpoint unavailable".into())
    })?;
    let verify = routes.provider(hint.chain, 1).map_err(|_| {
        scanner::ScannerError::Configuration("hint verify endpoint unavailable".into())
    })?;
    if !read.ready() || !verify.ready() {
        return Ok(Outcome::Parked);
    }
    let Some(chain) = scanner::chain_routes(routes)
        .into_iter()
        .find(|chain| chain.chain.chain_id == hint.chain)
    else {
        return Ok(Outcome::Deferred);
    };
    let work = async {
        if crate::reconciler::chain_is_blocked(pool, hint.chain).await? {
            return Ok(Outcome::Deferred);
        }
        // Readiness can change after dequeue, including while the freeze check awaits a DB slot.
        if !read.ready() || !verify.ready() {
            return Ok(Outcome::Parked);
        }
        if !db::daily_budgets::claim(pool, "hints", HINTS_PER_DAY).await? {
            return Ok(Outcome::Exhausted);
        }
        count("budget_spent");
        let read_reader = FinalizedReader::new(read.clone());
        let verify_reader = FinalizedReader::new(verify.clone());
        let tokens = chain.routes.keys().copied().collect::<Vec<_>>();
        let mut poll = 0_u32;
        let mut target = loop {
            if let Some(receipt) = read_reader.hint_receipt(hint.tx).await? {
                break receipt.block_number();
            }
            let delay = if base_family(hint.chain) {
                1
            } else {
                1_u64 << poll.min(2)
            };
            tokio::time::sleep(Duration::from_secs(delay)).await;
            poll = poll.saturating_add(1);
        };
        let mut confirmation_poll =
            tokio::time::interval(Duration::from_secs(if base_family(hint.chain) {
                1
            } else {
                4
            }));
        confirmation_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let evidence = loop {
            confirmation_poll.tick().await;
            let read_head = read_reader
                .confirmation_heads(chain.chain.confirmations)
                .await?;
            // Spend verify calls only once read has reached the required depth.
            if !chain.chain.confirmations.reached(target, read_head) {
                continue;
            }
            let verify_head = verify_reader
                .confirmation_heads(chain.chain.confirmations)
                .await?;
            if !chain.chain.confirmations.reached(target, verify_head) {
                continue;
            }
            // Receipt visibility can lag the head. Poll cheaply until verify includes the tx
            // at depth, then refresh read's receipt too, so a reorg during the wait cannot
            // turn the original discovery receipt into a dual-verified record.
            let Some(verified) = verify_reader.hint_receipt(hint.tx).await? else {
                continue;
            };
            if !chain
                .chain
                .confirmations
                .reached(verified.block_number(), read_head)
                || !chain
                    .chain
                    .confirmations
                    .reached(verified.block_number(), verify_head)
            {
                target = target.max(verified.block_number());
                continue;
            }
            let Some(current) = read_reader.hint_receipt(hint.tx).await? else {
                continue;
            };
            target = current.block_number();
            if !chain.chain.confirmations.reached(target, read_head)
                || !chain.chain.confirmations.reached(target, verify_head)
            {
                continue;
            }
            let evidence = current.complete(hint.address.address, &tokens).await?;
            let independent = verified.complete(hint.address.address, &tokens).await?;
            if independent != evidence {
                read.disagreement("eth_getTransactionReceipt");
                return Err(scanner::ScannerError::Disagreement);
            }
            if evidence.transfers.is_empty() {
                return Ok(Outcome::Deferred);
            }
            break evidence;
        };
        let mut tx = pool.begin().await?;
        db::rpc::guard_in(&mut tx, hint.chain).await?;
        // RPC runs outside the chain lock. A changed address snapshot must be retried
        // by scanning, even if its new creation boundary would still admit this transfer.
        let current: Option<(String, i64)> = sqlx::query_as(
            "SELECT address,created_block FROM addresses WHERE id=$1 AND chain_id=$2",
        )
        .bind(hint.address.id)
        .bind(i64::try_from(hint.chain).map_err(|_| scanner::ScannerError::SnapshotChanged)?)
        .fetch_optional(&mut *tx)
        .await?;
        let expected = (
            format!("{:#x}", hint.address.address),
            i64::try_from(hint.address.created_block)
                .map_err(|_| scanner::ScannerError::SnapshotChanged)?,
        );
        if current.as_ref() != Some(&expected) {
            return Ok(Outcome::Deferred);
        }
        let mut recorded = false;
        for log in evidence.transfers {
            // Match coverage scanning's inclusive address creation boundary, using the
            // freshly agreed inclusion rather than the initial discovery receipt.
            if log.block_number < hint.address.created_block {
                continue;
            }
            let deposit = scanner::resolve_log(log, &hint.address, &chain, chrono::Utc::now());
            // Inspect the full position history after RPC and under the chain lock. Hints
            // have confirmation evidence only: identical reversed evidence is dropped,
            // and changed evidence waits for the scanner's finalized successor path.
            // insert_scanned_deposit_in already refuses Confirmed inserts at positions
            // with history; this explicit check keeps the reversal policy clear here.
            let reversed: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM deposits WHERE chain_id=$1 AND tx_hash=$2 \
                 AND receipt_log_index=$3 AND state='reversed')",
            )
            .bind(i64::try_from(hint.chain).map_err(|_| scanner::ScannerError::SnapshotChanged)?)
            .bind(format!("{:#x}", deposit.tx_hash))
            .bind(
                i64::try_from(deposit.receipt_log_index)
                    .map_err(|_| scanner::ScannerError::SnapshotChanged)?,
            )
            .fetch_one(&mut *tx)
            .await?;
            if reversed {
                continue;
            }
            if let Some(id) =
                db::insert_scanned_deposit_in(&mut tx, &deposit, db::Evidence::Confirmed).await?
            {
                // Both timestamps use PostgreSQL's transaction clock; rollback can prove that
                // the hint, rather than a later scanner pass, wrote this verification marker.
                sqlx::query("UPDATE deposits SET dual_verified_at=created_at WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                recorded = true;
            }
        }
        tx.commit().await?;
        Ok(if recorded {
            Outcome::Recorded
        } else {
            Outcome::Deferred
        })
    };
    read.hint_call_limits(verify, bounded_task(hint.chain, work))
        .await
}

async fn bounded_task(
    chain: u64,
    work: impl Future<Output = Result<Outcome, scanner::ScannerError>>,
) -> Result<Outcome, scanner::ScannerError> {
    let deadline = Duration::from_secs(if base_family(chain) { 30 } else { 90 });
    match tokio::time::timeout(deadline, work).await {
        Ok(result) => result,
        Err(_) => Ok(Outcome::Exhausted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn whole_task_deadlines_cancel_work_without_a_result_signal() {
        for (chain, seconds) in [(1, 90), (11155111, 90), (8453, 30), (84532, 30)] {
            let start = Instant::now();
            let wrote = std::sync::atomic::AtomicBool::new(false);
            bounded_task(chain, async {
                tokio::time::sleep(Duration::from_secs(120)).await;
                wrote.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Outcome::Deferred)
            })
            .await
            .unwrap();
            assert_eq!(start.elapsed(), Duration::from_secs(seconds));
            assert!(!wrote.load(std::sync::atomic::Ordering::SeqCst));
        }
    }
    fn hint(object: Uuid, chain: u64) -> Hint {
        Hint {
            chain,
            tx: B256::repeat_byte(1),
            object,
            address: db::ScanAddress {
                id: Uuid::new_v4(),
                address: alloy_primitives::Address::repeat_byte(2),
                created_block: 0,
                backfilled: false,
                backfilled_through: None,
            },
        }
    }
    #[tokio::test]
    async fn panic_releases_running_task_and_consolidates_queued_duplicates() {
        let queue = Arc::new(HintQueue::default());
        let hint = hint(Uuid::new_v4(), 1);
        queue.enqueue(hint.clone());
        queue.entries.lock().unwrap()[0].status = Status::Running;
        // Admission can preserve a retry while the original is running, but only one retry.
        queue.enqueue(hint.clone());
        queue.enqueue(hint.clone());
        assert_eq!(queue.pending(), 1);
        let lease = RunningLease {
            queue: queue.clone(),
            hint: hint.clone(),
            status: Status::Pending,
        };
        let task = tokio::spawn(async move {
            let _lease = lease;
            panic!("fixture task panic");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(queue.pending(), 1);
        assert!(
            queue
                .entries
                .lock()
                .unwrap()
                .iter()
                .all(|entry| entry.status != Status::Running)
        );
        queue.enqueue(hint.clone());
        assert_eq!(queue.pending(), 1);
        queue.finish(&hint, Status::Done);
        queue.enqueue(hint);
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn hint_metrics_expose_budget_and_all_outcomes() {
        for result in [
            "budget_spent",
            "recorded",
            "exhausted",
            "deferred",
            "parked",
        ] {
            count(result);
        }
        let families = collect_metrics().unwrap();
        let family = families
            .iter()
            .find(|family| family.name() == "topup_hint_total")
            .unwrap();
        for result in [
            "budget_spent",
            "recorded",
            "exhausted",
            "deferred",
            "parked",
        ] {
            assert!(family.get_metric().iter().any(|metric| {
                metric
                    .get_label()
                    .iter()
                    .any(|label| label.name() == "result" && label.value() == result)
                    && metric.get_counter().get_value() >= 1.0
            }));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deduplicates_by_chain_tx_object_and_bounds_pending_hints_and_ttl() {
        let queue = HintQueue::default();
        let object = Uuid::new_v4();
        queue.enqueue(hint(object, 1));
        queue.enqueue(hint(object, 1));
        assert_eq!(queue.pending(), 1);
        queue.enqueue(hint(object, 8453));
        queue.enqueue(hint(Uuid::new_v4(), 1));
        assert_eq!(queue.pending(), 3);
        for _ in 0..300 {
            queue.enqueue(hint(Uuid::new_v4(), 1));
        }
        assert_eq!(queue.pending(), QUEUE_CAPACITY);
        assert!(queue.take_ready(&RouteSet::default()).is_none());
        assert_eq!(queue.pending(), QUEUE_CAPACITY);
        tokio::time::advance(QUEUE_TTL).await;
        assert_eq!(queue.pending(), 0);
        queue.enqueue(hint(object, 1));
        assert_eq!(queue.pending(), 1);
    }
}
