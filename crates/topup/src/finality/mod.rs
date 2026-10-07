//! Finality watch (architecture §7): independently re-read receipts at the agreed checkpoint.
//! Each due deposit receives its own recheck time, and bounded oldest-first pages keep a stuck
//! deposit from starving later ones. An unchanged transfer below the checkpoint becomes final;
//! one re-included above it is followed. Reversal requires an agreed finalized receipt without
//! its transfer or a service-known finalized replacement with the same sender and nonce.
//! Missing receipts without that positive evidence wait and alert; no nonce search proves loss.
//! Reversal always reopens a consumed quote and restores its reservation. Dual coverage later
//! completes expiry or cancellation. A canonical successor is inserted atomically with reversal.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FinalizedHead, FinalizedReader, ReceiptLookup, TransferLog,
};
use topup_core::deposit::{DepositState, reverse};
use topup_core::identity::reversed_event_id;
use topup_core::money::AtomicAmount;
use uuid::Uuid;

use crate::db::{self, EventObject, Evidence, NewOutboxEvent};
use crate::routes::RouteSet;
use crate::scanner::{FinalizedHeads, chain_routes, resolve_log};
use crate::tenancy::Scope;

/// Delay between passes without a new `finalized` advance.
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// Deposits claimed per page, oldest block first.
pub const WATCH_PAGE: i64 = 500;
/// Pages one pass reads at most; the next pass continues at once.
pub const WATCH_PAGES_PER_PASS: usize = 10;
/// A claimed deposit that is still neither final nor reversed is read again after this long.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(60);
/// A transaction that left the chain without a replacement is alerted on after this long.
const PENDING_AFTER_REORG_ALERT: TimeDelta = TimeDelta::hours(1);

/// Failure of one watch pass; the next pass retries.
#[derive(Debug, thiserror::Error)]
pub enum FinalityError {
    /// A chain read failed.
    #[error("{0}")]
    Chain(#[from] ChainError),
    /// A database operation failed.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
    /// The chain has no configured providers.
    #[error("chain {0} is not configured")]
    UnknownChain(u64),
}

#[async_trait]
trait WatchReader: Send + Sync {
    async fn evidence(
        &self,
        tx: B256,
        position: u64,
        needed: u64,
    ) -> Result<topup_adapters::chain::evm::FinalityEvidence, ChainError>;
}

#[async_trait]
impl<R: ChainReader + Send + Sync> WatchReader for R {
    async fn evidence(
        &self,
        tx: B256,
        position: u64,
        needed: u64,
    ) -> Result<topup_adapters::chain::evm::FinalityEvidence, ChainError> {
        ChainReader::finality_evidence(self, tx, position, needed).await
    }
}

struct WatchChain {
    primary: Arc<dyn WatchReader>,
    secondary: Arc<dyn WatchReader>,
}

/// Work done by one pass over a chain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatchStats {
    /// Deposits read.
    pub watched: u64,
    /// Deposits that became final.
    pub finalized: u64,
    /// Deposits whose re-included transaction was followed.
    pub followed: u64,
    /// Deposits reversed.
    pub reversed: u64,
    /// Deposits whose chain reads failed; each is read again at its recheck time.
    pub failed: u64,
    /// Whether the pass stopped at [`WATCH_PAGES_PER_PASS`] with deposits still due.
    pub more: bool,
}

/// The finality watch over every configured chain.
pub struct FinalityWatch {
    pool: PgPool,
    routes: Arc<RouteSet>,
    chains: BTreeMap<u64, WatchChain>,
}

impl FinalityWatch {
    /// Watches every configured chain on its first two providers.
    pub fn from_routes(pool: PgPool, routes: Arc<RouteSet>) -> Result<Self, String> {
        let mut chains = BTreeMap::new();
        for chain_id in routes.chain_ids() {
            let reader = |index| -> Result<Arc<dyn WatchReader>, String> {
                let client = routes
                    .provider(chain_id, index)
                    .map_err(|error| error.to_string())?;
                Ok(Arc::new(FinalizedReader::new(Arc::clone(client))))
            };
            chains.insert(
                chain_id,
                WatchChain {
                    primary: reader(0)?,
                    secondary: reader(1)?,
                },
            );
        }
        Ok(Self {
            pool,
            routes,
            chains,
        })
    }

    /// Watches one chain with injected readers, for tests; `routes` render event objects.
    pub fn single<R1, R2>(
        pool: PgPool,
        routes: Arc<RouteSet>,
        chain_id: u64,
        primary: R1,
        secondary: R2,
    ) -> Self
    where
        R1: ChainReader + Send + Sync + 'static,
        R2: ChainReader + Send + Sync + 'static,
    {
        Self {
            pool,
            routes,
            chains: BTreeMap::from([(
                chain_id,
                WatchChain {
                    primary: Arc::new(primary),
                    secondary: Arc::new(secondary),
                },
            )]),
        }
    }

