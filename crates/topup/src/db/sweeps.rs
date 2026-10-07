//! Chain-sourced sweep records: the forwarder factory's finalized events for known addresses, and
//! the swept linkage they drive (design §4, §13).

use serde_json::json;
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
use topup_adapters::chain::evm::FactoryLog;
use topup_adapters::chain::flush::FactoryEvent;
use uuid::Uuid;

use super::types::{address_hex, b256_hex, to_i64};

/// Insert-only factory records must still match complete dual receipt evidence on re-coverage.
pub(crate) async fn stored_factory_evidence_matches(
    executor: impl PgExecutor<'_>,
    chain: u64,
    hash: alloy_primitives::B256,
    logs: &[FactoryLog],
) -> Result<bool, sqlx::Error> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT f.log_index,f.block_number,f.block_hash,a.address AS forwarder,f.token,\
         f.treasury,f.amount_atomic::text AS amount,NULL::text AS reason FROM flushed f \
         JOIN addresses a ON a.id=f.address_id WHERE f.chain_id=$1 AND f.tx_hash=$2 \
         UNION ALL SELECT f.log_index,f.block_number,f.block_hash,a.address,f.token,\
         NULL::text,NULL::text,f.reason FROM flush_failures f JOIN addresses a \
         ON a.id=f.address_id WHERE f.chain_id=$1 AND f.tx_hash=$2",
    )
    .bind(to_i64(chain, "factory evidence chain")?)
    .bind(b256_hex(hash))
    .fetch_all(executor)
    .await?;
    for row in rows {
        let position: i64 = row.try_get("log_index")?;
        let Some(log) = logs
            .iter()
            .find(|log| i64::try_from(log.log_index).ok() == Some(position))
        else {
            return Ok(false);
        };
        if i64::try_from(log.block_number).ok() != Some(row.try_get("block_number")?)
            || b256_hex(log.block_hash) != row.try_get::<String, _>("block_hash")?
            || address_hex(log.event.forwarder()) != row.try_get::<String, _>("forwarder")?
        {
            return Ok(false);
        }
        let token: String = row.try_get("token")?;
        let reason: Option<String> = row.try_get("reason")?;
        let matches = match &log.event {
            FactoryEvent::Flushed(event) => {
                reason.is_none()
                    && token == address_hex(event.token)
                    && row.try_get::<Option<String>, _>("treasury")?
                        == Some(address_hex(event.treasury))
                    && row.try_get::<Option<String>, _>("amount")? == Some(event.amount.to_string())
            }
            FactoryEvent::FlushFailed(event) => {
                token == address_hex(event.token)
                    && reason == Some(format!("0x{}", hex::encode(&event.reason)))
            }
            FactoryEvent::ForwarderCreated(_) => false,
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Factory events recorded by one commit; events about unknown pairs are not counted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FactoryCommit {
    /// `ForwarderCreated` events that recorded a deployment.
    pub created: u64,
    /// `Flushed` events newly recorded.
    pub flushed: u64,
    /// `FlushFailed` events newly recorded.
    pub failed: u64,
    /// Deposits marked swept by the recorded `Flushed` events.
    pub swept: u64,
}

/// Records finalized factory events about known forwarders and marks the deposits they sweep.
///
/// Anyone can call the permissionless factory, so an event counts only for an address of
/// `chain_id` whose stored treasury is the event's: `ForwarderCreated` and `Flushed` carry the
/// treasury; `FlushFailed` does not, and the forwarder address, which commits to its treasury,
/// identifies it. Every other event is ignored. Inserts are keyed by the log, so a re-read window
/// records nothing twice.
pub async fn commit_factory_logs(
    pool: &PgPool,
    chain_id: u64,
    logs: &[FactoryLog],
) -> Result<FactoryCommit, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let result = commit_factory_logs_in(&mut transaction, chain_id, logs).await?;
    transaction.commit().await?;
    Ok(result)
}

pub(crate) async fn commit_factory_logs_in(
    transaction: &mut Transaction<'_, Postgres>,
    chain_id: u64,
    logs: &[FactoryLog],
) -> Result<FactoryCommit, sqlx::Error> {
    let mut commit = FactoryCommit::default();
    if logs.is_empty() {
        return Ok(commit);
    }
    let chain_id = to_i64(chain_id, "flushed.chain_id")?;
    let mut flushed_addresses = Vec::new();
    for log in logs {
        let tx_hash = b256_hex(log.tx_hash);
        let log_index = to_i64(log.log_index, "flushed.log_index")?;
        let block_number = to_i64(log.block_number, "flushed.block_number")?;
        let block_hash = b256_hex(log.block_hash);
        match &log.event {
            FactoryEvent::ForwarderCreated(event) => {
                let updated = sqlx::query(
                    r#"
                    UPDATE addresses SET deployed_block = $4
                    WHERE chain_id = $1 AND address = $2 AND treasury = $3
                      AND deployed_block IS NULL
                    "#,
                )
                .bind(chain_id)
                .bind(address_hex(event.forwarder))
                .bind(address_hex(event.treasury))
                .bind(block_number)
                .execute(&mut **transaction)
                .await?;
                commit.created += updated.rows_affected();
            }
            FactoryEvent::Flushed(event) => {
                let address_id: Option<Uuid> = sqlx::query_scalar(
                    r#"
                    INSERT INTO flushed
                        (chain_id, tx_hash, log_index, address_id, token, treasury,
                         amount_atomic, block_number, block_hash)
                    SELECT $1, $2, $3, a.id, $5, a.treasury, $7::text::numeric, $8, $9
                    FROM addresses a
                    WHERE a.chain_id = $1 AND a.address = $4 AND a.treasury = $6
                    ON CONFLICT (chain_id, tx_hash, log_index) DO NOTHING
                    RETURNING address_id
                    "#,
                )
                .bind(chain_id)
                .bind(&tx_hash)
                .bind(log_index)
                .bind(address_hex(event.forwarder))
                .bind(address_hex(event.token))
                .bind(address_hex(event.treasury))
                .bind(event.amount.to_string())
                .bind(block_number)
                .bind(&block_hash)
                .fetch_optional(&mut **transaction)
                .await?;
                if let Some(address_id) = address_id {
                    commit.flushed += 1;
                    flushed_addresses.push(address_id);
                }
            }
            FactoryEvent::FlushFailed(event) => {
                let address_id: Option<Uuid> = sqlx::query_scalar(
                    r#"
                    INSERT INTO flush_failures
                        (chain_id, tx_hash, log_index, address_id, token, reason, block_number,
                         block_hash)
                    SELECT $1, $2, $3, a.id, $5, $6, $7, $8
                    FROM addresses a
                    WHERE a.chain_id = $1 AND a.address = $4
                    ON CONFLICT (chain_id, tx_hash, log_index) DO NOTHING
                    RETURNING address_id
                    "#,
                )
                .bind(chain_id)
                .bind(&tx_hash)
                .bind(log_index)
                .bind(address_hex(event.forwarder))
                .bind(address_hex(event.token))
                .bind(format!("0x{}", hex::encode(&event.reason)))
                .bind(block_number)
                .bind(&block_hash)
                .fetch_optional(&mut **transaction)
                .await?;
                if let Some(address_id) = address_id {
                    commit.failed += 1;
                    // A per-account condition: the merchant's treasury or token refused the
                    // transfer. It is recorded for the merchant, not raised as a platform alert.
                    tracing::warn!(
                        chain_id,
                        %address_id,
                        token = %address_hex(event.token),
                        tx_hash,
                        "a flush target failed; its deposits stay unswept"
                    );
                }
            }
        }
    }
    if !flushed_addresses.is_empty() {
        commit.swept = mark_swept(transaction, None, &flushed_addresses)
            .await?
            .len()
            .try_into()
            .unwrap_or(u64::MAX);
    }
    Ok(commit)
}

/// Marks credited deposits swept by the first finalized `Flushed` event after them, and returns
/// their ids (architecture §7).
///
/// Only a final deposit is swept, and `flushed` holds only finalized events, so the swept
/// accounting never depends on an unfinalized sweep or a transfer that could still move. It runs
/// for one deposit when it becomes final or credited (`deposit_id`), for the addresses of newly
/// indexed events (`address_ids`), and for every deposit when both are empty.
pub(crate) async fn mark_swept(
    transaction: &mut Transaction<'_, Postgres>,
    deposit_id: Option<Uuid>,
    address_ids: &[Uuid],
) -> Result<Vec<Uuid>, sqlx::Error> {
    let everything = deposit_id.is_none() && address_ids.is_empty();
    sqlx::query_scalar(
        r#"
        WITH candidate AS (
            SELECT DISTINCT ON (d.id) d.id, f.tx_hash, f.log_index
            FROM deposits d
            JOIN flushed f ON f.address_id = d.address_id AND f.token = d.asset_contract
            WHERE d.state = 'credited'
              AND d.final_at IS NOT NULL
              AND (d.block_number, d.log_index) < (f.block_number, f.log_index)
              AND ($3 OR d.id = $1 OR d.address_id = ANY($2))
            ORDER BY d.id, f.block_number, f.log_index
        ), updated AS (
            UPDATE deposits d
            SET state = 'swept', attempt = 0, lease_token = NULL, lease_until = NULL,
                updated_at = now()
            FROM candidate
            WHERE d.id = candidate.id AND d.state = 'credited'
            RETURNING d.id, candidate.tx_hash, candidate.log_index
        ), transitions AS (
            INSERT INTO transitions (id, deposit_id, from_state, to_state, attempt, evidence)
            SELECT gen_random_uuid(), id, 'credited', 'swept', 0,
                   $4::jsonb || jsonb_build_object('tx_hash', tx_hash, 'log_index', log_index)
            FROM updated
        )
        SELECT id FROM updated ORDER BY id
        "#,
    )
    .bind(deposit_id)
    .bind(address_ids)
    .bind(everything)
    .bind(json!({"outcome": "advance", "source": "finalized_flushed"}))
    .fetch_all(&mut **transaction)
    .await
}
