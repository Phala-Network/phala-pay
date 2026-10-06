//! ERC-20 deposit scanners: the per-block scan of the head loop ([`head`]), which records
//! transfers at the route's confirmation about one block time after they reach it, and the
//! finalized backstop, which records any transfer the per-block scan missed and indexes the
//! forwarder factory's `ForwarderCreated`, `Flushed`, and `FlushFailed` events for the same
//! addresses, whoever called the factory (design §13), once per `finalized` advance.

mod head;

pub use head::{
    DEFAULT_HEAD_POLL_INTERVAL, FinalizedHeads, HeadScan, head_poll_interval, head_scan_once,
    scan_new_blocks,
};

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::chain_retry::backing_off;
use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FinalizedReader, MAX_BLOCKS_PER_REQUEST, TransferLog,
};
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::retry::backoff;
use topup_core::route::{Backstop, ChainConfig};
use tracing::Instrument as _;

use crate::db::{self, NewDeposit, ScanAddress, ScanCommit};
use crate::jitter::{JitterSource as _, OsJitter};
use crate::routes::RouteSet;

/// Maximum inclusive block count scanned in one window.
pub const MAX_SCAN_WINDOW: u64 = MAX_BLOCKS_PER_REQUEST;
/// Maximum block windows per address page in one finalized pass.
const MAX_WINDOWS_PER_SCAN: u64 = 64;

/// Scanner timing (`topup run --head-poll-interval-s`, `--finalized-poll-interval-s`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanConfig {
    /// Delay between `eth_blockNumber` polls of provider A; `None` uses each chain's
    /// [`head_poll_interval`], one block time.
    pub head_poll_interval: Option<Duration>,
    /// Least delay between reads of provider A's `finalized` head, whose advance wakes the
    /// finalized backstop, the finality watch, and the reconciler.
    pub finalized_poll_interval: Duration,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            head_poll_interval: None,
            finalized_poll_interval: DEFAULT_FINALIZED_POLL_INTERVAL,
        }
    }
}

/// Default least delay between `finalized` reads: an Ethereum epoch (6.4 minutes) advances it,
/// so a minute adds at most a sixth of an epoch before an advance is acted on.
pub const DEFAULT_FINALIZED_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Longest wait of the finalized backstop without an advance: a published head could be missed
/// only if the head loop stopped, and the backstop then reads `finalized` itself.
const BACKSTOP_FALLBACK_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Scanner failure.
#[derive(Debug, thiserror::Error)]
pub enum ScannerError {
    /// A route file or environment value is invalid.
    #[error("invalid scanner configuration: {0}")]
    Configuration(String),
    /// A chain read failed.
    #[error("{0}")]
    Chain(#[from] ChainError),
    /// A database operation failed.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
    /// The provider finalized head is behind the durable cursor: a node that has not caught up,
    /// such as a load-balanced gateway's after a restart; the pass is retried.
    #[error("provider finalized head {finalized} is behind durable cursor {cursor}")]
    FinalizedBehindCursor {
        /// Durable fully scanned block.
        cursor: u64,
        /// Provider's current finalized block.
        finalized: u64,
    },
    /// A transfer returned for the filter did not resolve to a tracked address.
    #[error("transfer recipient {0:#x} is not tracked")]
    UnknownRecipient(Address),
    /// A per-chain scanner task stopped unexpectedly.
    #[error("scanner task failed: {0}")]
    Task(String),
}

impl ScannerError {
    /// Whether the pass may succeed on a later attempt. A provider answering a finalized head below
    /// one it answered before, or below the durable cursor, is a node that has not caught up: the
    /// pass waits for it, with the scanner's monitor unhealthy meanwhile.
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Database(_)
                | Self::FinalizedBehindCursor { .. }
                | Self::Chain(
                    ChainError::Rpc(_)
                        | ChainError::Group(_)
                        | ChainError::Transport(_)
                        | ChainError::MissingField(_)
                        | ChainError::InvalidTimestamp(_)
                        | ChainError::InvalidTransfer(_)
                        | ChainError::Reorganized(_)
                        | ChainError::FinalizedHeadRegressed { .. }
                )
        )
    }

    fn category(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "configuration",
            Self::Chain(ChainError::FinalizedHeadRegressed { .. }) => "finalized_regression",
            Self::Chain(_) => "chain_read",
            Self::Database(_) => "database",
            Self::FinalizedBehindCursor { .. } => "finalized_behind_cursor",
            Self::UnknownRecipient(_) => "unknown_recipient",
            Self::Task(_) => "task",
        }
    }
}

/// Active routes and chain settings for one EVM chain.
#[derive(Clone, Debug)]
pub struct ChainRoutes {
    /// Shared chain configuration.
    pub chain: ChainConfig,
    routes: BTreeMap<Address, RouteSelection>,
    backstop: Backstop,
}

impl ChainRoutes {
    /// Whether transfers are requested token-wide and kept locally: unless a current route of the
    /// chain asks for address mode, whose any-token requests then cover every route.
    #[must_use]
    pub fn token_mode(&self) -> bool {
        self.backstop == Backstop::Token
    }
}

#[derive(Clone, Debug)]
struct RouteSelection {
    name: String,
    version: u64,
}

