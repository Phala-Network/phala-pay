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
//! A refund with a transaction attached cannot be canceled: the merchant might pay twice. It stays
//! reserved until verified, or until the transaction is proven never to pay it: `failed` with
//! `transaction_dropped` once neither provider has a receipt and, at `finalized` on both, the
//! sender's nonce (kept when a provider first returns the transaction) was consumed by another
//! transaction; or with `transaction_not_found` when neither provider has returned the transaction
//! for [`NOT_FOUND_AFTER`] since it was attached. The merchant then requests a new refund.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use alloy::sol;
use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use moka::future::Cache;
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::EvmClient;
use topup_adapters::risk::oracle::{SanctionsOracle, SanctionsSource};
use topup_core::refund::{ExpectedRefund, RefundFailure, RefundTransfer, match_refund_transfer};
use topup_core::route::RouteFile;
use topup_core::screening::SanctionsAnswer;
use uuid::Uuid;

use crate::db::{EventObject, NewOutboxEvent};
use crate::routes::RouteSet;
use crate::tenancy::Scope;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 amount);
}

/// A refund transaction neither provider has ever returned fails this long after it was attached.
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

    /// Reads `account`'s nonce at the provider's `finalized` block.
    async fn finalized_nonce(
        &self,
        chain_id: u64,
        account: Address,
    ) -> Result<u64, RefundReadError>;
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

async fn finalized_block(client: &EvmClient) -> Result<u64, RefundReadError> {
    client
        .finalized_block()
        .await
        .map_err(|_| RefundReadError::Rpc("finalized head fetch"))?
        .ok_or(RefundReadError::MissingField("finalized block"))
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
        if block_number > finalized_block(client).await? {
            return Ok(RefundReceipt::Pending);
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

    async fn finalized_nonce(
        &self,
        chain_id: u64,
        account: Address,
    ) -> Result<u64, RefundReadError> {
        let client = self.client(chain_id)?;
        let finalized = finalized_block(client).await?;
        client
            .nonce_at(account, finalized)
            .await
            .map_err(|_| RefundReadError::Rpc("nonce fetch"))
    }
}

/// Sanctions screening of a refund destination on the route's chain and oracle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationScreening {
    /// Neither provider lists the destination.
    Clear,
    /// A provider lists the destination.
    Sanctioned,
    /// A provider could not answer; the request can be retried.
    Unavailable,
}

/// Screens refund destinations (design §8: Phala's software does not help move funds to a
/// sanctioned address).
#[async_trait]
pub trait DestinationScreener: Send + Sync {
    /// Screens `destination` with `route`'s sanctions oracle on its chain.
    async fn screen(&self, route: &RouteFile, destination: Address) -> DestinationScreening;

    /// Screens a destination and may reuse a recent clear verdict; for paths where funds can only
    /// reach the merchant's own address.
    async fn screen_cached(&self, route: &RouteFile, destination: Address) -> DestinationScreening {
        self.screen(route, destination).await
    }
}

/// Screens through the route's sanctions oracle on its chain's first two providers, at provider
/// A's `finalized` block, as the deposit screening step does at the deposit's block.
pub struct OracleDestinationScreener {
    routes: Arc<RouteSet>,
}

impl OracleDestinationScreener {
    /// Screens on the providers of `routes`.
    #[must_use]
    pub const fn new(routes: Arc<RouteSet>) -> Self {
        Self { routes }
    }

    async fn answers(
        &self,
        route: &RouteFile,
        destination: Address,
    ) -> Option<[SanctionsAnswer; 2]> {
        let chain_id = route.chain.chain_id;
        let primary = self.routes.provider(chain_id, 0).ok()?;
        let secondary = self.routes.provider(chain_id, 1).ok()?;
        let block = match primary.finalized_block().await {
            Ok(Some(block)) => block,
            Ok(None) | Err(_) => return None,
        };
        let oracle = SanctionsOracle::new(
            Arc::clone(primary),
            Arc::clone(secondary),
            route.screening.sanctions_oracle,
        )
        .ok()?;
        let result = oracle.sanctions(destination, block).await;
        Some([result.provider_a, result.provider_b])
    }
}

#[async_trait]
impl DestinationScreener for OracleDestinationScreener {
    async fn screen(&self, route: &RouteFile, destination: Address) -> DestinationScreening {
        match self.answers(route, destination).await {
            Some(answers) if answers.contains(&SanctionsAnswer::Sanctioned) => {
                DestinationScreening::Sanctioned
            }
            Some([SanctionsAnswer::Clear, SanctionsAnswer::Clear]) => DestinationScreening::Clear,
            Some(_) | None => DestinationScreening::Unavailable,
        }
    }
}

