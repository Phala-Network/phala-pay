//! Merchant refunds (design D5): screening a refund's destination, and verifying the transaction
//! the merchant attached with `mark_paid` once it is final on both providers.
//!
//! The merchant pays a refund from the treasury of the deposit's own address; the service sends
//! nothing. At `finalized`, both providers must show the same receipt with a `Transfer` of the
//! deposit's token from that treasury to the destination for exactly the amount, in a log no other
//! refund uses. A log is named by its position in the receipt, which, like a deposit's identity,
//! survives the transaction's re-inclusion in another block before finality. Then the refund is
//! `succeeded` (`refund.updated`) and `deposit.refunded` is sent; a finalized transaction that does
//! not pay it makes it `failed` with a `failure_reason`, which releases its reservation of the
//! deposit, and sends `refund.updated` and `refund.failed`.
//!
//! A refund with a transaction attached cannot be canceled: the merchant might pay twice. It
//! stays reserved until both providers agree on a finalized receipt that resolves it. A payout
//! never seen by either provider for 24 hours raises an alert and remains pending. Missing
//! receipts and sender nonce changes alone cannot prove that the payout will never pay it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use alloy::sol;
use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::EvmClient;
use topup_core::refund::{ExpectedRefund, RefundTransfer, match_refund_transfer};
use topup_core::route::RouteFile;
use uuid::Uuid;

use crate::db::{EventObject, NewOutboxEvent};
use crate::routes::RouteSet;
use crate::scanner::FinalizedHeads;
use crate::tenancy::Scope;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 amount);
}

/// Alert age for an attached refund transaction neither provider has ever returned.
pub const NOT_FOUND_AFTER: TimeDelta = TimeDelta::hours(24);

/// Maximum automatic recovery passes per explicit replacement candidate.
pub const MAX_REPLACEMENT_ATTEMPTS: i16 = 8;

/// One provider's view of an attached refund transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefundReceipt {
    /// No receipt: the transaction is in no block, pending, dropped, or unknown.
    Missing,
    /// The receipt's block is above the published agreed checkpoint.
    Pending {
        /// The receipt moved to this block above the agreed checkpoint.
        block_number: u64,
    },
    /// The receipt has a canonical header at or below the published agreed checkpoint.
    Finalized {
        /// Including block number.
        block_number: u64,
        /// Including block hash.
        block_hash: B256,
        /// Whether EVM execution succeeded.
        succeeded: bool,
        /// Every ERC-20 `Transfer` log of the receipt, in order.
        transfers: Vec<RefundTransfer>,
    },
}

