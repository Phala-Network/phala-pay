use std::collections::BTreeSet;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use serde_json::json;
use sqlx::{Connection as _, PgConnection, PgPool, Row};
use tokio::time::{MissedTickBehavior, interval, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::routes::RouteSet;

use super::{Finding, ReconciliationError};

/// Persists a finding once and returns whether this call inserted it.
///
/// Every first insertion writes an audit row.
pub(crate) async fn persist_finding(
    pool: &PgPool,
    finding: &Finding,
) -> Result<bool, ReconciliationError> {
    let mut transaction = pool.begin().await?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO reconciliation_findings
            (id, fingerprint, check_name, subjects, expected, observed, repair_applied, incomplete)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (fingerprint) DO NOTHING
        "#,
    )
    .bind(finding.id)
    .bind(&finding.fingerprint)
    .bind(finding.check.code())
    .bind(serde_json::to_value(&finding.subjects)?)
    .bind(&finding.expected)
    .bind(&finding.observed)
    .bind(finding.repair_applied)
    .bind(finding.incomplete)
    .execute(&mut *transaction)
    .await?
    .rows_affected()
        == 1;

    if inserted {
        let action = if finding.repair_applied {
            "reconciliation_repair"
        } else {
            "reconciliation_mismatch"
        };
        let subject = serde_json::to_string(&finding.subjects)?;
        let reason = json!({
            "check": finding.check.code(),
            "expected": finding.expected,
            "observed": finding.observed,
        })
        .to_string();
        crate::audit::insert_with_id(
            &mut *transaction,
            Uuid::new_v5(&finding.id, b"audit"),
            &crate::audit::Entry {
                account_id: None,
                actor: &crate::audit::Actor::system("reconciler"),
                action,
                subject: &subject,
                reason: &reason,
            },
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(inserted)
}

pub(crate) async fn block_chain(
    pool: &PgPool,
    chain_id: u64,
    check_name: &str,
    reason: &str,
) -> Result<(), ReconciliationError> {
    sqlx::query(
        r#"
        INSERT INTO reconciliation_blocks
            (block_key, scope, chain_id, address_id, check_name, reason)
        VALUES ($1, 'chain', $2, NULL, $3, $4)
        ON CONFLICT (block_key) DO NOTHING
        "#,
    )
    .bind(format!("chain:{chain_id}"))
    .bind(db_i64(chain_id)?)
    .bind(check_name)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns whether a chain has a persistent reconciliation freeze.
///
/// A frozen chain pauses its scanner, pumps, finality watch, and quote creation until an operator
/// lifts the block.
pub async fn chain_is_blocked(pool: &PgPool, chain_id: u64) -> Result<bool, sqlx::Error> {
    let chain_id =
        i64::try_from(chain_id).map_err(|error| sqlx::Error::Encode(error.to_string().into()))?;
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reconciliation_blocks WHERE scope = 'chain' AND chain_id = $1) OR COALESCE((SELECT frozen OR awaiting_anchor FROM rpc_chain_state WHERE chain_id=$1),false)",
    )
    .bind(chain_id)
    .fetch_one(pool)
    .await
}

/// Returns the configured chains which reconciliation has frozen.
pub async fn frozen_chains(pool: &PgPool, routes: &RouteSet) -> Result<BTreeSet<u64>, sqlx::Error> {
    let mut frozen = BTreeSet::new();
    for chain_id in routes.chain_ids() {
        if chain_is_blocked(pool, chain_id).await? {
            frozen.insert(chain_id);
        }
    }
    Ok(frozen)
}

/// One forwarder's ledger for a token at one finalized block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ForwarderLedger {
    pub(crate) address_id: Uuid,
    pub(crate) address: Address,
    /// Deposits at or below the block that are not reversed, whatever their state.
    pub(crate) deposits: U256,
    /// Finalized `Flushed` amounts at or below the block.
    pub(crate) flushed: U256,
}

