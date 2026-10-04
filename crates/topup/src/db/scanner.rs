use alloy_primitives::Address as EvmAddress;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::identity::event_id;
use uuid::Uuid;

use super::deposits::{Evidence, NewDeposit, insert_deposit_in};
use super::types::{parse_address, to_i64, to_u64};

/// Address metadata required by the finalized-log scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanAddress {
    /// Address row identifier.
    pub id: Uuid,
    /// Physical EVM address.
    pub address: EvmAddress,
    /// Earliest block requiring inspection.
    pub created_block: u64,
    /// Whether the one-time pre-cursor range has been scanned.
    pub backfilled: bool,
    /// Last block through which that range has been committed while it is not complete.
    pub backfilled_through: Option<u64>,
}

impl ScanAddress {
    /// First block the address's one-time backfill still has to read: its creation block, or the
    /// block after the backfill's committed progress.
    #[must_use]
    pub fn backfill_start(&self) -> u64 {
        self.backfilled_through
            .map_or(self.created_block, |through| {
                self.created_block.max(through.saturating_add(1))
            })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ScanAddressRecord {
    id: Uuid,
    address: String,
    created_block: i64,
    backfilled: bool,
    backfilled_through: Option<i64>,
}

impl TryFrom<ScanAddressRecord> for ScanAddress {
    type Error = sqlx::Error;

    fn try_from(record: ScanAddressRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: record.id,
            address: parse_address(&record.address)?,
            created_block: to_u64(record.created_block, "addresses.created_block")?,
            backfilled: record.backfilled,
            backfilled_through: record
                .backfilled_through
                .map(|block| to_u64(block, "addresses.backfilled_through"))
                .transpose()?,
        })
    }
}

/// Scanner writes committed atomically for one block range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanCommit {
    /// Number of newly inserted deposits; duplicates are excluded.
    pub inserted: u64,
    /// Newly inserted deposits rejected as unsupported assets.
    pub unsupported_inserted: u64,
    /// Transaction and receipt positions inserted by this transaction, excluding duplicates.
    pub inserted_positions: Vec<(alloy_primitives::B256, u64)>,
}