/// Chain-read failure while reading a refund transaction.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RefundReadError {
    /// No RPC client is configured for the chain.
    #[error("no refund reader for chain {0}")]
    UnknownChain(u64),
    /// A provider is unusable.
    #[error("refund RPC configuration: {0}")]
    Configuration(String),
    /// The RPC request failed during the named operation.
    #[error("refund RPC failed during {0}")]
    Rpc(&'static str),
    /// The RPC response omitted a required field.
    #[error("refund RPC omitted `{0}`")]
    MissingField(&'static str),
}

/// One provider's finality-aware reads of refund transactions.
#[async_trait]
pub trait RefundChainReader: Send + Sync {
    /// Reads only the receipt's inclusion height as an untrusted scheduling hint.
    async fn receipt_block(
        &self,
        chain_id: u64,
        tx_hash: B256,
    ) -> Result<Option<u64>, RefundReadError>;

    /// Reads the receipt and its canonical header, trusting the published agreed checkpoint.
    async fn receipt(
        &self,
        chain_id: u64,
        tx_hash: B256,
        checkpoint: u64,
    ) -> Result<RefundReceipt, RefundReadError>;

    /// Reads the transaction's sender and nonce, pending or included; `None` when the provider
    /// does not know it.
    async fn origin(
        &self,
        chain_id: u64,
        tx_hash: B256,
    ) -> Result<Option<(Address, u64)>, RefundReadError>;
}

/// Refund reads through one provider of each chain.
pub struct EvmRefundChainReader {
    clients: BTreeMap<u64, Arc<EvmClient>>,
}

impl EvmRefundChainReader {
    /// Reads every configured chain through its provider at `index` (0 is A, 1 is B).
    pub fn from_routes(routes: &RouteSet, index: usize) -> Result<Self, RefundReadError> {
        let mut clients = BTreeMap::new();
        for chain_id in routes.chain_ids() {
            let client = routes
                .provider(chain_id, index)
                .map_err(|error| RefundReadError::Configuration(error.to_string()))?;
            clients.insert(chain_id, Arc::clone(client));
        }
        Ok(Self::new(clients))
    }

    /// Reads through explicit per-chain clients.
    #[must_use]
    pub const fn new(clients: BTreeMap<u64, Arc<EvmClient>>) -> Self {
        Self { clients }
    }

    fn client(&self, chain_id: u64) -> Result<&EvmClient, RefundReadError> {
        self.clients
            .get(&chain_id)
            .map(AsRef::as_ref)
            .ok_or(RefundReadError::UnknownChain(chain_id))
    }
}

#[async_trait]
impl RefundChainReader for EvmRefundChainReader {
    async fn receipt_block(
        &self,
        chain_id: u64,
        tx_hash: B256,
    ) -> Result<Option<u64>, RefundReadError> {
        let receipt = self
            .client(chain_id)?
            .receipt(tx_hash)
            .await
            .map_err(|_| RefundReadError::Rpc("transaction receipt fetch"))?;
        receipt
            .map(|receipt| {
                receipt
                    .block_number
                    .ok_or(RefundReadError::MissingField("receipt.block_number"))
            })
            .transpose()
    }

    async fn receipt(
        &self,
        chain_id: u64,
        tx_hash: B256,
        checkpoint: u64,
    ) -> Result<RefundReceipt, RefundReadError> {
        let client = self.client(chain_id)?;
        let receipt = client
            .receipt(tx_hash)
            .await
            .map_err(|_| RefundReadError::Rpc("transaction receipt fetch"))?;
        let Some(receipt) = receipt else {
            return Ok(RefundReceipt::Missing);
        };
        let block_number = receipt
            .block_number
            .ok_or(RefundReadError::MissingField("receipt.block_number"))?;
        let block_hash = receipt
            .block_hash
            .ok_or(RefundReadError::MissingField("receipt.block_hash"))?;
        if block_number > checkpoint {
            return Ok(RefundReceipt::Pending { block_number });
        }
        let header = client
            .price_block(alloy::eips::BlockNumberOrTag::Number(block_number))
            .await
            .map_err(|_| RefundReadError::Rpc("receipt header"))?;
        if header.0 != block_number || header.1 != block_hash || receipt.transaction_hash != tx_hash
        {
            return Err(RefundReadError::Rpc("receipt identity"));
        }
        let mut transfers = Vec::new();
        for (position, log) in (0_u64..).zip(receipt.logs()) {
            let Ok(transfer) = log.log_decode_validate::<Transfer>() else {
                continue;
            };
            transfers.push(RefundTransfer {
                receipt_log_index: position,
                token: log.address(),
                from: transfer.inner.data.from,
                to: transfer.inner.data.to,
                amount: transfer.inner.data.amount,
            });
        }
        Ok(RefundReceipt::Finalized {
            block_number,
            block_hash,
            succeeded: receipt.status(),
            transfers,
        })
    }

    async fn origin(
        &self,
        chain_id: u64,
        tx_hash: B256,
    ) -> Result<Option<(Address, u64)>, RefundReadError> {
        self.client(chain_id)?
            .transaction_origin(tx_hash)
            .await
            .map_err(|_| RefundReadError::Rpc("transaction fetch"))
    }
}

/// Sanctions screening of a refund destination on the latest verified lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationScreening {
    /// Fresh verified lists do not name the destination.
    Clear,
    /// An active snapshot or manual entry names the destination.
    Sanctioned,
    /// The verified lists could not provide a fresh answer; the request can be retried.
    Unavailable,
}

/// Screens refund destinations (design §8: Phala's software does not help move funds to a
/// sanctioned address).
#[async_trait]
pub trait DestinationScreener: Send + Sync {
    /// Screens `destination` with `route`'s sanctions oracle on its chain.
    async fn screen(&self, route: &RouteFile, destination: Address) -> DestinationScreening;
}

/// Screening that is never available, for an instance that creates no refunds.
pub struct UnavailableDestinationScreener;

#[async_trait]
impl DestinationScreener for UnavailableDestinationScreener {
    async fn screen(&self, _route: &RouteFile, _destination: Address) -> DestinationScreening {
        DestinationScreening::Unavailable
    }
}

/// Runtime scheduling for refund verification.
#[derive(Clone, Copy, Debug)]
pub struct RefundVerificationConfig {
    /// Delay between empty polls.
    pub poll_interval: Duration,
    /// Maximum duration of both providers' reads.
    pub observe_timeout: Duration,
}

impl Default for RefundVerificationConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            observe_timeout: Duration::from_secs(20),
        }
    }
}

/// What one verification pass did with a refund.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verification {
    /// No refund was due.
    Idle,
    /// The transaction is not final on both providers, the providers disagree, or a read failed.
    Waiting,
    /// The refund succeeded.
    Succeeded,
    /// The refund failed.
    Failed,
}

/// PostgreSQL-backed verification of attached refund transactions on two providers.
pub struct RefundVerificationWorker<A, B> {
    pool: PgPool,
    routes: Arc<RouteSet>,
    primary: A,
    secondary: B,
    config: RefundVerificationConfig,
    heads: FinalizedHeads,
}

