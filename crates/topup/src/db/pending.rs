//! Display-only transfers seen above the finalized head (architecture §8, §12).
//!
//! Nothing here reads or writes deposits, transitions, quotes, exposure, or reconciliation
//! state. Support, amount matching, and timeliness are computed by readers, so an
//! unfinalized transfer can never produce a stored rejection or credit.

use alloy_primitives::{Address as EvmAddress, B256};
use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgPool, Postgres, Transaction};
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use uuid::Uuid;

use super::types::{
    address_hex, atomic_decimal, b256_hex, parse_address, parse_atomic_decimal, parse_b256, to_i64,
    to_u64,
};

/// One transfer to a watched address observed above `finalized` on provider A.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewPendingTransfer {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transaction hash.
    pub tx_hash: B256,
    /// Position of the log in its transaction's receipt.
    pub receipt_log_index: u64,
    /// Block-wide log index.
    pub log_index: u64,
    /// Block number at observation time.
    pub block_number: u64,
    /// Block hash at observation time.
    pub block_hash: B256,
    /// Block timestamp.
    pub block_time: DateTime<Utc>,
    /// Receiving address row.
    pub address_id: Uuid,
    /// Token contract that emitted the event.
    pub asset_contract: EvmAddress,
    /// Transfer sender.
    pub from_address: EvmAddress,
    /// Atomic token amount.
    pub amount_atomic: AtomicAmount,
}

/// A stored pending transfer as shown to products.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTransfer {
    /// Identifier the deposit has or will have: the receipt position's revision-0 deposit.
    pub deposit_id: Uuid,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transaction hash.
    pub tx_hash: B256,
    /// Block-wide log index.
    pub log_index: u64,
    /// Block number at the last observation.
    pub block_number: u64,
    /// Block timestamp.
    pub block_time: DateTime<Utc>,
    /// Provider A latest block at the last observation.
    pub head_block: u64,
    /// Receiving address.
    pub address: EvmAddress,
    /// Token contract that emitted the event.
    pub asset_contract: EvmAddress,
    /// Transfer sender.
    pub from_address: EvmAddress,
    /// Atomic token amount.
    pub amount_atomic: AtomicAmount,
    /// First observation time.
    pub first_seen_at: DateTime<Utc>,
}

impl PendingTransfer {
    /// Blocks including the transfer's own block, as of the last head scan.
    #[must_use]
    pub fn confirmations(&self) -> u64 {
        self.head_block
            .saturating_sub(self.block_number)
            .saturating_add(1)
    }
}

/// Result of one committed head scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HeadCommit {
    /// Transfers upserted.
    pub seen: u64,
    /// Rows removed as reorged, unwatched, or finalized.
    pub removed: u64,
}

/// Replaces the pending view of blocks `from_block..=head_block` with `transfers`, in one
/// transaction: rows in that range not seen this time (reorged or no longer watched) and rows at
/// or below the finalized cursor are deleted; seen rows are upserted. Rows above `head_block`
/// (a provider briefly behind) are left for the next scan that covers them. No event is written:
/// products read unfinalized payments from `GET /v1/quotes/{id}`.
pub async fn commit_head_scan(
    pool: &PgPool,
    chain_id: u64,
    from_block: u64,
    head_block: u64,
    transfers: &[NewPendingTransfer],
) -> Result<HeadCommit, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    super::rpc::guard_in(&mut transaction, chain_id).await?;
    let result = commit_head_scan_in(
        &mut transaction,
        chain_id,
        from_block,
        head_block,
        transfers,
    )
    .await?;
    transaction.commit().await?;
    Ok(result)
}
pub(crate) async fn commit_head_scan_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain_id: u64,
    from_block: u64,
    head_block: u64,
    transfers: &[NewPendingTransfer],
) -> Result<HeadCommit, sqlx::Error> {
    commit_head_page_in(
        transaction,
        chain_id,
        from_block,
        head_block,
        transfers,
        None,
    )
    .await
}

