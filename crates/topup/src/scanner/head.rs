//! The head loop and the per-block scan (architecture §8).
//!
//! One loop per chain polls provider A's `latest` with `eth_blockNumber` about once per block
//! time, and `finalized` on a slower cadence. The polls lock onto the chain's rhythm
//! ([`next_poll_delay`]), so a block is seen within about a quarter block time of arriving for
//! about 1.1 calls per block. Each new head runs one per-block scan: the
//! `Transfer` logs of the new block range for every issued address, in one request per range
//! (token mode) or per 1 000 addresses (address mode). Transfers at or below the route's
//! confirmation become `detected` deposits, and routed non-zero transfers above the finalized
//! cursor fill the display-only pending view. Nothing else polls provider A's heads: the finalized
//! backstop, the finality watch, and the reconciler wake on the `finalized` advances this loop
//! publishes ([`FinalizedHeads`]).
//!
//! The scan covers `(max(finalized cursor, fast cursor), latest]`, at most one scan window below
//! `latest`; the fast cursor advances to the confirmation horizon, so the blocks above it are
//! read again on the next head, and a block below it is never read by this loop again. A transfer
//! it does not record (in a block it skipped, or introduced below its cursor by a reorg deeper than
//! the confirmation) is recorded by the finalized backstop, whose insert is keyed by the same
//! identity. The address list is read after `latest`, so an address issued later can only be paid
//! in a block above the range scanned without it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{ChainReader, FinalizedHead, FinalizedReader};
use topup_core::route::{ChainConfig, ChainFamily, ChainHeads, Confirmations};

use super::{
    ChainRoutes, MAX_SCAN_WINDOW, ScanConfig, ScannerError, address_index, record_committed,
    resolve_logs,
};
use crate::db::{self, HeadCommit, NewPendingTransfer};

/// Head poll interval of a chain whose route credits at `safe` or `finalized`, or that has no
/// reviewed family: Ethereum's 12-second slot, also the rate at which an OP-stack `safe` head,
/// which follows L1, can advance.
pub const DEFAULT_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// The head poll interval of `chain` without an override: for a route crediting at a depth, whose
/// credit follows each block, its family's block time (12 s on Ethereum, 2 s on an OP-stack
/// chain); otherwise [`DEFAULT_HEAD_POLL_INTERVAL`].
#[must_use]
pub fn head_poll_interval(chain: &ChainConfig) -> Duration {
    match (chain.confirmations, ChainFamily::of(chain.chain_id)) {
        (Confirmations::Depth(_), Some(family)) => Duration::from_secs(family.block_seconds()),
        _ => DEFAULT_HEAD_POLL_INTERVAL,
    }
}

/// Provider A's `finalized` head of each chain, as the head loops last read it.
///
/// The head loops publish; the finalized backstop, the finality watch, and the reconciler wait on
/// an advance instead of reading `finalized` themselves.
#[derive(Clone, Debug)]
pub struct FinalizedHeads {
    heads: Arc<watch::Sender<BTreeMap<u64, FinalizedHead>>>,
}

impl Default for FinalizedHeads {
    fn default() -> Self {
        Self {
            heads: Arc::new(watch::Sender::new(BTreeMap::new())),
        }
    }
}

impl FinalizedHeads {
    /// Records `head` for `chain_id` when it is higher than the published one; returns whether
    /// it advanced.
    pub fn publish(&self, chain_id: u64, head: FinalizedHead) -> bool {
        self.heads.send_if_modified(|heads| {
            if heads
                .get(&chain_id)
                .is_some_and(|known| known.number >= head.number)
            {
                return false;
            }
            heads.insert(chain_id, head);
            true
        })
    }

    /// The published head of `chain_id`.
    #[must_use]
    pub fn get(&self, chain_id: u64) -> Option<FinalizedHead> {
        self.heads.borrow().get(&chain_id).copied()
    }

    /// Returns a receiver notified on every advance of any chain.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<BTreeMap<u64, FinalizedHead>> {
        self.heads.subscribe()
    }
}

/// Outcome of one per-block scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HeadScan {
    /// First block read.
    pub from_block: u64,
    /// Provider A `latest` head read through.
    pub latest: u64,
    /// Highest block at the route's confirmation; `None` for a route crediting at `finalized`.
    pub horizon: Option<u64>,
    /// Deposits newly recorded at the route's confirmation.
    pub inserted: u64,
    /// More addresses remain in the pinned range.
    pub work_remaining: bool,
    /// Pending-view rows written and removed.
    pub commit: HeadCommit,
}