async fn insert_rejected_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    deposit: &NewDeposit,
) -> Result<(), sqlx::Error> {
    let (account_id, livemode): (Uuid, bool) =
        sqlx::query_as("SELECT account_id, livemode FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&mut **transaction)
            .await?;
    let event = super::outbox::NewOutboxEvent::system(
        event_id("deposit.rejected", id),
        "deposit.rejected",
        crate::tenancy::Scope::new(account_id, livemode),
        super::outbox::EventObject::Deposit(id),
    );
    // A deposit is born rejected only for an asset without a route, so its representation reads
    // no route and none are needed to render it.
    debug_assert!(deposit.route.is_none());
    let no_routes = crate::routes::RouteSet::default();
    super::outbox::enqueue_in(transaction, &no_routes, &event, None).await
}

/// Returns the last completely committed block for a chain.
pub async fn get_cursor(pool: &PgPool, chain_id: u64) -> Result<Option<u64>, sqlx::Error> {
    let chain_id = to_i64(chain_id, "cursors.chain_id")?;
    let scanned_block =
        sqlx::query_scalar::<_, i64>("SELECT scanned_block FROM cursors WHERE chain_id = $1")
            .bind(chain_id)
            .fetch_optional(pool)
            .await?;
    scanned_block
        .map(|value| to_u64(value, "cursors.scanned_block"))
        .transpose()
}

/// Starts a chain's finalized cursor at `scanned_block`, the provider's `finalized` head read when
/// the chain is first configured, unless the chain has a cursor; returns whether it started one.
///
/// Addresses are issued at the committed cursor (§8), so a chain must have one before its first
/// address: without it an address would be created at block 0 and backfilled from genesis.
pub async fn initialize_cursor(
    pool: &PgPool,
    chain_id: u64,
    scanned_block: u64,
    scanned_block_time: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let started = sqlx::query(
        r#"
        INSERT INTO cursors (chain_id, scanned_block, scanned_block_time)
        VALUES ($1, $2, $3)
        ON CONFLICT (chain_id) DO NOTHING
        "#,
    )
    .bind(to_i64(chain_id, "cursors.chain_id")?)
    .bind(to_i64(scanned_block, "cursors.scanned_block")?)
    .bind(scanned_block_time)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(started == 1)
}

/// Records that the backfill of `address_ids` is committed through `through`, after the window's
/// deposits and factory events, so a retry resumes after it. Progress never moves backwards.
pub async fn record_backfill_progress(
    pool: &PgPool,
    address_ids: &[Uuid],
    through: u64,
) -> Result<(), sqlx::Error> {
    if address_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        r#"
        UPDATE addresses
        SET backfilled_through = GREATEST(COALESCE(backfilled_through, 0), $2)
        WHERE id = ANY($1) AND NOT backfilled
        "#,
    )
    .bind(address_ids)
    .bind(to_i64(through, "addresses.backfilled_through")?)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns the last block the fast scanner committed at the route confirmation, if any.
pub async fn get_confirmed_cursor(
    pool: &PgPool,
    chain_id: u64,
) -> Result<Option<u64>, sqlx::Error> {
    let chain_id = to_i64(chain_id, "cursors.chain_id")?;
    let confirmed_block = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT confirmed_block FROM cursors WHERE chain_id = $1",
    )
    .bind(chain_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    confirmed_block
        .map(|value| to_u64(value, "cursors.confirmed_block"))
        .transpose()
}

/// Commits deposits the fast scanner found at the route confirmation and advances its cursor
/// to `confirmed_block`, atomically. A receipt position whose deposits were all reversed is final,
/// so a transfer read there before finality is stale, and it is left to finalized evidence. The
/// cursor never moves backwards, and it needs a finalized cursor row: the fast scan starts above
/// the finalized scanner's range.
pub async fn commit_confirmed_scan(
    pool: &PgPool,
    chain_id: u64,
    deposits: &[NewDeposit],
    confirmed_block: u64,
) -> Result<ScanCommit, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    super::rpc::guard_in(&mut transaction, chain_id).await?;
    let result =
        commit_confirmed_scan_in(&mut transaction, chain_id, deposits, confirmed_block).await?;
    transaction.commit().await?;
    Ok(result)
}
pub(crate) async fn commit_confirmed_scan_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain_id: u64,
    deposits: &[NewDeposit],
    confirmed_block: u64,
) -> Result<ScanCommit, sqlx::Error> {
    let commit = insert_deposits_in(transaction, deposits, Evidence::Confirmed).await?;
    sqlx::query(
        r#"
        UPDATE cursors
        SET confirmed_block = GREATEST(COALESCE(confirmed_block, 0), $2)
        WHERE chain_id = $1
        "#,
    )
    .bind(to_i64(chain_id, "cursors.chain_id")?)
    .bind(to_i64(confirmed_block, "cursors.confirmed_block")?)
    .execute(&mut **transaction)
    .await?;
    Ok(commit)
}

/// Inserts a transfer read from the chain as a deposit, with its `deposit.rejected` event if it
/// is born rejected, and returns its id; `None` when its receipt position is held, or when only
/// finalized `evidence` may take it (`Evidence`).
pub(crate) async fn insert_scanned_deposit_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deposit: &NewDeposit,
    evidence: Evidence,
) -> Result<Option<Uuid>, sqlx::Error> {
    let id = insert_deposit_in(transaction, deposit, evidence).await?;
    if let Some(id) = id
        && deposit.state == DepositState::Rejected
    {
        insert_rejected_event(transaction, id, deposit).await?;
    }
    Ok(id)
}

async fn insert_deposits_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deposits: &[NewDeposit],
    evidence: Evidence,
) -> Result<ScanCommit, sqlx::Error> {
    let mut inserted = 0_u64;
    let mut unsupported_inserted = 0_u64;
    let mut inserted_positions = Vec::new();
    for deposit in deposits {
        if insert_scanned_deposit_in(transaction, deposit, evidence)
            .await?
            .is_some()
        {
            inserted_positions.push((deposit.tx_hash, deposit.receipt_log_index));
            inserted = inserted.checked_add(1).ok_or_else(|| {
                sqlx::Error::Protocol("inserted deposit count overflowed u64".to_owned())
            })?;
            if deposit.reason == Some(RejectReason::UnsupportedAsset) {
                unsupported_inserted = unsupported_inserted.checked_add(1).ok_or_else(|| {
                    sqlx::Error::Protocol("unsupported deposit count overflowed u64".to_owned())
                })?;
            }
        }
    }
    Ok(ScanCommit {
        inserted,
        unsupported_inserted,
        inserted_positions,
    })
}