pub(crate) async fn commit_head_page_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain_id: u64,
    from_block: u64,
    head_block: u64,
    transfers: &[NewPendingTransfer],
    addresses: Option<&[Uuid]>,
) -> Result<HeadCommit, sqlx::Error> {
    let chain = to_i64(chain_id, "pending_transfers.chain_id")?;
    let from_block = to_i64(from_block, "pending_transfers.block_number")?;
    let head = to_i64(head_block, "pending_transfers.head_block")?;

    // `FOR SHARE` makes a concurrent cursor advance wait for this transaction (or this read wait
    // for it), so a row at or below the committed cursor is never inserted after the finalized
    // scanner deleted that range.
    let cursor = sqlx::query_scalar::<_, i64>(
        "SELECT scanned_block FROM cursors WHERE chain_id = $1 FOR SHARE",
    )
    .bind(chain)
    .fetch_optional(&mut **transaction)
    .await?
    .unwrap_or(-1);
    let tx_hashes = transfers
        .iter()
        .map(|transfer| b256_hex(transfer.tx_hash))
        .collect::<Vec<_>>();
    let log_indexes = transfers
        .iter()
        .map(|transfer| to_i64(transfer.log_index, "pending_transfers.log_index"))
        .collect::<Result<Vec<_>, _>>()?;
    let removed = sqlx::query(
        r#"
        DELETE FROM pending_transfers
        WHERE chain_id = $1
          AND ($7::uuid[] IS NULL OR address_id = ANY($7))
          AND (
              block_number <= $2
              OR (
                  block_number BETWEEN $3 AND $6
                  AND (tx_hash, log_index) NOT IN (
                      SELECT seen.tx_hash, seen.log_index
                      FROM unnest($4::text[], $5::bigint[]) AS seen (tx_hash, log_index)
                  )
              )
          )
        "#,
    )
    .bind(chain)
    .bind(cursor)
    .bind(from_block)
    .bind(&tx_hashes)
    .bind(&log_indexes)
    .bind(head)
    .bind(addresses)
    .execute(&mut **transaction)
    .await?
    .rows_affected();

    let mut commit = HeadCommit {
        removed,
        ..HeadCommit::default()
    };
    for transfer in transfers {
        let block_number = to_i64(transfer.block_number, "pending_transfers.block_number")?;
        if block_number <= cursor {
            continue;
        }
        upsert(transaction, chain, head, transfer).await?;
        commit.seen = commit.seen.saturating_add(1);
    }
    Ok(commit)
}

async fn upsert(
    transaction: &mut Transaction<'_, Postgres>,
    chain: i64,
    head: i64,
    transfer: &NewPendingTransfer,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO pending_transfers (
            chain_id, tx_hash, log_index, block_number, block_hash, block_time, head_block,
            address_id, asset_contract, from_address, amount_atomic, receipt_log_index
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::text::numeric, $12)
        ON CONFLICT (chain_id, tx_hash, log_index) DO UPDATE
        SET block_number = EXCLUDED.block_number,
            receipt_log_index = EXCLUDED.receipt_log_index,
            block_hash = EXCLUDED.block_hash,
            block_time = EXCLUDED.block_time,
            head_block = EXCLUDED.head_block,
            address_id = EXCLUDED.address_id,
            asset_contract = EXCLUDED.asset_contract,
            from_address = EXCLUDED.from_address,
            amount_atomic = EXCLUDED.amount_atomic
        "#,
    )
    .bind(chain)
    .bind(b256_hex(transfer.tx_hash))
    .bind(to_i64(transfer.log_index, "pending_transfers.log_index")?)
    .bind(to_i64(
        transfer.block_number,
        "pending_transfers.block_number",
    )?)
    .bind(b256_hex(transfer.block_hash))
    .bind(transfer.block_time)
    .bind(head)
    .bind(transfer.address_id)
    .bind(address_hex(transfer.asset_contract))
    .bind(address_hex(transfer.from_address))
    .bind(atomic_decimal(transfer.amount_atomic))
    .bind(to_i64(
        transfer.receipt_log_index,
        "pending_transfers.receipt_log_index",
    )?)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Deletes pending rows the finalized scanner now covers; runs in the cursor-advance transaction.