// Ten minutes is well below the daily treasury re-screen baseline; fund-moving paths stay fresh.
const CLEAR_VERDICT_TTL: Duration = Duration::from_secs(10 * 60);
// Bound memory to 10,000 clear destinations while letting Moka evict entries individually.
const CLEAR_VERDICT_CAPACITY: u64 = 10_000;

/// Caches clear sweepable treasury destinations so interactive API paths do not queue on RPC
/// budgets: a clear destination is reused for 10 minutes, while refund, deposit, and treasury
/// screening paths call [`DestinationScreener::screen`] and remain fresh.
pub struct CachedDestinationScreener {
    inner: Arc<dyn DestinationScreener>,
    entries: Cache<(u64, Address, Address), ()>,
}

impl CachedDestinationScreener {
    /// Wraps `inner` with the production ten-minute clear-verdict cache.
    #[must_use]
    pub fn new(inner: Arc<dyn DestinationScreener>) -> Self {
        Self::with_ttl(inner, CLEAR_VERDICT_TTL)
    }

    /// Wraps `inner` with a cache using `ttl`; intended for short-lived tests as well as the
    /// production constructor.
    #[must_use]
    pub fn with_ttl(inner: Arc<dyn DestinationScreener>, ttl: Duration) -> Self {
        Self {
            inner,
            entries: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(CLEAR_VERDICT_CAPACITY)
                .build(),
        }
    }
}

#[async_trait]
impl DestinationScreener for CachedDestinationScreener {
    async fn screen(&self, route: &RouteFile, destination: Address) -> DestinationScreening {
        let verdict = self.inner.screen(route, destination).await;
        if verdict == DestinationScreening::Sanctioned {
            self.entries
                .invalidate(&(
                    route.chain.chain_id,
                    route.screening.sanctions_oracle,
                    destination,
                ))
                .await;
        }
        verdict
    }

