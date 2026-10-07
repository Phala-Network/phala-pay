//! Restore mode (docs/architecture.md §14, docs/design/multi-tenant.md §13).
//!
//! A database restored from backup holds the backup's state, not the business's: within the RPO
//! window it can have lost deposit addresses given to customers, API key revocations (the keys
//! work again), treasury cancellations, webhook endpoint changes, and events the merchant already
//! received. So the service starts **frozen** after a restore: merchant writes answer
//! `503 service_restoring`, and nothing credits, settles, expires a quote, applies a treasury
//! change, verifies a refund, or delivers an event, while reads, health, the scanner (the rescan
//! from the restored cursor), and the reconciler run. The operator reconciles through
//! `/v1/admin/restore/…` and unfreezes (`deploy/RESTORE.md`).
//!
//! A restore is known two ways: `restore-check`, which runs only in the restore-check variant
//! after a restore on boot, records it ([`freeze_after_restore`]); and `topup run` finds a
//! PostgreSQL timeline newer than the one acknowledged ([`detect`]), since every promotion out of
//! archive recovery starts a new timeline and crash recovery does not. Either way the freeze is a
//! row of `restores`, so it survives the upgrade from the restore-check variant to the service.

use std::collections::BTreeMap;
use std::str::FromStr as _;
use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{FromRow, PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use topup_core::money::{AtomicAmount, MinorAmount, PRICE_SCALE, ScaledPrice};
use topup_core::valuation::ValuationSource;
use uuid::Uuid;

use crate::audit::{self, Actor};
use crate::routes::RouteSet;

/// The current WAL insert timeline, from the name of the current WAL file.
const CURRENT_TIMELINE: &str =
    "('x' || left(pg_walfile_name(pg_current_wal_insert_lsn()), 8))::bit(32)::bigint";

/// How often a held service task checks whether the freeze was lifted.
pub const UNFREEZE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The deposit events a rescan re-derives, whose object is the deposit and whose id is
/// `event_id(type, deposit)`: the only events [`import_delivered_event`] accepts.
pub const REDERIVED_EVENT_TYPES: [&str; 3] =
    ["deposit.credited", "deposit.rejected", "deposit.reversed"];

/// How a restore was detected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Detection {
    /// `restore-check` ran after a restore on boot of the restore-check variant.
    RestoreCheck,
    /// `topup run` found a PostgreSQL timeline newer than the acknowledged one.
    Timeline,
}

impl Detection {
    /// The stored code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::RestoreCheck => "restore_check",
            Self::Timeline => "timeline",
        }
    }
}

/// A detected restore; frozen until `unfrozen_at` is set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Restore {
    /// Restore id.
    pub id: Uuid,
    /// When the service found the restore and froze.
    pub detected_at: DateTime<Utc>,
    /// `restore_check` or `timeline`.
    pub detected_by: String,
    /// The PostgreSQL timeline the restore promoted to.
    pub timeline_id: i64,
    /// Newest heartbeat in the restored database: changes after it may be lost.
    pub restore_point: Option<DateTime<Utc>>,
    /// Each chain's scanned block when the restore was detected, where the rescan starts.
    pub restored_cursors: BTreeMap<u64, u64>,
    /// When the operator unfroze the service.
    pub unfrozen_at: Option<DateTime<Utc>>,
    /// Who unfroze it.
    pub unfrozen_by: Option<String>,
    /// Why, with the operator's checklist.
    pub unfreeze_reason: Option<String>,
}

#[derive(FromRow)]
struct RestoreRow {
    id: Uuid,
    detected_at: DateTime<Utc>,
    detected_by: String,
    timeline_id: i64,
    restore_point: Option<DateTime<Utc>>,
    restored_cursors: Json<BTreeMap<String, i64>>,
    unfrozen_at: Option<DateTime<Utc>>,
    unfrozen_by: Option<String>,
    unfreeze_reason: Option<String>,
}

impl TryFrom<RestoreRow> for Restore {
    type Error = sqlx::Error;

    fn try_from(row: RestoreRow) -> Result<Self, Self::Error> {
        let invalid = || sqlx::Error::Decode("restores.restored_cursors is invalid".into());
        let restored_cursors = row
            .restored_cursors
            .0
            .into_iter()
            .map(|(chain_id, block)| {
                Ok((
                    chain_id.parse::<u64>().map_err(|_| invalid())?,
                    u64::try_from(block).map_err(|_| invalid())?,
                ))
            })
            .collect::<Result<_, sqlx::Error>>()?;
        Ok(Self {
            id: row.id,
            detected_at: row.detected_at,
            detected_by: row.detected_by,
            timeline_id: row.timeline_id,
            restore_point: row.restore_point,
            restored_cursors,
            unfrozen_at: row.unfrozen_at,
            unfrozen_by: row.unfrozen_by,
            unfreeze_reason: row.unfreeze_reason,
        })
    }
}

const RESTORE_COLUMNS: &str = "id, detected_at, detected_by, timeline_id, restore_point, \
     restored_cursors, unfrozen_at, unfrozen_by, unfreeze_reason";

/// Records the restore `restore-check` runs after, freezing the service unless a freeze is
/// already in place, and acknowledges the current timeline.
pub async fn freeze_after_restore(pool: &PgPool) -> Result<Restore, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let (_, current) = lock_timeline(&mut transaction).await?;
    let restore = freeze_in(&mut transaction, Detection::RestoreCheck, current).await?;
    // Invalidate prior acceptance under the same restore lock, before any validation starts.
    // A concurrent unfreeze must see either the old completed check or this pending check.
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: None,
            actor: &Actor::system("restore-check"),
            action: "restore.validation",
            subject: &format!("restore:{}", restore.id),
            reason: r#"{"status":"incomplete","failures":["validation running or interrupted"]}"#,
        },
    )
    .await?;
    acknowledge(&mut transaction, current).await?;
    transaction.commit().await?;
    Ok(restore)
}