/// Reads provider A's heads and scans the blocks above the scanned range once.
///
/// Returns `None` while reconciliation has frozen the chain, before the finalized scanner's first
/// commit, and when no block is new.
pub async fn head_scan_once<R: ChainReader>(
    pool: &PgPool,
    reader: &R,
    routes: &ChainRoutes,
) -> Result<Option<HeadScan>, ScannerError> {
    let confirmations = routes.chain.confirmations;
    let latest = reader
        .confirmation_heads(Confirmations::Depth(1))
        .await?
        .latest;
    let safe = if confirmations.needs_safe() {
        reader.confirmation_heads(Confirmations::Safe).await?.safe
    } else {
        None
    };
    let finalized = reader.finalized_head().await?.number;
    scan_new_blocks(
        pool,
        reader,
        routes,
        ChainHeads {
            latest,
            safe,
            finalized,
        },
    )
    .await
}

/// Scans `(max(finalized cursor, fast cursor), latest]` for transfers to every issued address:
/// those at or below the confirmation horizon are recorded as deposits, and the routed non-zero
/// ones replace the pending view of the range.
pub async fn scan_new_blocks<R: ChainReader>(
    pool: &PgPool,
    reader: &R,
    routes: &ChainRoutes,
    heads: ChainHeads,
) -> Result<Option<HeadScan>, ScannerError> {
    let chain_id = routes.chain.chain_id;
    if crate::reconciler::chain_is_blocked(pool, chain_id).await? {
        return Ok(None);
    }
    let Some(finalized_cursor) = db::get_cursor(pool, chain_id).await? else {
        return Ok(None);
    };
    let mut latest = heads.latest.unwrap_or(heads.finalized);
    let replay: Option<i64> = sqlx::query_scalar("SELECT min(GREATEST(from_block,COALESCE(replayed_through+1,from_block))) FROM rpc_reorg_ranges WHERE chain_id=$1 AND epoch=COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0) AND COALESCE(replayed_through,from_block-1)<to_block")
        .bind(i64::try_from(chain_id).map_err(|_|ScannerError::Configuration("chain id overflow".into()))?).fetch_one(pool).await?;
    let confirmations = routes.chain.confirmations;
    let scanned = db::get_confirmed_cursor(pool, chain_id)
        .await?
        .unwrap_or(0)
        .max(finalized_cursor);
    let mut from_block = if let Some(replay) = replay {
        let from = u64::try_from(replay)
            .map_err(|_| ScannerError::Configuration("invalid replay height".into()))?;
        latest = latest.min(from.saturating_add(MAX_SCAN_WINDOW.saturating_sub(1)));
        from
    } else {
        scanned
            .saturating_add(1)
            .max(latest.saturating_sub(MAX_SCAN_WINDOW.saturating_sub(1)))
    };
    if from_block > latest {
        return Ok(None);
    }
    // Cap confirmation progress at the window actually read, including bounded replay.
    let mut horizon = (confirmations != Confirmations::Finalized)
        .then(|| confirmations.horizon(heads).min(latest));
    // Read after `latest`: an address issued from here on is paid only above `latest`.
    let sweep = db::address_sweep(pool, chain_id, "head", scanned).await?;
    let epoch = db::sweep_epoch(pool, chain_id).await?;
    if let Some(sweep) = &sweep {
        from_block = u64::try_from(sweep.from_block)
            .map_err(|_| ScannerError::Configuration("invalid sweep start".into()))?;
        latest = u64::try_from(sweep.through_block)
            .map_err(|_| ScannerError::Configuration("invalid sweep height".into()))?;
        horizon = sweep
            .horizon
            .map(u64::try_from)
            .transpose()
            .map_err(|_| ScannerError::Configuration("invalid sweep horizon".into()))?;
    }
    let (addresses, more) =
        db::scan_address_page(pool, chain_id, sweep.as_ref().map(|s| s.last_id)).await?;
    let request = super::window_request(routes, &addresses, from_block, latest, false);
    let window = reader.read_window(&request).await?;
    let logs = window.transfers;
    let index = address_index(&addresses);

    let mut confirmed_deposits = Vec::new();
    if let Some(horizon) = horizon.filter(|horizon| replay.is_some() || *horizon > scanned) {
        let confirmed = logs
            .iter()
            .filter(|log| log.block_number <= horizon)
            .cloned()
            .collect();
        confirmed_deposits = resolve_logs(confirmed, &index, routes)?;
    }

    let mut pending = Vec::new();
    for log in &logs {
        if log.amount.value().is_zero() || !routes.routes.contains_key(&log.token) {
            continue;
        }
        let address = index
            .get(&log.to)
            .ok_or(ScannerError::UnknownRecipient(log.to))?;
        pending.push(NewPendingTransfer {
            chain_id,
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
    let (committed, commit) = db::rpc::commit_head_page(
        pool,
        chain_id,
        (from_block, latest),
        &confirmed_deposits,
        horizon.filter(|h| replay.is_some() || *h > scanned),
        &pending,
        window.proof.as_ref(),
        Some(&addresses.iter().map(|a| a.id).collect::<Vec<_>>()),
        !more,
    )
    .await?;
    record_committed(chain_id, &mut super::ScanStats::default(), &committed)?;
    let progress = if more {
        Some(db::AddressSweep {
            epoch,
            anchor: i64::try_from(scanned)
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            from_block: i64::try_from(from_block)
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            through_block: i64::try_from(latest)
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            block_time: None,
            horizon: horizon
                .map(i64::try_from)
                .transpose()
                .map_err(|_| ScannerError::Configuration("cursor overflow".into()))?,
            last_id: addresses
                .last()
                .ok_or_else(|| ScannerError::Configuration("empty address page".into()))?
                .id,
        })
    } else {
        None
    };
    db::save_address_sweep(pool, chain_id, "head", progress.as_ref()).await?;
    let inserted = committed.inserted;
    Ok(Some(HeadScan {
        from_block,
        latest,
        horizon,
        inserted,
        work_remaining: more,
        commit,
    }))
}

/// Transfers to `addresses` in the inclusive range, requested as the chain's routes select: every
/// transfer of the routed tokens kept locally (token mode), or transfers of any token to the
/// Quick re-polls after a poll that found no new head, before the loop falls back to one block
/// time (a missed slot, or a stalled chain).
const QUICK_REPOLLS: u32 = 4;
/// Locked polls re-check the phase every this many blocks, as block arrival drifts.
const RELOCK_EVERY: u32 = 8;

/// How the head loop paces its polls against block arrival.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Pacing {
    /// Consecutive polls without a new head.
    misses: u32,
    /// Polls with a new head since the phase was last locked.
    hits: u32,
    /// Whether the last block arrived within a quarter block time before the poll that saw it:
    /// a poll that found a new head followed quick re-polls, or the chain makes several blocks
    /// per poll, when the phase does not matter.
    locked: bool,
}

impl Pacing {
    fn observe(&mut self, previous: Option<u64>, latest: u64) {
        match previous {
            Some(previous) if previous == latest => {
                self.misses = self.misses.saturating_add(1);
            }
            _ => {
                let quick = (1..=QUICK_REPOLLS).contains(&self.misses);
                let several = previous.is_some_and(|previous| latest > previous.saturating_add(1));
                if quick || several {
                    self.locked = true;
                    self.hits = 0;
                } else if self.misses > QUICK_REPOLLS {
                    self.locked = false;
                }
                self.hits = self.hits.saturating_add(1);
                self.misses = 0;
            }
        }
    }
}

/// What the head loop remembers between polls.
#[derive(Debug, Default)]
struct HeadState {
    /// `latest` of the last poll.
    polled_latest: Option<u64>,
    pacing: Pacing,
    /// `latest` of the last successful scan; the same head is not scanned twice.
    scanned_latest: Option<u64>,
    work_remaining: bool,
    finalized: Option<FinalizedHead>,
    finalized_read_at: Option<Instant>,
}

/// One poll: `eth_blockNumber`, `finalized` when due, and a per-block scan when `latest` moved.
async fn poll_once(
    state: &mut HeadState,
    pool: &PgPool,
    reader: &FinalizedReader,
    routes: &ChainRoutes,
    config: &ScanConfig,
    finalized_heads: &FinalizedHeads,
) -> Result<Option<HeadScan>, ScannerError> {
    let chain_id = routes.chain.chain_id;
    if crate::reconciler::chain_is_blocked(pool, chain_id).await? {
        return Ok(None);
    }
    let latest = reader.latest_head().await?;
    state.pacing.observe(state.polled_latest, latest);
    state.polled_latest = Some(latest);
    let due = state
        .finalized_read_at
        .is_none_or(|at| at.elapsed() >= config.finalized_poll_interval);
    if due {
        let head = reader.finalized_head().await?;
        state.finalized_read_at = Some(Instant::now());
        state.finalized = Some(head);
        if finalized_heads.publish(chain_id, head) {
            tracing::debug!(chain_id, finalized = head.number, "finalized head advanced");
        }
    }
    let safe = if routes.chain.confirmations.needs_safe() {
        Some(reader.safe_head().await?)
    } else {
        None
    };
    let replay_pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rpc_reorg_ranges WHERE chain_id=$1 AND epoch=COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0) AND COALESCE(replayed_through,from_block-1)<to_block)")
        .bind(i64::try_from(chain_id).map_err(|_| ScannerError::Configuration("chain id overflow".into()))?)
        .fetch_one(pool).await?;
    if state.scanned_latest == Some(latest) && !replay_pending {
        return Ok(None);
    }
    let heads = ChainHeads {
        latest: Some(latest),
        safe,
        finalized: state.finalized.map_or(0, |head| head.number),
    };
    let scan = scan_new_blocks(pool, reader, routes, heads).await?;
    state.work_remaining = scan.as_ref().is_some_and(|scan| scan.work_remaining);
    state.scanned_latest = (!state.work_remaining).then_some(latest);
    Ok(scan)
}