impl<A, B> RefundVerificationWorker<A, B>
where
    A: RefundChainReader,
    B: RefundChainReader,
{
    /// Verifies on provider A (`primary`) and provider B (`secondary`); `routes` render the
    /// objects of the events it sends.
    pub const fn new(
        pool: PgPool,
        routes: Arc<RouteSet>,
        primary: A,
        secondary: B,
        config: RefundVerificationConfig,
        heads: FinalizedHeads,
    ) -> Self {
        Self {
            pool,
            routes,
            primary,
            secondary,
            config,
            heads,
        }
    }

    /// Verifies at most one due refund.
    pub async fn check_once(&self) -> Result<Verification, sqlx::Error> {
        self.check_at(Utc::now()).await
    }

    /// Verifies using an explicit clock for deterministic integration fixtures.
    #[cfg(feature = "test-support")]
    pub async fn check_once_at(&self, now: DateTime<Utc>) -> Result<Verification, sqlx::Error> {
        self.check_at(now).await
    }

    async fn check_at(&self, now: DateTime<Utc>) -> Result<Verification, sqlx::Error> {
        let published: BTreeMap<_, _> = self
            .heads
            .subscribe()
            .borrow()
            .iter()
            .map(|(&chain, head)| (chain.to_string(), head.number))
            .collect();
        let Some(row) = claim_due_refund(&self.pool, &json!(published), now).await? else {
            return Ok(Verification::Idle);
        };
        let mut check = row.into_check()?;
        let checkpoint = self.heads.get(check.chain_id).map(|head| head.number);
        if check.observed_block.is_none() {
            // Phase A uses receipts only. Verify is an hourly fallback, even if read fails.
            let (a, b) = tokio::join!(self.probe(&self.primary, &check), async {
                if check.poll_verify {
                    self.probe(&self.secondary, &check).await
                } else {
                    (None, false)
                }
            });
            if a.1 || b.1 {
                self.probe_origin(&mut check, now).await?;
            }
            let block = [a.0, b.0].into_iter().flatten().min();
            let Some(block) = block else {
                let overdue = now.signed_duration_since(check.paid_at) >= NOT_FOUND_AFTER;
                persist_evidence(
                    &self.pool,
                    &check,
                    &json!({
                        "result": if overdue { "not_found_overdue" } else { "not_included" },
                        "reversal": "unproven",
                    }),
                )
                .await?;
                if overdue {
                    tracing::warn!(tags.alert="TopupRefundProgressAge",chain_id=check.chain_id,refund_id=%check.refund_id,
                        "refund receipt is still absent after 24 hours; reservation remains pending");
                }
                return Ok(Verification::Waiting);
            };
            observe_block(&self.pool, &check, Some(block), now).await?;
            check.observed_block = Some(block);
            // Phase B makes no RPC while the inclusion height exceeds the published checkpoint.
            if checkpoint.is_none_or(|checkpoint| block > checkpoint) {
                return Ok(Verification::Waiting);
            }
            schedule_verification(&self.pool, &check, now).await?;
        }
        let Some(checkpoint) = checkpoint else {
            return Ok(Verification::Waiting);
        };
        // Phase C: each endpoint independently verifies receipt + header(b), and origin once.
        let reads = async {
            tokio::join!(
                self.primary
                    .receipt(check.chain_id, check.tx_hash, checkpoint),
                self.secondary
                    .receipt(check.chain_id, check.tx_hash, checkpoint)
            )
        };
        let (primary, secondary) = match tokio::time::timeout(self.config.observe_timeout, reads)
            .await
        {
            Ok((Ok(primary), Ok(secondary))) => (primary, secondary),
            Ok((Err(error), _) | (_, Err(error))) => {
                tracing::warn!(refund_id=%check.refund_id,%error,"refund verification chain read failed");
                return Ok(Verification::Waiting);
            }
            Err(_) => {
                persist_evidence(&self.pool, &check, &json!({"result":"observe_timeout"})).await?;
                return Ok(Verification::Waiting);
            }
        };
        self.remember_origin(&mut check).await?;
        let (block_number, block_hash, succeeded, transfers) = match (&primary, &secondary) {
            (
                RefundReceipt::Finalized {
                    block_number,
                    block_hash,
                    succeeded,
                    transfers,
                },
                _,
            ) if primary == secondary && *block_number <= checkpoint => {
                (*block_number, *block_hash, *succeeded, transfers)
            }
            _ => {
                let replacement = self.resolve_replacement(&check, checkpoint).await?;
                if matches!(replacement, ReplacementVerification::Resolved) {
                    return Ok(Verification::Failed);
                }
                let moved = [&primary, &secondary]
                    .into_iter()
                    .filter_map(|receipt| match receipt {
                        RefundReceipt::Pending { block_number } => Some(*block_number),
                        _ => None,
                    })
                    .min()
                    .or(match replacement {
                        ReplacementVerification::Moved(block) => Some(block),
                        _ => None,
                    });
                if let Some(block) = moved {
                    observe_block(&self.pool, &check, Some(block), now).await?;
                } else if primary == RefundReceipt::Missing
                    && secondary == RefundReceipt::Missing
                    && (check.replacement_hash.is_none()
                        || matches!(replacement, ReplacementVerification::Missing))
                {
                    observe_block(&self.pool, &check, None, now).await?;
                    let overdue = now.signed_duration_since(check.paid_at) >= NOT_FOUND_AFTER;
                    persist_evidence(&self.pool, &check, &json!({"result":if overdue {"not_found_overdue"} else {"not_included"},"reversal":"unproven"})).await?;
                    if overdue {
                        tracing::warn!(tags.alert="TopupRefundProgressAge",chain_id=check.chain_id,refund_id=%check.refund_id,
                            "refund receipt is still absent after 24 hours; reservation remains pending");
                    }
                    return Ok(Verification::Waiting);
                }
                persist_evidence(
                    &self.pool,
                    &check,
                    &json!({"result":"unresolved","reversal":"unproven"}),
                )
                .await?;
                if primary != secondary {
                    tracing::warn!(tags.alert="TopupRpcDisagreement",chain_id=check.chain_id,refund_id=%check.refund_id,
                        "providers disagree on a refund transaction");
                }
                return Ok(Verification::Waiting);
            }
        };
        let used = used_logs(&self.pool, &check).await?;
        let outcome = match_refund_transfer(
            &check.expected,
            succeeded,
            transfers,
            check.receipt_log_index,
            &used,
        );
        let evidence = json!({
            "result": match outcome {
                Ok(_) => "matched",
                Err(reason) => reason.code(),
            },
            "block_number": block_number,
            "block_hash": format!("{block_hash:#x}"),
            "succeeded": succeeded,
            "token": format!("{:#x}", check.expected.token),
            "treasury": format!("{:#x}", check.expected.treasury),
            "destination_address": format!("{:#x}", check.expected.destination),
            "amount_atomic": check.expected.amount.to_string(),
            "transfers": transfers.iter().map(|transfer| json!({
                "receipt_log_index": transfer.receipt_log_index,
                "token": format!("{:#x}", transfer.token),
                "from": format!("{:#x}", transfer.from),
                "to": format!("{:#x}", transfer.to),
                "amount_atomic": transfer.amount.to_string(),
            })).collect::<Vec<_>>(),
        });
        match outcome {
            Ok(receipt_log_index) => {
                if succeed(
                    &self.pool,
                    &self.routes,
                    &check,
                    receipt_log_index,
                    &evidence,
                )
                .await?
                {
                    return Ok(Verification::Succeeded);
                }
                // The refund was held, changed, or another refund took the log since it was read.
                persist_evidence(
                    &self.pool,
                    &check,
                    &json!({"result": "verification_deferred"}),
                )
                .await?;
                Ok(Verification::Waiting)
            }
            Err(reason) => {
                if !fail(&self.pool, &self.routes, &check, reason.code(), &evidence).await? {
                    return Ok(Verification::Waiting);
                }
                tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), reason = reason.code(), "finalized refund transaction does not pay the refund");
                Ok(Verification::Failed)
            }
        }
    }

    async fn probe(
        &self,
        reader: &dyn RefundChainReader,
        check: &RefundCheck,
    ) -> (Option<u64>, bool) {
        let reads = async {
            if let Ok(Some(block)) = reader.receipt_block(check.chain_id, check.tx_hash).await {
                return (Some(block), false);
            }
            if let Some(replacement) = check.replacement_hash {
                let block = reader
                    .receipt_block(check.chain_id, replacement)
                    .await
                    .ok()
                    .flatten();
                return (block, true);
            }
            (None, true)
        };
        tokio::time::timeout(self.config.observe_timeout, reads)
            .await
            .unwrap_or((None, true))
    }

    /// A positively identified finalized replacement proves the attachment cannot be included.
    async fn resolve_replacement(
        &self,
        check: &RefundCheck,
        checkpoint: u64,
    ) -> Result<ReplacementVerification, sqlx::Error> {
        let (Some(replacement), Some(origin)) = (check.replacement_hash, check.origin) else {
            return Ok(ReplacementVerification::Waiting);
        };
        let reads = async {
            tokio::join!(
                self.primary
                    .receipt(check.chain_id, replacement, checkpoint),
                self.secondary
                    .receipt(check.chain_id, replacement, checkpoint),
                self.primary.origin(check.chain_id, replacement),
                self.secondary.origin(check.chain_id, replacement),
            )
        };
        let Ok((Ok(a), Ok(b), origin_a, origin_b)) =
            tokio::time::timeout(self.config.observe_timeout, reads).await
        else {
            return Ok(ReplacementVerification::Waiting);
        };
        if let Some(block) = [&a, &b]
            .into_iter()
            .filter_map(|receipt| match receipt {
                RefundReceipt::Pending { block_number } => Some(*block_number),
                _ => None,
            })
            .min()
        {
            return Ok(ReplacementVerification::Moved(block));
        }
        if a == RefundReceipt::Missing && b == RefundReceipt::Missing {
            return Ok(ReplacementVerification::Missing);
        }
        let (Ok(Some(from_a)), Ok(Some(from_b))) = (origin_a, origin_b) else {
            return Ok(ReplacementVerification::Waiting);
        };
        if from_a != origin || from_b != origin || a != b {
            return Ok(ReplacementVerification::Waiting);
        }
        let RefundReceipt::Finalized {
            block_number,
            block_hash,
            ..
        } = a
        else {
            return Ok(ReplacementVerification::Waiting);
        };
        if block_number > checkpoint {
            return Ok(ReplacementVerification::Waiting);
        }
        let changed=fail(&self.pool, &self.routes, check, "transaction_dropped", &json!({
            "result":"replacement_finalized", "replacement_transaction_hash":format!("{replacement:#x}"),
            "block_number":block_number, "block_hash":format!("{block_hash:#x}"),
            "sender":format!("{:#x}",origin.0), "nonce":origin.1,
        })).await?;
        Ok(if changed {
            ReplacementVerification::Resolved
        } else {
            ReplacementVerification::Waiting
        })
    }

    /// Keeps the sender and nonce when both providers agree, for operator investigation.
    /// These fields alone do not prove a replacement; a failed read leaves them unknown.
    async fn remember_origin(&self, check: &mut RefundCheck) -> Result<bool, sqlx::Error> {
        if check.origin.is_some() {
            return Ok(true);
        }
        if check.origin_checked && check.replacement_hash.is_none() {
            return Ok(false);
        }
        // Origin is immutable by transaction hash. Ordinary verification attempts it once;
        // an operator's replacement proof may retry an origin that was unavailable.
        sqlx::query("UPDATE refunds SET origin_checked=true WHERE id=$1 AND tx_hash=$2")
            .bind(check.refund_id)
            .bind(format!("{:#x}", check.tx_hash))
            .execute(&self.pool)
            .await?;
        self.read_origin(check).await
    }

    /// Preserve a pending transaction before nodes can evict it. At most three durable dual
    /// attempts, at attachment, ten minutes and one hour; stop as soon as both providers agree.
    async fn probe_origin(
        &self,
        check: &mut RefundCheck,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        if check.origin.is_some() {
            return Ok(());
        }
        let updated = sqlx::query("UPDATE refunds SET origin_probes=origin_probes+1 WHERE id=$1 AND tx_hash=$2 AND tx_from IS NULL AND origin_probes<3 AND $3 >= paid_at + CASE origin_probes WHEN 0 THEN interval '0 seconds' WHEN 1 THEN interval '10 minutes' ELSE interval '1 hour' END")
            .bind(check.refund_id).bind(format!("{:#x}",check.tx_hash)).bind(now)
            .execute(&self.pool).await?;
        if updated.rows_affected() == 1 {
            self.read_origin(check).await?;
        }
        Ok(())
    }

    async fn read_origin(&self, check: &mut RefundCheck) -> Result<bool, sqlx::Error> {
        let reads = async {
            tokio::join!(
                self.primary.origin(check.chain_id, check.tx_hash),
                self.secondary.origin(check.chain_id, check.tx_hash)
            )
        };
        let Ok((Ok(a), Ok(b))) = tokio::time::timeout(self.config.observe_timeout, reads).await
        else {
            return Ok(false);
        };
        if a != b {
            return Ok(false);
        }
        let Some((from, nonce)) = a else {
            return Ok(true);
        };
        sqlx::query(
            r#"
            UPDATE refunds
            SET tx_from = $3, tx_nonce = $4::text::numeric, updated_at = now()
            WHERE id = $1 AND tx_hash = $2 AND tx_from IS NULL
            "#,
        )
        .bind(check.refund_id)
        .bind(format!("{:#x}", check.tx_hash))
        .bind(format!("{from:#x}"))
        .bind(nonce.to_string())
        .execute(&self.pool)
        .await?;
        check.origin = Some((from, nonce));
        Ok(true)
    }

    /// Runs until cancellation, retrying database and chain failures indefinitely.
    pub async fn run(&self, cancellation: CancellationToken) {
        let mut advances = self.heads.subscribe();
        loop {
            advances.borrow_and_update();
            if cancellation.is_cancelled() {
                return;
            }
            let pause = tokio::select! {
                biased;
                () = cancellation.cancelled() => return,
                result = self.check_once() => match result {
                    Ok(verification) => verification == Verification::Idle,
                    Err(error) => {
                        tracing::error!(%error, "refund verification database poll failed");
                        true
                    }
                }
            };
            if pause {
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    () = tokio::time::sleep(self.config.poll_interval) => {}
                    _ = advances.changed() => {}
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ReplacementVerification {
    Resolved,
    Waiting,
    Missing,
    Moved(u64),
}

struct RefundCheck {
    refund_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    deposit_id: Uuid,
    chain_id: u64,
    tx_hash: B256,
    receipt_log_index: Option<u64>,
    observed_block: Option<u64>,
    replacement_hash: Option<B256>,
    poll_verify: bool,
    origin_checked: bool,
    paid_at: DateTime<Utc>,
    /// The transaction's sender and nonce, once a provider returned it.
    origin: Option<(Address, u64)>,
    expected: ExpectedRefund,
}

#[derive(Clone, FromRow)]
struct RefundCheckRow {
    refund_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    deposit_id: Uuid,
    chain_id: i64,
    tx_hash: String,
    receipt_log_index: Option<i64>,
    observed_block: Option<i64>,
    replacement_hash: Option<String>,
    replacement_attempts: i16,
    poll_verify: bool,
    origin_checked: bool,
    paid_at: DateTime<Utc>,
    tx_from: Option<String>,
    tx_nonce: Option<String>,
    token: String,
    treasury: String,
    destination_address: String,
    amount_atomic: String,
}

impl RefundCheckRow {
    fn into_check(self) -> Result<RefundCheck, sqlx::Error> {
        Ok(RefundCheck {
            refund_id: self.refund_id,
            account_id: self.account_id,
            livemode: self.livemode,
            deposit_id: self.deposit_id,
            chain_id: u64::try_from(self.chain_id).map_err(decode_error)?,
            tx_hash: self.tx_hash.parse().map_err(decode_error)?,
            receipt_log_index: self
                .receipt_log_index
                .map(u64::try_from)
                .transpose()
                .map_err(decode_error)?,
            observed_block: self
                .observed_block
                .map(u64::try_from)
                .transpose()
                .map_err(decode_error)?,
            replacement_hash: if self.replacement_attempts < MAX_REPLACEMENT_ATTEMPTS {
                self.replacement_hash
                    .map(|hash| hash.parse())
                    .transpose()
                    .map_err(decode_error)?
            } else {
                None
            },
            poll_verify: self.poll_verify,
            origin_checked: self.origin_checked,
            paid_at: self.paid_at,
            origin: match (self.tx_from, self.tx_nonce) {
                (Some(from), Some(nonce)) => Some((
                    from.parse().map_err(decode_error)?,
                    nonce.parse().map_err(decode_error)?,
                )),
                _ => None,
            },
            expected: ExpectedRefund {
                token: self.token.parse().map_err(decode_error)?,
                treasury: self.treasury.parse().map_err(decode_error)?,
                destination: self.destination_address.parse().map_err(decode_error)?,
                amount: self.amount_atomic.parse::<U256>().map_err(decode_error)?,
            },
        })
    }
}

/// Locks a refund's deposit before its refund row, and reports whether it has no sanctions hit.
/// The lock is held until the caller's transaction ends; no lock is kept across chain reads.
pub(crate) async fn lock_refund_deposit(
    connection: &mut PgConnection,
    scope: Scope,
    refund_id: Uuid,
) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT deposit.sanctions_hit_at IS NULL \
         FROM deposits AS deposit JOIN refunds AS refund ON refund.deposit_id = deposit.id \
         WHERE refund.id = $1 AND refund.account_id = $2 AND refund.livemode = $3 \
         FOR UPDATE OF deposit",
    )
    .bind(refund_id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(connection)
    .await
}

/// Claims the refund due first; published checkpoints gate phase B without chain reads. The expected
/// sender is the treasury of the deposit's own address, fixed when the address was issued.
async fn claim_due_refund(
    pool: &PgPool,
    published: &Value,
    now: DateTime<Utc>,
) -> Result<Option<RefundCheckRow>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    // Lock the deposit first, as mark-paid and the final verification transaction do. A locked
    // or sanctioned deposit is skipped without delaying verification of other deposits.
    let candidate: Option<Uuid> = sqlx::query_scalar(
        "SELECT refund.id FROM refunds AS refund \
         JOIN deposits AS deposit ON deposit.id = refund.deposit_id \
         WHERE refund.status = 'pending' AND refund.tx_hash IS NOT NULL \
           AND refund.next_check_at <= $1 AND deposit.sanctions_hit_at IS NULL \
           AND (refund.observed_block IS NULL OR refund.observed_block <= \
                COALESCE(($2::jsonb ->> refund.chain_id::text)::bigint, -1)) \
         ORDER BY refund.next_check_at, refund.id \
         FOR UPDATE OF deposit SKIP LOCKED LIMIT 1",
    )
    .bind(now)
    .bind(published)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(candidate) = candidate else {
        transaction.rollback().await?;
        return Ok(None);
    };
    let claimed = sqlx::query_as::<_, RefundCheckRow>(
        r#"
        UPDATE refunds AS refund
        SET next_check_at = $2 + interval '1 hour', updated_at = now()
        FROM deposits AS deposit, addresses AS address
        WHERE refund.id = $1 AND refund.status = 'pending' AND refund.tx_hash IS NOT NULL
          AND refund.next_check_at <= $2
          AND deposit.id = refund.deposit_id AND deposit.sanctions_hit_at IS NULL
          AND address.id = deposit.address_id
        RETURNING refund.id AS refund_id, refund.account_id, refund.livemode, refund.deposit_id,
                  refund.chain_id, refund.tx_hash, refund.receipt_log_index, refund.paid_at,
                  refund.tx_from, refund.tx_nonce::text AS tx_nonce,
                  refund.observed_block, refund.replacement_hash, refund.replacement_attempts, refund.origin_checked,
                  ($2 >= refund.paid_at + interval '1 hour' AND
                    (refund.verify_checked_at IS NULL OR $2 >= refund.verify_checked_at + interval '1 hour')) AS poll_verify,
                  deposit.asset_contract AS token, address.treasury,
                  refund.destination_address, refund.amount_atomic::text AS amount_atomic
        "#,
    )
    .bind(candidate)
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(row) = &claimed {
        let check = row.clone().into_check()?;
        if check.replacement_hash.is_some() {
            sqlx::query(
                "UPDATE refunds SET replacement_attempts=replacement_attempts+1 WHERE id=$1",
            )
            .bind(check.refund_id)
            .execute(&mut *transaction)
            .await?;
        }
        if check.observed_block.is_some() {
            schedule_verification(&mut *transaction, &check, now).await?;
        } else {
            sqlx::query("UPDATE refunds SET next_check_at=$2, verify_checked_at=CASE WHEN $3 THEN $4 ELSE verify_checked_at END WHERE id=$1")
                .bind(check.refund_id).bind(next_probe(check.paid_at, now)).bind(check.poll_verify).bind(now)
                .execute(&mut *transaction).await?;
        }
    }
    transaction.commit().await?;
    Ok(claimed)
}

/// Next inclusion probe, without replaying missed ticks after downtime.
fn next_probe(paid_at: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
    let age = now.signed_duration_since(paid_at).num_seconds();
    let seconds = [60, 120, 300, 600]
        .into_iter()
        .find(|&tick| tick > age)
        .unwrap_or_else(|| {
            if age < 7200 {
                age.saturating_div(600)
                    .saturating_add(1)
                    .saturating_mul(600)
            } else {
                age.saturating_add(if age >= 86400 { 21600 } else { 3600 })
            }
        });
    paid_at + TimeDelta::seconds(seconds)
}

async fn observe_block(
    pool: &PgPool,
    check: &RefundCheck,
    block: Option<u64>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE refunds SET observed_block=$3, next_check_at=$4 WHERE id=$1 AND status='pending' AND tx_hash=$2")
        .bind(check.refund_id).bind(format!("{:#x}",check.tx_hash))
        .bind(block.map(to_i64).transpose()?).bind(if block.is_some() {now} else {next_probe(check.paid_at,now)})
        .execute(pool).await?;
    Ok(())
}

/// Advance before I/O, so failures and process restarts do not reset retry accounting.
async fn schedule_verification<'c>(
    db: impl sqlx::Executor<'c, Database = sqlx::Postgres>,
    check: &RefundCheck,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE refunds SET next_check_at=$3 + CASE WHEN $3 >= paid_at + interval '24 hours' THEN interval '6 hours' WHEN verification_attempts=0 THEN interval '10 minutes' ELSE interval '1 hour' END, verification_attempts=verification_attempts+1 WHERE id=$1 AND status='pending' AND tx_hash=$2")
        .bind(check.refund_id).bind(format!("{:#x}",check.tx_hash)).bind(now).execute(db).await?;
    Ok(())
}