pub(crate) async fn delete_finalized_in(
    transaction: &mut Transaction<'_, Postgres>,
    chain_id: i64,
    scanned_block: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM pending_transfers WHERE chain_id = $1 AND block_number <= $2")
        .bind(chain_id)
        .bind(scanned_block)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct PendingRecord {
    address_id: Uuid,
    chain_id: i64,
    tx_hash: String,
    receipt_log_index: i64,
    log_index: i64,
    block_number: i64,
    block_time: DateTime<Utc>,
    head_block: i64,
    address: String,
    asset_contract: String,
    from_address: String,
    amount_atomic: String,
    first_seen_at: DateTime<Utc>,
}

impl TryFrom<PendingRecord> for PendingTransfer {
    type Error = sqlx::Error;

    fn try_from(record: PendingRecord) -> Result<Self, Self::Error> {
        let chain_id = to_u64(record.chain_id, "pending_transfers.chain_id")?;
        let tx_hash = parse_b256(&record.tx_hash)?;
        let log_index = to_u64(record.log_index, "pending_transfers.log_index")?;
        let receipt_log_index = to_u64(
            record.receipt_log_index,
            "pending_transfers.receipt_log_index",
        )?;
        Ok(Self {
            deposit_id: deposit_id(chain_id, tx_hash, receipt_log_index),
            chain_id,
            tx_hash,
            log_index,
            block_number: to_u64(record.block_number, "pending_transfers.block_number")?,
            block_time: record.block_time,
            head_block: to_u64(record.head_block, "pending_transfers.head_block")?,
            address: parse_address(&record.address)?,
            asset_contract: parse_address(&record.asset_contract)?,
            from_address: parse_address(&record.from_address)?,
            amount_atomic: parse_atomic_decimal(&record.amount_atomic)?,
            first_seen_at: record.first_seen_at,
        })
    }
}

// A receipt position with a reversed deposit is final (a deposit is reversed only by final
// evidence, §7), so a row there is a stale read dual coverage has not deleted yet: it is
// not shown. Every row shown is at a position with no deposit, or with the revision-0 deposit the
// fast scan recorded from it, so its deposit id is the position's revision-0 id.
const PENDING_SELECT: &str = r#"
    SELECT pending.address_id, pending.chain_id, pending.tx_hash, pending.receipt_log_index, pending.log_index,
           pending.block_number,
           pending.block_time, pending.head_block, address.address, pending.asset_contract,
           pending.from_address, pending.amount_atomic::text AS amount_atomic,
           pending.first_seen_at
    FROM pending_transfers AS pending
    JOIN addresses AS address ON address.id = pending.address_id
    WHERE pending.block_number > COALESCE(
        (SELECT scan.scanned_block FROM cursors AS scan WHERE scan.chain_id = pending.chain_id),
        -1
    )
      AND NOT EXISTS (
          SELECT 1 FROM deposits AS reversed
          WHERE reversed.chain_id = pending.chain_id AND reversed.tx_hash = pending.tx_hash
            AND reversed.receipt_log_index = pending.receipt_log_index
            AND reversed.state = 'reversed'
      )
"#;

/// Pending transfers to one address, oldest first.
pub async fn list_address_pending<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    address_id: Uuid,
) -> Result<Vec<PendingTransfer>, sqlx::Error> {
    let query = format!(
        "{PENDING_SELECT} AND pending.address_id = $1 \
         ORDER BY pending.block_number, pending.log_index, pending.tx_hash"
    );
    let records = sqlx::query_as::<_, PendingRecord>(AssertSqlSafe(query))
        .bind(address_id)
        .fetch_all(executor)
        .await?;
    records.into_iter().map(TryInto::try_into).collect()
}

/// Pending transfers for a page of scoped address ids, oldest first within each address.
/// Callers obtain the ids from an authenticated parent query; no arbitrary client ids belong here.
pub async fn list_addresses_pending<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    address_ids: &[Uuid],
) -> Result<Vec<(Uuid, PendingTransfer)>, sqlx::Error> {
    if address_ids.is_empty() {
        return Ok(Vec::new());
    }
    let query = format!(
        "{PENDING_SELECT} AND pending.address_id = ANY($1) \
         ORDER BY pending.address_id, pending.block_number, pending.log_index, pending.tx_hash"
    );
    let records = sqlx::query_as::<_, PendingRecord>(AssertSqlSafe(query))
        .bind(address_ids)
        .fetch_all(executor)
        .await?;
    records
        .into_iter()
        .map(|record| {
            let address_id = record.address_id;
            PendingTransfer::try_from(record).map(|transfer| (address_id, transfer))
        })
        .collect()
}
