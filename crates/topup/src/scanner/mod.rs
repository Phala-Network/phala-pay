//! Read-only fast discovery and atomic dual-source finalized coverage (§2.1–2.2).
use crate::{db, routes::RouteSet};
use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use db::chain_reads::{self, Boundary};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FinalizedHead, FinalizedReader, TransferLog,
};
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::route::{ChainConfig, Confirmations};

/// Read discovery cadence; coverage is independent of fast cursor advances.
pub const FAST_INTERVAL: Duration = Duration::from_secs(300);
/// Validate and publish finalized checkpoints independently of log coverage.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(600);
/// Complete negative evidence cadence, staggered across configured chains.
pub const COVERAGE_INTERVAL: Duration = Duration::from_secs(3_600);
/// Observation-only chains retain their contract recovery cadence.
pub const CONTRACT_RECOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// Inclusive normal round limit.
pub const COVERAGE_LIMIT: u64 = 3_000;
/// Every sixth round catches up at this inclusive limit.
pub const CATCHUP_LIMIT: u64 = 19_200;
/// All scanner cadences are fixed by the attested design.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanConfig;
/// Per-chain checkpoint announcements used by finality and reconciliation.
#[derive(Clone, Debug)]
pub struct FinalizedHeads(Arc<tokio::sync::watch::Sender<BTreeMap<u64, FinalizedHead>>>);
impl Default for FinalizedHeads {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(BTreeMap::new())))
    }
}
impl FinalizedHeads {
    /// Publish an agreed checkpoint advance.
    pub fn publish(&self, chain: u64, head: FinalizedHead) -> bool {
        self.0.send_if_modified(|heads| {
            if heads
                .get(&chain)
                .is_some_and(|old| old.number >= head.number)
            {
                return false;
            }
            heads.insert(chain, head);
            true
        })
    }
    /// Last announced checkpoint.
    pub fn get(&self, chain: u64) -> Option<FinalizedHead> {
        self.0.borrow().get(&chain).copied()
    }
    /// Subscribe to checkpoint advances.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<BTreeMap<u64, FinalizedHead>> {
        self.0.subscribe()
    }
}
/// A failed round commits no ledger, coverage, address, compatibility cursor or cleanup writes.
#[derive(Debug, thiserror::Error)]
pub enum ScannerError {
    /// Invalid route settings.
    #[error("invalid scanner configuration: {0}")]
    Configuration(String),
    /// One endpoint could not supply complete evidence.
    #[error("{0}")]
    Chain(#[from] ChainError),
    /// Persistence failed.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
    /// Independently derived evidence disagreed.
    #[error("dual-source chain evidence disagreed")]
    Disagreement,
    /// Addresses, coverage or the unverified deposit set changed during the read round.
    #[error("coverage snapshot changed; retry the round")]
    SnapshotChanged,
    /// A transfer did not pay a snapshotted address.
    #[error("transfer recipient {0:#x} is not tracked")]
    UnknownRecipient(Address),
    /// A supervised task stopped.
    #[error("scanner task failed: {0}")]
    Task(String),
}
/// Current token routing of a single payment chain.
#[derive(Clone, Debug)]
pub struct ChainRoutes {
    /// Attested chain configuration.
    pub chain: ChainConfig,
    pub(crate) routes: BTreeMap<Address, (String, u64)>,
}
/// Group current token routes by payment chain.
pub fn chain_routes(routes: &RouteSet) -> Vec<ChainRoutes> {
    routes
        .chain_ids()
        .filter_map(|chain| {
            Some(ChainRoutes {
                chain: routes.chain(chain)?.clone(),
                routes: routes
                    .current()
                    .filter(|r| r.chain.chain_id == chain)
                    .map(|r| (r.asset.contract, (r.route.clone(), r.version)))
                    .collect(),
            })
        })
        .collect()
}
/// Counts from one committed coverage round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Newly inserted receipt positions.
    pub inserted: u64,
    /// Actually covered end, never the distant checkpoint.
    pub cursor: u64,
    /// Agreed checkpoint height.
    pub finalized: u64,
    /// Addresses whose own dual backfill reached the cursor.
    pub backfilled_addresses: u64,
    /// Additional history remains for later scheduled rounds.
    pub work_remaining: bool,
    /// Independently reverified factory events.
    pub factory: db::FactoryCommit,
}
/// Initialize every healthy chain before the API can issue on it. Failed chains retry in their loop.
pub async fn initialize_cursors(pool: &PgPool, routes: &RouteSet) -> Result<(), ScannerError> {
    db::rpc::check_start(pool, &routes.chain_ids().collect::<Vec<_>>()).await?;
    for chain in routes.chain_ids() {
        let a = routes
            .provider(chain, 0)
            .map_err(|e| ScannerError::Configuration(e.to_string()))?;
        let b = routes
            .provider(chain, 1)
            .map_err(|e| ScannerError::Configuration(e.to_string()))?;
        if (!a.contract_ready() || !b.contract_ready())
            && !crate::contracts::check_and_enforce(pool, a, b, chain, routes.routes()).await?
        {
            continue;
        }
        let read = FinalizedReader::new(
            routes
                .provider(chain, 0)
                .map_err(|e| ScannerError::Configuration(e.to_string()))?
                .clone(),
        );
        let verify = FinalizedReader::new(
            routes
                .provider(chain, 1)
                .map_err(|e| ScannerError::Configuration(e.to_string()))?
                .clone(),
        );
        if let Err(error) = initialize_chain(pool, chain, &read, &verify).await {
            tracing::warn!(chain_id=chain,%error,"chain initialization waits for both endpoints");
        }
    }
    Ok(())
}
/// New chains start at the agreed checkpoint; upgrades include the oldest address's creation block.
pub async fn initialize_chain<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    chain: u64,
    read: &R,
    verify: &V,
) -> Result<Boundary, ScannerError> {
    let checkpoint = crate::checkpoint::advance(pool, chain, read, verify).await?;
    let chain_key = i64::try_from(chain).map_err(|_| ScannerError::SnapshotChanged)?;
    for _ in 0..3 {
        let existing = chain_reads::coverage(pool, chain).await?;
        let first: Option<i64> =
            sqlx::query_scalar("SELECT min(created_block) FROM addresses WHERE chain_id=$1")
                .bind(chain_key)
                .fetch_one(pool)
                .await?;
        let boundary = if let Some(existing) = existing {
            existing
        } else {
            let start = first
                .map(|n| u64::try_from(n).map(|n| n.saturating_sub(1)))
                .transpose()
                .map_err(|_| ScannerError::SnapshotChanged)?
                .unwrap_or(checkpoint.number)
                .min(checkpoint.number);
            let (a, b) = tokio::try_join!(read.header(start), verify.header(start))?;
            if a != b {
                return Err(ScannerError::Disagreement);
            }
            Boundary {
                number: start,
                hash: a.0,
                time: a.1,
            }
        };
        let mut tx = pool.begin().await?;
        db::rpc::guard_in(&mut tx, chain).await?;
        let durable: Option<i64> =
            sqlx::query_scalar("SELECT through_block FROM chain_coverage WHERE chain_id=$1")
                .bind(chain_key)
                .fetch_optional(&mut *tx)
                .await?;
        let current_first: Option<i64> =
            sqlx::query_scalar("SELECT min(created_block) FROM addresses WHERE chain_id=$1")
                .bind(chain_key)
                .fetch_one(&mut *tx)
                .await?;
        if durable.and_then(|n| u64::try_from(n).ok()) != existing.map(|b| b.number)
            || current_first != first
        {
            tx.rollback().await?;
            continue;
        }
        chain_reads::initialize_coverage_in(&mut tx, chain, boundary).await?;
        tx.commit().await?;
        return Ok(boundary);
    }
    Err(ScannerError::SnapshotChanged)
}
/// Fast candidates remain provisional; PR 3 uses the same public insertion boundary and receipt key.
pub async fn fast_once<R: ChainReader>(
    pool: &PgPool,
    read: &R,
    routes: &ChainRoutes,
) -> Result<db::ScanCommit, ScannerError> {
    let chain = routes.chain.chain_id;
    let coverage = chain_reads::coverage(pool, chain)
        .await?
        .ok_or(ScannerError::SnapshotChanged)?;
    let latest = read.latest_header().await?.number;
    let heads = match routes.chain.confirmations {
        Confirmations::Depth(_) => topup_core::route::ChainHeads {
            latest: Some(latest),
            safe: None,
            finalized: coverage.number,
        },
        Confirmations::Finalized => topup_core::route::ChainHeads {
            latest: Some(latest),
            safe: None,
            finalized: coverage.number,
        },
        Confirmations::Safe => read.confirmation_heads(routes.chain.confirmations).await?,
    };
    let cursor = db::get_confirmed_cursor(pool, chain)
        .await?
        .unwrap_or(coverage.number);
    let from = cursor.saturating_add(1);
    let addresses = db::list_scan_addresses(pool, chain).await?;
    let index: BTreeMap<_, _> = addresses.into_iter().map(|a| (a.address, a)).collect();
    tracing::debug!(
        chain_id = chain,
        from,
        latest,
        addresses = index.len(),
        "fast discovery range"
    );
    let mut deposits = Vec::new();
    let mut pending = Vec::new();
    if from <= latest {
        let recipients: Vec<_> = index.keys().copied().collect();
        for log in read.transfer_logs_to(&recipients, from, latest).await? {
            let Some(address) = index.get(&log.to) else {
                return Err(ScannerError::UnknownRecipient(log.to));
            };
            if !routes.routes.contains_key(&log.token)
                || log.block_number < address.created_block
                || log.amount.value().is_zero()
            {
                continue;
            }
            if routes.chain.confirmations.reached(log.block_number, heads) {
                deposits.push(resolve_log(log, address, routes, Utc::now()));
            } else if log.block_number > coverage.number {
                pending.push(db::NewPendingTransfer {
                    chain_id: chain,
                    tx_hash: log.tx_hash,
                    receipt_log_index: log.receipt_log_index,
                    log_index: log.log_index,
                    block_number: log.block_number,
                    block_hash: log.block_hash,
                    block_time: log.block_time,
                    address_id: address.id,
                    asset_contract: log.token,
                    from_address: log.from,
                    amount_atomic: log.amount,
                });
            }
        }
    }
    let mut tx = pool.begin().await?;
    tracing::debug!(
        chain_id = chain,
        deposits = deposits.len(),
        pending = pending.len(),
        "fast discovery candidates"
    );
    db::rpc::guard_in(&mut tx, chain).await?;
    let commit = db::scanner::commit_confirmed_scan_in(
        &mut tx,
        chain,
        &deposits,
        routes.chain.confirmations.horizon(heads),
    )
    .await?;
    if from <= latest {
        db::pending::commit_head_scan_in(&mut tx, chain, from, latest, &pending).await?;
    }
    tx.commit().await?;
    Ok(commit)
}
struct Range {
    addresses: Vec<db::CoveredAddress>,
    from: u64,
    end: u64,
}
impl Range {
    fn start(&self, address: &db::CoveredAddress) -> u64 {
        address
            .through
            .map_or(address.address.created_block, |n| {
                n.saturating_add(1).max(address.address.created_block)
            })
            .max(self.from)
    }
}