/// Logs of the transaction that pay or are named by another live refund.
async fn used_logs(pool: &PgPool, check: &RefundCheck) -> Result<BTreeSet<u64>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT receipt_log_index
        FROM refunds
        WHERE chain_id = $1 AND tx_hash = $2 AND id <> $3 AND receipt_log_index IS NOT NULL
          AND status IN ('pending', 'succeeded')
        "#,
    )
    .bind(to_i64(check.chain_id)?)
    .bind(format!("{:#x}", check.tx_hash))
    .bind(check.refund_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|index| u64::try_from(index).map_err(decode_error))
        .collect()
}

async fn persist_evidence(
    pool: &PgPool,
    check: &RefundCheck,
    evidence: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE refunds
        SET confirmation_evidence = $3, updated_at = now()
        WHERE id = $1 AND status = 'pending' AND tx_hash = $2
        "#,
    )
    .bind(check.refund_id)
    .bind(format!("{:#x}", check.tx_hash))
    .bind(evidence)
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks the refund `succeeded` with its log's receipt position and sends `refund.updated` and `deposit.refunded`;
/// `false` when its deposit has a sanctions hit, the refund is no longer pending, or another
/// refund took the log first.
async fn succeed(
    pool: &PgPool,
    routes: &RouteSet,
    check: &RefundCheck,
    receipt_log_index: u64,
    evidence: &Value,
) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let scope = Scope::new(check.account_id, check.livemode);
    if lock_refund_deposit(&mut transaction, scope, check.refund_id).await? != Some(true) {
        transaction.rollback().await?;
        return Ok(false);
    }
    let object = EventObject::Refund(check.refund_id);
    let before = crate::db::render(&mut transaction, routes, scope, object).await?;
    let updated = sqlx::query(
        r#"
        UPDATE refunds
        SET status = 'succeeded', receipt_log_index = $3, confirmation_evidence = $4,
            updated_at = now()
        WHERE id = $1 AND status = 'pending' AND tx_hash = $2
        "#,
    )
    .bind(check.refund_id)
    .bind(format!("{:#x}", check.tx_hash))
    .bind(to_i64(receipt_log_index)?)
    .bind(evidence)
    .execute(&mut *transaction)
    .await;
    match updated {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            transaction.rollback().await?;
            return Ok(false);
        }
        Err(sqlx::Error::Database(error))
            if error.constraint() == Some("refunds_transfer_unique") =>
        {
            transaction.rollback().await?;
            return Ok(false);
        }
        Err(error) => return Err(error),
    }
    let updated = NewOutboxEvent::system(Uuid::new_v4(), "refund.updated", scope, object);
    crate::db::enqueue_in(&mut transaction, routes, &updated, Some(&before)).await?;
    // The event's object is the deposit, with its refunded amount (as Stripe's `charge.refunded`
    // is the charge); its id is derived from the refund, one event per refund.
    let refunded = NewOutboxEvent::system(
        topup_core::identity::event_id("deposit.refunded", check.refund_id),
        "deposit.refunded",
        scope,
        EventObject::Deposit(check.deposit_id),
    );
    crate::db::enqueue_in(&mut transaction, routes, &refunded, None).await?;
    transaction.commit().await?;
    Ok(true)
}

