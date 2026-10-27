//! Display-only views of transfers seen before they are recorded as deposits (architecture §12).
//!
//! Support, amount matching, and timeliness are computed here at read time and never stored, so
//! an unfinalized transfer can never produce a stored rejection or credit.

use std::collections::HashMap;

use alloy_primitives::Address as EvmAddress;
use chrono::{DateTime, TimeDelta, Utc};
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use topup_core::valuation::amount_within_tolerance;
use uuid::Uuid;

use crate::db::{self, PendingTransfer};
use crate::locks::{RateLock, RateLockStatus};

use super::error::ApiError;
use super::models::Payment;
use sqlx::PgConnection;

/// Typical Ethereum delay from inclusion to the `finalized` tag: a block in epoch `n` is final
/// once the checkpoint of epoch `n + 1` finalizes, 64 to 95 slots of 12 s (12.8 to 19 minutes).
/// This is an estimate for display; it is not tied to a chain's beacon genesis.
const ESTIMATED_FINALITY_DELAY: TimeDelta = TimeDelta::minutes(15);

/// The payment the quote page shows, following the consumption rule of §9: the deposit that
/// consumed the quote; otherwise the first transfer that would consume it (recorded deposits
/// first, then transfers seen above `finalized` that are not deposits yet); otherwise the first
/// transfer at all. A reversed deposit is no payment. On a canceled quote no payment matches,
/// because every payment is valued at spot.
pub(super) async fn quote_payment(
    connection: &mut PgConnection,
    route: &RouteFile,
    lock: &RateLock,
) -> Result<Option<Payment>, ApiError> {
    let payments = QuotePayments::load(connection, &[lock.id]).await?;
    Ok(payments.payment(route, lock))
}

/// Two reads for the selected page, independent of its quote count. Address ids come only from
/// scoped quote lookups, retaining the account and mode boundary of their parent query.
pub(super) struct QuotePayments {
    deposits: HashMap<Uuid, Vec<Observed>>,
    pending: HashMap<Uuid, Vec<Observed>>,
}

impl QuotePayments {
    pub(super) async fn load(
        connection: &mut PgConnection,
        quote_ids: &[Uuid],
    ) -> Result<Self, ApiError> {
        let deposits = address_deposits(&mut *connection, quote_ids).await?;
        let mut pending: HashMap<Uuid, Vec<Observed>> = HashMap::new();
        for (address_id, transfer) in db::list_quotes_pending(connection, quote_ids).await? {
            pending
                .entry(address_id)
                .or_default()
                .push(Observed::from(transfer));
        }
        Ok(Self { deposits, pending })
    }

    pub(super) fn payment(&self, route: &RouteFile, lock: &RateLock) -> Option<Payment> {
        let deposits = self
            .deposits
            .get(&lock.address_id)
            .map_or(&[][..], Vec::as_slice);
        if let Some(consumed) = deposits
            .iter()
            .find(|deposit| !deposit.reversed && Some(deposit.deposit_id) == lock.consumed_by)
        {
            return Some(payment(route, lock, consumed));
        }
        let pending = self
            .pending
            .get(&lock.address_id)
            .map_or(&[][..], Vec::as_slice);
        let mut observed = deposits
            .iter()
            .filter(|deposit| {
                !deposit.reversed
                    && deposit.asset_contract == route.asset.contract
                    && deposit.block_time > lock.window_start
                    && deposit.block_time <= lock.expires_at
            })
            .chain(pending.iter().filter(|transfer| {
                transfer.asset_contract == route.asset.contract
                    && transfer.block_time > lock.window_start
                    && transfer.block_time <= lock.expires_at
                    && !deposits
                        .iter()
                        .any(|deposit| deposit.deposit_id == transfer.deposit_id)
            }));
        let first = observed.next()?;
        let shown = std::iter::once(first)
            .chain(observed)
            .find(|candidate| terms(route, lock, candidate).consumes_lock())
            .unwrap_or(first);
        Some(payment(route, lock, shown))
    }
}

struct Observed {
    status: &'static str,
    reversed: bool,
    deposit_id: Uuid,
    tx_hash: alloy_primitives::B256,
    block_time: DateTime<Utc>,
    confirmations: Option<u64>,
    asset_contract: EvmAddress,
    amount_atomic: AtomicAmount,
    chain_id: u64,
}