/// Freezes the service when PostgreSQL runs on a timeline newer than the acknowledged one (a
/// restore that booted straight into the service), and returns the active freeze, if any.
pub async fn detect(pool: &PgPool) -> Result<Option<Restore>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let (acknowledged, current) = lock_timeline(&mut transaction).await?;
    if current > acknowledged {
        freeze_in(&mut transaction, Detection::Timeline, current).await?;
        acknowledge(&mut transaction, current).await?;
    }
    let active = active_in(&mut transaction).await?;
    transaction.commit().await?;
    Ok(active)
}

/// The acknowledged and the current timeline, with the acknowledged one locked.
async fn lock_timeline(connection: &mut PgConnection) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT timeline_id, {CURRENT_TIMELINE} FROM restore_timeline FOR UPDATE"
    )))
    .fetch_one(connection)
    .await
}

async fn acknowledge(connection: &mut PgConnection, timeline: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE restore_timeline SET timeline_id = GREATEST(timeline_id, $1)")
        .bind(timeline)
        .execute(connection)
        .await?;
    Ok(())
}

/// The active freeze, or a new one recording the restore point and each chain's cursor.
async fn freeze_in(
    connection: &mut PgConnection,
    detection: Detection,
    timeline: i64,
) -> Result<Restore, sqlx::Error> {
    if let Some(active) = active_in(connection).await? {
        return Ok(active);
    }
    let restore: Restore = sqlx::query_as::<_, RestoreRow>(sqlx::AssertSqlSafe(format!(
        r#"
        INSERT INTO restores (id, detected_by, timeline_id, restore_point, restored_cursors)
        SELECT $1, $2, $3,
               (SELECT max(recorded_at) FROM heartbeat),
               COALESCE((SELECT jsonb_object_agg(chain_id::text, scanned_block) FROM cursors),
                        '{{}}'::jsonb)
        RETURNING {RESTORE_COLUMNS}
        "#
    )))
    .bind(Uuid::new_v4())
    .bind(detection.code())
    .bind(timeline)
    .fetch_one(&mut *connection)
    .await?
    .try_into()?;
    // The restored payment settings may be stale, and no delivery proves the merchant's latest:
    // every account and mode is held until the merchant reconfirms, in the transaction that
    // records the restore, before any recorder runs (docs/design/payment-settings.md §11).
    crate::payment_config::hold_all(&mut *connection, restore.id).await?;
    tracing::error!(
        restore_id = %restore.id,
        detected_by = detection.code(),
        timeline,
        restore_point = ?restore.restore_point,
        "database restored from backup: the service is frozen until an operator reconciles and \
         unfreezes it (deploy/RESTORE.md)"
    );
    Ok(restore)
}

async fn active_in(connection: &mut PgConnection) -> Result<Option<Restore>, sqlx::Error> {
    sqlx::query_as::<_, RestoreRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RESTORE_COLUMNS} FROM restores WHERE unfrozen_at IS NULL FOR UPDATE"
    )))
    .fetch_optional(connection)
    .await?
    .map(Restore::try_from)
    .transpose()
}

/// The active freeze, if any.
pub async fn active(pool: &PgPool) -> Result<Option<Restore>, sqlx::Error> {
    latest_matching(pool, "WHERE unfrozen_at IS NULL").await
}

/// The most recent restore, frozen or not.
pub async fn latest(pool: &PgPool) -> Result<Option<Restore>, sqlx::Error> {
    latest_matching(pool, "").await
}

async fn latest_matching(
    pool: &PgPool,
    filter: &'static str,
) -> Result<Option<Restore>, sqlx::Error> {
    sqlx::query_as::<_, RestoreRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RESTORE_COLUMNS} FROM restores {filter} ORDER BY detected_at DESC, id LIMIT 1"
    )))
    .fetch_optional(pool)
    .await?
    .map(Restore::try_from)
    .transpose()
}

/// Whether the service is frozen after a restore.
pub async fn is_frozen(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM restores WHERE unfrozen_at IS NULL)")
        .fetch_one(pool)
        .await
}

/// Waits until the service is not frozen; `false` when `cancellation` fired first. A failed check
/// is logged and retried: a task held here starts only once a check shows no freeze.
pub async fn wait_until_unfrozen(
    pool: &PgPool,
    poll: Duration,
    cancellation: &CancellationToken,
) -> bool {
    let mut announced = false;
    loop {
        match is_frozen(pool).await {
            Ok(false) => return true,
            Ok(true) if !announced => {
                announced = true;
                tracing::info!("held while the service is frozen after a restore");
            }
            Ok(true) => {}
            Err(error) => tracing::warn!(%error, "failed to read the restore freeze"),
        }
        tokio::select! {
            () = cancellation.cancelled() => return false,
            () = tokio::time::sleep(poll) => {}
        }
    }
}

/// The rescan of one chain since the restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainRescan {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The chain's scanned block when the restore was detected.
    pub restored_block: Option<u64>,
    /// The chain's scanned block now.
    pub scanned_block: Option<u64>,
    /// Block time of the finalized head the scanner last committed through.
    pub scanned_block_time: Option<DateTime<Utc>>,
    /// Issued addresses of the chain whose history the scanner has not read yet, such as
    /// re-issued deposit addresses.
    pub pending_backfills: i64,
    /// Whether a reconciliation block freezes the chain: its scanner is paused, so it is left out
    /// of the rescan, and its crediting stays stopped until the block is lifted.
    pub blocked: bool,
    /// Whether the chain is rescanned: finalized past the moment the restore was detected, and
    /// every issued address backfilled.
    pub complete: bool,
}