    async fn screen_cached(&self, route: &RouteFile, destination: Address) -> DestinationScreening {
        let key = (
            route.chain.chain_id,
            route.screening.sanctions_oracle,
            destination,
        );
        // Moka shares Err(verdict) with concurrent waiters without caching it; Option would
        // lose the distinction between sanctioned and unavailable screening.
        match self
            .entries
            .try_get_with(key, async {
                match self.inner.screen(route, destination).await {
                    DestinationScreening::Clear => Ok(()),
                    verdict => Err(verdict),
                }
            })
            .await
        {
            Ok(()) => DestinationScreening::Clear,
            Err(verdict) => *verdict,
        }
    }
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
    /// Delay before an attached transaction is read again.
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
        let retry_seconds = i32::try_from(self.config.retry_interval.as_secs()).unwrap_or(i32::MAX);
        let Some(row) = claim_due_refund(&self.pool, retry_seconds).await? else {
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
                return self.not_included(&mut check).await;
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
                tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), "providers disagree on a finalized refund transaction");
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

    /// Neither provider has a receipt: the transaction is pending, dropped, or unknown. It fails
    /// as `transaction_dropped` once its sender's nonce is consumed at `finalized` on both
    /// providers, or as `transaction_not_found` when no provider has returned it for
    /// [`NOT_FOUND_AFTER`] since it was attached; otherwise it waits.
    async fn not_included(&self, check: &mut RefundCheck) -> Result<Verification, sqlx::Error> {
        self.remember_origin(check).await?;
        let Some((from, nonce)) = check.origin else {
            if Utc::now().signed_duration_since(check.paid_at) < NOT_FOUND_AFTER {
                persist_evidence(&self.pool, check, &json!({"result": "not_found"})).await?;
                return Ok(Verification::Waiting);
            }
            let reason = RefundFailure::TransactionNotFound;
            let evidence = json!({
                "result": reason.code(),
                "paid_at": check.paid_at.timestamp(),
            });
            if !fail(&self.pool, &self.routes, check, reason.code(), &evidence).await? {
                return Ok(Verification::Waiting);
            }
            tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), "no provider ever returned the refund transaction");
            return Ok(Verification::Failed);
        };
        let reads = async {
            tokio::join!(
                self.primary.finalized_nonce(check.chain_id, from),
                self.secondary.finalized_nonce(check.chain_id, from)
            )
        };
        let (primary, secondary) = match tokio::time::timeout(self.config.observe_timeout, reads)
            .await
        {
            Ok((Ok(primary), Ok(secondary))) => (primary, secondary),
            Ok((Err(error), _) | (_, Err(error))) => {
                tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), %error, "refund nonce read failed");
                return Ok(Verification::Waiting);
            }
            Err(_) => {
                persist_evidence(&self.pool, check, &json!({"result": "observe_timeout"})).await?;
                return Ok(Verification::Waiting);
            }
        };
        let evidence = |result: &str| {
            json!({
                "result": result,
                "tx_from": format!("{from:#x}"),
                "tx_nonce": nonce,
                "provider_a_finalized_nonce": primary,
                "provider_b_finalized_nonce": secondary,
            })
        };
        if primary <= nonce || secondary <= nonce {
            persist_evidence(&self.pool, check, &evidence("not_included")).await?;
            return Ok(Verification::Waiting);
        }
        let reason = RefundFailure::TransactionDropped;
        if !fail(
            &self.pool,
            &self.routes,
            check,
            reason.code(),
            &evidence(reason.code()),
        )
        .await?
        {
            return Ok(Verification::Waiting);
        }
        tracing::warn!(refund_id = %crate::ids::format(crate::ids::REFUND, check.refund_id), "the refund transaction was dropped and its nonce consumed");
        Ok(Verification::Failed)
    }

    /// Keeps the transaction's sender and nonce the first time a provider returns it, so that a
    /// transaction dropped later can be proven dropped. A failed read leaves it unknown.
    async fn remember_origin(&self, check: &mut RefundCheck) -> Result<(), sqlx::Error> {
        if check.origin.is_some() {
            return Ok(());
        }
        let mut origin = None;
        for reader in [
            &self.primary as &dyn RefundChainReader,
            &self.secondary as &dyn RefundChainReader,
        ] {
            let read = tokio::time::timeout(
                self.config.observe_timeout,
                reader.origin(check.chain_id, check.tx_hash),
            )
            .await;
            if let Ok(Ok(Some(found))) = read {
                origin = Some(found);
                break;
            }
        }
        let Some((from, nonce)) = origin else {
            return Ok(());
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
        Ok(())
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

/// Claims the refund due first and pushes its next check back by `retry_seconds`. The expected
/// sender is the treasury of the deposit's own address, fixed when the address was issued.
async fn claim_due_refund(
    pool: &PgPool,
    retry_seconds: i32,
) -> Result<Option<RefundCheckRow>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    // Lock the deposit first, as mark-paid and the final verification transaction do. A locked
    // or sanctioned deposit is skipped without delaying verification of other deposits.
    let candidate: Option<Uuid> = sqlx::query_scalar(
        "SELECT refund.id FROM refunds AS refund \
         JOIN deposits AS deposit ON deposit.id = refund.deposit_id \
         WHERE refund.status = 'pending' AND refund.tx_hash IS NOT NULL \
           AND refund.next_check_at <= now() AND deposit.sanctions_hit_at IS NULL \
         ORDER BY refund.next_check_at, refund.id \
         FOR UPDATE OF deposit SKIP LOCKED LIMIT 1",
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let claimed = sqlx::query_as::<_, RefundCheckRow>(
        r#"
        UPDATE refunds AS refund
        SET next_check_at = now() + make_interval(secs => $1), updated_at = now()
        FROM deposits AS deposit, addresses AS address
        WHERE refund.id = $2 AND refund.status = 'pending' AND refund.tx_hash IS NOT NULL
          AND refund.next_check_at <= now()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingScreener {
        calls: Arc<AtomicUsize>,
        verdict: DestinationScreening,
    }

    #[async_trait]
    impl DestinationScreener for CountingScreener {
        async fn screen(&self, _route: &RouteFile, _destination: Address) -> DestinationScreening {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.verdict
        }
    }

    fn route() -> RouteFile {
        serde_saphyr::from_str(include_str!("../tests/fixtures/phala-cloud-pha.yaml"))
            .expect("route fixture parses")
    }

    #[tokio::test]
    async fn clear_verdicts_are_cached_within_ttl() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingScreener {
            calls: Arc::clone(&calls),
            verdict: DestinationScreening::Clear,
        });
        let cached = CachedDestinationScreener::with_ttl(inner, Duration::from_secs(600));
        let route = route();
        let destination = Address::repeat_byte(1);

        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn screen_always_delegates_without_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingScreener {
            calls: Arc::clone(&calls),
            verdict: DestinationScreening::Clear,
        });
        let cached = CachedDestinationScreener::new(inner);
        let route = route();
        let destination = Address::repeat_byte(4);

        assert_eq!(
            cached.screen(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(
            cached.screen(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn sanctioned_screen_evicts_cached_clear() {
        struct EvictingScreener {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl DestinationScreener for EvictingScreener {
            async fn screen(
                &self,
                _route: &RouteFile,
                _destination: Address,
            ) -> DestinationScreening {
                let call = self.calls.fetch_add(1, Ordering::Relaxed);
                if call == 0 {
                    DestinationScreening::Clear
                } else {
                    DestinationScreening::Sanctioned
                }
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let cached = CachedDestinationScreener::new(Arc::new(EvictingScreener {
            calls: Arc::clone(&calls),
        }));
        let route = route();
        let destination = Address::repeat_byte(5);

        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(
            cached.screen(&route, destination).await,
            DestinationScreening::Sanctioned
        );
        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Sanctioned
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn sanctioned_and_unavailable_verdicts_are_not_cached() {
        for verdict in [
            DestinationScreening::Sanctioned,
            DestinationScreening::Unavailable,
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let inner = Arc::new(CountingScreener {
                calls: Arc::clone(&calls),
                verdict,
            });
            let cached = CachedDestinationScreener::new(inner);
            let route = route();
            let destination = Address::repeat_byte(2);

            assert_eq!(cached.screen_cached(&route, destination).await, verdict);
            assert_eq!(cached.screen_cached(&route, destination).await, verdict);
            assert_eq!(calls.load(Ordering::Relaxed), 2);
        }
    }

    async fn assert_concurrent_misses(verdict: DestinationScreening) {
        struct GatedScreener {
            calls: Arc<AtomicUsize>,
            release: Arc<tokio::sync::Semaphore>,
            verdict: DestinationScreening,
        }

        #[async_trait]
        impl DestinationScreener for GatedScreener {
            async fn screen(
                &self,
                _route: &RouteFile,
                _destination: Address,
            ) -> DestinationScreening {
                self.calls.fetch_add(1, Ordering::Relaxed);
                let _permit = self.release.acquire().await.expect("gate remains open");
                self.verdict
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let cached = CachedDestinationScreener::new(Arc::new(GatedScreener {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
            verdict,
        }));
        let route = route();
        let destination = Address::repeat_byte(6);
        let mut requests: Vec<_> = (0..8)
            .map(|_| Box::pin(cached.screen_cached(&route, destination)))
            .collect();

        // Poll every request while the initializer is blocked so all are concurrent misses.
        for request in &mut requests {
            assert!(futures_util::poll!(request.as_mut()).is_pending());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        release.add_permits(1);
        let results = tokio::time::timeout(
            Duration::from_secs(1),
            futures_util::future::join_all(requests),
        )
        .await
        .expect("all waiters complete");
        assert_eq!(results, vec![verdict; 8]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        assert_eq!(cached.screen_cached(&route, destination).await, verdict);
        let expected_calls = if verdict == DestinationScreening::Clear {
            1
        } else {
            2
        };
        assert_eq!(calls.load(Ordering::Relaxed), expected_calls);
    }

    #[tokio::test]
    async fn concurrent_clear_misses_screen_once() {
        assert_concurrent_misses(DestinationScreening::Clear).await;
    }

    #[tokio::test]
    async fn concurrent_unavailable_misses_share_verdict_without_caching() {
        assert_concurrent_misses(DestinationScreening::Unavailable).await;
    }

    #[tokio::test]
    async fn concurrent_sanctioned_misses_share_verdict_without_caching() {
        assert_concurrent_misses(DestinationScreening::Sanctioned).await;
    }

    #[tokio::test]
    async fn clear_verdict_expires_after_positive_ttl() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingScreener {
            calls: Arc::clone(&calls),
            verdict: DestinationScreening::Clear,
        });
        let ttl = Duration::from_millis(1);
        let cached = CachedDestinationScreener::with_ttl(inner, ttl);
        let route = route();
        let destination = Address::repeat_byte(7);

        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        // Moka's mock clock is private and its std::time::Instant ignores Tokio's paused clock.
        tokio::time::sleep(ttl).await;
        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn expired_clear_verdicts_are_screened_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingScreener {
            calls: Arc::clone(&calls),
            verdict: DestinationScreening::Clear,
        });
        let cached = CachedDestinationScreener::with_ttl(inner, Duration::ZERO);
        let route = route();
        let destination = Address::repeat_byte(3);

        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(
            cached.screen_cached(&route, destination).await,
            DestinationScreening::Clear
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
