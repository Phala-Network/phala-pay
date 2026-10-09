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
use crate::tenancy::Scope;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 amount);
}

/// Alert age for an attached refund transaction neither provider has ever returned.
pub const NOT_FOUND_AFTER: TimeDelta = TimeDelta::hours(24);

/// One provider's view of an attached refund transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefundReceipt {
    /// No receipt: the transaction is in no block, pending, dropped, or unknown.
    Missing,
    /// The receipt's block is above the provider's `finalized`.
    Pending,
    /// The receipt is at or below `finalized`.
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
    /// Reads the transaction's receipt on `chain_id`.
    async fn receipt(&self, chain_id: u64, tx_hash: B256)
    -> Result<RefundReceipt, RefundReadError>;

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
    pool: PgPool,
}

impl EvmRefundChainReader {
    /// Reads every configured chain through its provider at `index` (0 is A, 1 is B).
    pub fn from_routes(
        pool: PgPool,
        routes: &RouteSet,
        index: usize,
    ) -> Result<Self, RefundReadError> {
        let mut clients = BTreeMap::new();
        for chain_id in routes.chain_ids() {
            let client = routes
                .provider(chain_id, index)
                .map_err(|error| RefundReadError::Configuration(error.to_string()))?;
            clients.insert(chain_id, Arc::clone(client));
        }
        Ok(Self::new(pool, clients))
    }

    /// Reads through explicit per-chain clients.
    #[must_use]
    pub const fn new(pool: PgPool, clients: BTreeMap<u64, Arc<EvmClient>>) -> Self {
        Self { clients, pool }
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
    async fn receipt(
        &self,
        chain_id: u64,
        tx_hash: B256,
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
        let checkpoint = crate::db::chain_reads::checkpoint(&self.pool, chain_id)
            .await
            .map_err(|_| RefundReadError::Rpc("checkpoint persistence"))?
            .ok_or(RefundReadError::MissingField("checkpoint"))?;
        let pin = client
            .price_block(alloy::eips::BlockNumberOrTag::Number(checkpoint.number))
            .await
            .map_err(|_| RefundReadError::Rpc("checkpoint header"))?;
        if pin.1 != checkpoint.hash {
            return Err(RefundReadError::Rpc("checkpoint conflict"));
        }
        if block_number > checkpoint.number {
            return Ok(RefundReceipt::Pending);
        }
        let header = client
            .price_block(alloy::eips::BlockNumberOrTag::Number(block_number))
            .await
            .map_err(|_| RefundReadError::Rpc("receipt header"))?;
        if header.1 != block_hash || receipt.transaction_hash != tx_hash {
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
    /// Minimum retry delay. Checks wait at least 60 s for the first 30 min after attachment,
    /// 10 min until 24 h, then 1 h indefinitely.
    pub retry_interval: Duration,
    /// Maximum duration of both providers' reads.
    pub observe_timeout: Duration,
}

impl Default for RefundVerificationConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            retry_interval: Duration::from_secs(60),
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
    ) -> Self {
        Self {
            pool,
            routes,
            primary,
            secondary,
            config,
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
        let retry_seconds = i32::try_from(self.config.retry_interval.as_secs()).unwrap_or(i32::MAX);
        let Some(row) = claim_due_refund(&self.pool, retry_seconds, now).await? else {
            return Ok(Verification::Idle);
        };
        let mut check = row.into_check()?;
        let reads = async {
            tokio::join!(
                self.primary.receipt(check.chain_id, check.tx_hash),
                self.secondary.receipt(check.chain_id, check.tx_hash)
            )
        };
        let (primary, secondary) = match tokio::time::timeout(self.config.observe_timeout, reads)
            .await
        {
            Ok((Ok(primary), Ok(secondary))) => (primary, secondary),
            Ok((Err(error), _) | (_, Err(error))) => {
                tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), %error, "refund verification chain read failed");
                return Ok(Verification::Waiting);
            }
            Err(_) => {
                persist_evidence(&self.pool, &check, &json!({"result": "observe_timeout"})).await?;
                tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), "refund verification timed out");
                return Ok(Verification::Waiting);
            }
        };
        let (block_number, block_hash, succeeded, transfers) = match (&primary, &secondary) {
            (
                RefundReceipt::Finalized {
                    block_number,
                    block_hash,
                    succeeded,
                    transfers,
                },
                _,
            ) if primary == secondary => (*block_number, *block_hash, *succeeded, transfers),
            (RefundReceipt::Missing, RefundReceipt::Missing) => {
                return self.not_included(&mut check, now).await;
            }
            (RefundReceipt::Pending | RefundReceipt::Missing, _)
            | (_, RefundReceipt::Pending | RefundReceipt::Missing) => {
                self.remember_origin(&mut check).await?;
                persist_evidence(&self.pool, &check, &json!({"result": "pending"})).await?;
                return Ok(Verification::Waiting);
            }
            _ => {
                persist_evidence(&self.pool, &check, &json!({"result": "providers_disagree"}))
                    .await?;
                tracing::warn!(tags.alert="TopupRpcDisagreement",chain_id=check.chain_id,refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), "providers disagree on a finalized refund transaction");
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

    /// Missing receipts keep the reservation. Neither absence nor elapsed time proves that a
    /// transaction cannot pay; alert after 24 hours without ever observing its origin.
    async fn not_included(
        &self,
        check: &mut RefundCheck,
        now: DateTime<Utc>,
    ) -> Result<Verification, sqlx::Error> {
        if !self.remember_origin(check).await? {
            return Ok(Verification::Waiting);
        }
        if check.origin.is_none() {
            let overdue = now.signed_duration_since(check.paid_at) >= NOT_FOUND_AFTER;
            persist_evidence(
                &self.pool,
                check,
                &json!({
                    "result": if overdue { "not_found_overdue" } else { "not_found" },
                    "paid_at": check.paid_at.timestamp(),
                    "reversal": "unproven",
                }),
            )
            .await?;
            if overdue {
                tracing::warn!(tags.alert="TopupRefundProgressAge",chain_id=check.chain_id,refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), "no provider has returned the refund transaction for 24 hours; reservation remains pending");
            }
            return Ok(Verification::Waiting);
        }
        persist_evidence(
            &self.pool,
            check,
            &json!({"result":"not_included","reversal":"unproven"}),
        )
        .await?;
        Ok(Verification::Waiting)
    }

    /// Keeps the sender and nonce when both providers agree, for operator investigation.
    /// These fields alone do not prove a replacement; a failed read leaves them unknown.
    async fn remember_origin(&self, check: &mut RefundCheck) -> Result<bool, sqlx::Error> {
        if check.origin.is_some() {
            return Ok(true);
        }
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
        loop {
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
                }
            }
        }
    }
}