/// The rescan since `restore` on every configured chain.
pub async fn rescan_progress(
    pool: &PgPool,
    restore: &Restore,
    routes: &RouteSet,
) -> Result<Vec<ChainRescan>, sqlx::Error> {
    let mut chains = Vec::new();
    for chain_id in routes.chain_ids() {
        let key = i64::try_from(chain_id)
            .map_err(|error| sqlx::Error::Encode(error.to_string().into()))?;
        let (scanned_block, scanned_block_time, pending_backfills): (
            Option<i64>,
            Option<DateTime<Utc>>,
            i64,
        ) = sqlx::query_as(
            r#"
            SELECT (SELECT through_block FROM chain_coverage WHERE chain_id = $1),
                   (SELECT through_time FROM chain_coverage WHERE chain_id = $1),
                   (SELECT count(*) FROM addresses a LEFT JOIN chain_coverage c ON c.chain_id=a.chain_id
                    WHERE a.chain_id = $1 AND (a.dual_covered_through IS NULL
                        OR a.dual_covered_through IS DISTINCT FROM c.through_block))
            "#,
        )
        .bind(key)
        .fetch_one(pool)
        .await?;
        let blocked = crate::reconciler::chain_is_blocked(pool, chain_id).await?;
        let caught_up = scanned_block_time.is_some_and(|time| time >= restore.detected_at);
        chains.push(ChainRescan {
            chain_id,
            restored_block: restore.restored_cursors.get(&chain_id).copied(),
            scanned_block: scanned_block.and_then(|block| u64::try_from(block).ok()),
            scanned_block_time,
            pending_backfills,
            blocked,
            complete: blocked || (caught_up && pending_backfills == 0),
        });
    }
    Ok(chains)
}

/// Why the service cannot be unfrozen.
#[derive(Debug, thiserror::Error)]
pub enum UnfreezeError {
    /// No restore freeze is active.
    #[error("the service is not frozen")]
    NotFrozen,
    /// A chain is not rescanned yet, or restore acceptance checks have not passed.
    #[error("the rescan or acceptance checks since the restore are incomplete")]
    RescanIncomplete(Vec<ChainRescan>),
    /// PostgreSQL rejected or failed the operation.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

/// Persists acceptance evidence in the existing append-only audit history.
pub async fn record_validation(
    pool: &PgPool,
    restore: &Restore,
    status: &str,
    failures: &[String],
) -> Result<(), sqlx::Error> {
    audit::insert(
        pool,
        &audit::Entry {
            account_id: None,
            actor: &Actor::system("restore-check"),
            action: "restore.validation",
            subject: &format!("restore:{}", restore.id),
            reason: &serde_json::json!({"status": status, "failures": failures}).to_string(),
        },
    )
    .await
}

// Explicit admin-only escape hatch through the existing reason field. Ordinary reasons cannot
// accidentally override failed checks. The API appends its checklist after the supplied reason.
fn override_reason<'a>(actor: &Actor, reason: &'a str) -> Option<&'a str> {
    if actor.actor_type != audit::ActorType::Admin {
        return None;
    }
    let supplied = reason.split("; checklist:").next()?.trim();
    supplied
        .strip_prefix("override-critical-checks:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Lifts the active freeze after rescan and acceptance (or explicit admin override), recording who and why in the restore
/// and in `audit` in one transaction.
pub async fn unfreeze(
    pool: &PgPool,
    routes: &RouteSet,
    actor: &Actor,
    reason: &str,
) -> Result<Restore, UnfreezeError> {
    let mut transaction = pool.begin().await?;
    let restore = active_in(&mut transaction)
        .await?
        .ok_or(UnfreezeError::NotFrozen)?;
    let chains = rescan_progress(pool, &restore, routes).await?;
    if chains.iter().any(|chain| !chain.complete) {
        return Err(UnfreezeError::RescanIncomplete(chains));
    }
    let accepted: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT reason::jsonb ->> 'status' = 'ok' FROM audit \
         WHERE action = 'restore.validation' AND subject = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1), false)",
    )
    .bind(format!("restore:{}", restore.id))
    .fetch_one(&mut *transaction)
    .await?;
    if !accepted {
        let Some(reason) = override_reason(actor, reason) else {
            // The existing API incomplete-recovery contract also gates acceptance checks.
            return Err(UnfreezeError::RescanIncomplete(Vec::new()));
        };
        audit::insert(
            &mut *transaction,
            &audit::Entry {
                account_id: None,
                actor,
                action: "restore.critical_checks_override",
                subject: &format!("restore:{}", restore.id),
                reason,
            },
        )
        .await?;
    }
    let unfrozen: Restore = sqlx::query_as::<_, RestoreRow>(sqlx::AssertSqlSafe(format!(
        "UPDATE restores SET unfrozen_at = now(), unfrozen_by = $2, unfreeze_reason = $3 \
         WHERE id = $1 RETURNING {RESTORE_COLUMNS}"
    )))
    .bind(restore.id)
    .bind(actor.to_string())
    .bind(reason)
    .fetch_one(&mut *transaction)
    .await?
    .try_into()?;
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: None,
            actor,
            action: "restore.unfreeze",
            subject: &format!("restore:{}", restore.id),
            reason,
        },
    )
    .await?;
    transaction.commit().await?;
    tracing::warn!(restore_id = %restore.id, "restore freeze lifted by the operator");
    Ok(unfrozen)
}

/// An event as the merchant received it, with its identity checked by the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct DeliveredEvent {
    /// Event id, `event_id(type, deposit)`.
    pub id: Uuid,
    /// The event's account.
    pub account_id: Uuid,
    /// The event's mode.
    pub livemode: bool,
    /// One of [`REDERIVED_EVENT_TYPES`].
    pub event_type: String,
    /// The deposit the event is about.
    pub deposit_id: Uuid,
    /// The event's `created`.
    pub created: DateTime<Utc>,
    /// The event's `actor`.
    pub actor: String,
    /// The event's `data` exactly as delivered.
    pub data: Value,
    /// The credit its deposit snapshot carries: always for `deposit.credited`, and for a
    /// `deposit.reversed` of a deposit that was valued (a rejected deposit may never have been).
    pub credit: Option<DeliveredCredit>,
    /// The deposit's identity and chain evidence.
    pub identity: DeliveredIdentity,
}

