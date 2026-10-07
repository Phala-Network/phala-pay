//! Durable dual-source checkpoint and coverage evidence, independent of N-1 RPC tables.
use super::types::{b256_hex, to_i64, to_u64};
use alloy_primitives::B256;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// A complete agreed chain boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// Inclusive block number.
    pub number: u64,
    /// Independently agreed canonical hash.
    pub hash: B256,
    /// Independently agreed block time.
    pub time: DateTime<Utc>,
}
/// Last checkpoint, if this chain has been initialized.
pub async fn checkpoint(pool: &PgPool, chain: u64) -> Result<Option<Boundary>, sqlx::Error> {
    load(pool, chain, false).await
}
/// Last fully committed dual-source coverage boundary.
pub async fn coverage(pool: &PgPool, chain: u64) -> Result<Option<Boundary>, sqlx::Error> {
    load(pool, chain, true).await
}
async fn load(pool: &PgPool, chain: u64, coverage: bool) -> Result<Option<Boundary>, sqlx::Error> {
    let query = if coverage {
        "SELECT through_block AS number, through_hash AS hash, through_time AS time FROM chain_coverage WHERE chain_id=$1"
    } else {
        "SELECT block_number AS number, block_hash AS hash, block_time AS time FROM chain_checkpoints WHERE chain_id=$1"
    };
    sqlx::query(query)
        .bind(to_i64(chain, "chain id")?)
        .fetch_optional(pool)
        .await?
        .map(|row| {
            Ok(Boundary {
                number: to_u64(row.try_get("number")?, "chain boundary")?,
                hash: row
                    .try_get::<String, _>("hash")?
                    .parse()
                    .map_err(|_| sqlx::Error::Protocol("invalid boundary hash".into()))?,
                time: row.try_get("time")?,
            })
        })
        .transpose()
}
/// Commit an agreed checkpoint without ever lowering its height.
pub async fn advance_checkpoint(
    pool: &PgPool,
    chain: u64,
    boundary: Boundary,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    super::rpc::guard_in(&mut tx, chain).await?;
    sqlx::query("INSERT INTO chain_checkpoints(chain_id,block_number,block_hash,block_time) VALUES($1,$2,$3,$4) ON CONFLICT(chain_id) DO UPDATE SET block_number=$2,block_hash=$3,block_time=$4,updated_at=now() WHERE chain_checkpoints.block_number < $2")
        .bind(to_i64(chain,"chain id")?).bind(to_i64(boundary.number,"checkpoint")?).bind(b256_hex(boundary.hash)).bind(boundary.time).execute(&mut *tx).await?;
    tx.commit().await
}
/// Initialize coverage before address issuance; upgrade/restored chains reset all address markers.
pub async fn initialize_coverage(
    pool: &PgPool,
    chain: u64,
    boundary: Boundary,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    super::rpc::guard_in(&mut tx, chain).await?;
    initialize_coverage_in(&mut tx, chain, boundary).await?;
    tx.commit().await
}
pub(crate) async fn initialize_coverage_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
    boundary: Boundary,
) -> Result<(), sqlx::Error> {
    let inserted = sqlx::query("INSERT INTO chain_coverage(chain_id,through_block,through_hash,through_time) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(to_i64(chain,"chain id")?).bind(to_i64(boundary.number,"coverage")?).bind(b256_hex(boundary.hash)).bind(boundary.time).execute(&mut **tx).await?.rows_affected();
    if inserted == 1 {
        sqlx::query("UPDATE addresses SET dual_covered_through=NULL WHERE chain_id=$1")
            .bind(to_i64(chain, "chain id")?)
            .execute(&mut **tx)
            .await?;
    }
    let number = common_coverage_in(tx, chain).await?;
    set_compat_cursor_in(
        tx,
        chain,
        number,
        (number == boundary.number).then_some(boundary.time),
    )
    .await
}
/// The negative-evidence boundary shared by every address, capped by committed chain coverage.
pub(crate) async fn common_coverage_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
) -> Result<u64, sqlx::Error> {
    let number: i64 = sqlx::query_scalar("SELECT LEAST(c.through_block, COALESCE((SELECT min(GREATEST(COALESCE(a.dual_covered_through, GREATEST(a.created_block-1,0)),GREATEST(a.created_block-1,0))) FROM addresses a WHERE a.chain_id=c.chain_id),c.through_block)) FROM chain_coverage c WHERE c.chain_id=$1")
        .bind(to_i64(chain,"chain id")?).fetch_one(&mut **tx).await?;
    to_u64(number, "common coverage")
}
/// Rebase or advance the N-1 cursor in the same transaction as the coverage it describes.
pub(crate) async fn set_compat_cursor_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
    number: u64,
    time: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO cursors(chain_id,scanned_block,scanned_block_time) VALUES($1,$2,$3) ON CONFLICT(chain_id) DO UPDATE SET scanned_block=$2,scanned_block_time=$3")
        .bind(to_i64(chain,"chain id")?).bind(to_i64(number,"common coverage")?).bind(time).execute(&mut **tx).await?;
    Ok(())
}
/// Record a chain-wide conflict using the existing audited freeze/lift gate.
pub async fn freeze(pool: &PgPool, chain: u64, check: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    super::rpc::lock_reconciliation_in(&mut tx, &format!("chain:{chain}")).await?;
    sqlx::query("SELECT chain_id FROM chain_coverage WHERE chain_id=$1 FOR UPDATE")
        .bind(to_i64(chain, "chain id")?)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO reconciliation_blocks(block_key,scope,chain_id,address_id,check_name,reason) VALUES($1,'chain',$2,NULL,$3,$3) ON CONFLICT DO NOTHING")
        .bind(format!("chain:{chain}")).bind(to_i64(chain,"chain id")?).bind(check).execute(&mut *tx).await?;
    tx.commit().await
}