/// Loads every issued address of a chain: every quote's, whatever its status, and every deposit
/// address's network on the chain, active, retired, or superseded.
pub async fn list_scan_addresses(
    pool: &PgPool,
    chain_id: u64,
) -> Result<Vec<ScanAddress>, sqlx::Error> {
    let chain_id = to_i64(chain_id, "addresses.chain_id")?;
    let records = sqlx::query_as::<_, ScanAddressRecord>(
        r#"
        SELECT id, address, created_block, backfilled, backfilled_through
        FROM addresses
        WHERE chain_id = $1
        ORDER BY created_block, id
        "#,
    )
    .bind(chain_id)
    .fetch_all(pool)
    .await?;
    records.into_iter().map(TryInto::try_into).collect()
}

/// Loads the issued address `address` of a chain, if there is one.
pub(crate) async fn find_scan_address<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    chain_id: u64,
    address: EvmAddress,
) -> Result<Option<ScanAddress>, sqlx::Error> {
    let record = sqlx::query_as::<_, ScanAddressRecord>(
        r#"
        SELECT id, address, created_block, backfilled, backfilled_through
        FROM addresses
        WHERE chain_id = $1 AND address = $2
        "#,
    )
    .bind(to_i64(chain_id, "addresses.chain_id")?)
    .bind(super::types::address_hex(address))
    .fetch_optional(executor)
    .await?;
    record.map(TryInto::try_into).transpose()
}

/// Commits deposits, backfill markers, and an optional cursor advance atomically. A cursor
/// advance also deletes the display-only pending rows it now covers.
///
/// `scanned_block_time` is the block time of `scanned_block` when the advance reaches the
/// finalized head the scanner observed; it is ignored without a cursor advance. The stored time
/// never moves backwards and stays a lower bound on the cursor block's time, so quote expiry
/// (§9) can rely on every block up to that time being committed.
///
/// A deposit born `rejected` (no route for its asset) never passes through a pump step, so its
/// `deposit.rejected` event is written here, in the same transaction and only on first insert.
/// Deposits already recorded by the fast scanner are left as they are: the insert is keyed by the
/// receipt position, and the finality watch follows their evidence, or reverses a deposit whose
/// position holds another transfer at finality and records that transfer.
pub async fn commit_scan(
    pool: &PgPool,
    chain_id: u64,
    deposits: &[NewDeposit],
    backfilled_address_ids: &[Uuid],
    scanned_block: Option<u64>,
    scanned_block_time: Option<DateTime<Utc>>,
) -> Result<ScanCommit, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let result = commit_scan_in(
        &mut transaction,
        chain_id,
        deposits,
        backfilled_address_ids,
        scanned_block,
        scanned_block_time,
    )
    .await?;
    transaction.commit().await?;
    Ok(result)
}

pub(crate) async fn commit_scan_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain_id: u64,
    deposits: &[NewDeposit],
    backfilled_address_ids: &[Uuid],
    scanned_block: Option<u64>,
    scanned_block_time: Option<DateTime<Utc>>,
) -> Result<ScanCommit, sqlx::Error> {
    let commit = insert_deposits_in(transaction, deposits, Evidence::Finalized).await?;

    if !backfilled_address_ids.is_empty() {
        sqlx::query("UPDATE addresses SET backfilled = true WHERE id = ANY($1)")
            .bind(backfilled_address_ids)
            .execute(&mut **transaction)
            .await?;
    }

    if let Some(scanned_block) = scanned_block {
        let chain_id = to_i64(chain_id, "cursors.chain_id")?;
        let scanned_block = to_i64(scanned_block, "cursors.scanned_block")?;
        sqlx::query(
            r#"
            INSERT INTO cursors (chain_id, scanned_block, scanned_block_time)
            VALUES ($1, $2, $3)
            ON CONFLICT (chain_id) DO UPDATE
            SET scanned_block = EXCLUDED.scanned_block,
                scanned_block_time = GREATEST(
                    cursors.scanned_block_time, EXCLUDED.scanned_block_time
                )
            WHERE cursors.scanned_block <= EXCLUDED.scanned_block
            "#,
        )
        .bind(chain_id)
        .bind(scanned_block)
        .bind(scanned_block_time)
        .execute(&mut **transaction)
        .await?;
        super::pending::delete_finalized_in(transaction, chain_id, scanned_block).await?;
    }

    Ok(commit)
}