/// A deposit's identity and chain evidence as its delivered snapshot shows them: enough to restore
/// a reversed deposit the restore lost ([`import_delivered_event`]). The caller checked that
/// `deposit_revision_id(chain_id, tx_hash, receipt_log_index, revision)` is the deposit's id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveredIdentity {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transfer transaction hash.
    pub tx_hash: B256,
    /// Position of the transfer among its transaction's receipt logs.
    pub receipt_log_index: u64,
    /// Deposits reversed at the position before this one.
    pub revision: u64,
    /// Block-wide transfer log index.
    pub log_index: u64,
    /// Including block number.
    pub block_number: u64,
    /// Including block hash.
    pub block_hash: B256,
    /// Chain block time.
    pub block_time: DateTime<Utc>,
    /// The receiving forwarder.
    pub address: Address,
    /// Token contract.
    pub asset_contract: Address,
    /// Transfer sender.
    pub from_address: Address,
    /// Token amount.
    pub amount_atomic: AtomicAmount,
    /// The reversed deposit this one replaced.
    pub replaces: Option<Uuid>,
    /// The deposit that replaced this one.
    pub replaced_by: Option<Uuid>,
    /// The deposit's `created`.
    pub created: DateTime<Utc>,
    /// The deposit's `metadata`.
    pub metadata: Value,
}

/// The credit a delivered `deposit.credited` or `deposit.reversed` told the merchant, and the
/// transfer it was for. A settled amount is immutable: the confirm step values the deposit the
/// rescan re-derives at it, unless the chain's transfer contradicts it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveredCredit {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transfer transaction hash.
    pub tx_hash: B256,
    /// The receiving forwarder.
    pub address: Address,
    /// Token contract.
    pub asset_contract: Address,
    /// Transfer sender.
    pub from_address: Address,
    /// Token amount.
    pub amount_atomic: AtomicAmount,
    /// The valuation price, `exchange_rate`.
    pub price: ScaledPrice,
    /// `spot`, or `lock` (delivered as `quote`).
    pub source: ValuationSource,
    /// The credit, `amount`.
    pub credit_minor: MinorAmount,
    /// `valued_at`.
    pub valuation_at: DateTime<Utc>,
}

/// A delivered credit imported for a deposit and not discarded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedCredit {
    /// The imported event that carried it.
    pub event_id: Uuid,
    /// The event's account.
    pub account_id: Uuid,
    /// The event's mode.
    pub livemode: bool,
    /// The credit.
    pub credit: DeliveredCredit,
}

impl ImportedCredit {
    /// The first field in which the delivered transfer differs from the chain's
    /// ([`DeliveredTransfer::contradiction`]).
    #[must_use]
    pub fn contradiction(
        &self,
        deposit: &crate::db::Deposit,
        transfer: &topup_adapters::chain::evm::TransferLog,
    ) -> Option<&'static str> {
        let credit = &self.credit;
        DeliveredTransfer {
            account_id: self.account_id,
            livemode: self.livemode,
            chain_id: credit.chain_id,
            tx_hash: credit.tx_hash,
            address: credit.address,
            asset_contract: credit.asset_contract,
            from_address: credit.from_address,
            amount_atomic: credit.amount_atomic,
        }
        .contradiction(deposit, transfer)
    }
}

/// The transfer a delivered deposit event names: what the chain must show for the outcome the
/// merchant was told to stand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveredTransfer {
    /// The event's account.
    pub account_id: Uuid,
    /// The event's mode.
    pub livemode: bool,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transfer transaction hash.
    pub tx_hash: B256,
    /// The receiving forwarder.
    pub address: Address,
    /// Token contract.
    pub asset_contract: Address,
    /// Transfer sender.
    pub from_address: Address,
    /// Token amount.
    pub amount_atomic: AtomicAmount,
}

impl DeliveredTransfer {
    /// The first field in which the delivered transfer differs from the chain's: the deposit's
    /// account, mode, chain, and transaction, and the canonical transfer's recipient, token,
    /// sender, and amount. The deposit's id, which the event's derives from, fixes its receipt
    /// position.
    #[must_use]
    pub fn contradiction(
        &self,
        deposit: &crate::db::Deposit,
        transfer: &topup_adapters::chain::evm::TransferLog,
    ) -> Option<&'static str> {
        [
            ("account", self.account_id == deposit.account_id),
            ("livemode", self.livemode == deposit.livemode),
            ("chain_id", self.chain_id == deposit.chain_id),
            ("tx_hash", self.tx_hash == deposit.tx_hash),
            ("address", self.address == transfer.to),
            ("asset_contract", self.asset_contract == transfer.token),
            ("from_address", self.from_address == transfer.from),
            ("amount_atomic", self.amount_atomic == transfer.amount),
        ]
        .into_iter()
        .find_map(|(field, same)| (!same).then_some(field))
    }
}

#[derive(FromRow)]
struct ImportedCreditRow {
    event_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    chain_id: i64,
    tx_hash: String,
    address: String,
    asset_contract: String,
    from_address: String,
    amount_atomic: String,
    price_scaled: String,
    price_source: String,
    credit_minor: String,
    valuation_at: DateTime<Utc>,
}

impl TryFrom<ImportedCreditRow> for ImportedCredit {
    type Error = sqlx::Error;

    fn try_from(row: ImportedCreditRow) -> Result<Self, Self::Error> {
        let invalid = || sqlx::Error::Decode("restore_delivered_credits is invalid".into());
        let address = |text: &str| Address::from_str(text).map_err(|_| invalid());
        Ok(Self {
            event_id: row.event_id,
            account_id: row.account_id,
            livemode: row.livemode,
            credit: DeliveredCredit {
                chain_id: u64::try_from(row.chain_id).map_err(|_| invalid())?,
                tx_hash: B256::from_str(&row.tx_hash).map_err(|_| invalid())?,
                address: address(&row.address)?,
                asset_contract: address(&row.asset_contract)?,
                from_address: address(&row.from_address)?,
                amount_atomic: AtomicAmount::new(
                    U256::from_str(&row.amount_atomic).map_err(|_| invalid())?,
                ),
                price: row
                    .price_scaled
                    .parse::<u64>()
                    .ok()
                    .and_then(|price| ScaledPrice::new(price, PRICE_SCALE).ok())
                    .ok_or_else(invalid)?,
                source: match row.price_source.as_str() {
                    "spot" => ValuationSource::Spot,
                    "lock" => ValuationSource::Lock,
                    _ => return Err(invalid()),
                },
                credit_minor: MinorAmount::new(
                    row.credit_minor.parse::<u64>().map_err(|_| invalid())?,
                ),
                valuation_at: row.valuation_at,
            },
        })
    }
}