/// Permanent per-chain pilot cap, counting all historical quote and reusable addresses.
pub const ISSUED_ADDRESS_CAP: i64 = 1_000;
/// Outcome of admission while holding the per-chain issuance lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressAdmission {
    /// The chain is initialized and has space for the requested addresses.
    Admitted,
    /// Dual coverage has not been initialized yet.
    NotReady,
    /// The permanent pilot quota would be exceeded.
    CapacityReached,
}
/// Current permanent usage; retirement and expiration do not free capacity.
pub async fn issued_address_count(pool: &PgPool, chain: u64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM addresses WHERE chain_id=$1")
        .bind(to_i64(chain, "chain id")?)
        .fetch_one(pool)
        .await
}
/// Serialize issuance against coverage initialization and the per-chain pilot cap.
pub async fn admit_address(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
) -> Result<AddressAdmission, sqlx::Error> {
    admit_addresses(tx, chain, 1).await
}
/// Admit a batch atomically against the same chain boundary and pilot cap.
pub async fn admit_addresses(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
    additional: usize,
) -> Result<AddressAdmission, sqlx::Error> {
    super::rpc::guard_in(tx, chain).await?;
    let boundary: Option<i64> =
        sqlx::query_scalar("SELECT through_block FROM chain_coverage WHERE chain_id=$1 FOR UPDATE")
            .bind(to_i64(chain, "chain id")?)
            .fetch_optional(&mut **tx)
            .await?;
    if boundary.is_none() {
        return Ok(AddressAdmission::NotReady);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM addresses WHERE chain_id=$1")
        .bind(to_i64(chain, "chain id")?)
        .fetch_one(&mut **tx)
        .await?;
    let additional = i64::try_from(additional)
        .map_err(|_| sqlx::Error::Protocol("address count overflow".into()))?;
    Ok(
        if count
            .checked_add(additional)
            .is_some_and(|total| total <= ISSUED_ADDRESS_CAP)
        {
            AddressAdmission::Admitted
        } else {
            AddressAdmission::CapacityReached
        },
    )
}