/// Marks the refund `failed` and sends `refund.updated` and `refund.failed` (Stripe's event for a
/// failed refund), whose object is the refund with its `failure_reason`; the `refund.failed` id is
/// derived from the refund, one event per refund. Nothing is sent when the refund is no longer
/// pending or its deposit has a sanctions hit.
async fn fail(
    pool: &PgPool,
    routes: &RouteSet,
    check: &RefundCheck,
    reason: &str,
    evidence: &Value,
) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let scope = Scope::new(check.account_id, check.livemode);
    if lock_refund_deposit(&mut transaction, scope, check.refund_id).await? != Some(true) {
        transaction.rollback().await?;
        return Ok(false);
    }
    let object = EventObject::Refund(check.refund_id);
    let before = crate::db::render(&mut transaction, routes, scope, object).await?;
    let updated = sqlx::query(
        r#"
        UPDATE refunds
        SET status = 'failed', failure_reason = $3, confirmation_evidence = $4, updated_at = now()
        WHERE id = $1 AND status = 'pending' AND tx_hash = $2
        "#,
    )
    .bind(check.refund_id)
    .bind(format!("{:#x}", check.tx_hash))
    .bind(reason)
    .bind(evidence)
    .execute(&mut *transaction)
    .await?;
    let changed = updated.rows_affected() == 1;
    if changed {
        let updated = NewOutboxEvent::system(Uuid::new_v4(), "refund.updated", scope, object);
        crate::db::enqueue_in(&mut transaction, routes, &updated, Some(&before)).await?;
        let failed = NewOutboxEvent::system(
            topup_core::identity::event_id("refund.failed", check.refund_id),
            "refund.failed",
            scope,
            object,
        );
        crate::db::enqueue_in(&mut transaction, routes, &failed, None).await?;
    }
    transaction.commit().await?;
    Ok(changed)
}

fn to_i64(value: u64) -> Result<i64, sqlx::Error> {
    i64::try_from(value).map_err(decode_error)
}

fn decode_error(error: impl Display) -> sqlx::Error {
    sqlx::Error::Decode(error.to_string().into())
}