/// Polls the head about once per `config` head interval until cancellation. Failures only delay
/// credit (the finalized backstop records what the per-block scan misses) and the pending view,
/// so they are logged and retried. The scanner's monitor is checked in from here, healthy while
/// polls and the backstop (`backstop_healthy`) succeed.
pub(super) async fn run_head_loop(
    pool: &PgPool,
    reader: &FinalizedReader,
    routes: &ChainRoutes,
    config: &ScanConfig,
    finalized_heads: &FinalizedHeads,
    backstop_healthy: &AtomicBool,
    cancellation: CancellationToken,
) {
    let chain_id = routes.chain.chain_id;
    let interval = config
        .head_poll_interval
        .unwrap_or_else(|| head_poll_interval(&routes.chain));
    let monitor = crate::observability::CronMonitor::scanner(chain_id);
    let mut state = HeadState::default();
    loop {
        // Delays count from the poll's start, so the time a scan takes does not shift the phase.
        let polled_at = Instant::now();
        let result = tokio::select! {
            () = cancellation.cancelled() => return,
            result = poll_once(&mut state, pool, reader, routes, config, finalized_heads) => result,
        };
        match result {
            Ok(scan) => {
                monitor.check_in(backstop_healthy.load(Ordering::Relaxed));
                if let Some(scan) = scan {
                    tracing::debug!(
                        chain_id,
                        from_block = scan.from_block,
                        latest = scan.latest,
                        horizon = scan.horizon,
                        inserted = scan.inserted,
                        seen = scan.commit.seen,
                        removed = scan.commit.removed,
                        "per-block scan committed"
                    );
                    if scan.inserted > 0 {
                        tracing::info!(
                            chain_id,
                            horizon = scan.horizon,
                            inserted = scan.inserted,
                            "deposits recorded at the route confirmation"
                        );
                    }
                }
            }
            Err(error) => {
                state.work_remaining = false;
                tracing::warn!(chain_id,error_category=error.category(),%error,"per-block scan failed; retrying");
            }
        }
        let delay = if state.work_remaining {
            Duration::from_millis(100)
        } else {
            next_poll_delay(interval, state.pacing)
        };
        tokio::select! {
            () = cancellation.cancelled() => return,
            () = tokio::time::sleep_until(polled_at + delay) => {}
        }
    }
}