fn same_transfer_evidence(existing: &db::Deposit, deposit: &db::NewDeposit) -> bool {
    existing.address_id == deposit.address_id
        && existing.block_number == deposit.block_number
        && existing.block_hash == deposit.block_hash
        && existing.block_time == deposit.block_time
        && existing.log_index == deposit.log_index
        && existing.asset_contract == deposit.asset_contract
        && existing.from_address == deposit.from_address
        && existing.amount_atomic == deposit.amount_atomic
        && existing.tx_from == Some(deposit.tx_from)
        && existing.tx_nonce == Some(deposit.tx_nonce)
}

/// One bounded coverage round. Every sixth hourly round uses the approved six-hour catch-up bound.
pub async fn coverage_once<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    read: &R,
    verify: &V,
    routes: &ChainRoutes,
    round: u64,
) -> Result<ScanStats, ScannerError> {
    let chain = routes.chain.chain_id;
    let cursor = match chain_reads::coverage(pool, chain).await? {
        Some(cursor) => cursor,
        None => initialize_chain(pool, chain, read, verify).await?,
    };
    let checkpoint = chain_reads::checkpoint(pool, chain)
        .await?
        .ok_or(ScannerError::Disagreement)?;
    let limit = if round.is_multiple_of(6) {
        CATCHUP_LIMIT
    } else {
        COVERAGE_LIMIT
    };
    let end = checkpoint.number.min(cursor.number.saturating_add(limit));
    if end < cursor.number {
        return Err(ScannerError::Disagreement);
    }
    let (a, b) = tokio::try_join!(read.header(end), verify.header(end))?;
    if (end == checkpoint.number && (a.0 != checkpoint.hash || b.0 != checkpoint.hash))
        || (end == cursor.number && (a.0 != cursor.hash || b.0 != cursor.hash))
    {
        chain_reads::freeze(pool, chain, "finalized_checkpoint_conflict").await?;
        tracing::error!(
            tags.alert = "TopupFinalizedCheckpointConflict",
            chain_id = chain,
            "stored coverage boundary hash changed; chain frozen"
        );
        return Err(ScannerError::Disagreement);
    }
    if a != b {
        return Err(ScannerError::Disagreement);
    }
    let chain_key = i64::try_from(chain).map_err(|_| ScannerError::SnapshotChanged)?;
    let address_snapshot: Vec<(uuid::Uuid, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id,created_block,dual_covered_through FROM addresses WHERE chain_id=$1 ORDER BY id",
    )
    .bind(chain_key)
    .fetch_all(pool)
    .await?;
    let caught = db::coverage_addresses(pool, chain, cursor.number, false).await?;
    let lagging = db::coverage_addresses(pool, chain, cursor.number, true).await?;
    let mut ranges = Vec::new();
    if end > cursor.number {
        ranges.push(Range {
            addresses: caught,
            from: cursor.number.saturating_add(1),
            end,
        });
    }
    if let Some(start) = lagging
        .iter()
        .map(|a| {
            a.through.map_or(a.address.created_block, |n| {
                n.saturating_add(1).max(a.address.created_block)
            })
        })
        .min()
    {
        ranges.push(Range {
            addresses: lagging,
            from: start,
            end: end.min(start.saturating_add(limit).saturating_sub(1)),
        });
    }
    let mut candidates = BTreeSet::new();
    let mut factory_candidates = BTreeSet::new();
    let mut index: BTreeMap<_, _> = db::list_scan_addresses(pool, chain)
        .await?
        .into_iter()
        .map(|a| (a.address, a))
        .collect();
    for range in &ranges {
        if range.from > range.end {
            continue;
        }
        let recipients: Vec<_> = range.addresses.iter().map(|a| a.address.address).collect();
        let (a, b) = tokio::try_join!(
            read.coverage_logs(
                routes.chain.contracts.forwarder_factory,
                &recipients,
                range.from,
                range.end
            ),
            verify.coverage_logs(
                routes.chain.contracts.forwarder_factory,
                &recipients,
                range.from,
                range.end
            )
        )?;
        for address in &range.addresses {
            index.insert(address.address.address, address.address.clone());
            let start = range.start(address);
            // Insert-only factory evidence is reverified even when both getLogs responses omit it.
            let hashes: Vec<String> = sqlx::query_scalar("SELECT tx_hash FROM flushed WHERE chain_id=$1 AND address_id=$2 AND block_number BETWEEN $3 AND $4 UNION SELECT tx_hash FROM flush_failures WHERE chain_id=$1 AND address_id=$2 AND block_number BETWEEN $3 AND $4")
                .bind(i64::try_from(chain).map_err(|_|ScannerError::SnapshotChanged)?).bind(address.address.id).bind(i64::try_from(start).map_err(|_|ScannerError::SnapshotChanged)?).bind(i64::try_from(range.end).map_err(|_|ScannerError::SnapshotChanged)?).fetch_all(pool).await?;
            for hash in hashes {
                factory_candidates.insert(
                    hash.parse::<B256>()
                        .map_err(|_| ScannerError::SnapshotChanged)?,
                );
            }
        }
        for (transfers, factory) in [a, b] {
            for log in transfers {
                if range
                    .addresses
                    .iter()
                    .any(|a| a.address.address == log.to && log.block_number >= range.start(a))
                {
                    candidates.insert((log.tx_hash, log.receipt_log_index));
                }
            }
            for log in factory {
                if range.addresses.iter().any(|a| {
                    a.address.address == log.event.forwarder() && log.block_number >= range.start(a)
                }) {
                    factory_candidates.insert(log.tx_hash);
                }
            }
        }
    }
    // The prospective common boundary includes every address, including those outside
    // this bounded backfill page. Fetch its header before taking the DB-only commit lock.
    let common = address_snapshot
        .iter()
        .try_fold(end, |common, (id, created, through)| {
            let created = u64::try_from(*created).map_err(|_| ScannerError::SnapshotChanged)?;
            let proposed = ranges
                .iter()
                .filter(|r| {
                    r.addresses
                        .iter()
                        .any(|a| a.address.id == *id && r.start(a) <= r.end)
                })
                .map(|r| r.end)
                .max();
            let through = through
                .map(u64::try_from)
                .transpose()
                .map_err(|_| ScannerError::SnapshotChanged)?;
            Ok::<_, ScannerError>(
                common.min(
                    proposed
                        .or(through)
                        .unwrap_or(created.saturating_sub(1))
                        .max(created.saturating_sub(1)),
                ),
            )
        })?;
    let common_time = if common == end {
        a.1
    } else {
        let (a, b) = tokio::try_join!(read.header(common), verify.header(common))?;
        if a != b {
            return Err(ScannerError::Disagreement);
        }
        a.1
    };
    let mut factory_receipts = Vec::new();
    for hash in factory_candidates {
        let (a, b) = tokio::try_join!(
            read.factory_receipt(hash, routes.chain.contracts.forwarder_factory),
            verify.factory_receipt(hash, routes.chain.contracts.forwarder_factory)
        )?;
        if a != b {
            return Err(ScannerError::Disagreement);
        }
        factory_receipts.push((hash, a.ok_or(ScannerError::Disagreement)?));
    }
    let mut evidence = BTreeMap::new();
    // Any change to the unverified set invalidates this round's RPC evidence.
    // Abandon the round after the DB-only lock recheck; the next round reads afresh.
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM deposits WHERE chain_id=$1 AND dual_verified_at IS NULL AND state <> 'reversed'",
    ).bind(chain_key).fetch_all(pool).await?;
    for (_, deposit) in db::deposits_by_ids(pool, &ids).await? {
        let deposit = deposit?;
        candidates.insert((deposit.tx_hash, deposit.receipt_log_index));
    }
    for &(hash, position) in &candidates {
        let marker: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
            "SELECT dual_verified_at FROM deposits WHERE chain_id=$1 AND tx_hash=$2 AND receipt_log_index=$3 AND state <> 'reversed'",
        ).bind(chain_key).bind(format!("{hash:#x}")).bind(i64::try_from(position).map_err(|_|ScannerError::SnapshotChanged)?)
            .fetch_optional(pool).await?;
        if marker.flatten().is_some() {
            continue;
        }
        let (a, b) = tokio::try_join!(
            read.receipt_transfer(hash, position),
            verify.receipt_transfer(hash, position)
        )?;
        if a != b {
            return Err(ScannerError::Disagreement);
        }
        evidence.insert((hash, position), a);
    }
    let mut tx = pool.begin().await?;
    db::rpc::guard_in(&mut tx, chain).await?;
    let durable: i64 =
        sqlx::query_scalar("SELECT through_block FROM chain_coverage WHERE chain_id=$1 FOR UPDATE")
            .bind(chain_key)
            .fetch_one(&mut *tx)
            .await?;
    let current_addresses: Vec<(uuid::Uuid, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id,created_block,dual_covered_through FROM addresses WHERE chain_id=$1 ORDER BY id",
    )
    .bind(chain_key)
    .fetch_all(&mut *tx)
    .await?;
    if u64::try_from(durable).ok() != Some(cursor.number) || current_addresses != address_snapshot {
        return Err(ScannerError::SnapshotChanged);
    }
    let locked_ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM deposits WHERE chain_id=$1 AND dual_verified_at IS NULL AND state <> 'reversed' FOR UPDATE",
    ).bind(chain_key).fetch_all(&mut *tx).await?;
    if ids.iter().collect::<BTreeSet<_>>() != locked_ids.iter().collect::<BTreeSet<_>>() {
        tx.rollback().await?;
        return Err(ScannerError::SnapshotChanged);
    }
    let mut unverified = BTreeMap::<_, Vec<db::Deposit>>::new();
    for (_, deposit) in db::deposits_by_ids(&mut *tx, &locked_ids).await? {
        let deposit = deposit?;
        unverified
            .entry((deposit.tx_hash, deposit.receipt_log_index))
            .or_default()
            .push(deposit);
    }
    let mut deposits = Vec::new();
    let mut corrections = Vec::new();
    for (&(hash, position), evidence) in &evidence {
        let Some(log) = evidence.transfer() else {
            continue;
        }; // absence never releases a held quote
        if log.block_number > end {
            continue;
        }
        // A provisional identity keeps its original recipient until the dual finality
        // reversal/successor transaction releases that receipt position.
        if let Some(records) = unverified.get(&(hash, position)) {
            let changed_recipient = records.iter().any(|existing| {
                index
                    .get(&log.to)
                    .is_none_or(|address| existing.address_id != address.id)
            });
            if changed_recipient {
                if records
                    .iter()
                    .any(|existing| existing.state != DepositState::Detected)
                {
                    tx.rollback().await?;
                    chain_reads::freeze(pool, chain, "unverified_evidence_mismatch").await?;
                    tracing::error!(
                        tags.alert = "TopupUnverifiedEvidenceMismatch",
                        chain_id = chain,
                        "agreed evidence contradicts a permanent record; chain frozen"
                    );
                    return Err(ScannerError::Disagreement);
                }
                let next = index
                    .get(&log.to)
                    .filter(|address| log.block_number >= address.created_block)
                    .map(|address| resolve_log(log.clone(), address, routes, Utc::now()));
                for existing in records {
                    let mut evidence = serde_json::json!({"stage":"coverage","result":"reversed","reason":"recipient_changed"});
                    if crate::finality::reverse_in(
                        &mut tx,
                        existing.id,
                        existing.state,
                        existing.attempt,
                        &mut evidence,
                        next.as_ref(),
                    )
                    .await?
                    .is_none()
                    {
                        return Err(ScannerError::SnapshotChanged);
                    }
                }
                continue;
            }
        }
        let address = index
            .get(&log.to)
            .ok_or(ScannerError::UnknownRecipient(log.to))?;
        let covered_end = ranges
            .iter()
            .filter(|range| {
                range
                    .addresses
                    .iter()
                    .any(|a| a.address.id == address.id && range.start(a) <= range.end)
            })
            .map(|range| range.end)
            .max();
        let already_covered: Option<i64> =
            sqlx::query_scalar("SELECT dual_covered_through FROM addresses WHERE id=$1")
                .bind(address.id)
                .fetch_one(&mut *tx)
                .await?;
        let target = covered_end
            .into_iter()
            .chain(already_covered.and_then(|n| u64::try_from(n).ok()))
            .max();
        if target.is_none_or(|target| log.block_number > target) {
            continue;
        }
        let deposit = resolve_log(log.clone(), address, routes, Utc::now());
        if let Some(records) = unverified.get(&(hash, position)) {
            for existing in records {
                let equal = same_transfer_evidence(existing, &deposit);
                if !equal && existing.state != DepositState::Detected {
                    tx.rollback().await?;
                    chain_reads::freeze(pool, chain, "unverified_evidence_mismatch").await?;
                    tracing::error!(
                        tags.alert = "TopupUnverifiedEvidenceMismatch",
                        chain_id = chain,
                        "agreed evidence contradicts a permanent record; chain frozen"
                    );
                    return Err(ScannerError::Disagreement);
                }
                corrections.push((existing.id, existing.state, deposit.clone()));
            }
        } else {
            deposits.push(deposit);
        }
    }
    let mut factory = Vec::new();
    for (hash, receipt) in &factory_receipts {
        if !db::sweeps::stored_factory_evidence_matches(&mut *tx, chain, *hash, &receipt.logs)
            .await?
        {
            tx.rollback().await?;
            chain_reads::freeze(pool, chain, "unverified_evidence_mismatch").await?;
            tracing::error!(
                tags.alert = "TopupUnverifiedEvidenceMismatch",
                chain_id = chain,
                "agreed evidence contradicts a permanent record; chain frozen"
            );
            return Err(ScannerError::Disagreement);
        }
        factory.extend(
            receipt
                .logs
                .iter()
                .filter(|log| log.block_number <= end && index.contains_key(&log.event.forwarder()))
                .cloned(),
        );
    }
    let mut stats = ScanStats {
        cursor: end,
        finalized: checkpoint.number,
        work_remaining: end < checkpoint.number,
        ..ScanStats::default()
    };
    for deposit in &deposits {
        // An insert followed by a reversal during RPC leaves the active-id snapshot
        // unchanged. Inspect all position history under the lock before recording cached
        // evidence, so a reversed payment can never re-enter as a fresh finalized revision.
        let history_ids: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM deposits WHERE chain_id=$1 AND tx_hash=$2 AND receipt_log_index=$3 ORDER BY revision DESC FOR UPDATE",
        ).bind(chain_key).bind(format!("{:#x}", deposit.tx_hash))
            .bind(i64::try_from(deposit.receipt_log_index).map_err(|_| ScannerError::SnapshotChanged)?)
            .fetch_all(&mut *tx).await?;
        let mut history = BTreeMap::new();
        for (id, record) in db::deposits_by_ids(&mut *tx, &history_ids).await? {
            history.insert(id, record?);
        }
        let reversed: Vec<_> = history_ids
            .iter()
            .filter_map(|id| history.get(id))
            .filter(|record| record.state == DepositState::Reversed)
            .collect();
        if reversed
            .iter()
            .any(|record| same_transfer_evidence(record, deposit))
        {
            continue;
        }
        let inserted = match reversed.first() {
            Some(record) => crate::finality::successor_in(&mut tx, record.id, deposit).await?,
            None => {
                let inserted =
                    db::insert_scanned_deposit_in(&mut tx, deposit, db::Evidence::Finalized)
                        .await?;
                if let Some(id) = inserted {
                    sqlx::query("UPDATE deposits SET dual_verified_at=now() WHERE id=$1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                inserted
            }
        };
        if inserted.is_some() {
            stats.inserted = stats.inserted.saturating_add(1);
        }
    }
    for (id, state, deposit) in corrections {
        let changed = sqlx::query("UPDATE deposits SET log_index=$2,block_number=$3,block_hash=$4,block_time=$5,asset_contract=$6,from_address=$7,amount_atomic=$8::text::numeric,tx_from=$9,tx_nonce=$10::text::numeric,route=$11,route_version=$12,dual_verified_at=now(),updated_at=now() WHERE id=$1 AND state=$13 AND dual_verified_at IS NULL")
        .bind(id).bind(i64::try_from(deposit.log_index).map_err(|_|ScannerError::SnapshotChanged)?).bind(i64::try_from(deposit.block_number).map_err(|_|ScannerError::SnapshotChanged)?).bind(format!("{:#x}",deposit.block_hash)).bind(deposit.block_time).bind(format!("{:#x}",deposit.asset_contract)).bind(format!("{:#x}",deposit.from_address)).bind(deposit.amount_atomic.value().to_string()).bind(format!("{:#x}",deposit.tx_from)).bind(deposit.tx_nonce.to_string()).bind(deposit.route).bind(deposit.route_version.map(i64::try_from).transpose().map_err(|_|ScannerError::SnapshotChanged)?).bind(db::state_code(state)).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(ScannerError::SnapshotChanged);
        }
    }
    stats.factory = db::sweeps::commit_factory_logs_in(&mut tx, chain, &factory).await?;
    for range in &ranges {
        for address in &range.addresses {
            if range.start(address) > range.end {
                continue;
            }
            let caught_up = range.end == end;
            let changed = sqlx::query("UPDATE addresses SET dual_covered_through=$2,backfilled=CASE WHEN $3 THEN true ELSE backfilled END,backfilled_through=CASE WHEN $3 THEN $2 ELSE backfilled_through END WHERE id=$1 AND created_block=$4 AND dual_covered_through IS NOT DISTINCT FROM $5")
            .bind(address.address.id).bind(i64::try_from(range.end).map_err(|_|ScannerError::SnapshotChanged)?).bind(caught_up).bind(i64::try_from(address.address.created_block).map_err(|_|ScannerError::SnapshotChanged)?).bind(address.through.map(i64::try_from).transpose().map_err(|_|ScannerError::SnapshotChanged)?).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(ScannerError::SnapshotChanged);
            }
            if caught_up {
                stats.backfilled_addresses = stats.backfilled_addresses.saturating_add(1);
            }
        }
    }
    sqlx::query("UPDATE chain_coverage SET through_block=$2,through_hash=$3,through_time=$4,updated_at=now() WHERE chain_id=$1 AND through_block < $2")
    .bind(i64::try_from(chain).map_err(|_|ScannerError::SnapshotChanged)?).bind(i64::try_from(end).map_err(|_|ScannerError::SnapshotChanged)?).bind(format!("{:#x}",a.0)).bind(a.1).execute(&mut *tx).await?;
    if chain_reads::common_coverage_in(&mut tx, chain).await? != common {
        return Err(ScannerError::SnapshotChanged);
    }
    chain_reads::set_compat_cursor_in(&mut tx, chain, common, Some(common_time)).await?;
    db::pending::delete_finalized_in(
        &mut tx,
        i64::try_from(chain).map_err(|_| ScannerError::SnapshotChanged)?,
        i64::try_from(end).map_err(|_| ScannerError::SnapshotChanged)?,
    )
    .await?;
    tx.commit().await?;
    Ok(stats)
}
/// Runs one scheduled coverage round, retrying an invalidated snapshot once with fresh RPCs.
pub async fn coverage_round<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    read: &R,
    verify: &V,
    routes: &ChainRoutes,
    round: u64,
) -> Result<ScanStats, ScannerError> {
    match coverage_once(pool, read, verify, routes, round).await {
        Err(ScannerError::SnapshotChanged) => {
            coverage_once(pool, read, verify, routes, round).await
        }
        result => result,
    }
}