    /// Reads the agreed checkpoint and re-reads every deposit of `chain_id` that is neither
    /// final nor reversed and whose recorded block is at or below it.
    pub async fn watch_once(&self, chain_id: u64) -> Result<WatchStats, FinalityError> {
        let Some(checkpoint) = db::chain_reads::checkpoint(&self.pool, chain_id).await? else {
            return Ok(WatchStats::default());
        };
        let finalized = checkpoint.number;
        self.watch_at(chain_id, finalized).await
    }

    /// [`Self::watch_once`] at the agreed checkpoint announced by coverage.
    async fn watch_at(
        &self,
        chain_id: u64,
        primary_finalized: u64,
    ) -> Result<WatchStats, FinalityError> {
        let chain = self
            .chains
            .get(&chain_id)
            .ok_or(FinalityError::UnknownChain(chain_id))?;
        let mut stats = WatchStats::default();
        let Some(checkpoint) = db::chain_reads::checkpoint(&self.pool, chain_id).await? else {
            return Ok(stats);
        };
        let primary_finalized = primary_finalized.min(checkpoint.number);
        if crate::reconciler::chain_is_blocked(&self.pool, chain_id).await? {
            return Ok(stats);
        }
        for _ in 0..WATCH_PAGES_PER_PASS {
            let deposits = claim_unfinal_deposits(&self.pool, chain_id, primary_finalized).await?;
            let Some(last) = deposits.len().checked_sub(1) else {
                return Ok(stats);
            };
            let full = last + 1 == usize::try_from(WATCH_PAGE).unwrap_or(usize::MAX);
            let secondary_finalized = primary_finalized;
            for deposit in deposits {
                stats.watched = stats.watched.saturating_add(1);
                let watched = self
                    .watch_deposit(
                        chain,
                        chain_id,
                        &deposit,
                        primary_finalized,
                        secondary_finalized,
                    )
                    .await;
                match watched {
                    Ok(Applied::Final) => stats.finalized = stats.finalized.saturating_add(1),
                    Ok(Applied::Followed) => stats.followed = stats.followed.saturating_add(1),
                    Ok(Applied::Reversed) => stats.reversed = stats.reversed.saturating_add(1),
                    Ok(Applied::Nothing) => {}
                    // One deposit's failed read holds back no other: it is read again at its
                    // recheck time.
                    Err(FinalityError::Chain(error)) => {
                        stats.failed = stats.failed.saturating_add(1);
                        tracing::warn!(
                            chain_id,
                            deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                            %error,
                            "finality read failed; the deposit is read again later"
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            if !full {
                return Ok(stats);
            }
        }
        stats.more = true;
        Ok(stats)
    }

    /// Reads independent receipts and service-known replacement evidence, then applies the verdict.
    async fn watch_deposit(
        &self,
        chain: &WatchChain,
        chain_id: u64,
        deposit: &WatchedDeposit,
        primary_finalized: u64,
        secondary_finalized: u64,
    ) -> Result<Applied, FinalityError> {
        let (primary_evidence, secondary_evidence) = tokio::try_join!(
            chain.primary.evidence(
                deposit.tx_hash,
                deposit.receipt_log_index,
                primary_finalized
            ),
            chain.secondary.evidence(
                deposit.tx_hash,
                deposit.receipt_log_index,
                secondary_finalized
            ),
        )?;
        let replacement = if matches!(primary_evidence.receipt, ReceiptLookup::Missing)
            && matches!(secondary_evidence.receipt, ReceiptLookup::Missing)
        {
            self.known_replacement(
                chain,
                chain_id,
                deposit,
                primary_finalized.min(secondary_finalized),
            )
            .await?
        } else {
            false
        };
        let primary_finalized = primary_evidence.finalized;
        let secondary_finalized = secondary_evidence.finalized;
        let primary = primary_evidence.receipt;
        let secondary = secondary_evidence.receipt;
        let verdict = decide(
            deposit,
            Observed {
                finalized: primary_finalized,
                receipt: &primary,
            },
            Observed {
                finalized: secondary_finalized,
                receipt: &secondary,
            },
            replacement,
        );
        self.apply(deposit, verdict, chain_id).await
    }

    async fn known_replacement(
        &self,
        chain: &WatchChain,
        chain_id: u64,
        deposit: &WatchedDeposit,
        checkpoint: u64,
    ) -> Result<bool, FinalityError> {
        let hashes: Vec<String> = sqlx::query_scalar("SELECT DISTINCT tx_hash FROM deposits WHERE chain_id=$1 AND tx_from=$2 AND tx_nonce=$3::text::numeric AND tx_hash != $4")
            .bind(i64::try_from(chain_id).map_err(|_|sqlx::Error::Protocol("chain overflow".into()))?).bind(format!("{:#x}",deposit.origin.0)).bind(deposit.origin.1.to_string()).bind(format!("{:#x}",deposit.tx_hash)).fetch_all(&self.pool).await?;
        for hash in hashes {
            let hash = hash
                .parse()
                .map_err(|_| sqlx::Error::Protocol("replacement hash invalid".into()))?;
            let (a, b) = tokio::try_join!(
                chain.primary.evidence(hash, 0, checkpoint),
                chain.secondary.evidence(hash, 0, checkpoint)
            )?;
            if a.receipt != b.receipt {
                continue;
            }
            if let ReceiptLookup::Included {
                block_number,
                tx_from,
                tx_nonce,
                ..
            } = a.receipt
                && block_number <= checkpoint
                && (tx_from, tx_nonce) == deposit.origin
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Runs a pass per chain whenever `heads` publishes an agreed checkpoint advance,
    /// and every [`RETRY_INTERVAL`] besides for deposits whose recheck time came, until
    /// cancellation. A pass that leaves deposits due is followed by another at once.
    pub async fn run(&self, heads: FinalizedHeads, cancellation: CancellationToken) {
        let mut advances = heads.subscribe();
        let monitor = crate::observability::CronMonitor::finality_watch();
        loop {
            let published = advances.borrow_and_update().clone();
            let mut healthy = true;
            let mut more = false;
            for &chain_id in self.chains.keys() {
                let Some(&FinalizedHead {
                    number: finalized, ..
                }) = published.get(&chain_id)
                else {
                    continue;
                };
                let result = tokio::select! {
                    () = cancellation.cancelled() => return,
                    result = self.watch_at(chain_id, finalized) => result,
                };
                match result {
                    Ok(stats) => {
                        healthy &= stats.failed == 0;
                        more |= stats.more;
                        if stats.watched > 0 {
                            tracing::info!(
                                chain_id,
                                finalized,
                                watched = stats.watched,
                                became_final = stats.finalized,
                                followed = stats.followed,
                                reversed = stats.reversed,
                                failed = stats.failed,
                                more = stats.more,
                                "finality watch pass"
                            );
                        }
                    }
                    Err(error) => {
                        healthy = false;
                        tracing::warn!(chain_id, %error, "finality watch pass failed; retrying");
                    }
                }
            }
            monitor.check_in(healthy);
            if more {
                if cancellation.is_cancelled() {
                    return;
                }
                continue;
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                changed = advances.changed() => {
                    if changed.is_err() {
                        tokio::time::sleep(RETRY_INTERVAL).await;
                    }
                }
                () = tokio::time::sleep(RETRY_INTERVAL) => {}
            }
        }
    }

    async fn apply(
        &self,
        deposit: &WatchedDeposit,
        verdict: Verdict,
        chain_id: u64,
    ) -> Result<Applied, FinalityError> {
        match verdict {
            Verdict::Wait => Ok(Applied::Nothing),
            Verdict::Pending => {
                if Utc::now().signed_duration_since(deposit.block_time) > PENDING_AFTER_REORG_ALERT
                {
                    tracing::warn!(
                        tags.alert = "TopupDepositPendingAfterReorg",
                        tags.chain_id = chain_id,
                        tags.state = db::state_code(deposit.state),
                        deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                        tx_hash = %deposit.tx_hash,
                        "a deposit's transaction left the chain and is still pending"
                    );
                }
                Ok(Applied::Nothing)
            }
            Verdict::Follow(block) => {
                let applied = record_evidence(&self.pool, deposit, &block, false).await?;
                Ok(if applied {
                    Applied::Followed
                } else {
                    Applied::Nothing
                })
            }
            Verdict::Final(block) => {
                let applied = record_evidence(&self.pool, deposit, &block, true).await?;
                Ok(if applied {
                    Applied::Final
                } else {
                    Applied::Nothing
                })
            }
            Verdict::Reverse(evidence, successor) => {
                let reversed = reverse_deposit(
                    &self.pool,
                    &self.routes,
                    chain_id,
                    deposit,
                    evidence,
                    successor.as_deref(),
                )
                .await?;
                match reversed {
                    Some(Reversed { successor }) => {
                        tracing::warn!(
                            tags.alert = "TopupDepositReversed",
                            tags.chain_id = chain_id,
                            tags.state = db::state_code(deposit.state),
                            deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                            tx_hash = %deposit.tx_hash,
                            successor_deposit_id = successor.map(tracing::field::display),
                            "a deposit's transfer is not in the final chain; the deposit is \
                             reversed"
                        );
                        Ok(Applied::Reversed)
                    }
                    None => Ok(Applied::Nothing),
                }
            }
        }
    }
}

enum Applied {
    Final,
    Followed,
    Reversed,
    Nothing,
}

/// A reversal [`reverse_deposit`] committed.
struct Reversed {
    /// The new deposit recorded for the transfer now at the reversed deposit's position, if any.
    successor: Option<Uuid>,
}

/// A deposit that is neither final nor reversed, with the stored evidence the watch compares.
#[derive(Clone, Debug)]
struct WatchedDeposit {
    id: Uuid,
    state: DepositState,
    attempt: i32,
    tx_hash: B256,
    receipt_log_index: u64,
    log_index: u64,
    block_number: u64,
    block_hash: B256,
    block_time: DateTime<Utc>,
    address: Address,
    asset_contract: Address,
    from_address: Address,
    amount_atomic: AtomicAmount,
    /// The transaction's sender and nonce.
    origin: (Address, u64),
}

impl WatchedDeposit {
    fn is_same_transfer(&self, transfer: &TransferLog) -> bool {
        transfer.to == self.address
            && transfer.token == self.asset_contract
            && transfer.from == self.from_address
            && transfer.amount == self.amount_atomic
    }
}

/// The block a transfer is in now.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BlockEvidence {
    log_index: u64,
    block_number: u64,
    block_hash: B256,
    block_time: DateTime<Utc>,
}

impl From<&TransferLog> for BlockEvidence {
    fn from(transfer: &TransferLog) -> Self {
        Self {
            log_index: transfer.log_index,
            block_number: transfer.block_number,
            block_hash: transfer.block_hash,
            block_time: transfer.block_time,
        }
    }
}

#[derive(Clone, Copy)]
struct Observed<'a> {
    finalized: u64,
    receipt: &'a ReceiptLookup,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    /// Both providers show the transfer at or below `finalized`.
    Final(BlockEvidence),
    /// Both providers show the transfer re-included in a newer block that is not final.
    Follow(BlockEvidence),
    /// The transfer is not part of the final chain; the transfer both providers show at its
    /// receipt position instead, if any, becomes a new deposit.
    Reverse(Value, Option<Box<TransferLog>>),
    /// The transaction is in no block and its nonce is still unused.
    Pending,
    /// Nothing to record now.
    Wait,
}

/// Decides from complete agreed receipts or positive service-known replacement evidence.
fn decide(
    deposit: &WatchedDeposit,
    primary: Observed<'_>,
    secondary: Observed<'_>,
    replacement: bool,
) -> Verdict {
    match (primary.receipt, secondary.receipt) {
        (
            ReceiptLookup::Included {
                block_number,
                block_hash,
                transfer: primary_transfer,
                ..
            },
            ReceiptLookup::Included {
                block_hash: secondary_hash,
                transfer: secondary_transfer,
                ..
            },
        ) if block_hash == secondary_hash && primary.receipt == secondary.receipt => {
            let is_final =
                *block_number <= primary.finalized && *block_number <= secondary.finalized;
            match primary_transfer.as_deref() {
                Some(transfer) if deposit.is_same_transfer(transfer) => {
                    let block = BlockEvidence::from(transfer);
                    if is_final {
                        Verdict::Final(block)
                    } else if block.block_hash != deposit.block_hash
                        || block.log_index != deposit.log_index
                    {
                        Verdict::Follow(block)
                    } else {
                        Verdict::Wait
                    }
                }
                // A detected deposit's evidence is provisional: its confirm step corrects a
                // transfer both providers agree on, still to this address.
                Some(transfer)
                    if deposit.state == DepositState::Detected
                        && transfer.to == deposit.address =>
                {
                    Verdict::Wait
                }
                _ if is_final => Verdict::Reverse(
                    json!({
                        "stage": "finality",
                        "result": if primary_transfer.is_some() {
                            "transfer_changed_at_finality"
                        } else {
                            "transfer_absent_at_finality"
                        },
                        "block_number": block_number,
                        "block_hash": format!("{block_hash:#x}"),
                        "provider_a_finalized": primary.finalized,
                        "provider_b_finalized": secondary.finalized,
                    }),
                    primary_transfer.clone(),
                ),
                _ => Verdict::Wait,
            }
        }
        (ReceiptLookup::Missing, ReceiptLookup::Missing) if replacement => Verdict::Reverse(
            json!({"stage":"finality","result":"known_finalized_replacement"}),
            None,
        ),
        (ReceiptLookup::Missing, ReceiptLookup::Missing) => Verdict::Pending,

        _ => {
            tracing::warn!(
                tags.alert = "TopupRpcDisagreement",
                deposit_id = %deposit.id,
                "finality receipt evidence disagreed; waiting"
            );
            Verdict::Wait
        }
    }
}

/// Claims the next page of deposits of `chain_id` that are neither final nor reversed, recorded
/// at or below `finalized`, and due: never read, or past their recheck time. Claiming moves each
/// one's recheck time [`RECHECK_INTERVAL`] ahead, so a deposit that is still waiting afterwards
/// does not come back until then, and concurrent watchers skip each other's pages. Oldest block
/// first.
async fn claim_unfinal_deposits(
    pool: &PgPool,
    chain_id: u64,
    finalized: u64,
) -> Result<Vec<WatchedDeposit>, FinalityError> {
    let recheck_seconds = f64::from(u32::try_from(RECHECK_INTERVAL.as_secs()).unwrap_or(u32::MAX));
    let rows = sqlx::query(
        r#"
        WITH due AS (
            SELECT id
            FROM deposits
            WHERE chain_id = $1 AND final_at IS NULL AND state <> 'reversed'
              AND block_number <= $3
              AND (finality_check_at IS NULL OR finality_check_at <= now())
            ORDER BY block_number, id
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        UPDATE deposits AS deposit
        SET finality_check_at = now() + make_interval(secs => $4)
        FROM due, addresses AS address
        WHERE deposit.id = due.id AND address.id = deposit.address_id
        RETURNING deposit.id, deposit.state, deposit.attempt, deposit.tx_hash,
                  deposit.receipt_log_index, deposit.log_index, deposit.block_number,
                  deposit.block_hash, deposit.block_time, address.address,
                  deposit.asset_contract, deposit.from_address,
                  deposit.amount_atomic::text AS amount_atomic,
                  deposit.tx_from, deposit.tx_nonce::text AS tx_nonce
        "#,
    )
    .bind(to_i64(chain_id)?)
    .bind(WATCH_PAGE)
    .bind(to_i64(finalized)?)
    .bind(recheck_seconds)
    .fetch_all(pool)
    .await?;
    let mut deposits = rows
        .into_iter()
        .map(|row| {
            let origin = (
                parse(&row.try_get::<String, _>("tx_from")?)?,
                parse(&row.try_get::<String, _>("tx_nonce")?)?,
            );
            Ok(WatchedDeposit {
                id: row.try_get("id")?,
                state: db::parse_state(&row.try_get::<String, _>("state")?)?,
                attempt: row.try_get("attempt")?,
                tx_hash: parse(&row.try_get::<String, _>("tx_hash")?)?,
                receipt_log_index: to_u64(row.try_get("receipt_log_index")?)?,
                log_index: to_u64(row.try_get("log_index")?)?,
                block_number: to_u64(row.try_get("block_number")?)?,
                block_hash: parse(&row.try_get::<String, _>("block_hash")?)?,
                block_time: row.try_get("block_time")?,
                address: parse(&row.try_get::<String, _>("address")?)?,
                asset_contract: parse(&row.try_get::<String, _>("asset_contract")?)?,
                from_address: parse(&row.try_get::<String, _>("from_address")?)?,
                amount_atomic: AtomicAmount::new(
                    row.try_get::<String, _>("amount_atomic")?
                        .parse()
                        .map_err(decode_error)?,
                ),
                origin,
            })
        })
        .collect::<Result<Vec<_>, FinalityError>>()?;
    deposits.sort_by_key(|deposit| (deposit.block_number, deposit.id));
    Ok(deposits)
}

/// Records where the transfer is now and, when `is_final`, that the deposit is final; a final
/// credited deposit is then swept by a finalized `Flushed` event after it. Returns whether the row
/// changed.
async fn record_evidence(
    pool: &PgPool,
    deposit: &WatchedDeposit,
    block: &BlockEvidence,
    is_final: bool,
) -> Result<bool, FinalityError> {
    let mut transaction = pool.begin().await?;
    let updated = sqlx::query(
        r#"
        UPDATE deposits
        SET log_index = $2, block_number = $3, block_hash = $4, block_time = $5,
            final_at = CASE WHEN $6 THEN now() END,
            updated_at = now()
        WHERE id = $1 AND final_at IS NULL AND state <> 'reversed'
        RETURNING state
        "#,
    )
    .bind(deposit.id)
    .bind(to_i64(block.log_index)?)
    .bind(to_i64(block.block_number)?)
    .bind(format!("{:#x}", block.block_hash))
    .bind(block.block_time)
    .bind(is_final)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(row) = updated else {
        return Ok(false);
    };
    let state: String = row.try_get("state")?;
    let moved = block.block_hash != deposit.block_hash || block.log_index != deposit.log_index;
    insert_transition(
        &mut transaction,
        deposit.id,
        &state,
        &state,
        deposit.attempt,
        &json!({
            "stage": "finality",
            "result": if is_final { "final" } else { "followed" },
            "moved": moved,
            "log_index": block.log_index,
            "block_number": block.block_number,
            "block_hash": format!("{:#x}", block.block_hash),
        }),
    )
    .await?;
    if is_final {
        db::mark_swept(&mut transaction, Some(deposit.id), &[]).await?;
    }
    transaction.commit().await?;
    Ok(true)
}

/// Reverses a deposit that is still in the observed state and not final: the transition and, for
/// a deposit the account was told of (`credited` or `rejected`), `deposit.reversed`; a quote it
/// consumed always opens again with its reservation restored; coverage closes it later. Its pending
/// refunds without a transaction are canceled (design D1), while one marked paid stays tracked
/// until verification ends it. Refunds require a final deposit, so the cancel only keeps that
/// rule whole should one ever be pending.
///
/// `successor`, the final transfer now at the deposit's receipt position, is recorded as a new
/// deposit in the same transaction ([`reverse_in`]). Returns `None` when nothing was
/// reversed.
async fn reverse_deposit(
    pool: &PgPool,
    routes: &RouteSet,
    chain_id: u64,
    deposit: &WatchedDeposit,
    mut evidence: Value,
    successor: Option<&TransferLog>,
) -> Result<Option<Reversed>, FinalityError> {
    let chain = chain_routes(routes)
        .into_iter()
        .find(|chain| chain.chain.chain_id == chain_id)
        .ok_or(FinalityError::UnknownChain(chain_id))?;
    let mut transaction = pool.begin().await?;
    db::rpc::guard_in(&mut transaction, chain_id).await?;
    let next = match successor {
        Some(transfer) => {
            match db::find_scan_address(&mut *transaction, chain_id, transfer.to).await? {
                Some(address) if transfer.block_number >= address.created_block => {
                    Some(resolve_log(transfer.clone(), &address, &chain, Utc::now()))
                }
                _ => None,
            }
        }
        None => None,
    };
    let Some((account_id, livemode, successor)) = reverse_in(
        &mut transaction,
        deposit.id,
        deposit.state,
        deposit.attempt,
        &mut evidence,
        next.as_ref(),
    )
    .await?
    else {
        return Ok(None);
    };
    let scope = Scope::new(account_id, livemode);
    let pending_refunds: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM refunds WHERE deposit_id = $1 AND status = 'pending' AND tx_hash IS NULL \
         ORDER BY id FOR UPDATE",
    )
    .bind(deposit.id)
    .fetch_all(&mut *transaction)
    .await?;
    for refund in pending_refunds {
        let object = EventObject::Refund(refund);
        let before = db::render(&mut transaction, routes, scope, object).await?;
        sqlx::query("UPDATE refunds SET status = 'canceled', updated_at = now() WHERE id = $1")
            .bind(refund)
            .execute(&mut *transaction)
            .await?;
        let event = NewOutboxEvent::system(Uuid::new_v4(), "refund.updated", scope, object);
        db::enqueue_in(&mut transaction, routes, &event, Some(&before)).await?;
    }
    // Rendered last, so the object shows the deposit, its refunds, and its quote as the reversal
    // leaves them.
    if matches!(
        deposit.state,
        DepositState::Credited | DepositState::Rejected
    ) {
        let event = NewOutboxEvent::system(
            reversed_event_id(deposit.id),
            "deposit.reversed",
            scope,
            EventObject::Deposit(deposit.id),
        );
        db::enqueue_in(&mut transaction, routes, &event, None).await?;
    }
    transaction.commit().await?;
    Ok(Some(Reversed { successor }))
}

/// Shared DB-only reversal boundary. The caller holds the chain lock and commits the
/// reversal, canonical successor, quote reopening and any coverage in one transaction.
/// Delivered-credit callers also enqueue their refund and reversal events before committing.
pub(crate) async fn reverse_in(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
    state: DepositState,
    attempt: i32,
    evidence: &mut Value,
    next: Option<&db::NewDeposit>,
) -> Result<Option<(Uuid, bool, Option<Uuid>)>, sqlx::Error> {
    let Ok(transition) = reverse(state) else {
        return Ok(None);
    };
    let from = db::state_code(transition.from);
    let to = db::state_code(transition.to);
    let owner = sqlx::query_as::<_, (Uuid, bool)>(
        "UPDATE deposits SET state=$3,reason=NULL,lease_token=NULL,lease_until=NULL, \
         next_attempt_at=now(),updated_at=now(),dual_verified_at=now() \
         WHERE id=$1 AND state=$2 AND final_at IS NULL RETURNING account_id,livemode",
    )
    .bind(id)
    .bind(from)
    .bind(to)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((account, livemode)) = owner else {
        return Ok(None);
    };
    let successor = match next {
        Some(deposit) => {
            db::insert_scanned_deposit_in(
                transaction,
                deposit,
                Evidence::Successor { replaces: id },
            )
            .await?
        }
        None => None,
    };
    if let Some(next) = successor {
        sqlx::query("UPDATE deposits SET dual_verified_at=now() WHERE id=$1")
            .bind(next)
            .execute(&mut **transaction)
            .await?;
        if let Some(object) = evidence.as_object_mut() {
            object.insert("successor_deposit_id".into(), json!(next));
        }
    }
    insert_transition(transaction, id, from, to, attempt, evidence).await?;
    sqlx::query("UPDATE quotes SET consumed_by=NULL,status='open',exposure_reserved=true,closed_at=NULL WHERE consumed_by=$1")
        .bind(id).execute(&mut **transaction).await?;
    Ok(Some((account, livemode, successor)))
}

async fn insert_transition(
    transaction: &mut Transaction<'_, Postgres>,
    deposit_id: Uuid,
    from: &str,
    to: &str,
    attempt: i32,
    evidence: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO transitions (id, deposit_id, from_state, to_state, attempt, evidence)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(deposit_id)
    .bind(from)
    .bind(to)
    .bind(attempt)
    .bind(evidence)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn to_i64(value: u64) -> Result<i64, sqlx::Error> {
    i64::try_from(value).map_err(|error| sqlx::Error::Encode(error.to_string().into()))
}

fn to_u64(value: i64) -> Result<u64, sqlx::Error> {
    u64::try_from(value).map_err(decode_error)
}

fn parse<T: std::str::FromStr>(value: &str) -> Result<T, sqlx::Error>
where
    T::Err: std::fmt::Display,
{
    value.parse().map_err(decode_error)
}

fn decode_error(error: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::Decode(error.to_string().into())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    fn deposit(state: DepositState) -> WatchedDeposit {
        WatchedDeposit {
            id: Uuid::nil(),
            state,
            attempt: 0,
            tx_hash: B256::repeat_byte(1),
            receipt_log_index: 0,
            log_index: 5,
            block_number: 100,
            block_hash: B256::repeat_byte(2),
            block_time: DateTime::UNIX_EPOCH,
            address: Address::repeat_byte(3),
            asset_contract: Address::repeat_byte(4),
            from_address: Address::repeat_byte(5),
            amount_atomic: AtomicAmount::new(U256::from(7)),
            origin: (Address::repeat_byte(6), 9),
        }
    }

    fn transfer(deposit: &WatchedDeposit, block_number: u64, block_byte: u8) -> TransferLog {
        TransferLog {
            tx_hash: deposit.tx_hash,
            receipt_log_index: deposit.receipt_log_index,
            log_index: deposit.log_index,
            block_number,
            block_hash: B256::repeat_byte(block_byte),
            block_time: DateTime::UNIX_EPOCH,
            tx_from: Address::repeat_byte(6),
            tx_nonce: 9,
            token: deposit.asset_contract,
            from: deposit.from_address,
            to: deposit.address,
            amount: deposit.amount_atomic,
        }
    }

    fn included(transfer: Option<TransferLog>, block_number: u64, byte: u8) -> ReceiptLookup {
        ReceiptLookup::Included {
            block_number,
            block_hash: B256::repeat_byte(byte),
            status: true,
            block_time: DateTime::UNIX_EPOCH,
            tx_from: Address::repeat_byte(6),
            tx_nonce: 9,
            transfer: transfer.map(Box::new),
        }
    }

    fn observed(finalized: u64, receipt: &ReceiptLookup) -> Observed<'_> {
        Observed { finalized, receipt }
    }

    #[test]
    fn the_same_transfer_at_or_below_finalized_is_final_and_follows_its_block() {
        let deposit = deposit(DepositState::Credited);
        let receipt = included(Some(transfer(&deposit, 100, 2)), 100, 2);
        assert!(matches!(
            decide(&deposit, observed(100, &receipt), observed(120, &receipt), false),
            Verdict::Final(block) if block.block_hash == deposit.block_hash
        ));
        let moved = included(Some(transfer(&deposit, 104, 8)), 104, 8);
        assert!(matches!(
            decide(&deposit, observed(110, &moved), observed(110, &moved), false),
            Verdict::Final(block) if block.block_number == 104
        ));
    }

    #[test]
    fn a_re_included_transaction_is_followed_not_reversed() {
        let deposit = deposit(DepositState::Credited);
        let mut later = transfer(&deposit, 103, 8);
        later.log_index = 11;
        let receipt = included(Some(later), 103, 8);
        assert!(matches!(
            decide(&deposit, observed(90, &receipt), observed(90, &receipt), false),
            Verdict::Follow(block) if block.block_number == 103 && block.log_index == 11
        ));
        // Unchanged evidence below finality: nothing to write.
        let same = included(Some(transfer(&deposit, 100, 2)), 100, 2);
        assert_eq!(
            decide(&deposit, observed(90, &same), observed(99, &same), false),
            Verdict::Wait
        );
    }

    #[test]
    fn missing_receipts_require_a_known_finalized_replacement() {
        let deposit = deposit(DepositState::Credited);
        let missing = ReceiptLookup::Missing;
        assert_eq!(
            decide(
                &deposit,
                observed(200, &missing),
                observed(200, &missing),
                false
            ),
            Verdict::Pending
        );
        assert!(
            matches!(decide(&deposit,observed(200,&missing),observed(200,&missing),true),Verdict::Reverse(evidence,None) if evidence["result"] == "known_finalized_replacement")
        );
    }

    #[test]
    fn a_final_receipt_without_the_transfer_reverses_and_disagreement_waits() {
        let deposit = deposit(DepositState::Rejected);
        let without = included(None, 100, 2);
        assert!(matches!(
            decide(&deposit, observed(100, &without), observed(100, &without), false),
            Verdict::Reverse(evidence, None) if evidence["result"] == "transfer_absent_at_finality"
        ));
        // Not final yet: the transaction may still change.
        assert_eq!(
            decide(
                &deposit,
                observed(99, &without),
                observed(100, &without),
                false
            ),
            Verdict::Wait
        );
        let with = included(Some(transfer(&deposit, 100, 2)), 100, 2);
        assert_eq!(
            decide(
                &deposit,
                observed(100, &with),
                observed(100, &without),
                false
            ),
            Verdict::Wait
        );
        let missing = ReceiptLookup::Missing;
        assert_eq!(
            decide(
                &deposit,
                observed(100, &with),
                observed(100, &missing),
                false
            ),
            Verdict::Wait
        );
    }

    #[test]
    fn providers_disagreeing_on_the_transfer_at_finality_wait() {
        let deposit = deposit(DepositState::Credited);
        let mut ninety = transfer(&deposit, 100, 2);
        ninety.amount = AtomicAmount::new(U256::from(90));
        let mut eighty = ninety.clone();
        eighty.amount = AtomicAmount::new(U256::from(80));
        let primary = included(Some(ninety.clone()), 100, 2);
        let secondary = included(Some(eighty), 100, 2);
        assert_eq!(
            decide(
                &deposit,
                observed(100, &primary),
                observed(100, &secondary),
                false
            ),
            Verdict::Wait
        );
        // One provider still shows the recorded transfer.
        let recorded = included(Some(transfer(&deposit, 100, 2)), 100, 2);
        assert_eq!(
            decide(
                &deposit,
                observed(100, &primary),
                observed(100, &recorded),
                false
            ),
            Verdict::Wait
        );
        // Same content in blocks the providers disagree on.
        let elsewhere = included(Some(ninety), 100, 9);
        assert_eq!(
            decide(
                &deposit,
                observed(100, &primary),
                observed(100, &elsewhere),
                false
            ),
            Verdict::Wait
        );
    }

    #[test]
    fn a_detected_deposit_with_other_provisional_evidence_is_left_to_its_confirm_step() {
        let deposit = deposit(DepositState::Detected);
        let mut corrected = transfer(&deposit, 100, 2);
        corrected.amount = AtomicAmount::new(U256::from(8));
        let receipt = included(Some(corrected.clone()), 100, 2);
        assert_eq!(
            decide(
                &deposit,
                observed(100, &receipt),
                observed(100, &receipt),
                false
            ),
            Verdict::Wait
        );
        // Past `detected`, another transfer at the position reverses the deposit, and that
        // transfer becomes a new one.
        let credited = WatchedDeposit {
            state: DepositState::Credited,
            ..deposit
        };
        assert!(matches!(
            decide(
                &credited,
                observed(100, &receipt),
                observed(100, &receipt),
                false
            ),
            Verdict::Reverse(evidence, Some(successor))
                if evidence["result"] == "transfer_changed_at_finality" && *successor == corrected
        ));
    }
}