/// Maximum addresses resident in a scanner/reconciler work page.
pub const ADDRESS_PAGE_SIZE: usize = 1_000;

/// Reads a keyset page of all issued addresses, including retired and superseded addresses.
/// The lookahead row is removed; `more` means the durable sweep must continue next round.
pub async fn scan_address_page(
    pool: &PgPool,
    chain_id: u64,
    after: Option<Uuid>,
) -> Result<(Vec<ScanAddress>, bool), sqlx::Error> {
    let records = sqlx::query_as::<_, ScanAddressRecord>(
        "SELECT id,address,created_block,backfilled,backfilled_through FROM addresses \
         WHERE chain_id=$1 AND id >= $2 AND ($3::uuid IS NULL OR id <> $3) \
         ORDER BY id LIMIT 1001",
    )
    .bind(to_i64(chain_id, "address page chain")?)
    .bind(after.unwrap_or(Uuid::nil()))
    .bind(after)
    .fetch_all(pool)
    .await?;
    let more = records.len() > ADDRESS_PAGE_SIZE;
    let addresses = records
        .into_iter()
        .take(ADDRESS_PAGE_SIZE)
        .map(TryInto::try_into)
        .collect::<Result<_, _>>()?;
    Ok((addresses, more))
}

/// Pinned block range and address position of a partially completed sweep.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct AddressSweep {
    pub(crate) epoch: i64,
    pub(crate) anchor: i64,
    pub(crate) from_block: i64,
    pub(crate) through_block: i64,
    pub(crate) block_time: Option<DateTime<Utc>>,
    pub(crate) horizon: Option<i64>,
    pub(crate) last_id: Uuid,
}

pub(crate) async fn address_sweep(
    pool: &PgPool,
    chain: u64,
    lane: &str,
    anchor: u64,
) -> Result<Option<AddressSweep>, sqlx::Error> {
    sqlx::query_as(
        "SELECT epoch,anchor,from_block,through_block,block_time,horizon,last_id \
         FROM scan_address_sweeps \
         WHERE chain_id=$1 \
         AND lane=$2 \
         AND anchor=$3 \
         AND epoch=COALESCE((SELECT epoch \
         FROM rpc_chain_state \
         WHERE chain_id=$1),0)",
    )
    .bind(to_i64(chain, "sweep chain")?)
    .bind(lane)
    .bind(to_i64(anchor, "sweep anchor")?)
    .fetch_optional(pool)
    .await
}

pub(crate) async fn save_address_sweep(
    pool: &PgPool,
    chain: u64,
    lane: &str,
    sweep: Option<&AddressSweep>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    super::rpc::guard_in(&mut tx, chain).await?;
    let chain = to_i64(chain, "sweep chain")?;
    if let Some(sweep) = sweep {
        let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM rpc_chain_state WHERE chain_id=$1")
            .bind(chain)
            .fetch_one(&mut *tx)
            .await?;
        if epoch != sweep.epoch {
            return Err(sqlx::Error::Protocol("sweep epoch changed".into()));
        }
        sqlx::query(
            "INSERT INTO scan_address_sweeps( \
                 chain_id,lane,epoch,anchor,from_block,through_block,block_time,horizon,last_id) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) \
             ON CONFLICT(chain_id,lane) DO UPDATE SET \
                 epoch=EXCLUDED.epoch,anchor=EXCLUDED.anchor, \
                 from_block=EXCLUDED.from_block,through_block=EXCLUDED.through_block, \
                 block_time=EXCLUDED.block_time,horizon=EXCLUDED.horizon,last_id=EXCLUDED.last_id",
        )
        .bind(chain)
        .bind(lane)
        .bind(sweep.epoch)
        .bind(sweep.anchor)
        .bind(sweep.from_block)
        .bind(sweep.through_block)
        .bind(sweep.block_time)
        .bind(sweep.horizon)
        .bind(sweep.last_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM scan_address_sweeps WHERE chain_id=$1 AND lane=$2")
            .bind(chain)
            .bind(lane)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

pub(crate) async fn sweep_epoch(pool: &PgPool, chain: u64) -> Result<i64, sqlx::Error> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT epoch FROM rpc_chain_state WHERE chain_id=$1")
            .bind(to_i64(chain, "sweep chain")?)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0),
    )
}
