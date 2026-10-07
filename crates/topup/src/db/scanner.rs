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
pub async fn insert_scanned_deposit_in(
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

/// Coverage snapshot of one address, including progress unknown to N-1.
#[derive(Clone, Debug)]
pub struct CoveredAddress {
    /// Existing insertion metadata and compatibility flags.
    pub address: ScanAddress,
    /// Inclusive independently covered boundary; NULL means no dual coverage.
    pub through: Option<u64>,
}
/// Snapshot caught-up addresses, or one bounded lagging chunk ordered by oldest progress.
pub async fn coverage_addresses(
    pool: &PgPool,
    chain: u64,
    cursor: u64,
    lagging: bool,
) -> Result<Vec<CoveredAddress>, sqlx::Error> {
    let query = if lagging {
        "SELECT id,address,created_block,backfilled,backfilled_through,dual_covered_through FROM addresses WHERE chain_id=$1 AND (dual_covered_through IS NULL OR dual_covered_through < $2) ORDER BY dual_covered_through NULLS FIRST,created_block,id LIMIT 1000"
    } else {
        "SELECT id,address,created_block,backfilled,backfilled_through,dual_covered_through FROM addresses WHERE chain_id=$1 AND dual_covered_through=$2 ORDER BY id"
    };
    use sqlx::Row;
    let rows = sqlx::query(query)
        .bind(to_i64(chain, "chain id")?)
        .bind(to_i64(cursor, "coverage")?)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(CoveredAddress {
                address: ScanAddress {
                    id: row.try_get("id")?,
                    address: parse_address(&row.try_get::<String, _>("address")?)?,
                    created_block: to_u64(row.try_get("created_block")?, "created_block")?,
                    backfilled: row.try_get("backfilled")?,
                    backfilled_through: row
                        .try_get::<Option<i64>, _>("backfilled_through")?
                        .map(|v| to_u64(v, "backfilled_through"))
                        .transpose()?,
                },
                through: row
                    .try_get::<Option<i64>, _>("dual_covered_through")?
                    .map(|v| to_u64(v, "dual_covered_through"))
                    .transpose()?,
            })
        })
        .collect()
}
