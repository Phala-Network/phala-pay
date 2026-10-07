use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
use topup_core::deposit::{DepositState, RejectReason, Transition};
use topup_core::identity::deposit_revision_id;
use topup_core::money::{AtomicAmount, MinorAmount};
use uuid::Uuid;

use super::types::{
    address_hex, atomic_decimal, b256_hex, parse_address, parse_atomic_decimal, parse_b256,
    parse_optional_minor_decimal, parse_optional_u64_decimal, to_i64, to_u64,
};
use super::{parse_reason, parse_state, state_code};

/// A durable deposit row.
#[derive(Clone, Debug, PartialEq)]
pub struct Deposit {
    /// Deterministic deposit identifier.
    pub id: Uuid,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transfer transaction hash.
    pub tx_hash: B256,
    /// Position of the transfer log in its transaction's receipt; part of the identity.
    pub receipt_log_index: u64,
    /// Block-wide transfer log index; evidence that follows re-inclusion.
    pub log_index: u64,
    /// Including block number.
    pub block_number: u64,
    /// Including block hash.
    pub block_hash: B256,
    /// Chain block time.
    pub block_time: DateTime<Utc>,
    /// Receiving address row.
    pub address_id: Uuid,
    /// Owning account.
    pub account_id: Uuid,
    /// Mode of the receiving address.
    pub livemode: bool,
    /// The account's customer the quote was issued for.
    pub customer_id: Uuid,
    /// Selected route name, absent for unsupported assets.
    pub route: Option<String>,
    /// Selected route version.
    pub route_version: Option<u64>,
    /// Token contract address.
    pub asset_contract: Address,
    /// Transfer sender address.
    pub from_address: Address,
    /// Atomic token amount.
    pub amount_atomic: AtomicAmount,
    /// Transaction sender; absent only on a reversed deposit restored from a delivered event
    /// ([`crate::restore_mode::import_delivered_event`]), which does not carry it.
    pub tx_from: Option<Address>,
    /// Transaction nonce; absent only where `tx_from` is.
    pub tx_nonce: Option<u64>,
    /// When both providers showed the transfer at or below `finalized`; `None` while it can still
    /// be reversed.
    pub final_at: Option<DateTime<Utc>>,
    /// Current domain state.
    pub state: DepositState,
    /// Terminal rejection reason.
    pub reason: Option<RejectReason>,
    /// Retry attempt within the current state.
    pub attempt: i32,
    /// Earliest next processing time.
    pub next_attempt_at: DateTime<Utc>,
    /// Current lease ownership token.
    pub lease_token: Option<Uuid>,
    /// Current lease expiry.
    pub lease_until: Option<DateTime<Utc>>,
    /// Valuation observation time.
    pub valuation_at: Option<DateTime<Utc>>,
    /// Eight-decimal scaled price integer.
    pub price_scaled: Option<u64>,
    /// Price source code.
    pub price_source: Option<String>,
    /// Product minor-unit credit.
    pub credit_minor: Option<MinorAmount>,
    /// Stored quote evidence.
    pub quote: Option<Value>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last row update time.
    pub updated_at: DateTime<Utc>,
}

/// Values used to insert a transfer at the required confirmation as a deposit.
#[derive(Clone, Debug, PartialEq)]
pub struct NewDeposit {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Transfer transaction hash.
    pub tx_hash: B256,
    /// Position of the transfer log in its transaction's receipt.
    pub receipt_log_index: u64,
    /// Block-wide transfer log index.
    pub log_index: u64,
    /// Including block number.
    pub block_number: u64,
    /// Including block hash.
    pub block_hash: B256,
    /// Chain block time.
    pub block_time: DateTime<Utc>,
    /// Receiving address row; the deposit takes its account, mode, and customer from it.
    pub address_id: Uuid,
    /// Selected route name, absent for unsupported assets.
    pub route: Option<String>,
    /// Selected route version.
    pub route_version: Option<u64>,
    /// Token contract address.
    pub asset_contract: Address,
    /// Transfer sender address.
    pub from_address: Address,
    /// Atomic token amount.
    pub amount_atomic: AtomicAmount,
    /// Initial domain state.
    pub state: DepositState,
    /// Initial rejection reason when the asset is unsupported.
    pub reason: Option<RejectReason>,
    /// Earliest processing time.
    pub next_attempt_at: DateTime<Utc>,
    /// Transaction sender.
    pub tx_from: Address,
    /// Transaction nonce.
    pub tx_nonce: u64,
    /// Whether the deposit is known final when recorded. Scanners record `false`; the confirm step
    /// and the finality watch mark a deposit final once both providers show it at `finalized`.
    pub is_final: bool,
}

