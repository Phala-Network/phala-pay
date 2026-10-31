//! Read-only fast discovery and atomic dual-source finalized coverage (§2.1–2.2).
use crate::{db, routes::RouteSet};
use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use db::chain_reads::{self, Boundary};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FactoryLog, FinalizedHead, FinalizedReader, RawTransfer, TransferLog,
    TransferRules,
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
/// Backlogs above the normal limit catch up at this inclusive limit.
pub const CATCHUP_LIMIT: u64 = 19_200;
/// Activity windows for single-provider discovery; dual coverage always includes every address.
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Any quote created within this many hours makes its address hot, regardless of state.
    pub quote_window_hours: u32,
    /// A deposit's chain block time keeps its address hot for this many days.
    pub deposit_window_days: u32,
}
impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            quote_window_hours: 24,
            deposit_window_days: 7,
        }
    }
}
impl ScanConfig {
    pub(crate) fn validate(self) -> Result<(), String> {
        if !(1..=8_760).contains(&self.quote_window_hours) {
            return Err("discovery.quote_window_hours must be an integer from 1 to 8760".into());
        }
        if !(1..=365).contains(&self.deposit_window_days) {
            return Err("discovery.deposit_window_days must be an integer from 1 to 365".into());
        }
        Ok(())
    }
}
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
    pub(crate) rules: TransferRules,
}
/// A conservative floor, independent of live prices or customer settings. Stablecoins are
/// valued at $1; using the upper peg-band price is deliberately more permissive. Quote locks
/// bypass the spot credit minimum, so lower this bound by the largest allowed tolerance too.
fn guaranteed_floor(route: &topup_core::route::RouteFile) -> topup_core::money::AtomicAmount {
    use topup_core::{money::AtomicAmount, route::PricingMode};
    let atomic = route.merchant.min_deposit_atomic.min;
    if route.pricing.mode != PricingMode::Stablecoin {
        return atomic;
    }
    let derived = (|| {
        let ceil_div = |n: U256, d: U256| {
            n.checked_div(d)?
                .checked_add(U256::from(!n.checked_rem(d)?.is_zero()))
        };
        let scale = U256::from(10).checked_pow(U256::from(route.asset.decimals))?;
        let numerator = U256::from(route.merchant.min_amount.min)
            .checked_mul(scale)?
            .checked_mul(U256::from(10_000_u16))?;
        let denominator = U256::from(100_u16).checked_mul(U256::from(
            10_000_u32.checked_add(u32::from(route.pricing.peg_band_bps.value()))?,
        ))?;
        let smallest_quote = ceil_div(numerator, denominator)?;
        ceil_div(
            smallest_quote.checked_mul(U256::from(
                10_000_u16.checked_sub(route.merchant.quote_tolerance_bps.max.value())?,
            ))?,
            U256::from(10_000_u16),
        )
    })()
    .map(AtomicAmount::new)
    .unwrap_or_default();
    atomic.max(derived)
}