/// Validate and publish checkpoints even while coverage fails or has not yet run.
pub async fn checkpoint_loop<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    chain: u64,
    readers: (&R, &V),
    heads: &FinalizedHeads,
    ready: impl Fn() -> bool,
    cancellation: &CancellationToken,
) {
    let monitor = crate::observability::CronMonitor::checkpoint_scanner(chain);
    let mut interval = tokio::time::interval(CHECKPOINT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
        if !ready() {
            monitor.check_in(false);
            continue;
        }
        let result = tokio::select! {
            _ = cancellation.cancelled() => break,
            result = crate::checkpoint::advance(pool, chain, readers.0, readers.1) => result,
        };
        monitor.check_in(result.is_ok());
        match result {
            Ok(checkpoint) => {
                heads.publish(
                    chain,
                    FinalizedHead {
                        number: checkpoint.number,
                        time: checkpoint.time,
                    },
                );
            }
            Err(error) => {
                tracing::warn!(chain_id=chain,%error,"checkpoint waits; no advance published")
            }
        }
    }
}

/// Hourly dual coverage uses only published DB checkpoints; failures do not stop checkpoint checks.
pub async fn coverage_loop<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    chain: &ChainRoutes,
    readers: (&R, &V),
    stagger: Duration,
    ready: impl Fn() -> bool,
    cancellation: &CancellationToken,
) {
    let monitor = crate::observability::CronMonitor::coverage_scanner(chain.chain.chain_id);
    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now() + stagger, COVERAGE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut round = 0_u64;
    loop {
        tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
        if !ready() {
            monitor.check_in(false);
            continue;
        }
        round = round.saturating_add(1);
        let result = tokio::select! {
            _ = cancellation.cancelled() => break,
            result = coverage_round(pool,readers.0,readers.1,chain,round) => result,
        };
        monitor.check_in(result.is_ok());
        match result {
            Ok(stats) => tracing::debug!(
                chain_id = chain.chain.chain_id,
                ?stats,
                "dual coverage committed"
            ),
            Err(ScannerError::Disagreement) => tracing::warn!(
                tags.alert = "TopupRpcDisagreement",
                chain_id = chain.chain.chain_id,
                "coverage evidence disagreed; no range committed"
            ),
            Err(error) => {
                tracing::warn!(chain_id=chain.chain.chain_id,%error,"coverage waits; no range committed")
            }
        }
    }
}