impl From<PendingTransfer> for Observed {
    fn from(transfer: PendingTransfer) -> Self {
        Self {
            status: "seen",
            reversed: false,
            confirmations: Some(transfer.confirmations()),
            deposit_id: transfer.deposit_id,
            tx_hash: transfer.tx_hash,
            block_time: transfer.block_time,
            asset_contract: transfer.asset_contract,
            amount_atomic: transfer.amount_atomic,
            chain_id: transfer.chain_id,
        }
    }
}

struct Terms {
    supported: bool,
    in_time: bool,
    amount_within_tolerance: bool,
}

impl Terms {
    fn consumes_lock(&self) -> bool {
        self.supported && self.in_time && self.amount_within_tolerance
    }
}

fn terms(route: &RouteFile, lock: &RateLock, observed: &Observed) -> Terms {
    let open = lock.status != RateLockStatus::Cancelled;
    let supported = observed.chain_id == route.chain.chain_id
        && observed.asset_contract == route.asset.contract;
    Terms {
        supported,
        in_time: open
            && observed.block_time > lock.window_start
            && observed.block_time <= lock.expires_at,
        amount_within_tolerance: open
            && supported
            && amount_within_tolerance(
                observed.amount_atomic,
                lock.amount_atomic,
                lock.terms.quote_tolerance_bps,
            ),
    }
}

fn payment(route: &RouteFile, lock: &RateLock, observed: &Observed) -> Payment {
    Payment {
        status: observed.status.to_owned(),
        chain_id: observed.chain_id,
        asset: (observed.chain_id == route.chain.chain_id
            && observed.asset_contract == route.asset.contract)
            .then(|| route.asset.symbol.clone()),
        tx_hash: format!("{:#x}", observed.tx_hash),
        amount_atomic: observed.amount_atomic.value().to_string(),
        confirmations: observed.confirmations,
        estimated_final_at: (observed.status == "seen")
            .then(|| estimated_final_at(observed.block_time).timestamp()),
        matches_quote: Some(terms(route, lock, observed).consumes_lock()),
        deposit: crate::ids::format(crate::ids::DEPOSIT, observed.deposit_id),
    }
}

type DepositRow = (
    Uuid,
    Uuid,
    i64,
    String,
    String,
    DateTime<Utc>,
    String,
    String,
);

async fn address_deposits(
    connection: &mut PgConnection,
    quote_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Observed>>, ApiError> {
    if quote_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, DepositRow>(
        r#"
        SELECT address_id, id, chain_id, tx_hash, state, block_time, asset_contract, amount_atomic::text
        FROM deposits
        WHERE EXISTS (SELECT 1 FROM quotes q WHERE q.id=ANY($1) AND q.address_id=deposits.address_id
          AND ((deposits.block_time > q.window_start AND deposits.block_time <= q.expires_at)
               OR q.consumed_by=deposits.id))
        ORDER BY address_id, block_number, log_index, id
        "#,
    )
    .bind(quote_ids)
    .fetch_all(connection)
    .await?;
    rows.into_iter()
        .map(
            |(address_id, id, chain_id, tx_hash, state, block_time, asset, amount)| {
                let chain_id = u64::try_from(chain_id).ok()?;
                let tx_hash = tx_hash.parse().ok()?;
                Some((
                    address_id,
                    Observed {
                        status: "recorded",
                        reversed: state == "reversed",
                        deposit_id: id,
                        tx_hash,
                        block_time,
                        confirmations: None,
                        asset_contract: asset.parse().ok()?,
                        amount_atomic: AtomicAmount::new(amount.parse().ok()?),
                        chain_id,
                    },
                ))
            },
        )
        .collect::<Option<Vec<_>>>()
        .ok_or_else(ApiError::internal)
        .map(|rows| {
            let mut deposits: HashMap<Uuid, Vec<Observed>> = HashMap::new();
            for (address_id, row) in rows {
                deposits.entry(address_id).or_default().push(row);
            }
            deposits
        })
}

pub(super) fn estimated_final_at(block_time: DateTime<Utc>) -> DateTime<Utc> {
    block_time
        .checked_add_signed(ESTIMATED_FINALITY_DELAY)
        .unwrap_or(block_time)
}