/// The delivered credit imported for `deposit_id` and not discarded, which the confirm step values
/// the deposit at.
pub async fn imported_credit<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    deposit_id: Uuid,
) -> Result<Option<ImportedCredit>, sqlx::Error> {
    sqlx::query_as::<_, ImportedCreditRow>(
        r#"
        SELECT event_id, account_id, livemode, chain_id, tx_hash, address, asset_contract,
               from_address, amount_atomic::text AS amount_atomic,
               price_scaled::text AS price_scaled, price_source,
               credit_minor::text AS credit_minor, valuation_at
        FROM restore_delivered_credits
        WHERE deposit_id = $1 AND discarded_at IS NULL
        "#,
    )
    .bind(deposit_id)
    .fetch_optional(executor)
    .await?
    .map(ImportedCredit::try_from)
    .transpose()
}

/// What importing a delivered `deposit.reversed` did to its deposit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReversedDeposit {
    /// The reversed deposit was restored from the delivery.
    Restored,
    /// The ledger holds the deposit already.
    Recorded,
    /// No issued address of the event's account and mode is the deposit's: re-issue it, then
    /// import the event again.
    AddressUnknown,
    /// The rescan recorded the deposit's receipt position first, with a deposit that is not
    /// reversed at its revision or below (under the reversed deposit's id, or an earlier one): the
    /// reversed deposit is not restored over it. A finding `rescanned` until the operator settles
    /// it.
    Rescanned,
}

impl ReversedDeposit {
    /// The API code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Restored => "restored",
            Self::Recorded => "recorded",
            Self::AddressUnknown => "address_unknown",
            Self::Rescanned => "rescanned",
        }
    }
}

/// What importing a delivered event did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportOutcome {
    /// The snapshot was stored as the event, with no delivery.
    Imported,
    /// The event was recorded already with the same `data`.
    Matches,
    /// The event was recorded already with other `data`; the stored snapshot is kept.
    Mismatch,
}

impl ImportOutcome {
    /// The API code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Imported => "imported",
            Self::Matches => "matches",
            Self::Mismatch => "mismatch",
        }
    }
}

/// Stores `event`, delivered to the merchant after the restore point and so lost, as the event it
/// is: a rescan that re-derives its deposit then finds the event recorded and emits nothing, so
/// the merchant never receives it again with another body. Its credit, if any, is kept for the
/// deposit ([`imported_credit`]). An event already recorded keeps its stored snapshot; a different
/// delivered body is a mismatch, logged for the operator. The caller verified that the service
/// signed the delivery.
///
/// A `deposit.reversed` also restores its deposit, reversed, when the ledger lacks it
/// ([`ReversedDeposit`]): the deposit takes back its revision at its receipt position, so the
/// rescan records the transfer now at the position as the deposit that replaced it, with that
/// deposit's id and link, instead of under the reversed deposit's id. `routes` name the deposit's
/// route by its token.
pub async fn import_delivered_event(
    pool: &PgPool,
    routes: &RouteSet,
    restore: &Restore,
    event: &DeliveredEvent,
    actor: &Actor,
) -> Result<(ImportOutcome, Option<ReversedDeposit>), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO events (id, account_id, livemode, type, object_type, object_id, actor, data,
                            created)
        VALUES ($1, $2, $3, $4, 'deposit', $5, $6, $7, $8)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(event.id)
    .bind(event.account_id)
    .bind(event.livemode)
    .bind(&event.event_type)
    .bind(event.deposit_id)
    .bind(&event.actor)
    .bind(&event.data)
    .bind(event.created)
    .execute(&mut *transaction)
    .await?
    .rows_affected()
        > 0;
    let outcome = if inserted {
        sqlx::query("INSERT INTO restore_delivered_events (event_id, restore_id) VALUES ($1, $2)")
            .bind(event.id)
            .bind(restore.id)
            .execute(&mut *transaction)
            .await?;
        if let Some(credit) = &event.credit {
            insert_credit(&mut transaction, restore, event, credit).await?;
        }
        ImportOutcome::Imported
    } else {
        let stored: Value = sqlx::query_scalar("SELECT data FROM events WHERE id = $1")
            .bind(event.id)
            .fetch_one(&mut *transaction)
            .await?;
        if stored == event.data {
            ImportOutcome::Matches
        } else {
            tracing::error!(
                event_id = %crate::ids::format(crate::ids::EVENT, event.id),
                restore_id = %restore.id,
                "a delivered event differs from the recorded one; the recorded snapshot is kept"
            );
            ImportOutcome::Mismatch
        }
    };
    let reversed = if event.event_type == "deposit.reversed" {
        Some(restore_reversed(&mut transaction, routes, restore, event, &event.identity).await?)
    } else {
        None
    };
    let reason = match reversed {
        Some(reversed) => format!(
            "restore {}: {}; deposit {}",
            restore.id,
            outcome.code(),
            reversed.code()
        ),
        None => format!("restore {}: {}", restore.id, outcome.code()),
    };
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(event.account_id),
            actor,
            action: "restore.import_event",
            subject: &format!("event:{}", crate::ids::format(crate::ids::EVENT, event.id)),
            reason: &reason,
        },
    )
    .await?;
    transaction.commit().await?;
    Ok((outcome, reversed))
}