/// Work completed by one polling pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanStats {
    /// Deposits newly inserted; duplicate chain logs are excluded.
    pub inserted: u64,
    /// Highest cursor committed during the pass.
    pub cursor: u64,
    /// Finalized head observed during the pass.
    pub finalized: u64,
    /// Addresses whose one-time historical backfill completed.
    pub backfilled_addresses: u64,
    /// More address pages or block windows remain; continue without waiting for a new head.
    pub work_remaining: bool,
    /// Factory events recorded for known addresses.
    pub factory: db::FactoryCommit,
}

impl ScanStats {
    fn record_inserted(&mut self, inserted: u64) -> Result<(), ScannerError> {
        self.inserted = self
            .inserted
            .checked_add(inserted)
            .ok_or_else(|| ScannerError::Configuration("scan count overflow".to_owned()))?;
        Ok(())
    }

    fn record_factory(&mut self, commit: db::FactoryCommit) {
        self.factory.created = self.factory.created.saturating_add(commit.created);
        self.factory.flushed = self.factory.flushed.saturating_add(commit.flushed);
        self.factory.failed = self.factory.failed.saturating_add(commit.failed);
        self.factory.swept = self.factory.swept.saturating_add(commit.swept);
    }

    fn record_backfilled(&mut self, count: usize) -> Result<(), ScannerError> {
        let count = u64::try_from(count).map_err(|error| {
            ScannerError::Configuration(format!("backfill count is outside u64: {error}"))
        })?;
        self.backfilled_addresses = self
            .backfilled_addresses
            .checked_add(count)
            .ok_or_else(|| ScannerError::Configuration("backfill count overflow".to_owned()))?;
        Ok(())
    }
}

/// Groups the current route version of each chain asset by chain.
#[must_use]
pub fn chain_routes(routes: &RouteSet) -> Vec<ChainRoutes> {
    routes
        .chain_ids()
        .filter_map(|chain_id| {
            let chain = routes.chain(chain_id)?.clone();
            let backstop = if routes.current().any(|route| {
                route.chain.chain_id == chain_id && route.asset.backstop == Backstop::Addresses
            }) {
                Backstop::Addresses
            } else {
                Backstop::Token
            };
            let selected = routes
                .current()
                .filter(|route| route.chain.chain_id == chain_id)
                .map(|route| {
                    (
                        route.asset.contract,
                        RouteSelection {
                            name: route.route.clone(),
                            version: route.version,
                        },
                    )
                })
                .collect();
            Some(ChainRoutes {
                chain,
                routes: selected,
                backstop,
            })
        })
        .collect()
}

/// Starts the finalized cursor of every configured chain that has none at provider A's current
/// `finalized` head; `topup run` calls it before the API can issue an address on the chain.
///
/// Addresses are issued at their chain's committed cursor and backfilled from it (§8). Without a
/// cursor they would be issued at block 0, and the chain's first pass would walk its whole history
/// from genesis, so an address issued meanwhile would be created deep in that history and
/// backfilled from there before the cursor could move (Base Sepolia on staging, 2026-09-29). A
/// quote's address is derived when it is issued, and a deposit address's network on a chain is
/// covered from the chain's cursor when it is issued, so no deposit precedes the chain's cursor.
pub async fn initialize_cursors(pool: &PgPool, route_set: &RouteSet) -> Result<(), ScannerError> {
    for chain_id in route_set.chain_ids() {
        let client = route_set
            .provider(chain_id, 0)
            .map_err(|error| ScannerError::Configuration(error.to_string()))?;
        if db::get_cursor(pool, chain_id).await?.is_some() {
            continue;
        }
        if let Some(a) = client.group() {
            let b = route_set
                .provider(chain_id, 1)
                .map_err(|e| ScannerError::Configuration(e.to_string()))?
                .group()
                .ok_or_else(|| ScannerError::Configuration("RPC B group missing".to_owned()))?;
            let (Ok(ai), Ok(bi)) = (
                a.select(&Default::default(), None),
                b.select(&Default::default(), None),
            ) else {
                continue;
            };
            let deadline = tokio::time::Instant::now()
                .checked_add(Duration::from_millis(
                    a.policy.total_deadline_ms.min(b.policy.total_deadline_ms),
                ))
                .unwrap_or_else(tokio::time::Instant::now);
            let height = a
                .head(ai, "finalized", deadline)
                .await
                .map_err(ChainError::Group)?
                .number
                .min(
                    b.head(bi, "finalized", deadline)
                        .await
                        .map_err(ChainError::Group)?
                        .number,
                );
            let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{height:x}"),false]});
            let av = a
                .send(ai, &request, deadline)
                .await
                .map_err(ChainError::Group)?;
            let bv = b
                .send(bi, &request, deadline)
                .await
                .map_err(ChainError::Group)?;
            let ah = topup_adapters::chain::evm::group::HeadAnchor::parse(av.get("result").ok_or(
                ChainError::Group(topup_adapters::chain::evm::group::Failure::Malformed),
            )?)
            .map_err(ChainError::Group)?;
            let bh = topup_adapters::chain::evm::group::HeadAnchor::parse(bv.get("result").ok_or(
                ChainError::Group(topup_adapters::chain::evm::group::Failure::Malformed),
            )?)
            .map_err(ChainError::Group)?;
            if ah != bh {
                a.freeze().await.map_err(ChainError::Group)?;
                return Err(
                    ChainError::Group(topup_adapters::chain::evm::group::Failure::Fork).into(),
                );
            }
            a.persist_cursor(ai, &ah).await.map_err(ChainError::Group)?;
            b.persist_cursor(bi, &ah).await.map_err(ChainError::Group)?;
            let timestamp = av
                .get("result")
                .and_then(|v| v.get("timestamp"))
                .and_then(serde_json::Value::as_str)
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .and_then(|n| i64::try_from(n).ok())
                .and_then(|n| DateTime::from_timestamp(n, 0))
                .ok_or(ChainError::Group(
                    topup_adapters::chain::evm::group::Failure::Malformed,
                ))?;
            db::initialize_cursor(pool, chain_id, height, timestamp).await?;
        } else {
            initialize_cursor(pool, &FinalizedReader::new(Arc::clone(client)), chain_id).await?;
        }
    }
    Ok(())
}