/// The delay before the next head poll.
///
/// After a poll that found no new head, a quarter block time, up to [`QUICK_REPOLLS`] times, so a
/// block is seen within a quarter block time of arriving; then a block time (a missed slot or a
/// stalled chain). After a poll that found one: a block time while the phase is locked, when the
/// next block is due just before it; otherwise, and every [`RELOCK_EVERY`] blocks, three quarters
/// of one, which finds the next block early (moving the phase earlier) or misses it (locking it).
fn next_poll_delay(interval: Duration, pacing: Pacing) -> Duration {
    match pacing.misses {
        0 if pacing.locked && !pacing.hits.is_multiple_of(RELOCK_EVERY) => interval,
        0 => interval.saturating_sub(interval / 4),
        misses if misses <= QUICK_REPOLLS => interval / 4,
        _ => interval,
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    /// Polls a chain whose blocks arrive every 12 s at `offset` seconds into each slot; returns
    /// the delay from each block's arrival to the poll that saw it, and the polls made.
    fn simulate(offset: u64, start: u64, blocks: u64) -> (Vec<u64>, u64) {
        let interval = Duration::from_secs(12);
        let block_at = |time_ms: u64| time_ms.saturating_sub(offset * 1_000) / 12_000;
        let mut pacing = Pacing::default();
        let mut previous = None;
        let mut now = start * 1_000;
        let mut lags = Vec::new();
        let mut polls = 0;
        while block_at(now) < blocks {
            polls += 1;
            let latest = block_at(now);
            if previous.is_some_and(|previous| previous != latest) {
                lags.push((now - offset * 1_000 - latest * 12_000) / 1_000);
            }
            pacing.observe(previous, latest);
            previous = Some(latest);
            now += u64::try_from(next_poll_delay(interval, pacing).as_millis()).expect("ms");
        }
        (lags, polls)
    }

    #[test]
    fn polls_lock_onto_block_arrival_within_a_quarter_block_time() {
        for (offset, start) in [(0, 0), (5, 0), (11, 1), (7, 3)] {
            let (lags, polls) = simulate(offset, start, 200);
            // After a few blocks to find the phase, every block is seen within 3 s.
            assert!(
                lags.iter().skip(4).all(|lag| *lag <= 3),
                "{offset}/{start}: {lags:?}"
            );
            // About one poll per block, plus a phase check every eight blocks.
            assert!(polls <= 200 * 5 / 4, "{offset}/{start}: {polls} polls");
        }
    }

    #[test]
    fn silence_backs_off_to_one_block_time() {
        let interval = Duration::from_secs(12);
        let mut pacing = Pacing::default();
        pacing.observe(None, 10);
        for misses in 1..=QUICK_REPOLLS {
            pacing.observe(Some(10), 10);
            assert_eq!(pacing.misses, misses);
            assert_eq!(next_poll_delay(interval, pacing), Duration::from_secs(3));
        }
        pacing.observe(Some(10), 10);
        assert_eq!(next_poll_delay(interval, pacing), interval);
    }

    #[test]
    fn a_depth_route_polls_once_per_block_of_its_chain_family() {
        let chain = |chain_id, confirmations| ChainConfig {
            chain_id,
            confirmations,
            rpc_providers: Vec::new(),
            contracts: topup_core::route::ChainContracts {
                forwarder_factory: alloy_primitives::Address::ZERO,
                implementation: alloy_primitives::Address::ZERO,
            },
        };
        let interval =
            |chain_id, confirmations| head_poll_interval(&chain(chain_id, confirmations));
        assert_eq!(
            interval(1, Confirmations::Depth(2)),
            Duration::from_secs(12)
        );
        assert_eq!(
            interval(8_453, Confirmations::Depth(3)),
            Duration::from_secs(2)
        );
        // `safe` follows L1, and `finalized` credits only final blocks: one L1 slot.
        assert_eq!(
            interval(8_453, Confirmations::Safe),
            DEFAULT_HEAD_POLL_INTERVAL
        );
        assert_eq!(
            interval(8_453, Confirmations::Finalized),
            DEFAULT_HEAD_POLL_INTERVAL
        );
        assert_eq!(
            interval(137, Confirmations::Finalized),
            DEFAULT_HEAD_POLL_INTERVAL
        );
    }

    #[test]
    fn finalized_heads_publish_only_advances() {
        let heads = FinalizedHeads::default();
        let head = |number| FinalizedHead {
            number,
            time: DateTime::UNIX_EPOCH,
        };
        assert!(heads.publish(1, head(10)));
        assert!(!heads.publish(1, head(10)));
        assert!(!heads.publish(1, head(9)));
        assert!(heads.publish(1, head(11)));
        assert!(heads.publish(2, head(3)));
        assert_eq!(heads.get(1).map(|head| head.number), Some(11));
        assert_eq!(heads.get(3), None);
    }
}