/// Group current token routes by payment chain.
pub fn chain_routes(routes: &RouteSet) -> Vec<ChainRoutes> {
    routes
        .chain_ids()
        .filter_map(|chain| {
            Some(ChainRoutes {
                chain: routes.chain(chain)?.clone(),
                rules: TransferRules {
                    minimums: routes
                        .routes()
                        .iter()
                        .filter(|r| r.chain.chain_id == chain)
                        .fold(BTreeMap::new(), |mut floors, r| {
                            floors
                                .entry(r.asset.contract)
                                .and_modify(|floor: &mut topup_core::money::AtomicAmount| {
                                    *floor = (*floor).min(guaranteed_floor(r))
                                })
                                .or_insert(guaranteed_floor(r));
                            floors
                        }),
                    ignored_blocks: BTreeSet::new(),
                },
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
/// Discover hot recipients in bounded pages; candidates remain provisional until dual agreement.
pub async fn fast_once<R: ChainReader>(
    pool: &PgPool,
    read: &R,
    routes: &ChainRoutes,
    config: ScanConfig,
) -> Result<db::ScanCommit, ScannerError> {
    config.validate().map_err(ScannerError::Configuration)?;
    let now = Utc::now();
    let quoted_since = now - chrono::Duration::hours(i64::from(config.quote_window_hours));
    let deposited_since = now - chrono::Duration::days(i64::from(config.deposit_window_days));
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
    let mut commit = db::ScanCommit {
        inserted: 0,
        unsupported_inserted: 0,
        inserted_positions: Vec::new(),
    };
    if from <= latest {
        let mut after = None;
        loop {
            let (addresses, more) =
                db::scanner::hot_address_page(pool, chain, after, quoted_since, deposited_since)
                    .await?;
            if addresses.is_empty() {
                break;
            }
            after = addresses.last().map(|address| address.id);
            let ids: Vec<_> = addresses.iter().map(|address| address.id).collect();
            let index: BTreeMap<_, _> = addresses.into_iter().map(|a| (a.address, a)).collect();
            tracing::debug!(
                chain_id = chain,
                from,
                latest,
                addresses = index.len(),
                "fast discovery hot page"
            );
            let recipients: Vec<_> = index.keys().copied().collect();
            let mut deposits = Vec::new();
            let mut pending = Vec::new();
            let proven = db::scanner::proven_log_positions(pool, chain, &ids, from, latest).await?;
            let mut rules = routes.rules.clone();
            rules.ignored_blocks =
                db::ignored::historical_groups(pool, chain, &ids, from, latest).await?;
            for log in read
                .discovery_logs(&recipients, from, latest, &rules, &proven)
                .await?
            {
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
            let mut tx = pool.begin().await?;
            db::rpc::guard_in(&mut tx, chain).await?;
            let page = db::scanner::insert_deposits_in(&mut tx, &deposits, db::Evidence::Confirmed)
                .await?;
            db::pending::commit_head_page_in(&mut tx, chain, from, latest, &pending, Some(&ids))
                .await?;
            tx.commit().await?;
            commit.inserted += page.inserted;
            commit.unsupported_inserted += page.unsupported_inserted;
            commit.inserted_positions.extend(page.inserted_positions);
            if !more {
                break;
            }
        }
    }
    // A failed page leaves the cursor unchanged, so retrying also revisits committed pages.
    db::commit_confirmed_scan(pool, chain, &[], routes.chain.confirmations.horizon(heads)).await?;
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

/// One bounded coverage round; a backlog above the normal bound uses the catch-up limit.
pub async fn coverage_once<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    read: &R,
    verify: &V,
    routes: &ChainRoutes,
) -> Result<ScanStats, ScannerError> {
    let chain = routes.chain.chain_id;
    let cursor = match chain_reads::coverage(pool, chain).await? {
        Some(cursor) => cursor,
        None => initialize_chain(pool, chain, read, verify).await?,
    };
    let checkpoint = chain_reads::checkpoint(pool, chain)
        .await?
        .ok_or(ScannerError::Disagreement)?;
    let limit = if checkpoint.number.saturating_sub(cursor.number) > COVERAGE_LIMIT {
        CATCHUP_LIMIT
    } else {
        COVERAGE_LIMIT
    };
    let end = checkpoint.number.min(cursor.number.saturating_add(limit));
    if end < cursor.number {
        return Err(ScannerError::Disagreement);
    }
    // The checkpoint loop already verifies this immutable boundary on both endpoints.
    let (a, b) = if end == checkpoint.number {
        let header = (checkpoint.hash, checkpoint.time);
        (Ok(header), Ok(header))
    } else {
        tokio::join!(read.header(end), verify.header(end))
    };
    if end == cursor.number
        && [a.as_ref().ok(), b.as_ref().ok()]
            .iter()
            .flatten()
            .any(|header| header.0 != cursor.hash)
    {
        chain_reads::freeze(pool, chain, "finalized_checkpoint_conflict").await?;
        tracing::error!(
            tags.alert = "TopupFinalizedCheckpointConflict",
            chain_id = chain,
            "stored coverage boundary hash changed; chain frozen"
        );
        return Err(ScannerError::Disagreement);
    }
    let (a, b) = (a?, b?);
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
        // Address history can lag even while the global cursor is caught up.
        let limit = if end.saturating_sub(start) > COVERAGE_LIMIT {
            CATCHUP_LIMIT
        } else {
            COVERAGE_LIMIT
        };
        ranges.push(Range {
            addresses: lagging,
            from: start,
            end: end.min(start.saturating_add(limit).saturating_sub(1)),
        });
    }
    // Snapshot only rows whose actual ranges are covered. Other issuance/restore work must
    // not discard independent evidence for this bounded round.
    let mut covered_snapshot = BTreeMap::new();
    let mut covered_ids = Vec::new();
    let mut covered_physical = Vec::new();
    let mut covered_created = Vec::new();
    let mut covered_through = Vec::new();
    let mut covered_starts = Vec::new();
    let mut covered_ends = Vec::new();
    let mut proposed_progress = BTreeMap::new();
    for range in &ranges {
        for address in &range.addresses {
            let start = range.start(address);
            if start > range.end {
                continue;
            }
            let id = address.address.id;
            let physical = format!("{:#x}", address.address.address);
            let created = i64::try_from(address.address.created_block)
                .map_err(|_| ScannerError::SnapshotChanged)?;
            let through = address
                .through
                .map(i64::try_from)
                .transpose()
                .map_err(|_| ScannerError::SnapshotChanged)?;
            if covered_snapshot
                .insert(id, (physical.clone(), created, through))
                .is_some()
            {
                return Err(ScannerError::SnapshotChanged);
            }
            covered_ids.push(id);
            covered_physical.push(physical);
            covered_created.push(created);
            covered_through.push(through);
            covered_starts.push(i64::try_from(start).map_err(|_| ScannerError::SnapshotChanged)?);
            covered_ends.push(i64::try_from(range.end).map_err(|_| ScannerError::SnapshotChanged)?);
            proposed_progress.insert(id, range.end);
        }
    }
    let mut candidates = BTreeSet::new();
    let mut ignored_logs = BTreeMap::new();
    // Insert-only factory evidence must be reverified even when both log responses omit it.
    // One set-based lookup covers every address's own inclusive range.
    let hashes: Vec<String> = sqlx::query_scalar(
        "WITH covered AS (SELECT * FROM unnest($2::uuid[],$3::bigint[],$4::bigint[]) \
         AS ranges(address_id,start_block,end_block)) \
         SELECT f.tx_hash FROM flushed f JOIN covered c ON c.address_id=f.address_id \
         WHERE f.chain_id=$1 AND f.block_number BETWEEN c.start_block AND c.end_block \
         UNION SELECT f.tx_hash FROM flush_failures f JOIN covered c ON c.address_id=f.address_id \
         WHERE f.chain_id=$1 AND f.block_number BETWEEN c.start_block AND c.end_block",
    )
    .bind(chain_key)
    .bind(&covered_ids)
    .bind(&covered_starts)
    .bind(&covered_ends)
    .fetch_all(pool)
    .await?;
    let mut factory_candidates = hashes
        .into_iter()
        .map(|hash| {
            hash.parse::<B256>()
                .map_err(|_| ScannerError::SnapshotChanged)
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut factory_logs = [
        BTreeMap::<B256, BTreeMap<u64, FactoryLog>>::new(),
        BTreeMap::new(),
    ];
    let mut conflicting_factory_logs = BTreeSet::new();
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
        let ids: Vec<_> = range
            .addresses
            .iter()
            .map(|address| address.address.id)
            .collect();
        let proven =
            db::scanner::proven_log_positions(pool, chain, &ids, range.from, range.end).await?;
        let mut rules = routes.rules.clone();
        rules.ignored_blocks =
            db::ignored::historical_groups(pool, chain, &ids, range.from, range.end).await?;
        let (a, b) = tokio::try_join!(
            read.coverage_partition(
                routes.chain.contracts.forwarder_factory,
                &recipients,
                range.from,
                range.end,
                &rules,
                &proven
            ),
            verify.coverage_partition(
                routes.chain.contracts.forwarder_factory,
                &recipients,
                range.from,
                range.end,
                &rules,
                &proven
            )
        )?;
        let starts: BTreeMap<_, _> = range
            .addresses
            .iter()
            .map(|address| {
                index.insert(address.address.address, address.address.clone());
                (address.address.address, range.start(address))
            })
            .collect();
        let ignored = |logs: Vec<RawTransfer>| -> Result<BTreeMap<_, _>, ScannerError> {
            let mut kept = BTreeMap::new();
            for log in logs {
                if starts
                    .get(&log.to)
                    .is_none_or(|start| log.block_number < *start)
                {
                    continue;
                }
                let key = (log.block_hash, log.log_index);
                if let Some(previous) = kept.insert(key, log.clone())
                    && previous != log
                {
                    return Err(ScannerError::Disagreement);
                }
            }
            Ok(kept)
        };
        let ignored_a = ignored(a.ignored)?;
        let ignored_b = ignored(b.ignored)?;
        if ignored_a != ignored_b {
            return Err(ScannerError::Disagreement);
        }
        for (key, log) in ignored_a {
            if let Some(previous) = ignored_logs.insert(key, log.clone())
                && previous != log
            {
                return Err(ScannerError::Disagreement);
            }
        }
        for (endpoint, (transfers, factory)) in [(a.transfers, a.factory), (b.transfers, b.factory)]
            .into_iter()
            .enumerate()
        {
            for log in transfers {
                if starts
                    .get(&log.to)
                    .is_some_and(|start| log.block_number >= *start)
                {
                    candidates.insert((log.tx_hash, log.receipt_log_index));
                }
            }
            for log in factory {
                if starts
                    .get(&log.event.forwarder())
                    .is_some_and(|start| log.block_number >= *start)
                {
                    factory_candidates.insert(log.tx_hash);
                    let hash = log.tx_hash;
                    if let Some(previous) = factory_logs[endpoint]
                        .entry(hash)
                        .or_default()
                        .insert(log.log_index, log.clone())
                        && previous != log
                    {
                        conflicting_factory_logs.insert(hash);
                    }
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
            let proposed = proposed_progress.get(id).copied();
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
    let mut factory_evidence = Vec::new();
    for hash in factory_candidates {
        if let (Some(a), Some(b)) = (factory_logs[0].get(&hash), factory_logs[1].get(&hash))
            && a == b
            && !conflicting_factory_logs.contains(&hash)
        {
            let logs: Vec<_> = a.values().cloned().collect();
            if db::sweeps::stored_factory_evidence_matches(pool, chain, hash, &logs).await? {
                factory_evidence.push((hash, logs));
                continue;
            }
        }
        // Missing/disagreeing log responses need fresh dual receipts, including stored events
        // that both log queries omit. Never use single-source absence as negative evidence.
        let (a, b) = tokio::try_join!(
            read.factory_receipt(hash, routes.chain.contracts.forwarder_factory),
            verify.factory_receipt(hash, routes.chain.contracts.forwarder_factory)
        )?;
        if a != b {
            return Err(ScannerError::Disagreement);
        }
        factory_evidence.push((hash, a.ok_or(ScannerError::Disagreement)?.logs));
    }
    let mut evidence = BTreeMap::new();
    // Only covered addresses and their own covered blocks belong to this round.
    // Other deposits use their owner's shared claim and backoff, including S/L.
    // Any change to the covered unverified set invalidates this round's RPC evidence.
    // Abandon the round after the DB-only lock recheck; the next round reads afresh.
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM deposits WHERE chain_id=$1 AND dual_verified_at IS NULL AND state <> 'reversed' \
         AND EXISTS(SELECT 1 FROM unnest($2::uuid[],$3::bigint[],$4::bigint[]) AS covered(address_id,start_block,end_block) \
         WHERE deposits.address_id=covered.address_id AND deposits.block_number BETWEEN covered.start_block AND covered.end_block)",
    ).bind(chain_key).bind(&covered_ids).bind(&covered_starts).bind(&covered_ends).fetch_all(pool).await?;
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
    let current_addresses: Vec<(uuid::Uuid, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id,address,created_block,dual_covered_through FROM addresses \
         WHERE chain_id=$1 AND id=ANY($2) ORDER BY id",
    )
    .bind(chain_key)
    .bind(&covered_ids)
    .fetch_all(&mut *tx)
    .await?;
    let expected: Vec<_> = covered_snapshot
        .into_iter()
        .map(|(id, (address, created, through))| (id, address, created, through))
        .collect();
    if u64::try_from(durable).ok() != Some(cursor.number) || current_addresses != expected {
        tx.rollback().await?;
        return Err(ScannerError::SnapshotChanged);
    }
    let locked_ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM deposits WHERE chain_id=$1 AND dual_verified_at IS NULL AND state <> 'reversed' \
         AND EXISTS(SELECT 1 FROM unnest($2::uuid[],$3::bigint[],$4::bigint[]) AS covered(address_id,start_block,end_block) \
         WHERE deposits.address_id=covered.address_id AND deposits.block_number BETWEEN covered.start_block AND covered.end_block) FOR UPDATE",
    ).bind(chain_key).bind(&covered_ids).bind(&covered_starts).bind(&covered_ends).fetch_all(&mut *tx).await?;
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
    // A union candidate may identify a provisional row whose stored block/recipient is
    // outside the snapshot. Its in-range log paid for this receipt read; retain the
    // existing correction/freeze rules for that identity, without scanning other rows.
    for &(hash, position) in evidence.keys() {
        if unverified.contains_key(&(hash, position)) {
            continue;
        }
        let candidate_ids: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM deposits WHERE chain_id=$1 AND tx_hash=$2 AND receipt_log_index=$3 \
             AND dual_verified_at IS NULL AND state <> 'reversed' FOR UPDATE",
        )
        .bind(chain_key)
        .bind(format!("{hash:#x}"))
        .bind(i64::try_from(position).map_err(|_| ScannerError::SnapshotChanged)?)
        .fetch_all(&mut *tx)
        .await?;
        for (_, deposit) in db::deposits_by_ids(&mut *tx, &candidate_ids).await? {
            unverified
                .entry((hash, position))
                .or_default()
                .push(deposit?);
        }
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
                        tx.rollback().await?;
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
    for (hash, logs) in &factory_evidence {
        if !db::sweeps::stored_factory_evidence_matches(&mut *tx, chain, *hash, logs).await? {
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
            logs.iter()
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
            tx.rollback().await?;
            return Err(ScannerError::SnapshotChanged);
        }
    }
    db::ignored::record_in(
        &mut tx,
        chain,
        &ignored_logs,
        &index,
        &routes.rules,
        Utc::now(),
    )
    .await?;
    stats.factory = db::sweeps::commit_factory_logs_in(&mut tx, chain, &factory).await?;
    let changed = sqlx::query(
        "UPDATE addresses a SET dual_covered_through=p.end_block, \
         backfilled=CASE WHEN p.end_block=$2 THEN true ELSE a.backfilled END, \
         backfilled_through=CASE WHEN p.end_block=$2 THEN p.end_block ELSE a.backfilled_through END \
         FROM unnest($3::uuid[],$4::text[],$5::bigint[],$6::bigint[],$7::bigint[]) \
         AS p(id,address,created_block,old_through,end_block) \
         WHERE a.chain_id=$1 AND a.id=p.id AND a.address=p.address AND a.created_block=p.created_block \
         AND a.dual_covered_through IS NOT DISTINCT FROM p.old_through"
    ).bind(chain_key).bind(i64::try_from(end).map_err(|_| ScannerError::SnapshotChanged)?)
        .bind(&covered_ids).bind(&covered_physical).bind(&covered_created).bind(&covered_through)
        .bind(&covered_ends).execute(&mut *tx).await?.rows_affected();
    if usize::try_from(changed).ok() != Some(covered_ids.len()) {
        tx.rollback().await?;
        return Err(ScannerError::SnapshotChanged);
    }
    stats.backfilled_addresses = covered_ends
        .iter()
        .filter(|through| u64::try_from(**through).ok() == Some(end))
        .count()
        .try_into()
        .map_err(|_| ScannerError::SnapshotChanged)?;
    sqlx::query("UPDATE chain_coverage SET through_block=$2,through_hash=$3,through_time=$4,updated_at=now() WHERE chain_id=$1 AND through_block < $2")
    .bind(i64::try_from(chain).map_err(|_|ScannerError::SnapshotChanged)?).bind(i64::try_from(end).map_err(|_|ScannerError::SnapshotChanged)?).bind(format!("{:#x}",a.0)).bind(a.1).execute(&mut *tx).await?;
    // Uncovered rows may have changed during RPC. Preserve covered evidence and rebase
    // the compatibility boundary safely; an unprefetched timestamp implies no negative proof.
    let actual_common = chain_reads::common_coverage_in(&mut tx, chain).await?;
    let actual_time = if actual_common == end {
        Some(a.1)
    } else {
        (actual_common == common).then_some(common_time)
    };
    chain_reads::set_compat_cursor_in(&mut tx, chain, actual_common, actual_time).await?;
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
) -> Result<ScanStats, ScannerError> {
    match coverage_once(pool, read, verify, routes).await {
        Err(ScannerError::SnapshotChanged) => coverage_once(pool, read, verify, routes).await,
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
    loop {
        tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
        if !ready() {
            monitor.check_in(false);
            continue;
        }
        let result = tokio::select! {
            _ = cancellation.cancelled() => break,
            result = coverage_round(pool,readers.0,readers.1,chain) => result,
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
    config: ScanConfig,
    heads: FinalizedHeads,
    cancellation: CancellationToken,
) -> Result<(), ScannerError> {
    config.validate().map_err(ScannerError::Configuration)?;
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
                    result = fast_once(&fast_pool,&fast_read,&fast_chain,config) => result,
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

#[cfg(test)]
mod tests {
    use super::*;
    use topup_core::{
        money::{AtomicAmount, Bps, PRICE_SCALE, ScaledPrice, credit},
        route::PricingMode,
        valuation::{LockTerms, RouteValuation, lock_applies},
    };

    #[test]
    fn stablecoin_floor_preserves_spot_and_tolerated_quote_payments() {
        let mut route: topup_core::route::RouteFile =
            serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml"))
                .unwrap();
        route.pricing.mode = PricingMode::Stablecoin;
        route.asset.decimals = 6;
        route.merchant.min_deposit_atomic.min = AtomicAmount::default();
        route.merchant.min_amount.min = 500;
        route.merchant.quote_tolerance_bps.max = Bps::new(1000).unwrap();
        route.pricing.peg_band_bps = Bps::new(100).unwrap();
        let floor = guaranteed_floor(&route);
        assert_eq!(floor.value(), U256::from(4_455_447));
        let below = AtomicAmount::new(floor.value() - U256::from(1));
        let rules = TransferRules {
            minimums: BTreeMap::from([(route.asset.contract, floor)]),
            ..Default::default()
        };
        let mut raw = RawTransfer {
            tx_hash: B256::ZERO,
            log_index: 0,
            block_number: 0,
            block_hash: B256::ZERO,
            token: route.asset.contract,
            from: Address::ZERO,
            to: Address::ZERO,
            amount: below,
        };
        assert!(rules.ignored_reason(&raw).is_some());
        raw.amount = floor;
        assert!(rules.ignored_reason(&raw).is_none());
        let highest_price = ScaledPrice::new(101_000_000, PRICE_SCALE).unwrap();
        assert!(credit(below, highest_price, 6, 2).unwrap().value() < 500);
        // At the largest possible price, this is the smallest quoted amount for $5.
        let lock = LockTerms {
            asset: route.asset.contract,
            amount: AtomicAmount::new(U256::from(4_950_496)),
            price: highest_price,
            credit_minor: topup_core::money::MinorAmount::new(500),
            expires_at: topup_core::valuation::UnixSeconds::new(100),
            block_time: topup_core::valuation::UnixSeconds::new(99),
        };
        let terms = RouteValuation {
            asset: &route.asset,
            min_credit_minor: 500,
            lock_tolerance_bps: route.merchant.quote_tolerance_bps.max,
        };
        assert!(!lock_applies(below, &terms, &lock));
        assert!(lock_applies(floor, &terms, &lock));
        let tolerated = AtomicAmount::new(U256::from(4_500_000));
        raw.amount = tolerated;
        assert!(rules.ignored_reason(&raw).is_none());
        // The explicit atomic floor still applies to volatile assets; no price guess is used.
        route.pricing.mode = PricingMode::Spot;
        assert_eq!(guaranteed_floor(&route), AtomicAmount::default());
    }
}