/// Supervise independent per-chain cadences without stopping the API on an endpoint error.
pub async fn run(
    pool: PgPool,
    routes: &RouteSet,
    _config: ScanConfig,
    heads: FinalizedHeads,
    cancellation: CancellationToken,
) -> Result<(), ScannerError> {
    let mut tasks = tokio::task::JoinSet::new();
    for pair in routes.rpc().values() {
        let pair = pair.clone();
        let token = cancellation.clone();
        tasks.spawn(async move {
            crate::rpc_runtime::recover(pair, token).await;
        });
    }
    for (offset, chain) in chain_routes(routes).into_iter().enumerate() {
        let read = FinalizedReader::new(
            routes
                .provider(chain.chain.chain_id, 0)
                .map_err(|e| ScannerError::Configuration(e.to_string()))?
                .clone(),
        );
        let verify = FinalizedReader::new(
            routes
                .provider(chain.chain.chain_id, 1)
                .map_err(|e| ScannerError::Configuration(e.to_string()))?
                .clone(),
        );
        let contract_routes = routes.routes().to_vec();
        let contract_read = routes
            .provider(chain.chain.chain_id, 0)
            .map_err(|e| ScannerError::Configuration(e.to_string()))?
            .clone();
        let contract_verify = routes
            .provider(chain.chain.chain_id, 1)
            .map_err(|e| ScannerError::Configuration(e.to_string()))?
            .clone();
        let fast_pool = pool.clone();
        let fast_read = FinalizedReader::new(
            routes
                .provider(chain.chain.chain_id, 0)
                .map_err(|e| ScannerError::Configuration(e.to_string()))?
                .clone(),
        );
        let fast_chain = chain.clone();
        let fast_token = cancellation.clone();
        tasks.spawn(async move {
            let monitor = crate::observability::CronMonitor::fast_scanner(fast_chain.chain.chain_id);
            let mut interval = tokio::time::interval(FAST_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = fast_token.cancelled() => break, _ = interval.tick() => {} }
                if crate::reconciler::chain_is_blocked(&fast_pool,fast_chain.chain.chain_id).await.unwrap_or(true) {monitor.check_in(false); continue;}
                if !contract_read.contract_ready() || !contract_verify.contract_ready() {
                    let checked=tokio::select! {
                        _=fast_token.cancelled()=>break,
                        result=crate::contracts::check_and_enforce(&fast_pool,&contract_read,&contract_verify,fast_chain.chain.chain_id,&contract_routes)=>result,
                    };
                    if !matches!(checked,Ok(true)) {monitor.check_in(false); continue;}
                }
                let result = tokio::select! {
                    _ = fast_token.cancelled() => break,
                    result = fast_once(&fast_pool,&fast_read,&fast_chain) => result,
                };
                monitor.check_in(result.is_ok());
                if let Err(error) = result { tracing::warn!(chain_id=fast_chain.chain.chain_id,%error,"fast discovery waits"); }
            }
        });
        let coverage_read = routes
            .provider(chain.chain.chain_id, 0)
            .map_err(|e| ScannerError::Configuration(e.to_string()))?
            .clone();
        let coverage_verify = routes
            .provider(chain.chain.chain_id, 1)
            .map_err(|e| ScannerError::Configuration(e.to_string()))?
            .clone();
        let checkpoint_pool = pool.clone();
        let checkpoint_read = FinalizedReader::new(coverage_read.clone());
        let checkpoint_verify = FinalizedReader::new(coverage_verify.clone());
        let checkpoint_ready_read = coverage_read.clone();
        let checkpoint_ready_verify = coverage_verify.clone();
        let checkpoint_heads = heads.clone();
        let checkpoint_token = cancellation.clone();
        let chain_id = chain.chain.chain_id;
        tasks.spawn(async move {
            checkpoint_loop(
                &checkpoint_pool,
                chain_id,
                (&checkpoint_read, &checkpoint_verify),
                &checkpoint_heads,
                || {
                    checkpoint_ready_read.contract_ready()
                        && checkpoint_ready_verify.contract_ready()
                },
                &checkpoint_token,
            )
            .await;
        });
        let pool = pool.clone();
        let token = cancellation.clone();
        tasks.spawn(async move {
            coverage_loop(
                &pool,
                &chain,
                (&read, &verify),
                Duration::from_secs(u64::try_from(offset).unwrap_or(0).saturating_mul(30)),
                || coverage_read.contract_ready() && coverage_verify.contract_ready(),
                &token,
            )
            .await;
        });
    }
    for (chain, pair) in routes
        .rpc()
        .iter()
        .filter(|(chain, _)| routes.chain(**chain).is_none())
    {
        let chain = *chain;
        let pair = pair.clone();
        let pool = pool.clone();
        let token = cancellation.clone();
        let contracts = routes.routes().to_vec();
        tasks.spawn(async move {
            let mut interval=tokio::time::interval(CONTRACT_RECOVERY_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=token.cancelled()=>break,_=interval.tick()=>{}}
                if pair.read.contract_ready() && pair.verify.contract_ready() {continue;}
                if crate::reconciler::chain_is_blocked(&pool,chain).await.unwrap_or(true) {continue;}
                tokio::select! {
                    _=token.cancelled()=>break,
                    result=crate::contracts::check_and_enforce(&pool,&pair.read,&pair.verify,chain,&contracts)=>{
                        if let Err(error)=result {tracing::warn!(chain_id=chain,%error,"price chain contract check waits");}
                    }
                }
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|e| ScannerError::Task(e.to_string()))?;
    }
    Ok(())
}
/// Decode the agreed/provisional transfer into the existing deposit insertion rules.
pub(crate) fn resolve_log(
    log: TransferLog,
    address: &db::ScanAddress,
    routes: &ChainRoutes,
    next_attempt_at: DateTime<Utc>,
) -> db::NewDeposit {
    let selected = routes.routes.get(&log.token);
    db::NewDeposit {
        chain_id: routes.chain.chain_id,
        tx_hash: log.tx_hash,
        receipt_log_index: log.receipt_log_index,
        log_index: log.log_index,
        block_number: log.block_number,
        block_hash: log.block_hash,
        block_time: log.block_time,
        address_id: address.id,
        route: selected.map(|r| r.0.clone()),
        route_version: selected.map(|r| r.1),
        asset_contract: log.token,
        from_address: log.from,
        amount_atomic: log.amount,
        state: if selected.is_some() {
            DepositState::Detected
        } else {
            DepositState::Rejected
        },
        reason: selected.is_none().then_some(RejectReason::UnsupportedAsset),
        next_attempt_at,
        tx_from: log.tx_from,
        tx_nonce: log.tx_nonce,
        is_final: false,
    }
}