/// A deposit returned with a newly acquired five-minute processing lease.
pub type ClaimedDeposit = Deposit;

/// State and retry fields written by a single state-machine application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionUpdate {
    /// Core-validated state transition.
    pub transition: Transition,
    /// Rejection reason, required only when entering `rejected`.
    pub rejection_reason: Option<RejectReason>,
    /// Retry attempt stored on both the deposit and timeline row.
    pub attempt: i32,
    /// Earliest next processing time.
    pub next_attempt_at: DateTime<Utc>,
}

/// An outbox event committed with a state transition.
pub type OutboxEvent = super::outbox::NewOutboxEvent;

/// Canonical chain evidence corrected while a deposit remains detected.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalEvidence {
    /// Canonical block-wide log index.
    pub log_index: u64,
    /// Canonical block number.
    pub block_number: u64,
    /// Canonical finalized block hash.
    pub block_hash: B256,
    /// Canonical block timestamp.
    pub block_time: DateTime<Utc>,
    /// Canonical token contract.
    pub asset_contract: Address,
    /// Canonical transfer sender.
    pub from_address: Address,
    /// Canonical transfer amount.
    pub amount_atomic: AtomicAmount,
    /// Independently derived transaction sender.
    pub tx_from: Address,
    /// Independently derived sender nonce (receipt depositNonce for OP deposits).
    pub tx_nonce: u64,
    /// Route selected for the canonical token, when supported.
    pub route: Option<String>,
    /// Version selected for the canonical token, when supported.
    pub route_version: Option<u64>,
}

/// Valuation columns committed with a transition.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredValuation {
    /// Time at which finality and prices were observed.
    pub valuation_at: DateTime<Utc>,
    /// Eight-decimal scaled price.
    pub price_scaled: u64,
    /// Stable source code, `spot` or `lock`.
    pub price_source: String,
    /// Product credit in minor units.
    pub credit_minor: MinorAmount,
    /// Raw price observations and validation result.
    pub quote: Value,
}

/// Conditional single-use rate-lock consumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LockConsumption {
    /// Lock address row used as the rate-lock primary key.
    pub address_id: Uuid,
    /// Whether an existing consumption by this same deposit is accepted.
    pub idempotent: bool,
}

/// Additional writes atomically applied with one state transition.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TransitionEffects {
    /// Every decision field was independently agreed on both endpoints.
    pub dual_verified: bool,
    /// Optional correction to provisional scanner evidence.
    pub canonical_evidence: Option<CanonicalEvidence>,
    /// Optional valuation columns.
    pub valuation: Option<StoredValuation>,
    /// Optional conditional rate-lock consumption.
    pub lock_consumption: Option<LockConsumption>,
    /// Marks the deposit final: both providers showed its transfer at or below `finalized`.
    pub mark_final: bool,
    /// Records that screening named the sender of a deposit whose delivered credit stands, which
    /// keeps its forwarder from every sweep.
    pub sanctions_hit: bool,
}

/// Timeline and side effects written by one transition application.
#[derive(Clone, Copy, Debug)]
pub struct TransitionWrites<'a> {
    /// Evidence appended to the transition timeline.
    pub evidence: &'a Value,
    /// Structured deposit and lock writes.
    pub effects: &'a TransitionEffects,
    /// Outbox rows inserted after the state compare-and-swap succeeds.
    pub outbox_events: &'a [OutboxEvent],
}