/// Returns the ledger of every backfilled forwarder of `chain_id` that holds unswept `token` funds
/// by its ledger at `block` (deposits there minus finalized `Flushed` amounts is not zero) and
/// whose deposits there are all settled: final or reversed.
///
/// A forwarder with a deposit the finality watch has not settled yet is left for a later round,
/// because its transfer could still move or disappear. A forwarder whose ledger is swept to zero
/// is not read: a transfer the ledger lacks is found by the missing-deposit check, which reads
/// every issued address's finalized transfers.
pub(crate) async fn forwarder_ledgers(
    pool: &PgPool,
    chain_id: u64,
    token: Address,
    block: u64,
    addresses: &[Uuid],
) -> Result<Vec<ForwarderLedger>, ReconciliationError> {
    let rows = sqlx::query(
        r#"
        WITH deposit_totals AS (
            SELECT d.address_id,
                   SUM(d.amount_atomic) FILTER (WHERE d.state <> 'reversed') AS total,
                   bool_or(d.final_at IS NULL AND d.state <> 'reversed') AS unsettled
            FROM deposits d
            WHERE d.chain_id = $1 AND d.asset_contract = $2 AND d.block_number <= $3 AND d.address_id = ANY($4)
            GROUP BY d.address_id
        ), flushed_totals AS (
            SELECT f.address_id, SUM(f.amount_atomic) AS total
            FROM flushed f
            WHERE f.chain_id = $1 AND f.token = $2 AND f.block_number <= $3 AND f.address_id = ANY($4)
            GROUP BY f.address_id
        )
        SELECT a.id, a.address,
               COALESCE(d.total, 0)::text AS deposits,
               COALESCE(f.total, 0)::text AS flushed
        FROM addresses a
        LEFT JOIN deposit_totals d ON d.address_id = a.id
        LEFT JOIN flushed_totals f ON f.address_id = a.id
        WHERE a.chain_id = $1 AND a.backfilled AND a.id = ANY($4)
          AND COALESCE(d.total, 0) <> COALESCE(f.total, 0)
          AND NOT COALESCE(d.unsettled, false)
        ORDER BY a.id
        "#,
    )
    .bind(db_i64(chain_id)?)
    .bind(format!("{token:#x}"))
    .bind(db_i64(block)?)
    .bind(addresses)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let address: String = row.try_get("address")?;
            let deposits: String = row.try_get("deposits")?;
            let flushed: String = row.try_get("flushed")?;
            Ok(ForwarderLedger {
                address_id: row.try_get("id")?,
                address: address
                    .parse()
                    .map_err(|_| ReconciliationError::Invariant("stored address is invalid"))?,
                deposits: parse_u256(&deposits)?,
                flushed: parse_u256(&flushed)?,
            })
        })
        .collect()
}

/// Session advisory lock key which separates lease-holding processes from the post-restore gate.
const LEASE_OWNER_LOCK: i64 = 0x746f_7075_705f_6c73;

/// Hold on the lease-owner lock, released explicitly or when its connection closes.
///
/// Every process that leases deposits holds it shared for its lifetime; the post-restore gate
/// takes it exclusively, so it cannot preempt the leases of a running process.
pub struct LeaseOwnerLock {
    connection: PgConnection,
    exclusive: bool,
}

impl LeaseOwnerLock {
    /// Pings the lock connection every `every` until `shutdown` is cancelled.
    ///
    /// The lock lives only as long as its connection. When a ping fails the lock may already be
    /// gone, so this cancels `shutdown` to stop the processes it guards and returns the error.
    /// On cancellation it returns the still-held lock, so the caller can release it once those
    /// processes have stopped.
    pub async fn watch(
        mut self,
        every: Duration,
        shutdown: CancellationToken,
    ) -> Result<Self, ReconciliationError> {
        let mut ticks = interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ok(self),
                _ = ticks.tick() => {
                    let ping = timeout(Duration::from_secs(5), self.connection.ping());
                    if let Err(error) = match ping.await {
                        Ok(result) => result,
                        Err(_) => Err(sqlx::Error::PoolTimedOut),
                    } {
                        shutdown.cancel();
                        return Err(error.into());
                    }
                }
            }
        }
    }

    /// Releases the lock and closes its connection.
    pub async fn release(mut self) -> Result<(), ReconciliationError> {
        let unlock = if self.exclusive {
            "SELECT pg_advisory_unlock($1)"
        } else {
            "SELECT pg_advisory_unlock_shared($1)"
        };
        sqlx::query_scalar::<_, bool>(unlock)
            .bind(LEASE_OWNER_LOCK)
            .fetch_one(&mut self.connection)
            .await?;
        self.connection.close().await?;
        Ok(())
    }
}