struct RefundCheck {
    refund_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    deposit_id: Uuid,
    chain_id: u64,
    tx_hash: B256,
    receipt_log_index: Option<u64>,
    paid_at: DateTime<Utc>,
    /// The transaction's sender and nonce, once a provider returned it.
    origin: Option<(Address, u64)>,
    expected: ExpectedRefund,
}

#[derive(FromRow)]
struct RefundCheckRow {
    refund_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    deposit_id: Uuid,
    chain_id: i64,
    tx_hash: String,
    receipt_log_index: Option<i64>,
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

/// Claims the refund due first and schedules the next check from its attachment age. The expected
/// sender is the treasury of the deposit's own address, fixed when the address was issued.
async fn claim_due_refund(
    pool: &PgPool,
    retry_seconds: i32,
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
         ORDER BY refund.next_check_at, refund.id \
         FOR UPDATE OF deposit SKIP LOCKED LIMIT 1",
    )
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let claimed = sqlx::query_as::<_, RefundCheckRow>(
        r#"
        UPDATE refunds AS refund
        SET next_check_at = $3 + make_interval(secs => GREATEST($1, CASE
                WHEN $3 < refund.paid_at + interval '30 minutes' THEN 60
                WHEN $3 < refund.paid_at + interval '24 hours' THEN 600
                ELSE 3600
            END)), updated_at = now()
        FROM deposits AS deposit, addresses AS address
        WHERE refund.id = $2 AND refund.status = 'pending' AND refund.tx_hash IS NOT NULL
          AND refund.next_check_at <= $3
          AND deposit.id = refund.deposit_id AND deposit.sanctions_hit_at IS NULL
          AND address.id = deposit.address_id
        RETURNING refund.id AS refund_id, refund.account_id, refund.livemode, refund.deposit_id,
                  refund.chain_id, refund.tx_hash, refund.receipt_log_index, refund.paid_at,
                  refund.tx_from, refund.tx_nonce::text AS tx_nonce,
                  deposit.asset_contract AS token, address.treasury,
                  refund.destination_address, refund.amount_atomic::text AS amount_atomic
        "#,
    )
    .bind(retry_seconds)
    .bind(candidate)
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(claimed)
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
        Ok(_) => return Ok(false),
        Err(sqlx::Error::Database(error))
            if error.constraint() == Some("refunds_transfer_unique") =>
        {
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