/// Result of the lease-token compare-and-swap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyTransitionResult {
    /// The state, timeline, and outbox writes were applied.
    Applied,
    /// The expected state or lease token no longer matched.
    Stale,
    /// Another deposit consumed the selected rate lock before this transaction.
    LockUnavailable,
    /// Crediting the deposit before it is final would take its account's unfinalized credit past
    /// the cap; nothing was written and the caller rolls back.
    UnfinalizedCreditCapped(UnfinalizedCredit),
}

/// A scope's credit that is not final yet: the `credit_minor` of its credited deposits whose
/// block is not final on both providers, which a reorganization could still reverse, and the
/// account's cap on it (`accounts.max_unfinalized_credit`, the same for each mode).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnfinalizedCredit {
    /// Credit of the scope's credited deposits that are not final, in cents.
    pub credited: u64,
    /// The account's cap, in cents.
    pub cap: u64,
}

impl UnfinalizedCredit {
    /// Whether crediting `credit` more before finality stays within the cap.
    #[must_use]
    pub fn admits(self, credit: u64) -> bool {
        self.credited
            .checked_add(credit)
            .is_some_and(|total| total <= self.cap)
    }
}

/// The unfinalized credit of `account_id` in `livemode`, excluding deposit `excluding`.
pub async fn unfinalized_credit<'e>(
    executor: impl PgExecutor<'e>,
    account_id: Uuid,
    livemode: bool,
    excluding: Uuid,
) -> Result<UnfinalizedCredit, sqlx::Error> {
    let (credited, cap): (String, i64) = sqlx::query_as(
        r#"
        SELECT (
                   SELECT COALESCE(sum(credit_minor), 0)::text
                   FROM deposits
                   WHERE account_id = $1 AND livemode = $2 AND state = 'credited'
                     AND final_at IS NULL AND id <> $3
               ),
               (SELECT max_unfinalized_credit FROM accounts WHERE id = $1)
        "#,
    )
    .bind(account_id)
    .bind(livemode)
    .bind(excluding)
    .fetch_one(executor)
    .await?;
    Ok(UnfinalizedCredit {
        credited: credited
            .parse()
            .map_err(|_| decode_error("unfinalized credit"))?,
        cap: to_u64(cap, "accounts.max_unfinalized_credit")?,
    })
}

fn decode_error(what: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("{what} is out of range").into())
}

