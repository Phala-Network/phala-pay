//! Bounded, durable follow-up reads of the state immediately before address discovery starts.

use std::collections::BTreeMap;
use std::time::Duration;

use alloy_primitives::Address;
use sqlx::Row;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Reconciler, ReconciliationError};

const BATCH: i64 = 1_000;
const POLL: Duration = Duration::from_secs(60);

struct Pending {
    id: Uuid,
    address: Address,
}

impl Reconciler {
    /// Captures recent issuance pins outside the synchronous quote path. Failed jobs back off
    /// six hours; a manual reconciliation can retry them immediately after an operator repairs RPC.
    pub async fn run_opening_loop(&self, cancellation: CancellationToken) {
        let mut ticks = interval(POLL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = ticks.tick() => {},
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                result = capture(self, false) => {
                    if let Err(error) = result {
                        tracing::error!(%error, "custody opening capture failed");
                    }
                }
            }
        }
    }
}

pub(super) async fn capture(
    reconciler: &Reconciler,
    force: bool,
) -> Result<(), ReconciliationError> {
    let _capture = reconciler.opening_capture.lock().await;
    for route in reconciler.latest_asset_routes() {
        let chain_id = route.chain.chain_id;
        let token = format!("{:#x}", route.asset.contract);
        let chain_key = super::store::db_i64(chain_id)?;
        let Some(checkpoint) =
            crate::db::chain_reads::checkpoint(&reconciler.pool, chain_id).await?
        else {
            continue;
        };
        sqlx::query(
            "INSERT INTO custody_opening_jobs(address_id,token,start_block) \
             SELECT a.id,$2,a.created_block FROM addresses a \
             WHERE a.chain_id=$1 AND a.created_block>0 \
             AND a.created_block-1 <= $3 \
             AND NOT EXISTS (SELECT 1 FROM custody_opening_balances b \
                 WHERE b.address_id=a.id AND b.token=$2 AND b.start_block=a.created_block) \
             ON CONFLICT DO NOTHING",
        )
        .bind(chain_key)
        .bind(&token)
        .bind(super::store::db_i64(checkpoint.number)?)
        .execute(&reconciler.pool)
        .await?;
        let rows = sqlx::query(
            "WITH due AS ( \
                SELECT j.address_id,j.start_block,a.address \
                FROM custody_opening_jobs j JOIN addresses a ON a.id=j.address_id \
                WHERE a.chain_id=$1 AND j.token=$2 AND j.start_block=a.created_block \
                AND j.start_block-1 <= $5 AND ($3 OR j.next_attempt_at<=now()) \
                AND NOT EXISTS (SELECT 1 FROM custody_opening_balances b \
                    WHERE b.address_id=j.address_id AND b.token=j.token AND b.start_block=j.start_block) \
                ORDER BY j.attempts,j.created_at DESC,j.address_id LIMIT $4 \
                FOR UPDATE OF j SKIP LOCKED \
             ) UPDATE custody_opening_jobs j \
             SET attempts=j.attempts+1,next_attempt_at=now()+interval '6 hours' \
             FROM due WHERE j.address_id=due.address_id AND j.token=$2 AND j.start_block=due.start_block \
             RETURNING due.address_id,due.start_block,due.address",
        )
        .bind(chain_key).bind(&token).bind(force).bind(BATCH)
        .bind(super::store::db_i64(checkpoint.number)?)
        .fetch_all(&reconciler.pool).await?;
        let mut by_start = BTreeMap::<u64, Vec<Pending>>::new();
        for row in rows {
            let start: i64 = row.try_get("start_block")?;
            let address: String = row.try_get("address")?;
            by_start
                .entry(
                    u64::try_from(start)
                        .map_err(|_| ReconciliationError::Invariant("invalid opening start"))?,
                )
                .or_default()
                .push(Pending {
                    id: row.try_get("address_id")?,
                    address: address
                        .parse()
                        .map_err(|_| ReconciliationError::Invariant("invalid opening address"))?,
                });
        }
        if by_start.is_empty() {
            continue;
        }
        let read = reconciler.chain(chain_id)?;
        let verify = reconciler
            .verifiers
            .get(&chain_id)
            .ok_or(ReconciliationError::Invariant("custody verifier missing"))?;
        let mut failed_count = 0_i64;
        let mut failure_reason = "opening_evidence_unavailable";
        for (start, pending) in by_start {
            let block = start
                .checked_sub(1)
                .ok_or(ReconciliationError::Invariant("invalid opening start"))?;
            let addresses = pending.iter().map(|row| row.address).collect::<Vec<_>>();
            let evidence = async {
                let (a, b) =
                    tokio::try_join!(read.canonical_header(block), verify.canonical_header(block))?;
                if a != b || (block == checkpoint.number && a != checkpoint.hash) {
                    return Err(ReconciliationError::Chain(
                        "dual opening headers disagreed with canonical pin".into(),
                    ));
                }
                let (a_balance, b_balance) = tokio::try_join!(
                    read.token_balances_pinned(route.asset.contract, &addresses, a),
                    verify.token_balances_pinned(route.asset.contract, &addresses, a),
                )?;
                if a_balance.len() != addresses.len() || a_balance != b_balance {
                    return Err(ReconciliationError::Chain(
                        "dual opening balances disagreed".into(),
                    ));
                }
                Ok::<_, ReconciliationError>((a, a_balance))
            }
            .await;
            let (hash, balances) = match evidence {
                Ok(evidence) => evidence,
                Err(error) => {
                    failed_count = failed_count
                        .checked_add(i64::try_from(pending.len()).map_err(|_| {
                            ReconciliationError::Invariant("opening count overflow")
                        })?)
                        .ok_or(ReconciliationError::Invariant("opening count overflow"))?;
                    failure_reason = error.code();
                    continue;
                }
            };
            let ids = pending.iter().map(|row| row.id).collect::<Vec<_>>();
            let balances = balances.iter().map(ToString::to_string).collect::<Vec<_>>();
            let mut transaction = reconciler.pool.begin().await?;
            crate::db::rpc::guard_in(&mut transaction, chain_id).await?;
            sqlx::query(
                "INSERT INTO custody_opening_balances(address_id,token,start_block,block_hash,amount_atomic) \
                 SELECT a.id,$2,$3,$4,e.amount::numeric \
                 FROM unnest($1::uuid[],$5::text[]) AS e(id,amount) JOIN addresses a ON a.id=e.id \
                 WHERE a.chain_id=$6 AND a.created_block=$3 ON CONFLICT DO NOTHING",
            )
            .bind(&ids).bind(&token).bind(super::store::db_i64(start)?)
            .bind(format!("{hash:#x}")).bind(&balances).bind(chain_key)
            .execute(&mut *transaction).await?;
            transaction.commit().await?;
        }
        if failed_count > 0 {
            unavailable(chain_id, &route.route, failed_count, failure_reason);
        }
    }
    Ok(())
}

pub(super) fn unavailable(chain: u64, route: &str, count: i64, reason: &str) {
    tracing::warn!(
        tags.alert = "TopupCustodyOpeningUnavailable",
        tags.chain_id = chain,
        tags.route = route,
        excluded_forwarders = count,
        reason,
        "custody waits for independently verified opening balances"
    );
}