/// Starts `chain_id`'s finalized cursor at `reader`'s `finalized` head unless it has one; returns
/// the cursor it started.
pub async fn initialize_cursor<R: ChainReader>(
    pool: &PgPool,
    reader: &R,
    chain_id: u64,
) -> Result<Option<u64>, ScannerError> {
    if db::get_cursor(pool, chain_id).await?.is_some() {
        return Ok(None);
    }
    let head = reader.finalized_head().await?;
    if !db::initialize_cursor(pool, chain_id, head.number, head.time).await? {
        return Ok(None);
    }
    tracing::info!(
        chain_id,
        cursor = head.number,
        "finalized cursor started at the provider's finalized head"
    );
    Ok(Some(head.number))
}

/// Scans one chain through its current finalized head and commits durable progress.
///
/// Each window records the transfers to every address and the factory's events about them before
/// the cursor passes it, so everything at or below the cursor is indexed at finality. A window
/// costs one transfer request (token mode, or one per 1 000 addresses in address mode) and one
/// factory request, whatever the number of addresses.
pub async fn scan_once<R: ChainReader>(
    pool: &PgPool,
    reader: &R,
    routes: &ChainRoutes,
) -> Result<ScanStats, ScannerError> {
    let chain_id = routes.chain.chain_id;
    let cursor = db::get_cursor(pool, chain_id).await?.unwrap_or(0);
    if crate::reconciler::chain_is_blocked(pool, chain_id).await? {
        tracing::warn!(
            chain_id,
            "finalized chain scan paused because reconciliation froze the chain"
        );
        return Ok(ScanStats {
            cursor,
            ..ScanStats::default()
        });
    }
    let head = reader.finalized_head().await?;
    let finalized = head.number;
    if finalized < cursor {
        return Err(ScannerError::FinalizedBehindCursor { cursor, finalized });
    }
    let sweep = db::address_sweep(pool, chain_id, "finalized", cursor).await?;
    let epoch = db::sweep_epoch(pool, chain_id).await?;
    let target = match &sweep {
        Some(sweep) => u64::try_from(sweep.through_block)
            .map_err(|_| ScannerError::Configuration("invalid sweep height".into()))?,
        None => finalized.min(cursor.saturating_add(MAX_SCAN_WINDOW * MAX_WINDOWS_PER_SCAN)),
    };
    if target > finalized {
        return Err(ScannerError::FinalizedBehindCursor {
            cursor: target,
            finalized,
        });
    }
    let target_time = sweep
        .as_ref()
        .map_or((target == finalized).then_some(head.time), |s| s.block_time);
    let (addresses, more) =
        db::scan_address_page(pool, chain_id, sweep.as_ref().map(|s| s.last_id)).await?;
    let sweep_progress = if more {
        Some(db::AddressSweep {
            epoch,
            anchor: i64::try_from(cursor)
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            from_block: i64::try_from(cursor.saturating_add(1))
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            through_block: i64::try_from(target)
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            block_time: target_time,
            horizon: None,
            last_id: addresses
                .last()
                .ok_or_else(|| ScannerError::Configuration("empty address page".into()))?
                .id,
        })
    } else {
        None
    };
    let mut stats = ScanStats {
        cursor,
        finalized,
        work_remaining: more || target < finalized,
        ..ScanStats::default()
    };

    for (id, mut request, answering) in db::rpc::due_reviews(pool, chain_id).await? {
        if request.to > finalized {
            continue;
        }
        request.finalized = true;
        // A singleton still replays; independent coverage stays pending until another member exists.
        request.exclude_member = if reader.independent_review_available(&answering) {
            Some(answering)
        } else {
            None
        };
        match reader.read_window(&request).await {
            Ok(window) => {
                let review_addresses =
                    addresses_for_logs(pool, chain_id, &window.transfers).await?;
                let index = address_index(&review_addresses);
                let deposits = resolve_logs(window.transfers, &index, routes)?;
                db::rpc::commit_window(
                    pool,
                    chain_id,
                    &deposits,
                    &window.factory_logs,
                    window.proof.as_ref(),
                    db::rpc::WindowProgress {
                        reviewed: Some(id),
                        ..Default::default()
                    },
                )
                .await?;
                break;
            }
            Err(ChainError::Group(topup_adapters::chain::evm::group::Failure::Unavailable)) => {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    // Addresses issued at or below the cursor are read once from their creation block to the
    // cursor, together. Each committed window is kept, so a failed pass or a restart resumes the
    // backfill where it stopped: the cursor waits for it, and one that restarted from its creation
    // block after every failure would never complete once it outlasts a provider's budget.
    let pending_backfills = addresses
        .iter()
        .filter(|address| !address.backfilled && address.created_block <= cursor)
        .cloned()
        .collect::<Vec<_>>();
    if let Some(from) = pending_backfills
        .iter()
        .map(ScanAddress::backfill_start)
        .min()
    {
        let through = cursor.min(finalized);
        let windows = if from <= through {
            scan_windows(
                from,
                through.min(from.saturating_add(MAX_SCAN_WINDOW * MAX_WINDOWS_PER_SCAN - 1)),
            )?
        } else {
            Vec::new()
        };
        for (from_block, to_block) in windows {
            let window = pending_backfills
                .iter()
                .filter(|address| address.backfill_start() <= to_block)
                .cloned()
                .collect::<Vec<_>>();
            let committed = scan_window(
                pool,
                reader,
                routes,
                &window,
                from_block,
                to_block,
                db::rpc::WindowProgress {
                    through: Some((window.iter().map(|a| a.id).collect(), to_block)),
                    backfilled: if to_block == through {
                        window.iter().map(|a| a.id).collect()
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                },
            )
            .await?;
            record_committed(chain_id, &mut stats, &committed.0)?;
            stats.record_factory(committed.1);
        }
        let ids = pending_backfills
            .iter()
            .map(|address| address.id)
            .collect::<Vec<_>>();

        if from <= through && through.saturating_sub(from) >= MAX_SCAN_WINDOW * MAX_WINDOWS_PER_SCAN
        {
            stats.work_remaining = true;
            return Ok(stats);
        }
        stats.record_backfilled(ids.len())?;
    }

    let mut pending_backfill_marks = addresses
        .iter()
        .filter(|address| !address.backfilled)
        .map(|address| (address.id, address.created_block))
        .collect::<BTreeMap<_, _>>();
    for address in &pending_backfills {
        pending_backfill_marks.remove(&address.id);
    }
    let Some(start) = cursor.checked_add(1) else {
        return Ok(stats);
    };
    if start > target {
        db::save_address_sweep(pool, chain_id, "finalized", sweep_progress.as_ref()).await?;
        return Ok(stats);
    }

    for (from_block, to_block) in scan_windows(start, target)? {
        let backfilled = pending_backfill_marks
            .iter()
            .filter(|(_, created)| **created <= to_block)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let progress = db::rpc::WindowProgress {
            scanned: (!more).then_some((
                to_block,
                (to_block == target).then_some(target_time).flatten(),
            )),
            backfilled: backfilled.clone(),
            ..Default::default()
        };
        let committed = scan_window(
            pool, reader, routes, &addresses, from_block, to_block, progress,
        )
        .await?;
        record_committed(chain_id, &mut stats, &committed.0)?;
        stats.record_factory(committed.1);
        stats.record_backfilled(backfilled.len())?;
        for id in &backfilled {
            pending_backfill_marks.remove(id);
        }
        if !more {
            stats.cursor = to_block;
        }
    }
    db::save_address_sweep(pool, chain_id, "finalized", sweep_progress.as_ref()).await?;
    Ok(stats)
}

/// Historical reviews resolve only recipients actually returned by the bounded RPC window.
async fn addresses_for_logs(
    pool: &PgPool,
    chain: u64,
    logs: &[TransferLog],
) -> Result<Vec<ScanAddress>, ScannerError> {
    let recipients = logs
        .iter()
        .map(|log| log.to)
        .collect::<std::collections::BTreeSet<_>>();
    let mut addresses = Vec::new();
    for recipient in recipients {
        if let Some(address) = db::find_scan_address(pool, chain, recipient).await? {
            addresses.push(address);
        }
    }
    Ok(addresses)
}

/// Fixed selectors shared by finalized, head and historical review reads.
pub(crate) fn window_request(
    routes: &ChainRoutes,
    addresses: &[ScanAddress],
    from: u64,
    to: u64,
    finalized: bool,
) -> topup_adapters::chain::evm::window::WindowRequest {
    topup_adapters::chain::evm::window::WindowRequest {
        from,
        to,
        recipients: addresses.iter().map(|a| a.address).collect(),
        tokens: if routes.token_mode() {
            routes.routes.keys().copied().collect()
        } else {
            Vec::new()
        },
        factory: finalized.then_some(routes.chain.contracts.forwarder_factory),
        finalized,
        exclude_member: None,
    }
}
/// Records one window's transfers to `addresses` at or above each address's creation block, and
/// the factory's events about them.
async fn scan_window<R: ChainReader>(
    pool: &PgPool,
    reader: &R,
    routes: &ChainRoutes,
    addresses: &[ScanAddress],
    from_block: u64,
    to_block: u64,
    progress: db::rpc::WindowProgress,
) -> Result<(ScanCommit, db::FactoryCommit), ScannerError> {
    let chain_id = routes.chain.chain_id;
    let span = crate::observability::scanner_window_span(chain_id, from_block, to_block);
    async {
        let index = address_index(addresses);
        let created = |address: Address| index.get(&address).map(|known| known.created_block);
        let request = window_request(routes, addresses, from_block, to_block, true);
        let window = backing_off(|| reader.read_window(&request)).await?;
        let logs = window
            .transfers
            .into_iter()
            .filter(|log| created(log.to).is_some_and(|block| log.block_number >= block))
            .collect();
        let deposits = resolve_logs(logs, &index, routes)?;
        let factory_logs = window
            .factory_logs
            .into_iter()
            .filter(|log| {
                created(log.event.forwarder()).is_some_and(|block| log.block_number >= block)
            })
            .collect::<Vec<_>>();
        let (committed, indexed) = db::rpc::commit_window(
            pool,
            chain_id,
            &deposits,
            &factory_logs,
            window.proof.as_ref(),
            progress,
        )
        .await?;
        Ok((committed, indexed))
    }
    .instrument(span)
    .await
}

/// Runs every configured chain scanner until cancellation or all chains stop, publishing each
/// chain's `finalized` advances to `finalized_heads`.
pub async fn run(
    pool: PgPool,
    route_set: &RouteSet,
    config: ScanConfig,
    finalized_heads: FinalizedHeads,
    cancellation: CancellationToken,
) -> Result<(), ScannerError> {
    let scanners = cancellation.child_token();
    let mut tasks = JoinSet::new();
    for routes in chain_routes(route_set) {
        let chain_id = routes.chain.chain_id;
        let client = route_set
            .provider(chain_id, 0)
            .map_err(|error| ScannerError::Configuration(error.to_string()))?;
        let reader = FinalizedReader::new(Arc::clone(client));
        let chain_pool = pool.clone();
        let chain_cancellation = scanners.child_token();
        let chain_heads = finalized_heads.clone();
        tasks.spawn(async move {
            let result = run_chain(
                chain_pool,
                reader,
                routes,
                config,
                chain_heads,
                chain_cancellation,
            )
            .await;
            (chain_id, result)
        });
    }

    if tasks.is_empty() {
        return Err(ScannerError::Configuration(
            "no chain scanners were configured".to_owned(),
        ));
    }
    supervise(tasks, &scanners).await
}

/// Waits for the chain tasks until `scanners` is cancelled. The first chain to stop before then
/// stops every other chain and returns its error, so the scanner's service task fails and the
/// process exits to be restarted, instead of reporting healthy while one chain is not scanned.
async fn supervise(
    mut tasks: JoinSet<(u64, Result<(), ScannerError>)>,
    scanners: &CancellationToken,
) -> Result<(), ScannerError> {
    let mut failure = None;
    while let Some(joined) = tasks.join_next().await {
        let (chain_id, error) = match joined {
            Ok((_, Ok(()))) if scanners.is_cancelled() => continue,
            Ok((chain_id, Ok(()))) => (
                Some(chain_id),
                ScannerError::Task("chain scanner exited unexpectedly".to_owned()),
            ),
            Ok((chain_id, Err(error))) => (Some(chain_id), error),
            Err(error) => (None, ScannerError::Task(error.to_string())),
        };
        tracing::error!(
            chain_id,
            error_category = error.category(),
            %error,
            "chain scanner task stopped; stopping every chain scanner"
        );
        scanners.cancel();
        failure.get_or_insert(error);
    }
    failure.map_or(Ok(()), Err)
}

/// Runs one chain's head loop and finalized backstop on provider A's `reader` until
/// cancellation, or until the backstop stops on a non-retryable failure.
pub async fn run_chain(
    pool: PgPool,
    reader: FinalizedReader,
    routes: ChainRoutes,
    config: ScanConfig,
    finalized_heads: FinalizedHeads,
    cancellation: CancellationToken,
) -> Result<(), ScannerError> {
    let chain_id = routes.chain.chain_id;
    let backstop_healthy = AtomicBool::new(true);
    // The published head when the last pass started; the next pass waits for a higher one.
    let scanned_from = AtomicU64::new(0);
    let finalized_scan = run_scan_loop(
        chain_id,
        &backstop_healthy,
        cancellation.clone(),
        || {
            let published = finalized_heads.get(chain_id).map_or(0, |head| head.number);
            scanned_from.store(published, Ordering::Relaxed);
            let (pool, reader, routes, scanned_from) = (&pool, &reader, &routes, &scanned_from);
            async move {
                let result = scan_once(pool, reader, routes).await;
                if let Ok(stats) = &result {
                    scanned_from.fetch_max(stats.finalized, Ordering::Relaxed);
                }
                result
            }
        },
        |delay| {
            let mut advances = finalized_heads.subscribe();
            let known = scanned_from.load(Ordering::Relaxed);
            async move {
                let advanced = async {
                    let waited = advances
                        .wait_for(|heads| {
                            heads.get(&chain_id).is_some_and(|head| head.number > known)
                        })
                        .await
                        .is_ok();
                    if !waited {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = advanced => {}
                }
            }
        },
        || OsJitter.next_u64(),
    );
    let head_loop = head::run_head_loop(
        &pool,
        &reader,
        &routes,
        &config,
        &finalized_heads,
        &backstop_healthy,
        cancellation,
    );
    // The head loop returns only on cancellation; the finalized loop's result is the chain's.
    tokio::select! {
        result = finalized_scan => result,
        () = head_loop => Ok(()),
    }
}

/// Runs the finalized backstop after every `finalized` advance `sleep` waits for, or after
/// [`BACKSTOP_FALLBACK_INTERVAL`]; a transient failure retries with backoff and marks the
/// backstop unhealthy for the scanner's monitor until a pass succeeds.
async fn run_scan_loop<Scan, ScanFuture, Sleep, SleepFuture, Jitter>(
    chain_id: u64,
    healthy: &AtomicBool,
    cancellation: CancellationToken,
    mut scan: Scan,
    mut sleep: Sleep,
    mut jitter: Jitter,
) -> Result<(), ScannerError>
where
    Scan: FnMut() -> ScanFuture,
    ScanFuture: Future<Output = Result<ScanStats, ScannerError>>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Jitter: FnMut() -> u64,
{
    let mut retry_attempt = 0_u32;
    loop {
        let result = tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            result = scan() => result,
        };
        let delay = match result {
            Ok(stats) => {
                retry_attempt = 0;
                healthy.store(true, Ordering::Relaxed);
                tracing::info!(
                    chain_id,
                    cursor = stats.cursor,
                    inserted = stats.inserted,
                    backfilled_addresses = stats.backfilled_addresses,
                    flushed = stats.factory.flushed,
                    flush_failed = stats.factory.failed,
                    swept = stats.factory.swept,
                    "finalized chain scan committed"
                );
                if stats.work_remaining {
                    Duration::from_millis(100)
                } else {
                    BACKSTOP_FALLBACK_INTERVAL
                }
            }
            Err(error) if error.is_retryable() => {
                healthy.store(false, Ordering::Relaxed);
                let delay = backoff(retry_attempt, jitter());
                retry_attempt = retry_attempt.saturating_add(1);
                tracing::warn!(
                    chain_id,
                    error_category = error.category(),
                    %error,
                    retry_after_seconds = delay.as_secs(),
                    "finalized chain scan failed transiently"
                );
                delay
            }
            Err(error) => {
                healthy.store(false, Ordering::Relaxed);
                tracing::error!(
                    chain_id,
                    error_category = error.category(),
                    %error,
                    "finalized chain scanner stopped"
                );
                return Err(error);
            }
        };
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = sleep(delay) => {}
        }
    }
}

fn record_committed(
    chain_id: u64,
    stats: &mut ScanStats,
    committed: &ScanCommit,
) -> Result<(), ScannerError> {
    stats.record_inserted(committed.inserted)?;
    report_unsupported_inflows(chain_id, committed.unsupported_inserted);
    Ok(())
}

/// Raises `TopupUnsupportedInflows` for newly recorded transfers of unrouted tokens.
pub(crate) fn report_unsupported_inflows(chain_id: u64, count: u64) {
    if count > 0 {
        tracing::warn!(
            tags.alert = "TopupUnsupportedInflows",
            tags.chain_id = chain_id,
            count,
            "unsupported finalized inflows observed"
        );
    }
}

fn address_index(addresses: &[ScanAddress]) -> BTreeMap<Address, ScanAddress> {
    addresses
        .iter()
        .cloned()
        .map(|address| (address.address, address))
        .collect()
}

pub(crate) fn resolve_logs_for_reconciliation(
    logs: Vec<TransferLog>,
    addresses: &[ScanAddress],
    routes: &ChainRoutes,
) -> Result<Vec<NewDeposit>, ScannerError> {
    resolve_logs(logs, &address_index(addresses), routes)
}

fn resolve_logs(
    logs: Vec<TransferLog>,
    addresses: &BTreeMap<Address, ScanAddress>,
    routes: &ChainRoutes,
) -> Result<Vec<NewDeposit>, ScannerError> {
    let next_attempt_at = Utc::now();
    logs.into_iter()
        .map(|log| {
            let address = addresses
                .get(&log.to)
                .ok_or(ScannerError::UnknownRecipient(log.to))?;
            Ok(resolve_log(log, address, routes, next_attempt_at))
        })
        .collect()
}

/// The deposit a transfer to the issued `address` is recorded as: `detected` on the route of its
/// token, or `rejected(unsupported_asset)` for a token without one.
pub(crate) fn resolve_log(
    log: TransferLog,
    address: &ScanAddress,
    routes: &ChainRoutes,
    next_attempt_at: DateTime<Utc>,
) -> NewDeposit {
    let selected = routes.routes.get(&log.token);
    NewDeposit {
        chain_id: routes.chain.chain_id,
        tx_hash: log.tx_hash,
        receipt_log_index: log.receipt_log_index,
        log_index: log.log_index,
        block_number: log.block_number,
        block_hash: log.block_hash,
        block_time: log.block_time,
        address_id: address.id,
        route: selected.map(|route| route.name.clone()),
        route_version: selected.map(|route| route.version),
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

fn scan_windows(from_block: u64, to_block: u64) -> Result<Vec<(u64, u64)>, ScannerError> {
    if from_block > to_block {
        return Err(ScannerError::Configuration(format!(
            "scan range starts at {from_block} after {to_block}"
        )));
    }
    let mut windows = Vec::new();
    let mut start = from_block;
    loop {
        let end = start
            .saturating_add(MAX_SCAN_WINDOW.saturating_sub(1))
            .min(to_block);
        windows.push((start, end));
        if end == to_block {
            break;
        }
        start = end
            .checked_add(1)
            .ok_or_else(|| ScannerError::Configuration("scan block range overflow".to_owned()))?;
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future;
    use std::sync::Mutex;

    use alloy_primitives::{B256, U256};
    use chrono::DateTime;
    use topup_core::money::AtomicAmount;
    use topup_core::route::RouteFile;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn scanner_windows_are_inclusive_and_bounded() {
        assert_eq!(
            scan_windows(1, 4_001).expect("valid range"),
            vec![(1, 2_000), (2_001, 4_000), (4_001, 4_001)]
        );
    }

    #[tokio::test]
    async fn unfinished_pages_continue_without_waiting_for_a_new_finalized_head() {
        let mut results = VecDeque::from([
            Ok(ScanStats {
                work_remaining: true,
                ..ScanStats::default()
            }),
            Err(ScannerError::UnknownRecipient(Address::ZERO)),
        ]);
        let delays = Mutex::new(Vec::new());
        let result = run_scan_loop(
            1,
            &AtomicBool::new(true),
            CancellationToken::new(),
            || std::future::ready(results.pop_front().expect("one next page")),
            |delay| {
                delays.lock().unwrap().push(delay);
                std::future::ready(())
            },
            || 0,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(*delays.lock().unwrap(), vec![Duration::from_millis(100)]);
    }

    #[tokio::test]
    async fn scan_loop_retries_transient_failures_and_a_stale_finalized_head() {
        let results = Mutex::new(VecDeque::from([
            Err(ScannerError::Database(sqlx::Error::PoolTimedOut)),
            // A gateway node behind the one answered before, then behind the committed cursor
            // after a restart: the Base Sepolia incident of 2026-09-29.
            Err(ScannerError::Chain(ChainError::FinalizedHeadRegressed {
                previous: 47_446_696,
                current: 47_446_540,
            })),
            Err(ScannerError::FinalizedBehindCursor {
                cursor: 47_446_696,
                finalized: 47_446_540,
            }),
            Ok(ScanStats {
                inserted: 1,
                cursor: 2,
                finalized: 2,
                backfilled_addresses: 0,
                work_remaining: false,
                factory: db::FactoryCommit::default(),
            }),
            Err(ScannerError::UnknownRecipient(Address::ZERO)),
        ]));
        let sleeps = Mutex::new(Vec::new());
        let health = Mutex::new(Vec::new());

        let healthy = AtomicBool::new(true);
        let error = run_scan_loop(
            1,
            &healthy,
            CancellationToken::new(),
            || {
                health
                    .lock()
                    .expect("health lock")
                    .push(healthy.load(Ordering::Relaxed));
                future::ready(
                    results
                        .lock()
                        .expect("results lock")
                        .pop_front()
                        .expect("scripted scan result"),
                )
            },
            |duration| {
                sleeps.lock().expect("sleeps lock").push(duration);
                future::ready(())
            },
            || u64::MAX,
        )
        .await
        .expect_err("a non-retryable failure stops the loop");

        assert!(matches!(error, ScannerError::UnknownRecipient(_)));
        assert_eq!(
            *sleeps.lock().expect("sleeps lock"),
            vec![
                backoff(0, u64::MAX),
                backoff(1, u64::MAX),
                backoff(2, u64::MAX),
                BACKSTOP_FALLBACK_INTERVAL
            ]
        );
        // Unhealthy for the monitor while retrying, healthy again after the pass that succeeded.
        assert_eq!(
            *health.lock().expect("health lock"),
            vec![true, false, false, false, true]
        );
        assert!(!healthy.load(Ordering::Relaxed));
        assert!(results.lock().expect("results lock").is_empty());
    }

    #[tokio::test]
    async fn a_provider_that_never_answers_is_retried_instead_of_stalling_the_chain() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        let node = axum::Router::new().route("/", axum::routing::post(future::pending::<String>));
        let server = tokio::spawn(async move { axum::serve(listener, node).await });
        let reader = FinalizedReader::new(Arc::new(
            topup_adapters::chain::evm::EvmClient::with_timeout(
                &format!("http://{address}/"),
                Duration::from_millis(100),
            )
            .expect("local URL")
            .with_provider("provider-a"),
        ));
        let healthy = AtomicBool::new(true);
        let cancellation = CancellationToken::new();
        let sleeps = Mutex::new(Vec::new());

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_scan_loop(
                84_532,
                &healthy,
                cancellation.clone(),
                || async {
                    reader.finalized_head().await?;
                    Ok(ScanStats::default())
                },
                |duration| {
                    sleeps.lock().expect("sleeps lock").push(duration);
                    cancellation.cancel();
                    future::ready(())
                },
                || u64::MAX,
            ),
        )
        .await;
        server.abort();

        result
            .expect("the backstop's read is bounded")
            .expect("a timed-out read is retried, not fatal");
        assert_eq!(
            *sleeps.lock().expect("sleeps lock"),
            vec![backoff(0, u64::MAX)]
        );
        assert!(!healthy.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn one_stopped_chain_stops_every_chain_and_fails_the_scanner() {
        let scanners = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let healthy = scanners.child_token();
        tasks.spawn(async move {
            healthy.cancelled().await;
            (11_155_111, Ok(()))
        });
        tasks.spawn(async { (84_532, Err(ScannerError::UnknownRecipient(Address::ZERO))) });

        let error = tokio::time::timeout(Duration::from_secs(5), supervise(tasks, &scanners))
            .await
            .expect("the healthy chain is stopped instead of keeping the scanner alive")
            .expect_err("a stopped chain fails the scanner");

        assert!(matches!(error, ScannerError::UnknownRecipient(_)));
        assert!(scanners.is_cancelled());
    }

    #[tokio::test]
    async fn chains_stopped_by_shutdown_end_the_scanner_cleanly() {
        let scanners = CancellationToken::new();
        let mut tasks = JoinSet::new();
        for chain_id in [1, 84_532] {
            let chain = scanners.child_token();
            tasks.spawn(async move {
                chain.cancelled().await;
                (chain_id, Ok(()))
            });
        }
        scanners.cancel();

        supervise(tasks, &scanners)
            .await
            .expect("shutdown is not a failure");
    }

    #[test]
    fn highest_route_version_is_current_for_an_asset() {
        let token = Address::from([7_u8; 20]);
        let mut older = test_route_file(token);
        older.version = 1;
        let mut newer = older.clone();
        newer.version = 2;

        let chains = chain_routes(&RouteSet::new(vec![newer, older]).expect("versioned routes"));
        let chain = chains.first().expect("one chain");
        let selected = chain.routes.get(&token).expect("selected route");

        assert_eq!(selected.version, 2);
    }

    #[test]
    fn unsupported_asset_is_born_rejected() {
        let recipient = Address::from([1_u8; 20]);
        let address = ScanAddress {
            id: Uuid::new_v4(),
            address: recipient,
            created_block: 0,
            backfilled: false,
            backfilled_through: None,
        };
        let routes = test_routes(Address::from([2_u8; 20]));
        let log = TransferLog {
            tx_hash: B256::from([3_u8; 32]),
            receipt_log_index: 0,
            log_index: 0,
            block_number: 1,
            block_hash: B256::from([4_u8; 32]),
            block_time: DateTime::from_timestamp(1, 0).expect("timestamp"),
            tx_from: Address::from([6_u8; 20]),
            tx_nonce: 0,
            token: Address::from([5_u8; 20]),
            from: Address::from([6_u8; 20]),
            to: recipient,
            amount: AtomicAmount::new(U256::from(7)),
        };
        let deposits =
            resolve_logs(vec![log], &address_index(&[address]), &routes).expect("resolve log");
        let deposit = deposits.first().expect("one deposit");
        assert_eq!(deposit.state, DepositState::Rejected);
        assert_eq!(deposit.reason, Some(RejectReason::UnsupportedAsset));
        assert_eq!(deposit.route, None);
    }

    #[test]
    fn every_supported_asset_sent_to_one_address_selects_its_route() {
        // A deposit address takes every token of its chain: each transfer selects the route of its
        // token, and only a token without one is rejected.
        let recipient = Address::from([1_u8; 20]);
        let address = ScanAddress {
            id: Uuid::new_v4(),
            address: recipient,
            created_block: 0,
            backfilled: false,
            backfilled_through: None,
        };
        let pha = Address::from([2_u8; 20]);
        let usdc = Address::from([3_u8; 20]);
        let mut second = test_route_file(usdc);
        second.route = "phala-cloud-ethereum-usdc-usd".to_owned();
        second.asset.symbol = "usdc".to_owned();
        second.pricing.mode = topup_core::route::PricingMode::Stablecoin;
        second.pricing.primary.clear();
        second.pricing.check.clear();
        second.pricing.fx.clear();
        second.pricing.sources = vec![
            topup_core::price::Source::Chainlink {
                feed: "USDC_USD".into(),
                chain_id: 1,
                rpc_group: "a".into(),
                rpc_group_b: None,
                observation_chain_id: None,
            },
            topup_core::price::Source::Kraken {
                symbol: "USDCUSD".into(),
                company: "kraken".into(),
            },
        ];
        let chains = chain_routes(
            &RouteSet::new(vec![test_route_file(pha), second]).expect("two assets on one chain"),
        );
        let routes = chains.first().expect("one chain");
        let log = |token: Address, index: u64| TransferLog {
            tx_hash: B256::from([3_u8; 32]),
            receipt_log_index: index,
            log_index: index,
            block_number: 1,
            block_hash: B256::from([4_u8; 32]),
            block_time: DateTime::from_timestamp(1, 0).expect("timestamp"),
            tx_from: Address::from([6_u8; 20]),
            tx_nonce: 0,
            token,
            from: Address::from([6_u8; 20]),
            to: recipient,
            amount: AtomicAmount::new(U256::from(7)),
        };
        let deposits = resolve_logs(
            vec![log(pha, 0), log(usdc, 1), log(Address::from([5_u8; 20]), 2)],
            &address_index(&[address]),
            routes,
        )
        .expect("resolve logs");
        let selected: Vec<_> = deposits
            .iter()
            .map(|deposit| (deposit.route.as_deref(), deposit.state))
            .collect();
        assert_eq!(
            selected,
            vec![
                (Some("phala-cloud-ethereum-pha-usd"), DepositState::Detected),
                (
                    Some("phala-cloud-ethereum-usdc-usd"),
                    DepositState::Detected
                ),
                (None, DepositState::Rejected),
            ]
        );
    }

    fn test_routes(token: Address) -> ChainRoutes {
        let route = test_route_file(token);
        ChainRoutes {
            chain: route.chain,
            routes: BTreeMap::from([(
                token,
                RouteSelection {
                    name: route.route,
                    version: route.version,
                },
            )]),
            backstop: Backstop::Token,
        }
    }

    fn test_route_file(token: Address) -> RouteFile {
        let yaml = include_str!("../../tests/fixtures/phala-cloud-pha.yaml").replace(
            "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
            &format!("{token:#x}"),
        );
        serde_saphyr::from_str(&yaml).expect("route fixture")
    }
}
