//! Bounded, process-local transaction hints. Only dual-verified positive transfers are written.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alloy_primitives::B256;
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{ChainReader, FinalizedReader};
use uuid::Uuid;

use crate::{db, routes::RouteSet, scanner};

const QUEUE_CAPACITY: usize = 256;
const QUEUE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_IN_FLIGHT: usize = 4;
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
            || entries.iter().any(|entry| {
                entry.hint.chain == hint.chain
                    && entry.hint.tx == hint.tx
                    && entry.hint.object == hint.object
            })
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
        let entry = entries.iter_mut().find(|entry| {
            entry.status == Status::Pending
                && [0, 1].into_iter().all(|side| {
                    routes
                        .provider(entry.hint.chain, side)
                        .is_ok_and(|client| client.ready())
                })
        })?;
        entry.status = Status::Running;
        Some(entry.hint.clone())
    }

    fn finish(&self, hint: &Hint) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.iter_mut().find(|entry| {
            entry.hint.chain == hint.chain
                && entry.hint.tx == hint.tx
                && entry.hint.object == hint.object
        }) {
            entry.status = Status::Done;
        }
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
                let queue = self.clone();
                tasks.spawn(async move {
                    if process(&pool, &routes, &hint).await.is_err() {
                        // Do not log hashes, object ids, RPC URLs, or payer data.
                        tracing::warn!(chain = hint.chain, "hint deferred to scanner");
                    }
                    queue.finish(&hint);
                });
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

async fn process(
    pool: &PgPool,
    routes: &RouteSet,
    hint: &Hint,
) -> Result<(), scanner::ScannerError> {
    let read = routes.provider(hint.chain, 0).map_err(|_| {
        scanner::ScannerError::Configuration("hint read endpoint unavailable".into())
    })?;
    let verify = routes.provider(hint.chain, 1).map_err(|_| {
        scanner::ScannerError::Configuration("hint verify endpoint unavailable".into())
    })?;
    if !read.ready() || !verify.ready() {
        return Ok(());
    }
    let Some(chain) = scanner::chain_routes(routes)
        .into_iter()
        .find(|chain| chain.chain.chain_id == hint.chain)
    else {
        return Ok(());
    };
    let base = matches!(hint.chain, 8453 | 84532);
    let deadline = Duration::from_secs(if base { 30 } else { 90 });
    let work = async {
        if !db::daily_budgets::claim(pool, "hints", HINTS_PER_DAY).await? {
            return Ok(());
        }
        let read_reader = FinalizedReader::new(read.clone());
        let verify_reader = FinalizedReader::new(verify.clone());
        let tokens = chain.routes.keys().copied().collect::<Vec<_>>();
        let mut poll = 0_u32;
        let logs = loop {
            match read_reader
                .hint_transfers(hint.tx, hint.address.address, &tokens)
                .await?
            {
                Some(logs) => break logs,
                None => {
                    let delay = if base { 1 } else { 1_u64 << poll.min(2) };
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    poll = poll.saturating_add(1);
                }
            }
        };
        if logs.is_empty() {
            return Ok(());
        }
        let independently_decoded = verify_reader
            .hint_transfers(hint.tx, hint.address.address, &tokens)
            .await?;
        if independently_decoded.as_ref() != Some(&logs) {
            read.disagreement("hint_receipt");
            return Err(scanner::ScannerError::Disagreement);
        }
        loop {
            let read_head = read_reader
                .confirmation_heads(chain.chain.confirmations)
                .await?;
            let verify_head = verify_reader
                .confirmation_heads(chain.chain.confirmations)
                .await?;
            if logs.iter().all(|log| {
                chain
                    .chain
                    .confirmations
                    .reached(log.block_number, read_head)
                    && chain
                        .chain
                        .confirmations
                        .reached(log.block_number, verify_head)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(if base { 1 } else { 4 })).await;
        }
        let mut tx = pool.begin().await?;
        db::rpc::guard_in(&mut tx, hint.chain).await?;
        for log in logs {
            let deposit = scanner::resolve_log(log, &hint.address, &chain, chrono::Utc::now());
            if let Some(id) =
                db::insert_scanned_deposit_in(&mut tx, &deposit, db::Evidence::Confirmed).await?
            {
                sqlx::query("UPDATE deposits SET dual_verified_at=now() WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok::<_, scanner::ScannerError>(())
    };
    read.hint_call_limits(verify, async {
        match tokio::time::timeout(deadline, work).await {
            Ok(result) => result,
            Err(_) => Ok(()),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
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