/// Failure while validating or persisting a transition.
#[derive(Debug, thiserror::Error)]
pub enum ApplyTransitionError {
    /// The caller supplied fields inconsistent with the core transition.
    #[error("{0}")]
    InvalidInput(&'static str),
    /// PostgreSQL rejected or failed the operation.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, sqlx::FromRow)]
struct DepositRecord {
    id: Uuid,
    chain_id: i64,
    tx_hash: String,
    receipt_log_index: i64,
    log_index: i64,
    block_number: i64,
    block_hash: String,
    block_time: DateTime<Utc>,
    address_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    customer_id: Uuid,
    route: Option<String>,
    route_version: Option<i64>,
    asset_contract: String,
    from_address: String,
    amount_atomic: String,
    tx_from: Option<String>,
    tx_nonce: Option<String>,
    final_at: Option<DateTime<Utc>>,
    state: String,
    reason: Option<String>,
    attempt: i32,
    next_attempt_at: DateTime<Utc>,
    lease_token: Option<Uuid>,
    lease_until: Option<DateTime<Utc>>,
    valuation_at: Option<DateTime<Utc>>,
    price_scaled: Option<String>,
    price_source: Option<String>,
    credit_minor: Option<String>,
    quote: Option<Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<DepositRecord> for Deposit {
    type Error = sqlx::Error;

    fn try_from(record: DepositRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: record.id,
            chain_id: to_u64(record.chain_id, "deposits.chain_id")?,
            tx_hash: parse_b256(&record.tx_hash)?,
            receipt_log_index: to_u64(record.receipt_log_index, "deposits.receipt_log_index")?,
            log_index: to_u64(record.log_index, "deposits.log_index")?,
            block_number: to_u64(record.block_number, "deposits.block_number")?,
            block_hash: parse_b256(&record.block_hash)?,
            block_time: record.block_time,
            address_id: record.address_id,
            account_id: record.account_id,
            livemode: record.livemode,
            customer_id: record.customer_id,
            route: record.route,
            route_version: record
                .route_version
                .map(|value| to_u64(value, "deposits.route_version"))
                .transpose()?,
            asset_contract: parse_address(&record.asset_contract)?,
            from_address: parse_address(&record.from_address)?,
            amount_atomic: parse_atomic_decimal(&record.amount_atomic)?,
            tx_from: record.tx_from.as_deref().map(parse_address).transpose()?,
            tx_nonce: parse_optional_u64_decimal(record.tx_nonce.as_deref())?,
            final_at: record.final_at,
            state: parse_state(&record.state)?,
            reason: parse_reason(record.reason.as_deref())?,
            attempt: record.attempt,
            next_attempt_at: record.next_attempt_at,
            lease_token: record.lease_token,
            lease_until: record.lease_until,
            valuation_at: record.valuation_at,
            price_scaled: parse_optional_u64_decimal(record.price_scaled.as_deref())?,
            price_source: record.price_source,
            credit_minor: parse_optional_minor_decimal(record.credit_minor.as_deref())?,
            quote: record.quote,
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }
}

/// What a transfer was read from, which decides whether it may take a receipt position whose
/// deposits were all reversed (architecture §7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Evidence {
    /// Read at the route's confirmation, before finality (the per-block scan): only a position
    /// that never had a deposit. Such a read may be of a log a reorganization has since removed.
    Confirmed,
    /// Read at or below `finalized` (the finalized backstop, the reconciler, restore rescans).
    Finalized,
    /// The transfer the finality watch found final at the position of `replaces`, which it
    /// reversed in the same transaction.
    Successor {
        /// The reversed deposit.
        replaces: Uuid,
    },
}

/// Inserts a deposit read at or below `finalized` and returns `false` when a deposit that is not
/// reversed already holds its receipt position.
pub async fn insert_deposit(pool: &PgPool, deposit: &NewDeposit) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let inserted = insert_deposit_in(&mut transaction, deposit, Evidence::Finalized).await?;
    transaction.commit().await?;
    Ok(inserted.is_some())
}

/// Inserts a deposit and returns its id, or `None` when a deposit that is not reversed already
/// holds its receipt position: the same transfer seen again, or another one at that position,
/// which the finality watch settles when the position is final. A position whose deposits were
/// all reversed takes a new deposit with the next revision (`identity::deposit_revision_id`) from
/// finalized `evidence` only, so an existing deposit keeps its id and a concurrent insert of the
/// same revision does nothing. A successor names the deposit it replaces when both are in the same
/// account and mode: the one the finality watch reversed, or a reversed deposit restored after a
/// restore whose delivered `deposit.reversed` named this deposit as its successor
/// (`crate::restore_mode`). The deposit is bound to the payment settings of its account and mode
/// that the insert's own snapshot reads: their current revision, or, while they are held after a
/// restore, that restore (`crate::payment_config`, design §7).
pub async fn insert_deposit_in(
    transaction: &mut Transaction<'_, Postgres>,
    deposit: &NewDeposit,
    evidence: Evidence,
) -> Result<Option<Uuid>, sqlx::Error> {
    let chain_id = to_i64(deposit.chain_id, "deposits.chain_id")?;
    let tx_hash = b256_hex(deposit.tx_hash);
    let receipt_log_index = to_i64(deposit.receipt_log_index, "deposits.receipt_log_index")?;
    let revision: i64 = sqlx::query_scalar(
        "SELECT COALESCE(max(revision) + 1, 0) FROM deposits \
         WHERE chain_id = $1 AND tx_hash = $2 AND receipt_log_index = $3",
    )
    .bind(chain_id)
    .bind(&tx_hash)
    .bind(receipt_log_index)
    .fetch_one(&mut **transaction)
    .await?;
    let id = deposit_revision_id(
        deposit.chain_id,
        deposit.tx_hash,
        deposit.receipt_log_index,
        to_u64(revision, "deposits.revision")?,
    );
    let log_index = to_i64(deposit.log_index, "deposits.log_index")?;
    let block_number = to_i64(deposit.block_number, "deposits.block_number")?;
    let block_hash = b256_hex(deposit.block_hash);
    let route_version = deposit
        .route_version
        .map(|value| to_i64(value, "deposits.route_version"))
        .transpose()?;
    let asset_contract = address_hex(deposit.asset_contract);
    let from_address = address_hex(deposit.from_address);
    let amount_atomic = atomic_decimal(deposit.amount_atomic);
    let state = state_code(deposit.state);
    let reason = deposit.reason.map(RejectReason::code);
    let tx_from = address_hex(deposit.tx_from);
    let tx_nonce = deposit.tx_nonce.to_string();
    let (takes_revision, replaces) = match evidence {
        Evidence::Confirmed => (false, None),
        Evidence::Finalized => (true, None),
        Evidence::Successor { replaces } => (true, Some(replaces)),
    };
    // The barrier (crate::payment_config): the insert below, a later statement, binds the
    // deposit to the payment settings its own snapshot reads.
    crate::payment_config::lock_for_recording(transaction, deposit.address_id).await?;
    let result = sqlx::query!(
        r#"
        INSERT INTO deposits (
            id, chain_id, tx_hash, log_index, block_number, block_hash, block_time,
            address_id, account_id, livemode, customer_id, route, route_version, asset_contract,
            from_address, amount_atomic, state, reason, next_attempt_at, receipt_log_index,
            tx_from, tx_nonce, final_at, metadata, revision, replaces, settings_revision_id,
            settings_hold_id
        )
        SELECT
            $1, $2, $3, $4, $5, $6, $7, address.id, address.account_id, address.livemode,
            COALESCE(quote.customer_id, deposit_address.customer_id), $9, $10, $11, $12,
            $13::text::numeric, $14, $15, $16, $17, $18, $19::text::numeric,
            CASE WHEN $20 THEN now() END,
            COALESCE(quote.metadata, deposit_address.metadata), $21, replaced.id,
            CASE WHEN settings.status <> 'held' THEN settings.current_revision_id END,
            settings.held_by
        FROM addresses AS address
        JOIN payment_settings_state AS settings
            ON settings.account_id = address.account_id AND settings.livemode = address.livemode
        LEFT JOIN quotes AS quote ON quote.id = address.quote_id
        LEFT JOIN deposit_addresses AS deposit_address
            ON deposit_address.id = address.deposit_address_id
        LEFT JOIN deposits AS replaced
            ON replaced.id = COALESCE(
                   $23,
                   (SELECT deposit_id FROM restore_deposit_tombstones WHERE successor_id = $1)
               )
               AND replaced.account_id = address.account_id
               AND replaced.livemode = address.livemode
        WHERE address.id = $8 AND ($21 = 0::bigint OR $22)
        ON CONFLICT DO NOTHING
        "#,
        id,
        chain_id,
        tx_hash,
        log_index,
        block_number,
        block_hash,
        deposit.block_time,
        deposit.address_id,
        deposit.route,
        route_version,
        asset_contract,
        from_address,
        amount_atomic,
        state,
        reason,
        deposit.next_attempt_at,
        receipt_log_index,
        tx_from,
        tx_nonce,
        deposit.is_final,
        revision,
        takes_revision,
        replaces
    )
    .execute(&mut **transaction)
    .await?;
    Ok((result.rows_affected() == 1).then_some(id))
}

/// Fetches a deposit by its deterministic identifier.
pub async fn get_deposit(pool: &PgPool, id: Uuid) -> Result<Option<Deposit>, sqlx::Error> {
    let record = sqlx::query_as!(
        DepositRecord,
        r#"
        SELECT
            id, chain_id, tx_hash, receipt_log_index, log_index, block_number, block_hash,
            block_time, address_id, account_id, livemode, customer_id, route, route_version,
            asset_contract,
            from_address, amount_atomic::text AS "amount_atomic!", tx_from,
            tx_nonce::text AS tx_nonce, final_at, state, reason, attempt, next_attempt_at,
            lease_token, lease_until, valuation_at, price_scaled::text AS price_scaled,
            price_source, credit_minor::text AS credit_minor, quote, created_at, updated_at
        FROM deposits
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;
    record.map(TryInto::try_into).transpose()
}

/// Reads a bounded ID batch using the existing deposit decoder. Row decoding failures remain
/// associated with their IDs so reconciliation can report one corrupt row and keep progressing.
pub(crate) async fn deposits_by_ids(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Result<Deposit, sqlx::Error>)>, sqlx::Error> {
    let records = sqlx::query_as::<_, DepositRecord>(
        "SELECT id,chain_id,tx_hash,receipt_log_index,log_index,block_number,block_hash,block_time,
         address_id,account_id,livemode,customer_id,route,route_version,asset_contract,
         from_address,amount_atomic::text AS amount_atomic,tx_from,tx_nonce::text AS tx_nonce,
         final_at,state,reason,attempt,next_attempt_at,lease_token,lease_until,valuation_at,
         price_scaled::text AS price_scaled,price_source,credit_minor::text AS credit_minor,
         quote,created_at,updated_at FROM deposits WHERE id=ANY($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(records
        .into_iter()
        .map(|record| (record.id, record.try_into()))
        .collect())
}

/// Claims one due `detected` or `confirmed` deposit with a five-minute lease. A credited deposit
/// is not claimed: only a finalized `Flushed` event moves it on, and the scanner and the finality
/// watch apply that ([`crate::db::commit_factory_logs`]).
pub async fn claim_deposit(
    pool: &PgPool,
    lease_token: Uuid,
) -> Result<Option<ClaimedDeposit>, sqlx::Error> {
    let record = sqlx::query_as!(
        DepositRecord,
        r#"
        WITH candidate AS (
            SELECT id
            FROM deposits
            WHERE state IN ('detected', 'confirmed')
              AND next_attempt_at <= now()
              AND (lease_until IS NULL OR lease_until <= now())
            ORDER BY next_attempt_at, created_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        UPDATE deposits AS deposit
        SET lease_token = $1,
            lease_until = now() + interval '5 minutes',
            updated_at = now()
        FROM candidate
        WHERE deposit.id = candidate.id
        RETURNING
            deposit.id, deposit.chain_id, deposit.tx_hash, deposit.receipt_log_index,
            deposit.log_index, deposit.block_number, deposit.block_hash, deposit.block_time,
            deposit.address_id, deposit.account_id, deposit.livemode, deposit.customer_id,
            deposit.route, deposit.route_version,
            deposit.asset_contract, deposit.from_address,
            deposit.amount_atomic::text AS "amount_atomic!", deposit.tx_from,
            deposit.tx_nonce::text AS tx_nonce, deposit.final_at, deposit.state, deposit.reason, deposit.attempt, deposit.next_attempt_at,
            deposit.lease_token, deposit.lease_until, deposit.valuation_at,
            deposit.price_scaled::text AS price_scaled, deposit.price_source,
            deposit.credit_minor::text AS credit_minor, deposit.quote,
            deposit.created_at, deposit.updated_at
        "#,
        lease_token
    )
    .fetch_optional(pool)
    .await?;
    record.map(TryInto::try_into).transpose()
}

/// Applies a lease-token CAS and writes its timeline plus outbox events in the same transaction.
pub async fn apply_transition(
    transaction: &mut Transaction<'_, Postgres>,
    routes: &crate::routes::RouteSet,
    deposit_id: Uuid,
    expected_state: DepositState,
    lease_token: Uuid,
    update: TransitionUpdate,
    writes: TransitionWrites<'_>,
) -> Result<ApplyTransitionResult, ApplyTransitionError> {
    if update.transition.from != expected_state {
        return Err(ApplyTransitionError::InvalidInput(
            "expected state must match the transition source",
        ));
    }
    if update.attempt < 0 {
        return Err(ApplyTransitionError::InvalidInput(
            "transition attempt cannot be negative",
        ));
    }
    let enters_rejected = update.transition.to == DepositState::Rejected;
    if enters_rejected != update.rejection_reason.is_some() {
        return Err(ApplyTransitionError::InvalidInput(
            "a rejection reason is required only when entering rejected",
        ));
    }

    // Serialize credit/derived writes with finalized-fork freeze and audited recovery.
    let chain: i64 = sqlx::query_scalar("SELECT chain_id FROM deposits WHERE id=$1")
        .bind(deposit_id)
        .fetch_one(&mut **transaction)
        .await?;
    if update.transition.to == expected_state
        && *writes.effects == TransitionEffects::default()
        && writes.outbox_events.is_empty()
    {
        // A frozen chain can record a wait/retry and release its worker lease.
        super::rpc::lock_reconciliation_in(transaction, &format!("chain:{chain}")).await?;
    } else {
        super::rpc::guard_in(
            transaction,
            u64::try_from(chain).map_err(|e| sqlx::Error::Encode(e.into()))?,
        )
        .await?;
    }
    let expected = state_code(expected_state);
    let target = state_code(update.transition.to);
    let reason = update.rejection_reason.map(RejectReason::code);
    let matched = sqlx::query_scalar!(
        r#"
        UPDATE deposits
        SET state = $4,
            reason = $5,
            attempt = $6,
            next_attempt_at = $7,
            lease_token = NULL,
            lease_until = NULL,
            updated_at = now()
        WHERE id = $1 AND state = $2 AND lease_token = $3
        RETURNING id
        "#,
        deposit_id,
        expected,
        lease_token,
        target,
        reason,
        update.attempt,
        update.next_attempt_at
    )
    .fetch_optional(&mut **transaction)
    .await?;

    if matched.is_none() {
        return Ok(ApplyTransitionResult::Stale);
    }

    if let Some(consumption) = writes.effects.lock_consumption
        && !crate::locks::consume(
            transaction,
            consumption.address_id,
            deposit_id,
            consumption.idempotent,
        )
        .await?
    {
        return Ok(ApplyTransitionResult::LockUnavailable);
    }

    if let Some(canonical) = &writes.effects.canonical_evidence {
        sqlx::query(
            r#"
            UPDATE deposits
            SET block_number = $2,
                block_hash = $3,
                block_time = $4,
                asset_contract = $5,
                from_address = $6,
                amount_atomic = $7::text::numeric,
                route = $8,
                route_version = $9,
                log_index = $10,
                tx_from = $11,
                tx_nonce = $12::text::numeric,
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(deposit_id)
        .bind(to_i64(canonical.block_number, "deposits.block_number")?)
        .bind(b256_hex(canonical.block_hash))
        .bind(canonical.block_time)
        .bind(address_hex(canonical.asset_contract))
        .bind(address_hex(canonical.from_address))
        .bind(atomic_decimal(canonical.amount_atomic))
        .bind(&canonical.route)
        .bind(
            canonical
                .route_version
                .map(|version| to_i64(version, "deposits.route_version"))
                .transpose()?,
        )
        .bind(to_i64(canonical.log_index, "deposits.log_index")?)
        .bind(address_hex(canonical.tx_from))
        .bind(canonical.tx_nonce.to_string())
        .execute(&mut **transaction)
        .await?;
    }

    if writes.effects.dual_verified {
        sqlx::query(
            "UPDATE deposits SET dual_verified_at = COALESCE(dual_verified_at, now()) WHERE id=$1",
        )
        .bind(deposit_id)
        .execute(&mut **transaction)
        .await?;
    }

    if writes.effects.sanctions_hit {
        sqlx::query(
            "UPDATE deposits SET sanctions_hit_at = now(), updated_at = now() \
             WHERE id = $1 AND sanctions_hit_at IS NULL",
        )
        .bind(deposit_id)
        .execute(&mut **transaction)
        .await?;
    }

    if writes.effects.mark_final {
        sqlx::query(
            "UPDATE deposits SET final_at = now(), updated_at = now() \
             WHERE id = $1 AND final_at IS NULL",
        )
        .bind(deposit_id)
        .execute(&mut **transaction)
        .await?;
    }

    if let Some(valuation) = &writes.effects.valuation {
        sqlx::query(
            r#"
            UPDATE deposits
            SET valuation_at = $2,
                price_scaled = $3::text::numeric,
                price_source = $4,
                credit_minor = $5::text::numeric,
                quote = $6,
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(deposit_id)
        .bind(valuation.valuation_at)
        .bind(valuation.price_scaled.to_string())
        .bind(&valuation.price_source)
        .bind(valuation.credit_minor.value().to_string())
        .bind(&valuation.quote)
        .execute(&mut **transaction)
        .await?;
    }

    if update.transition.to == DepositState::Credited
        && let Some(capped) = exceeds_unfinalized_cap(transaction, deposit_id).await?
    {
        return Ok(ApplyTransitionResult::UnfinalizedCreditCapped(capped));
    }

    sqlx::query!(
        r#"
        INSERT INTO transitions (id, deposit_id, from_state, to_state, attempt, evidence)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        Uuid::new_v4(),
        deposit_id,
        expected,
        target,
        update.attempt,
        writes.evidence
    )
    .execute(&mut **transaction)
    .await?;

    // A deposit that is final when it is credited, or credited when it becomes final, is swept
    // at once by a finalized `Flushed` event already indexed after it.
    super::mark_swept(transaction, Some(deposit_id), &[]).await?;

    // Rendered after every write, so each event's object shows the deposit as this transition
    // leaves it.
    for event in writes.outbox_events {
        super::outbox::enqueue_in(transaction, routes, event, None).await?;
    }

    Ok(ApplyTransitionResult::Applied)
}

/// The scope's unfinalized credit when crediting `deposit_id`, not final yet, would take it past
/// the cap. A transaction-level advisory lock per account and mode serialises credits from this
/// check to commit, and under `READ COMMITTED` the sum, a later statement, sees every credit
/// committed before it. Finality and reversal only lower the sum, so they need no lock.
async fn exceeds_unfinalized_cap(
    transaction: &mut Transaction<'_, Postgres>,
    deposit_id: Uuid,
) -> Result<Option<UnfinalizedCredit>, sqlx::Error> {
    let (account_id, livemode, credit, is_final): (Uuid, bool, Option<String>, bool) =
        sqlx::query_as(
            "SELECT account_id, livemode, credit_minor::text, final_at IS NOT NULL \
             FROM deposits WHERE id = $1",
        )
        .bind(deposit_id)
        .fetch_one(&mut **transaction)
        .await?;
    if is_final {
        return Ok(None);
    }
    let credit = parse_optional_minor_decimal(credit.as_deref())?
        .ok_or_else(|| decode_error("a credited deposit's credit_minor"))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('unfinalized-credit:' || $1, 0))")
        .bind(format!("{account_id}:{livemode}"))
        .execute(&mut **transaction)
        .await?;
    let exposure = unfinalized_credit(&mut **transaction, account_id, livemode, deposit_id).await?;
    Ok((!exposure.admits(credit.value())).then_some(exposure))
}

/// Releases a still-owned lease after an atomic rate-lock race is lost.
pub async fn release_deposit_lease(
    pool: &PgPool,
    deposit_id: Uuid,
    lease_token: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE deposits
        SET lease_token = NULL,
            lease_until = NULL,
            next_attempt_at = now(),
            updated_at = now()
        WHERE id = $1 AND lease_token = $2
        "#,
    )
    .bind(deposit_id)
    .bind(lease_token)
    .execute(pool)
    .await?;
    Ok(())
}