/// Takes the lease-owner lock shared on a dedicated connection.
///
/// Fails while the post-restore gate is running.
pub async fn hold_lease_owner_lock(pool: &PgPool) -> Result<LeaseOwnerLock, ReconciliationError> {
    try_lease_owner_lock(pool, false)
        .await?
        .ok_or(ReconciliationError::LeaseOwnerLock(
            "post-restore reconciliation is running",
        ))
}

/// Takes the lease-owner lock exclusively on a dedicated connection.
///
/// Fails while any lease-holding process is connected.
pub(crate) async fn exclusive_lease_owner_lock(
    pool: &PgPool,
) -> Result<LeaseOwnerLock, ReconciliationError> {
    try_lease_owner_lock(pool, true)
        .await?
        .ok_or(ReconciliationError::LeaseOwnerLock(
            "a process holding deposit leases is running; stop it before post-restore reconciliation",
        ))
}

async fn try_lease_owner_lock(
    pool: &PgPool,
    exclusive: bool,
) -> Result<Option<LeaseOwnerLock>, ReconciliationError> {
    let mut connection = timeout(Duration::from_secs(5), pool.acquire())
        .await
        .map_err(|_| ReconciliationError::LeaseOwnerLock("timed out acquiring lock connection"))??
        .detach();
    let lock = if exclusive {
        "SELECT pg_try_advisory_lock($1)"
    } else {
        "SELECT pg_try_advisory_lock_shared($1)"
    };
    let held: bool = timeout(
        Duration::from_secs(5),
        sqlx::query_scalar(lock)
            .bind(LEASE_OWNER_LOCK)
            .fetch_one(&mut connection),
    )
    .await
    .map_err(|_| ReconciliationError::LeaseOwnerLock("timed out acquiring advisory lock"))??;
    Ok(held.then_some(LeaseOwnerLock {
        connection,
        exclusive,
    }))
}

/// Returns the next block of the missing-deposit scan, if one was recorded.
pub(crate) async fn deposit_cursor(
    pool: &PgPool,
    chain_id: u64,
) -> Result<Option<u64>, ReconciliationError> {
    let next: Option<i64> = sqlx::query_scalar(
        "SELECT next_block FROM reconciliation_deposit_cursors WHERE chain_id = $1",
    )
    .bind(db_i64(chain_id)?)
    .fetch_optional(pool)
    .await?;
    next.map(db_u64).transpose()
}

fn parse_u256(value: &str) -> Result<U256, ReconciliationError> {
    value
        .parse()
        .map_err(|_| ReconciliationError::Invariant("stored atomic total is invalid"))
}

fn db_i64(value: u64) -> Result<i64, ReconciliationError> {
    i64::try_from(value)
        .map_err(|_| ReconciliationError::Invariant("value exceeds PostgreSQL bigint"))
}

fn db_u64(value: i64) -> Result<u64, ReconciliationError> {
    u64::try_from(value).map_err(|_| ReconciliationError::Invariant("stored block is negative"))
}

/// Round-robin keyset position. NULL restarts a completed pass, so old rows are rechecked.
pub(crate) async fn work_cursor(
    pool: &PgPool,
    check: &str,
    chain: i64,
) -> Result<Option<Uuid>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT last_id FROM reconciliation_work_cursors WHERE check_name=$1 AND chain_id=$2",
    )
    .bind(check)
    .bind(chain)
    .fetch_optional(pool)
    .await?
    .flatten())
}
pub(crate) async fn save_work_cursor(
    pool: &PgPool,
    check: &str,
    chain: i64,
    last: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO reconciliation_work_cursors(check_name,chain_id,last_id) VALUES($1,$2,$3) ON CONFLICT(check_name,chain_id) DO UPDATE SET last_id=EXCLUDED.last_id").bind(check).bind(chain).bind(last).execute(pool).await?;
    Ok(())
}