/// Restores the reversed deposit of a delivered `deposit.reversed` the ledger lacks, as the
/// delivery shows it: at its receipt position and revision, on its issued address, valued at its
/// delivered credit if it had one, and linked to the deposit it replaced and the one that replaced
/// it, whichever of them is recorded first ([`crate::db::insert_deposit_in`] links a successor
/// recorded later).
async fn restore_reversed(
    connection: &mut PgConnection,
    routes: &RouteSet,
    restore: &Restore,
    event: &DeliveredEvent,
    identity: &DeliveredIdentity,
) -> Result<ReversedDeposit, sqlx::Error> {
    if let Some(held) = position_held(connection, restore, event, identity).await? {
        return Ok(held);
    }
    let encode = |error: std::num::TryFromIntError| sqlx::Error::Encode(Box::new(error));
    let chain_id = i64::try_from(identity.chain_id).map_err(encode)?;
    let issued: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM addresses \
         WHERE chain_id = $1 AND address = $2 AND account_id = $3 AND livemode = $4)",
    )
    .bind(chain_id)
    .bind(format!("{:#x}", identity.address))
    .bind(event.account_id)
    .bind(event.livemode)
    .fetch_one(&mut *connection)
    .await?;
    if !issued {
        return Ok(ReversedDeposit::AddressUnknown);
    }
    // The barrier every recorder takes (crate::payment_config): the insert below, a later
    // statement, binds the deposit to the payment settings its own snapshot reads.
    crate::payment_config::lock_scope_for_recording(
        &mut *connection,
        crate::tenancy::Scope::new(event.account_id, event.livemode),
    )
    .await?;
    let route = routes
        .routes()
        .iter()
        .find(|route| {
            route.livemode == event.livemode
                && route.chain.chain_id == identity.chain_id
                && route.asset.contract == identity.asset_contract
        })
        .map(|route| {
            Ok::<_, sqlx::Error>((
                route.route.clone(),
                i64::try_from(route.version).map_err(encode)?,
            ))
        })
        .transpose()?;
    let credit = event.credit.as_ref();
    let inserted = sqlx::query(
        r#"
        INSERT INTO deposits (
            id, chain_id, tx_hash, receipt_log_index, revision, log_index, block_number,
            block_hash, block_time, address_id, account_id, livemode, customer_id, route,
            route_version, asset_contract, from_address, amount_atomic, state, next_attempt_at,
            metadata, replaces, valuation_at, price_scaled, price_source, credit_minor, created_at,
            settings_revision_id, settings_hold_id
        )
        SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, address.id, address.account_id,
               address.livemode, COALESCE(quote.customer_id, deposit_address.customer_id), $10,
               $11, $12, $13, $14::text::numeric, 'reversed', now(), $15, replaced.id, $16,
               $17::text::numeric, $18, $19::text::numeric, $20,
               CASE WHEN settings.status <> 'held' THEN settings.current_revision_id END,
               settings.held_by
        FROM addresses AS address
        JOIN payment_settings_state AS settings
            ON settings.account_id = address.account_id AND settings.livemode = address.livemode
        LEFT JOIN quotes AS quote ON quote.id = address.quote_id
        LEFT JOIN deposit_addresses AS deposit_address
            ON deposit_address.id = address.deposit_address_id
        LEFT JOIN deposits AS replaced
            ON replaced.id = $21 AND replaced.account_id = address.account_id
               AND replaced.livemode = address.livemode
        WHERE address.chain_id = $2 AND address.address = $22 AND address.account_id = $23
          AND address.livemode = $24
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(event.deposit_id)
    .bind(chain_id)
    .bind(format!("{:#x}", identity.tx_hash))
    .bind(i64::try_from(identity.receipt_log_index).map_err(encode)?)
    .bind(i64::try_from(identity.revision).map_err(encode)?)
    .bind(i64::try_from(identity.log_index).map_err(encode)?)
    .bind(i64::try_from(identity.block_number).map_err(encode)?)
    .bind(format!("{:#x}", identity.block_hash))
    .bind(identity.block_time)
    .bind(route.as_ref().map(|(name, _)| name))
    .bind(route.as_ref().map(|(_, version)| *version))
    .bind(format!("{:#x}", identity.asset_contract))
    .bind(format!("{:#x}", identity.from_address))
    .bind(identity.amount_atomic.value().to_string())
    .bind(&identity.metadata)
    .bind(credit.map(|credit| credit.valuation_at))
    .bind(credit.map(|credit| credit.price.value().to_string()))
    .bind(credit.map(|credit| match credit.source {
        ValuationSource::Spot => "spot",
        ValuationSource::Lock => "lock",
    }))
    .bind(credit.map(|credit| credit.credit_minor.value().to_string()))
    .bind(identity.created)
    .bind(identity.replaces)
    .bind(format!("{:#x}", identity.address))
    .bind(event.account_id)
    .bind(event.livemode)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        > 0;
    if !inserted {
        // A concurrent rescan took the position or the id between the check and the insert.
        return Ok(position_held(connection, restore, event, identity)
            .await?
            .unwrap_or(ReversedDeposit::Rescanned));
    }
    sqlx::query(
        "INSERT INTO transitions (id, deposit_id, from_state, to_state, attempt, evidence) \
         VALUES ($1, $2, $3, 'reversed', 0, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(event.deposit_id)
    .bind(if credit.is_some() {
        "credited"
    } else {
        "rejected"
    })
    .bind(serde_json::json!({
        "result": "restored_from_delivered_event",
        "restore_id": restore.id,
        "event_id": event.id,
    }))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO restore_deposit_tombstones (deposit_id, event_id, restore_id, successor_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(event.deposit_id)
    .bind(event.id)
    .bind(restore.id)
    .bind(identity.replaced_by)
    .execute(&mut *connection)
    .await?;
    // A successor restored or recorded first names this deposit now.
    sqlx::query(
        "UPDATE deposits SET replaces = $1, updated_at = now() \
         WHERE id = $2 AND replaces IS NULL AND account_id = $3 AND livemode = $4",
    )
    .bind(event.deposit_id)
    .bind(identity.replaced_by)
    .bind(event.account_id)
    .bind(event.livemode)
    .execute(&mut *connection)
    .await?;
    Ok(ReversedDeposit::Restored)
}

/// How the ledger holds the reversed deposit of `event`, or its receipt position, when it does:
/// `Rescanned` when the rescan since `restore` recorded there a deposit that is not reversed at the
/// reversed deposit's revision or below (under its id, or an earlier one): the transfer it holds
/// is the reversed deposit's successor, recorded without its identity. A deposit at a higher
/// revision is a successor, whether the delivery named it or not (a reversal records none when
/// the transfer that took the position paid no issued address then). Otherwise
/// `Recorded` when the deposit is there (from the backup, where the finality watch reverses it if
/// it is not yet, or restored before).
async fn position_held(
    connection: &mut PgConnection,
    restore: &Restore,
    event: &DeliveredEvent,
    identity: &DeliveredIdentity,
) -> Result<Option<ReversedDeposit>, sqlx::Error> {
    let encode = |error: std::num::TryFromIntError| sqlx::Error::Encode(Box::new(error));
    let (recorded, rescanned): (bool, bool) = sqlx::query_as(
        r#"
        SELECT EXISTS (SELECT 1 FROM deposits WHERE id = $1),
               EXISTS (
                   SELECT 1 FROM deposits
                   WHERE chain_id = $2 AND tx_hash = $3 AND receipt_log_index = $4
                     AND state <> 'reversed' AND revision <= $5 AND created_at >= $6
               )
        "#,
    )
    .bind(event.deposit_id)
    .bind(i64::try_from(identity.chain_id).map_err(encode)?)
    .bind(format!("{:#x}", identity.tx_hash))
    .bind(i64::try_from(identity.receipt_log_index).map_err(encode)?)
    .bind(i64::try_from(identity.revision).map_err(encode)?)
    .bind(restore.detected_at)
    .fetch_one(&mut *connection)
    .await?;
    Ok(match (recorded, rescanned) {
        (_, true) => Some(ReversedDeposit::Rescanned),
        (true, false) => Some(ReversedDeposit::Recorded),
        (false, false) => None,
    })
}

/// Keeps the credit of an imported event for its deposit. A credited and a reversed event of one
/// deposit carry the same credit: the first one imported is kept, and another is logged.
async fn insert_credit(
    connection: &mut PgConnection,
    restore: &Restore,
    event: &DeliveredEvent,
    credit: &DeliveredCredit,
) -> Result<(), sqlx::Error> {
    let chain_id =
        i64::try_from(credit.chain_id).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO restore_delivered_credits (
            deposit_id, event_id, restore_id, account_id, livemode, chain_id, tx_hash, address,
            asset_contract, from_address, amount_atomic, price_scaled, price_source, credit_minor,
            valuation_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::text::numeric, $12::text::numeric,
                $13, $14::text::numeric, $15)
        ON CONFLICT (deposit_id) DO NOTHING
        "#,
    )
    .bind(event.deposit_id)
    .bind(event.id)
    .bind(restore.id)
    .bind(event.account_id)
    .bind(event.livemode)
    .bind(chain_id)
    .bind(format!("{:#x}", credit.tx_hash))
    .bind(format!("{:#x}", credit.address))
    .bind(format!("{:#x}", credit.asset_contract))
    .bind(format!("{:#x}", credit.from_address))
    .bind(credit.amount_atomic.value().to_string())
    .bind(credit.price.value().to_string())
    .bind(match credit.source {
        ValuationSource::Spot => "spot",
        ValuationSource::Lock => "lock",
    })
    .bind(credit.credit_minor.value().to_string())
    .bind(credit.valuation_at)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        > 0;
    if !inserted
        && imported_credit(&mut *connection, event.deposit_id)
            .await?
            .is_some_and(|kept| kept.credit != *credit)
    {
        tracing::error!(
            event_id = %crate::ids::format(crate::ids::EVENT, event.id),
            deposit_id = %crate::ids::format(crate::ids::DEPOSIT, event.deposit_id),
            "delivered events of one deposit carry different credits; the first imported is kept"
        );
    }
    Ok(())
}

/// Why [`discard_credit`] failed.
#[derive(Debug, thiserror::Error)]
pub enum DiscardError {
    /// No delivered credit was imported for the deposit.
    #[error("no delivered credit was imported for the deposit")]
    NotFound,
    /// PostgreSQL rejected or failed the operation.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

/// Discards the delivered credit of `deposit_id` (a finding `contradicted`: its transfer is not
/// the chain's), so the deposit is valued from the chain as any other; the operator settles the
/// difference with the merchant. Audited; discarding again changes nothing.
pub async fn discard_credit(
    pool: &PgPool,
    deposit_id: Uuid,
    actor: &Actor,
    reason: &str,
) -> Result<(), DiscardError> {
    let mut transaction = pool.begin().await?;
    let (account_id, restore_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT account_id, restore_id FROM restore_delivered_credits WHERE deposit_id = $1 \
         FOR UPDATE",
    )
    .bind(deposit_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(DiscardError::NotFound)?;
    let discarded = sqlx::query(
        "UPDATE restore_delivered_credits \
         SET discarded_at = now(), discarded_by = $2, discard_reason = $3 \
         WHERE deposit_id = $1 AND discarded_at IS NULL",
    )
    .bind(deposit_id)
    .bind(actor.to_string())
    .bind(reason)
    .execute(&mut *transaction)
    .await?
    .rows_affected()
        > 0;
    if discarded {
        audit::insert(
            &mut *transaction,
            &audit::Entry {
                account_id: Some(account_id),
                actor,
                action: "restore.delivered_credit_discard",
                subject: &format!(
                    "deposit:{}",
                    crate::ids::format(crate::ids::DEPOSIT, deposit_id)
                ),
                reason: &format!("restore {restore_id}: {}", reason.trim()),
            },
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// An imported event whose deposit the ledger does not yet hold as delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveredEventFinding {
    /// Event id.
    pub event_id: Uuid,
    /// Event type.
    pub event_type: String,
    /// The deposit.
    pub deposit_id: Uuid,
    /// `rescanned` (a `deposit.reversed` whose receipt position the rescan recorded first with a
    /// deposit that is not reversed nor its successor: its deposit could not be restored),
    /// `pending` (the rescan has not re-derived or valued the deposit yet), `contradicted` (the
    /// recorded transfer is not the delivered one: the deposit is held until the operator
    /// discards the delivered credit), or `mismatch` (the ledger's token amount or credit differs
    /// from the delivered one).
    pub status: &'static str,
    /// Delivered `amount_atomic`.
    pub delivered_amount_atomic: Option<String>,
    /// Delivered `amount` (the credit in minor units).
    pub delivered_amount: Option<String>,
    /// The ledger's `amount_atomic`.
    pub ledger_amount_atomic: Option<String>,
    /// The ledger's credit in minor units.
    pub ledger_amount: Option<String>,
}

#[derive(FromRow)]
struct ImportedRow {
    event_id: Uuid,
    event_type: String,
    deposit_id: Uuid,
    delivered_amount_atomic: Option<String>,
    delivered_amount: Option<String>,
    ledger_amount_atomic: Option<String>,
    ledger_amount: Option<String>,
    contradicted: bool,
    rescanned: bool,
}

/// Imported events of `restore` compared with the ledger: the count, and each one whose deposit
/// is not re-derived yet, whose recorded transfer contradicts its delivered credit, or whose
/// amounts differ from what the merchant received. The delivered snapshot stays the event; a
/// contradiction or a mismatch is the operator's to settle with the merchant.
pub async fn delivered_event_findings(
    pool: &PgPool,
    restore: &Restore,
) -> Result<(i64, Vec<DeliveredEventFinding>), sqlx::Error> {
    let rows = sqlx::query_as::<_, ImportedRow>(
        r#"
        SELECT event.id AS event_id, event.type AS event_type, event.object_id AS deposit_id,
               event.data #>> '{object,amount_atomic}' AS delivered_amount_atomic,
               event.data #>> '{object,amount}' AS delivered_amount,
               deposit.amount_atomic::text AS ledger_amount_atomic,
               deposit.credit_minor::text AS ledger_amount,
               COALESCE(CASE WHEN event.type = 'deposit.rejected' THEN
                        event.account_id <> deposit.account_id
                        OR event.livemode <> deposit.livemode
                        OR (event.data #>> '{object,chain_id}')::bigint <> deposit.chain_id
                        OR lower(event.data #>> '{object,tx_hash}') <> deposit.tx_hash
                        OR (event.data #>> '{object,receipt_log_index}')::bigint
                            <> deposit.receipt_log_index
                        OR lower(event.data #>> '{object,address}') <> address.address
                        OR lower(event.data #>> '{object,asset_contract}') <> deposit.asset_contract
                        OR lower(event.data #>> '{object,from_address}') <> deposit.from_address
                        OR (event.data #>> '{object,amount_atomic}')::numeric <> deposit.amount_atomic
                   ELSE credit.account_id <> deposit.account_id
                        OR credit.livemode <> deposit.livemode
                        OR credit.chain_id <> deposit.chain_id
                        OR credit.tx_hash <> deposit.tx_hash
                        OR credit.address <> address.address
                        OR credit.asset_contract <> deposit.asset_contract
                        OR credit.from_address <> deposit.from_address
                        OR credit.amount_atomic <> deposit.amount_atomic
                   END, false) AS contradicted,
               -- ReversedDeposit::Rescanned: the rescan took the reversed deposit's position.
               event.type = 'deposit.reversed' AND EXISTS (
                   SELECT 1 FROM deposits AS holder
                   WHERE holder.chain_id = (event.data #>> '{object,chain_id}')::bigint
                     AND holder.tx_hash = event.data #>> '{object,tx_hash}'
                     AND holder.receipt_log_index
                         = (event.data #>> '{object,receipt_log_index}')::bigint
                     AND holder.state <> 'reversed' AND holder.created_at >= $2
                     AND holder.revision <= (event.data #>> '{object,revision}')::bigint
               ) AS rescanned
        FROM restore_delivered_events AS imported
        JOIN events AS event ON event.id = imported.event_id
        LEFT JOIN deposits AS deposit ON deposit.id = event.object_id
        LEFT JOIN addresses AS address ON address.id = deposit.address_id
        LEFT JOIN restore_delivered_credits AS credit
            ON credit.deposit_id = event.object_id AND credit.discarded_at IS NULL
        WHERE imported.restore_id = $1
        ORDER BY event.created, event.id
        "#,
    )
    .bind(restore.id)
    .bind(restore.detected_at)
    .fetch_all(pool)
    .await?;
    let imported = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let findings = rows
        .into_iter()
        .filter_map(|row| {
            let status = match (
                &row.ledger_amount_atomic,
                &row.delivered_amount,
                &row.ledger_amount,
            ) {
                _ if row.rescanned => "rescanned",
                _ if row.contradicted => "contradicted",
                (None, _, _) | (Some(_), Some(_), None) => "pending",
                (Some(ledger), _, _) if Some(ledger) != row.delivered_amount_atomic.as_ref() => {
                    "mismatch"
                }
                (Some(_), Some(delivered), Some(ledger)) if delivered != ledger => "mismatch",
                _ => return None,
            };
            Some(DeliveredEventFinding {
                event_id: row.event_id,
                event_type: row.event_type,
                deposit_id: row.deposit_id,
                status,
                delivered_amount_atomic: row.delivered_amount_atomic,
                delivered_amount: row.delivered_amount,
                ledger_amount_atomic: row.ledger_amount_atomic,
                ledger_amount: row.ledger_amount,
            })
        })
        .collect();
    Ok((imported, findings))
}

#[cfg(test)]
mod acceptance_tests {
    use super::*;

    #[test]
    fn only_explicit_admin_override_with_a_reason_is_accepted() {
        let admin = Actor::admin("operator");
        assert_eq!(override_reason(&admin, "reconciled"), None);
        assert_eq!(
            override_reason(&admin, "override-critical-checks: ; checklist: done"),
            None
        );
        assert_eq!(
            override_reason(
                &Actor::system("worker"),
                "override-critical-checks: evidence"
            ),
            None
        );
        assert_eq!(
            override_reason(
                &Actor::api_key("merchant"),
                "override-critical-checks: evidence"
            ),
            None
        );
        assert_eq!(
            override_reason(
                &admin,
                "override-critical-checks: INC-42 independent evidence; checklist: done"
            ),
            Some("INC-42 independent evidence")
        );
    }
}
