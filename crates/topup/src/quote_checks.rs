//! On-demand read-only discovery; admission persists before network I/O.
use crate::{db, routes::RouteSet, scanner};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Atomically admit at most three checks per quote/rolling day, spaced by 15 seconds per address.
/// Locks serialize different quotes for one address. Exhausted/closed work claims no RPC budget.
pub async fn claim_at(
    pool: &PgPool,
    quote: Uuid,
    chain: u64,
    limit: u32,
    now: DateTime<Utc>,
) -> Result<Option<Uuid>, sqlx::Error> {
    if limit == 0 {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    let address: Option<Uuid> = sqlx::query_scalar(
        "SELECT a.id FROM quotes q JOIN addresses a ON a.id=q.address_id \
         WHERE q.id=$1 AND a.chain_id=$2 AND q.status='open' AND q.expires_at > $3 FOR UPDATE OF a",
    )
    .bind(quote)
    .bind(i64::try_from(chain).map_err(|_| sqlx::Error::Protocol("invalid chain".into()))?)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(address) = address else {
        return Ok(None);
    };
    // Recheck after acquiring the address lock; a concurrent quote close may have committed.
    let open: bool =
        sqlx::query_scalar("SELECT status='open' AND expires_at > $2 FROM quotes WHERE id=$1")
            .bind(quote)
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
    if !open {
        return Ok(None);
    }
    let (checks, recent): (i64,bool) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE quote_id=$1 AND checked_at > $3::timestamptz - interval '24 hours'), \
         COALESCE(bool_or(address_id=$2 AND checked_at > $3::timestamptz - interval '15 seconds'),false) \
         FROM quote_checks WHERE quote_id=$1 OR (address_id=$2 AND checked_at > $3::timestamptz - interval '15 seconds')")
        .bind(quote).bind(address).bind(now).fetch_one(&mut *tx).await?;
    if checks >= 3 || recent {
        return Ok(None);
    }
    let claimed = db::daily_budgets::claim_on(
        &mut *tx,
        now.date_naive(),
        &format!("quote_check:{chain}"),
        i32::try_from(limit).map_err(|_| sqlx::Error::Protocol("invalid check budget".into()))?,
    )
    .await?;
    if !claimed {
        return Ok(None);
    }
    sqlx::query("INSERT INTO quote_checks(quote_id,address_id,checked_at) VALUES($1,$2,$3)")
        .bind(quote)
        .bind(address)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(address))
}

#[derive(Clone, Copy)]
pub(crate) struct Check {
    pub quote: Uuid,
    pub address: Uuid,
    pub chain: u64,
}
struct Entry {
    check: Check,
    running: bool,
}
/// At most 256 queued/running checks, coalesced by physical address, with four active workers.
#[derive(Default)]
pub struct CheckQueue {
    entries: Mutex<VecDeque<Entry>>,
}
impl CheckQueue {
    /// Queued and running address checks, useful for bounded queue health and admission tests.
    pub fn pending(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
    pub(crate) fn enqueue(&self, check: Check) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= 256 || entries.iter().any(|e| e.check.address == check.address) {
            return;
        }
        entries.push_back(Entry {
            check,
            running: false,
        });
    }
    fn take(&self) -> Option<Check> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.iter_mut().find(|e| !e.running)?;
        entry.running = true;
        Some(entry.check)
    }
    fn finish(&self, address: Uuid) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|e| e.check.address != address);
    }
    /// Supervised immediate work, bounded to four tasks and a 30-second deadline per pass.
    /// Pending work is process-local; a restart falls back to normal scanning, preserving claims.
    pub async fn run(
        self: Arc<Self>,
        pool: PgPool,
        routes: Arc<RouteSet>,
        cancellation: CancellationToken,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = tick.tick() => {},
                done = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = done { tracing::warn!(%error,"quote check worker stopped"); }
                }
            }
            while tasks.len() < 4 {
                let Some(check) = self.take() else {
                    break;
                };
                let lease = Lease {
                    queue: self.clone(),
                    address: check.address,
                };
                let pool = pool.clone();
                let routes = routes.clone();
                tasks.spawn(async move {
                    let _lease = lease;
                    let work = async {
                        if crate::reconciler::chain_is_blocked(&pool, check.chain).await? {
                            return Ok(());
                        }
                        let Some(chain) = scanner::chain_routes(&routes)
                            .into_iter()
                            .find(|c| c.chain.chain_id == check.chain)
                        else {
                            return Ok(());
                        };
                        let (Ok(read), Ok(verify)) = (
                            routes.provider(check.chain, 0),
                            routes.provider(check.chain, 1),
                        ) else {
                            return Ok(());
                        };
                        if !read.contract_ready() || !verify.contract_ready() {
                            return Ok(());
                        }
                        scanner::check_quote_once_at(
                            &pool,
                            &topup_adapters::chain::evm::FinalizedReader::new(read.clone()),
                            &chain,
                            routes.discovery(),
                            check.quote,
                            Utc::now(),
                        )
                        .await?;
                        Ok::<_, scanner::ScannerError>(())
                    };
                    if !matches!(
                        tokio::time::timeout(Duration::from_secs(30), work).await,
                        Ok(Ok(()))
                    ) {
                        tracing::warn!(
                            chain_id = check.chain,
                            "quote discovery check deferred to normal scanning"
                        );
                    }
                });
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}
struct Lease {
    queue: Arc<CheckQueue>,
    address: Uuid,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.queue.finish(self.address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_quote_checks_share_one_address_task_even_while_running() {
        let queue = CheckQueue::default();
        let check = Check {
            quote: Uuid::new_v4(),
            address: Uuid::new_v4(),
            chain: 1,
        };
        queue.enqueue(check);
        queue.enqueue(Check {
            quote: Uuid::new_v4(),
            ..check
        });
        assert!(queue.take().is_some());
        queue.enqueue(check);
        assert!(queue.take().is_none());
        queue.finish(check.address);
        queue.enqueue(check);
        assert!(queue.take().is_some());
    }
}
