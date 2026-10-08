//! Treasuries (docs/design/multi-tenant.md D10): the address each account's forwarders pay on a
//! chain, set through the API with a proof, time-locked when a live one changes, and announced as
//! account events.
//!
//! **Proof.** `POST /v1/treasuries/challenge` issues an EIP-4361 (Sign-In with Ethereum) message
//! for `(account, mode, chain, address)`: `domain` is the authority of the service's public origin
//! and `uri` the origin, the statement names the account and mode, the nonce is single-use, and the
//! message expires after [`CHALLENGE_TTL`]. The merchant signs it and submits it unchanged. The
//! signature proves the treasury when either
//!
//! - it is a 65-byte ECDSA signature whose EIP-191 `personal_sign` recovery is the address (an
//!   EOA), or
//! - a contract is deployed at the address at the chain's `finalized` block and its EIP-1271
//!   `isValidSignature(hash, signature)` returns the magic value `0x1626ba7e` there, where `hash` is
//!   the message's EIP-191 hash, on both RPC providers (a Safe, with the owners' signatures of the
//!   Safe message or a `SignMessageLib` approval).
//!
//! An ERC-6492 signature (the magic suffix `0x6492…6492`, for a contract not deployed yet) is
//! refused, as is a contract that is not deployed on the chain: the treasury must exist there.
//! The message is rendered and parsed by the `siwe` crate and the signature recovered by Alloy.
//!
//! **Changes.** The first treasury of a chain, and every test-mode change, applies at once
//! (`treasury.created`, `current`). A later live change is created `pending`
//! (`treasury.created`) and applies [`TIME_LOCK`] after it is proven (`treasury.updated`), unless
//! the merchant cancels it first (`treasury.canceled`); a leaked key therefore cannot redirect
//! new payments unseen. The treasury it replaces becomes `replaced` (`treasury.updated`). When a
//! treasury applies the chain's network of
//! every deposit address of the account and mode is replaced by a forwarder over it, in the same
//! transaction; the replaced forwarders stay watched and credited and keep paying the old treasury,
//! which the forwarder's clone argument fixes for good. Quotes and new networks take the chain's
//! current treasury; quotes issued before keep their address.
//!
//! **Crediting pause.** In an incident, such as a compromised former treasury, the merchant
//! (`POST /v1/treasuries/{id}/pause`) or the operator (admin API) pauses crediting of deposits to
//! every forwarder over a treasury's address: they stay `pending` and no `deposit.credited` is
//! sent until both pauses are lifted, as a `settlement` pause holds them. Neither lifts the
//! other's pause. Each change is audited and announced as `treasury.updated`.

mod proof;
mod reads;
mod worker;

pub use proof::{
    ContractAnswer, ContractSignatures, EvmContractSignatures, UnavailableContractSignatures,
    create_challenge, find_challenge, verify_signature,
};
#[cfg(test)]
use proof::{render_message, statement};
pub(crate) use reads::get_in;
pub use reads::{
    IN_FORCE_TOLERANCE, ListFilter, current, current_on, get, in_force, list, pending_on,
};
pub use worker::TreasuryWorker;

use std::str::FromStr;

use alloy_primitives::Address;
#[cfg(test)]
use alloy_primitives::eip191_hash_message;
use chrono::{DateTime, Duration, Utc};
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Transaction};
use topup_core::route::RouteFile;
use uuid::Uuid;

use crate::api_keys::event_actor;
use crate::audit::{self, Actor};
use crate::deposit_addresses::{self, ChainContracts};
use crate::refunds::{DestinationScreener, DestinationScreening};
use crate::routes::RouteSet;
use crate::tenancy::Scope;

/// How long a later live treasury change waits before it applies.
pub const TIME_LOCK: Duration = Duration::hours(48);
/// How long a challenge for an EOA can be submitted.
pub const CHALLENGE_TTL: Duration = Duration::minutes(10);
/// How long a challenge for an address that holds code (a Safe) can be submitted: its owners
/// collect signatures, or approve the message on chain and wait for `finalized`, which takes
/// longer than an EOA's signature.
pub const CONTRACT_CHALLENGE_TTL: Duration = Duration::hours(24);
/// How often every current treasury is screened again.
pub const RESCREEN_INTERVAL: Duration = Duration::days(1);
/// Treasuries screened per pass of the worker.
const SCREEN_BATCH: i64 = 100;
/// How long spent and expired challenges are kept before they are pruned.
const CHALLENGE_RETENTION: Duration = Duration::days(1);

/// The ERC-6492 wrapper's magic suffix (<https://eips.ethereum.org/EIPS/eip-6492>).
const ERC6492_MAGIC_SUFFIX: [u8; 32] = [
    0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92,
    0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92,
];

/// The treasury event types. They are account security events: delivered to every enabled
/// endpoint of the mode whatever its `enabled_events` (design §11).
pub const EVENT_TYPES: [&str; 3] = ["treasury.created", "treasury.updated", "treasury.canceled"];

/// The public id of a treasury, `trs_` and the hex of its id.
#[must_use]
pub fn public_id(id: Uuid) -> String {
    crate::ids::format(crate::ids::TREASURY, id)
}

/// How the treasury proved control of its address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// An EIP-191 signature recovering to the address.
    Eoa,
    /// A deployed contract's EIP-1271 answer.
    Contract,
}

impl Kind {
    /// The stable API and database code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Eoa => "eoa",
            Self::Contract => "contract",
        }
    }

    fn parse(value: &str) -> Result<Self, TreasuryError> {
        match value {
            "eoa" => Ok(Self::Eoa),
            "contract" => Ok(Self::Contract),
            _ => Err(TreasuryError::DatabaseInvariant),
        }
    }
}

/// Lifecycle of a treasury.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// A live change waiting for its time-lock.
    Pending,
    /// The chain's current treasury.
    Active,
    /// A former treasury, replaced by a later one; forwarders issued over it still pay it.
    Replaced,
    /// A pending change the merchant canceled.
    Canceled,
}

impl Status {
    /// The stable API code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Replaced => "replaced",
            Self::Canceled => "canceled",
        }
    }

    /// Parses an API code.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "replaced" => Some(Self::Replaced),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }

    /// The SQL condition selecting treasuries in this status.
    const fn condition(self) -> &'static str {
        match self {
            Self::Pending => "applied_at IS NULL AND canceled_at IS NULL",
            Self::Active => "applied_at IS NOT NULL AND replaced_at IS NULL",
            Self::Replaced => "replaced_at IS NOT NULL",
            Self::Canceled => "canceled_at IS NOT NULL",
        }
    }
}

/// Why a pending treasury change was canceled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationReason {
    /// The merchant canceled it (`POST /v1/treasuries/{id}/cancel`).
    Requested,
    /// A sanctions list named the treasury when it was due to apply.
    Sanctioned,
}

impl CancellationReason {
    /// The stable API and database code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Sanctioned => "sanctioned",
        }
    }

    fn parse(value: &str) -> Result<Self, TreasuryError> {
        match value {
            "requested" => Ok(Self::Requested),
            "sanctioned" => Ok(Self::Sanctioned),
            _ => Err(TreasuryError::DatabaseInvariant),
        }
    }
}

/// An account's treasury of one chain and mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Treasury {
    /// Treasury identifier.
    pub id: Uuid,
    /// Mode.
    pub livemode: bool,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The treasury address.
    pub address: Address,
    /// How it was proven.
    pub kind: Kind,
    /// Lifecycle status.
    pub status: Status,
    /// When it applies, or applied.
    pub effective_at: DateTime<Utc>,
    /// When it was proven.
    pub created_at: DateTime<Utc>,
    /// When a later treasury replaced it.
    pub replaced_at: Option<DateTime<Utc>>,
    /// When it was canceled.
    pub canceled_at: Option<DateTime<Utc>>,
    /// Why it was canceled.
    pub cancellation_reason: Option<CancellationReason>,
    /// Who paused crediting of deposits to forwarders over the address: `merchant`, `operator`,
    /// both, or neither.
    pub crediting_paused_by: Vec<String>,
}

/// An issued EIP-4361 challenge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Challenge {
    /// Single-use nonce.
    pub nonce: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The address to prove.
    pub address: Address,
    /// The message to sign, exactly.
    pub message: String,
    /// When the message stops being accepted.
    pub expires_at: DateTime<Utc>,
}

/// The service's identity in challenge messages: EIP-4361's `domain` (the authority of the public
/// origin) and `uri` (the origin).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageOrigin {
    domain: String,
    uri: String,
}

impl MessageOrigin {
    /// The identity of the service at `origin`, such as `https://api.example`.
    #[must_use]
    pub fn new(origin: &crate::api::PublicOrigin) -> Self {
        let uri = origin.to_string();
        let domain = uri
            .split_once("://")
            .map_or(uri.as_str(), |(_, authority)| authority)
            .to_owned();
        Self { domain, uri }
    }
}

/// Treasury failure mapped by the API boundary.
#[derive(Debug, thiserror::Error)]
pub enum TreasuryError {
    /// No such treasury in the scope.
    #[error("treasury not found")]
    NotFound,
    /// The message is not an EIP-4361 message, or not the challenge's message.
    #[error("{0}")]
    MessageInvalid(&'static str),
    /// The message's nonce is not a challenge of this account and mode.
    #[error("the message's nonce is not a challenge of this account and mode")]
    ChallengeUnknown,
    /// The challenge expired.
    #[error("the challenge has expired; request a new one")]
    ChallengeExpired,
    /// The challenge was already used.
    #[error("the challenge was already used; request a new one")]
    ChallengeUsed,
    /// The message is for another chain than the request's.
    #[error("the message's chain differs from chain_id")]
    ChainMismatch,
    /// An ERC-6492 signature of a contract that is not deployed.
    #[error("ERC-6492 signatures are not accepted; deploy the treasury on the chain first")]
    Erc6492,
    /// Neither an EOA's nor the deployed contract's signature of the message.
    #[error("the signature does not prove the address")]
    SignatureInvalid,
    /// Not an EOA's signature, and no contract is deployed at the address.
    #[error("no contract is deployed at the address at the chain's finalized block")]
    NotDeployed,
    /// A chain read failed, or the providers disagree; retry.
    #[error("the chain could not be read")]
    Unavailable,
    /// A change of the chain's treasury is already pending.
    #[error("a treasury change is already pending on this chain")]
    ChangePending,
    /// The address is already the chain's treasury.
    #[error("the address is already the chain's treasury")]
    Unchanged,
    /// The treasury is not pending, so it cannot be canceled.
    #[error("the treasury is {}", .0.code())]
    NotPending(Status),
    /// The OS RNG failed.
    #[error("entropy unavailable")]
    EntropyUnavailable,
    /// Persisted data violated an internal invariant.
    #[error("treasury database invariant failed")]
    DatabaseInvariant,
    /// Deposit address networks could not be replaced.
    #[error("{0}")]
    DepositAddresses(#[from] deposit_addresses::DepositAddressError),
    /// PostgreSQL failed.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

/// A verified, screened proof, ready to be recorded.
#[derive(Clone, Debug)]
pub struct Proof {
    /// The challenge answered.
    pub challenge: Challenge,
    /// How the address was proven.
    pub kind: Kind,
    /// The signature, as submitted.
    pub signature: Vec<u8>,
}

/// Records `proof` as the scope's treasury of its chain, using its challenge. The first treasury
/// of a chain and any test-mode change apply at once; a later live change waits [`TIME_LOCK`].
/// `routes` supply the chain's forwarder contracts for the deposit address networks.
pub async fn submit<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &RouteSet,
    scope: Scope,
    actor: &Actor,
    proof: &Proof,
    now: DateTime<Utc>,
) -> Result<Treasury, TreasuryError> {
    let challenge = &proof.challenge;
    let chain_id =
        i64::try_from(challenge.chain_id).map_err(|_| TreasuryError::DatabaseInvariant)?;
    let mut transaction = db.begin().await?;
    lock(&mut transaction, scope, true).await?;
    let used = sqlx::query(
        "UPDATE treasury_challenges SET used_at = $2 \
         WHERE nonce = $1 AND used_at IS NULL AND expires_at > $2",
    )
    .bind(&challenge.nonce)
    .bind(now)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    if used != 1 {
        return Err(TreasuryError::ChallengeUsed);
    }
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM treasuries \
         WHERE account_id = $1 AND livemode = $2 AND chain_id = $3 \
           AND applied_at IS NULL AND canceled_at IS NULL)",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .fetch_one(&mut *transaction)
    .await?;
    if pending {
        return Err(TreasuryError::ChangePending);
    }
    let current = current_on(&mut transaction, scope, challenge.chain_id).await?;
    if current == Some(challenge.address) {
        return Err(TreasuryError::Unchanged);
    }
    let immediate = !scope.livemode() || current.is_none();
    let effective_at = if immediate {
        now
    } else {
        now.checked_add_signed(TIME_LOCK)
            .ok_or(TreasuryError::DatabaseInvariant)?
    };
    let id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO treasuries (
            id, account_id, livemode, chain_id, address, kind, proof_message, proof_signature,
            verified_at, effective_at, screened_at, created_by, created_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $9, $11, $9)
        "#,
    )
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .bind(format!("{:#x}", challenge.address))
    .bind(proof.kind.code())
    .bind(&challenge.message)
    .bind(format!("0x{}", hex::encode(&proof.signature)))
    .bind(now)
    .bind(effective_at)
    .bind(event_actor(actor))
    .execute(&mut *transaction)
    .await?;
    if immediate {
        apply(
            &mut transaction,
            routes,
            scope,
            id,
            now,
            actor,
            Announcement::Created,
        )
        .await?;
    } else {
        record(
            &mut transaction,
            scope,
            id,
            None,
            actor,
            "treasury.created",
            &format!("applies at {}", effective_at.to_rfc3339()),
        )
        .await?;
    }
    let treasury = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok(treasury)
}

/// Cancels the scope's pending treasury `id`.
pub async fn cancel<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    scope: Scope,
    actor: &Actor,
    id: Uuid,
) -> Result<Treasury, TreasuryError> {
    let mut transaction = db.begin().await?;
    lock(&mut transaction, scope, true).await?;
    let treasury = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::NotFound)?;
    if treasury.status != Status::Pending {
        return Err(TreasuryError::NotPending(treasury.status));
    }
    mark_canceled(
        &mut transaction,
        scope,
        id,
        CancellationReason::Requested,
        actor,
        "",
    )
    .await?;
    let treasury = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok(treasury)
}

/// Pauses (`pause`) or resumes crediting, for `owner`, of deposits to every forwarder over the
/// address of the scope's treasury `id`: every treasury of the scope and chain with that address
/// changes, each audited with `reason` and announced as `treasury.updated`. A deposit held by the
/// pause stays `pending` and is credited once no pause remains. Returns treasury `id`.
pub(crate) async fn set_crediting_paused<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    scope: Scope,
    id: Uuid,
    owner: crate::pause::PauseOwner,
    pause: bool,
    actor: &Actor,
    reason: &str,
) -> Result<Treasury, TreasuryError> {
    let mut transaction = db.begin().await?;
    lock(&mut transaction, scope, true).await?;
    let treasury = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::NotFound)?;
    let same_address: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM treasuries \
         WHERE account_id = $1 AND livemode = $2 AND chain_id = $3 AND address = $4 \
           AND ($5::text <> ALL (crediting_paused_by)) = $6 \
         ORDER BY id",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(i64::try_from(treasury.chain_id).map_err(|_| TreasuryError::DatabaseInvariant)?)
    .bind(format!("{:#x}", treasury.address))
    .bind(owner.code())
    .bind(pause)
    .fetch_all(&mut *transaction)
    .await?;
    for changed in same_address {
        let before = snapshot(&mut transaction, scope, changed).await?;
        sqlx::query(if pause {
            "UPDATE treasuries SET crediting_paused_by = \
             ARRAY(SELECT unnest(crediting_paused_by || ARRAY[$2::text]) ORDER BY 1) WHERE id = $1"
        } else {
            "UPDATE treasuries SET crediting_paused_by = array_remove(crediting_paused_by, $2::text) \
             WHERE id = $1"
        })
        .bind(changed)
        .bind(owner.code())
        .execute(&mut *transaction)
        .await?;
        record(
            &mut transaction,
            scope,
            changed,
            Some(&before),
            actor,
            "treasury.updated",
            reason,
        )
        .await?;
    }
    let treasury = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok(treasury)
}

/// Applies every pending treasury whose time-lock ended by `now`, one transaction each, and
/// returns how many applied. Each is screened again first: a treasury a sanctions list now names
/// is canceled instead (`cancellation_reason: sanctioned`, `treasury.canceled`), and one
/// that cannot be screened now, or whose chain has no current route to screen it with, stays
/// pending until a later pass.
pub async fn apply_due(
    pool: &PgPool,
    routes: &RouteSet,
    screening: &dyn DestinationScreener,
    now: DateTime<Utc>,
) -> Result<usize, TreasuryError> {
    let actor = Actor::system("treasury_time_lock");
    let due: Vec<(Uuid, Uuid, bool, i64, String)> = sqlx::query_as(
        r#"
        SELECT id, account_id, livemode, chain_id, address FROM treasuries
        WHERE applied_at IS NULL AND canceled_at IS NULL AND effective_at <= $1
        ORDER BY effective_at, id
        LIMIT $2
        "#,
    )
    .bind(now)
    .bind(SCREEN_BATCH)
    .fetch_all(pool)
    .await?;
    let mut applied = 0_usize;
    for (id, account_id, livemode, chain_id, address) in due {
        let scope = Scope::new(account_id, livemode);
        let (chain_id, address) = parse_chain_address(chain_id, &address)?;
        let Some(route) = chain_route(routes, livemode, chain_id) else {
            tracing::warn!(
                chain_id,
                "a due treasury's chain has no current route to screen it"
            );
            continue;
        };
        let sanctioned = match screening.screen(route, address).await {
            DestinationScreening::Clear => false,
            DestinationScreening::Sanctioned => true,
            DestinationScreening::Unavailable => {
                tracing::warn!(
                    chain_id,
                    "screening a due treasury is unavailable; retrying"
                );
                continue;
            }
        };
        let mut transaction = pool.begin().await?;
        // The scope lock first, as submissions and cancellations take it, then the row.
        lock(&mut transaction, scope, true).await?;
        let still_due: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM treasuries WHERE id = $1 \
             AND applied_at IS NULL AND canceled_at IS NULL AND effective_at <= $2)",
        )
        .bind(id)
        .bind(now)
        .fetch_one(&mut *transaction)
        .await?;
        if still_due && sanctioned {
            tracing::error!(
                tags.alert = "TopupTreasurySanctioned",
                treasury = %public_id(id),
                chain_id,
                "a pending treasury is on a sanctions list; its change was canceled"
            );
            mark_canceled(
                &mut transaction,
                scope,
                id,
                CancellationReason::Sanctioned,
                &actor,
                "the treasury is on a sanctions list at its effective time",
            )
            .await?;
        } else if still_due {
            // Recorded when this change applies, under its scope's lock, not when the pass
            // started: a pass screens up to a batch of changes first, and a restore's re-issue
            // takes a change as in force from here (`in_force`).
            let applied_at: DateTime<Utc> =
                sqlx::query_scalar("SELECT GREATEST($1::timestamptz, clock_timestamp())")
                    .bind(now)
                    .fetch_one(&mut *transaction)
                    .await?;
            sqlx::query("UPDATE treasuries SET screened_at = $2 WHERE id = $1")
                .bind(id)
                .bind(applied_at)
                .execute(&mut *transaction)
                .await?;
            let before = snapshot(&mut transaction, scope, id).await?;
            apply(
                &mut transaction,
                routes,
                scope,
                id,
                applied_at,
                &actor,
                Announcement::Updated(&before),
            )
            .await?;
            applied = applied.saturating_add(1);
        }
        transaction.commit().await?;
    }
    Ok(applied)
}

/// Why [`restore_application`] refused.
#[derive(Debug, thiserror::Error)]
pub enum RestoreApplicationError {
    /// The restored treasury is not the one the delivery shows applying, its change cannot apply
    /// under the time-lock, or a sanctions list names it now.
    #[error("{0}")]
    Refused(&'static str),
    /// The treasury could not be read or applied.
    #[error("{0}")]
    Treasury(#[from] TreasuryError),
}

impl From<sqlx::Error> for RestoreApplicationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Treasury(TreasuryError::Database(error))
    }
}

/// A treasury change applying, as the `treasury.updated` the time-lock sent for it shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Application {
    /// The treasury.
    pub id: Uuid,
    /// Its chain.
    pub chain_id: u64,
    /// Its address.
    pub address: Address,
    /// When it applied, the event's `created`: when the time-lock's transaction started, at most
    /// seconds before the `applied_at` it recorded under the scope's lock.
    pub applied_at: DateTime<Utc>,
}

/// Applies again, while the service is frozen after a restore (`crate::restore_mode`), the
/// scope's pending treasury whose change applied after the restore point and was lost with it, as
/// `application`, read from the delivered `treasury.updated` the service signed, shows it (the
/// caller verified the delivery). Otherwise it would apply only after the unfreeze, and the
/// deposit addresses and quotes issued over it meanwhile could not be re-issued before it.
///
/// The change's own rules hold: it is the restored treasury of that chain and address, proven
/// when it was submitted and still pending (a cancellation the merchant made after the restore
/// point is re-applied first, so its change never applied), and its time-lock ended by
/// `applied_at`. Like [`apply_due`], it needs a current route of the chain to screen it with (it
/// stays pending otherwise), and it is screened again: a sanctions list naming it now refuses it
/// (at the unfreeze [`apply_due`] cancels it), and a clear answer records `screened_at`. When
/// screening is unavailable, the screening [`apply_due`] made before applying it, which the signed
/// event attests, stands, and `screened_at` keeps its restored value, so [`rescreen_due`] screens
/// it again once the service is unfrozen.
///
/// It applies as [`apply_due`] applies it, at `applied_at`, so the treasury it replaced was in
/// force until then ([`in_force`]), audited with `audit_reason` as `restore.treasury_apply` in the
/// same transaction. No event is sent again: the merchant received the change's events when it
/// first applied, and new ones would carry the restore's time, which a later restore would take as
/// the change's. Returns the treasury and whether it applied now: a treasury in force already is
/// returned as it is.
pub async fn restore_application(
    pool: &PgPool,
    routes: &RouteSet,
    screening: &dyn DestinationScreener,
    scope: Scope,
    application: Application,
    actor: &Actor,
    audit_reason: &str,
) -> Result<(Treasury, bool), RestoreApplicationError> {
    let Application {
        id,
        chain_id,
        address,
        applied_at,
    } = application;
    let refused = |reason| Err(RestoreApplicationError::Refused(reason));
    let treasury = get(pool, scope, id).await?.ok_or(TreasuryError::NotFound)?;
    if (treasury.chain_id, treasury.address) != (chain_id, address) {
        return refused("the restored treasury has another chain or address than the delivery's");
    }
    match treasury.status {
        Status::Active | Status::Replaced => return Ok((treasury, false)),
        Status::Canceled => {
            return refused(
                "the restored treasury is canceled, but the service signed that it applied: \
                 escalate",
            );
        }
        Status::Pending => {}
    }
    // In whole seconds, as the event renders `created`.
    if treasury.effective_at.timestamp() > applied_at.timestamp() || applied_at > Utc::now() {
        return refused("the delivery does not show the change applying after its time-lock");
    }
    // As `apply_due`, which leaves a change pending while no current route can screen it.
    let Some(route) = chain_route(routes, scope.livemode(), chain_id) else {
        return refused(
            "no current route of the mode is on the treasury's chain to screen it: it stays \
             pending, as the time-lock leaves it",
        );
    };
    let screened = match screening.screen(route, address).await {
        DestinationScreening::Clear => true,
        DestinationScreening::Sanctioned => {
            return refused(
                "a sanctions list names the treasury now: escalate; at the unfreeze its change is \
                 canceled",
            );
        }
        DestinationScreening::Unavailable => false,
    };
    let mut transaction = pool.begin().await?;
    lock(&mut transaction, scope, true).await?;
    let current = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::DatabaseInvariant)?;
    if current.status != Status::Pending {
        return Ok((current, false));
    }
    if screened {
        sqlx::query("UPDATE treasuries SET screened_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
    }
    apply(
        &mut transaction,
        routes,
        scope,
        id,
        applied_at,
        actor,
        Announcement::Restored,
    )
    .await?;
    audit_change(
        &mut transaction,
        scope,
        id,
        actor,
        "restore.treasury_apply",
        audit_reason,
    )
    .await?;
    let treasury = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(TreasuryError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok((treasury, true))
}

/// What one pass of [`rescreen_due`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rescreen {
    /// Current treasuries screened clear.
    pub clear: usize,
    /// Current treasuries a sanctions list names; their accounts' `quotes` and `settlement` are
    /// paused.
    pub sanctioned: usize,
}

/// Screens again every current treasury last screened [`RESCREEN_INTERVAL`] or more before `now`
/// (design §8: "screened when set and daily"), up to a batch per pass. A treasury a sanctions list
/// now names pauses its account's `quotes` and `settlement` (audited, `account.updated`, and the
/// `TopupTreasurySanctioned` alert); the operator lifts the pause after review. A treasury that
/// cannot be screened now is tried again on a later pass.
pub async fn rescreen_due(
    pool: &PgPool,
    routes: &RouteSet,
    screening: &dyn DestinationScreener,
    now: DateTime<Utc>,
) -> Result<Rescreen, TreasuryError> {
    let before = now
        .checked_sub_signed(RESCREEN_INTERVAL)
        .ok_or(TreasuryError::DatabaseInvariant)?;
    let current: Vec<(Uuid, Uuid, bool, i64, String)> = sqlx::query_as(
        r#"
        SELECT id, account_id, livemode, chain_id, address FROM treasuries
        WHERE applied_at IS NOT NULL AND replaced_at IS NULL AND screened_at <= $1
        ORDER BY screened_at, id
        LIMIT $2
        "#,
    )
    .bind(before)
    .bind(SCREEN_BATCH)
    .fetch_all(pool)
    .await?;
    let actor = Actor::system("treasury_screening");
    let mut outcome = Rescreen::default();
    for (id, account_id, livemode, chain_id, address) in current {
        let (chain_id, address) = parse_chain_address(chain_id, &address)?;
        let Some(route) = chain_route(routes, livemode, chain_id) else {
            continue;
        };
        let answer = screening.screen(route, address).await;
        if answer == DestinationScreening::Unavailable {
            tracing::warn!(chain_id, "re-screening a treasury is unavailable; retrying");
            continue;
        }
        let mut transaction = pool.begin().await?;
        match answer {
            DestinationScreening::Unavailable => {}
            DestinationScreening::Clear => outcome.clear = outcome.clear.saturating_add(1),
            DestinationScreening::Sanctioned => {
                outcome.sanctioned = outcome.sanctioned.saturating_add(1);
                tracing::error!(
                    tags.alert = "TopupTreasurySanctioned",
                    treasury = %public_id(id),
                    chain_id,
                    "a current treasury is on a sanctions list; the account's quotes and \
                     settlement are paused"
                );
                crate::pause::mutate_account_scopes_in(
                    &mut transaction,
                    routes,
                    account_id,
                    crate::pause::PauseOwner::Operator,
                    &["quotes", "settlement"],
                    true,
                    &actor,
                    &format!(
                        "treasury {} on chain {chain_id} is on a sanctions list",
                        public_id(id)
                    ),
                )
                .await?;
            }
        }
        sqlx::query("UPDATE treasuries SET screened_at = $2 WHERE id = $1")
            .bind(id)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
    }
    Ok(outcome)
}

/// Re-screens pending changes immediately when a new verified publication activates.
/// Positive hits cancel through the same audited path as a hit at time-lock expiry.
pub async fn rescreen_pending(
    pool: &PgPool,
    routes: &RouteSet,
    screening: &dyn DestinationScreener,
) -> Result<(), TreasuryError> {
    let pending: Vec<(Uuid, Uuid, bool, i64, String)> = sqlx::query_as(
        "SELECT id,account_id,livemode,chain_id,address FROM treasuries WHERE applied_at IS NULL AND canceled_at IS NULL ORDER BY id")
        .fetch_all(pool).await?;
    let actor = Actor::system("sanctions_refresh");
    for (id, account_id, livemode, chain, address) in pending {
        let (chain, address) = parse_chain_address(chain, &address)?;
        let Some(route) = chain_route(routes, livemode, chain) else {
            continue;
        };
        match screening.screen(route, address).await {
            DestinationScreening::Clear => continue,
            DestinationScreening::Unavailable => return Err(TreasuryError::Unavailable),
            DestinationScreening::Sanctioned => {}
        }
        let scope = Scope::new(account_id, livemode);
        let mut tx = pool.begin().await?;
        lock(&mut tx, scope, true).await?;
        let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM treasuries WHERE id=$1 AND applied_at IS NULL AND canceled_at IS NULL)")
            .bind(id).fetch_one(&mut *tx).await?;
        if pending {
            mark_canceled(
                &mut tx,
                scope,
                id,
                CancellationReason::Sanctioned,
                &actor,
                "active sanctions snapshot names pending treasury",
            )
            .await?;
            tracing::error!(
                tags.alert = "TopupTreasurySanctioned",
                "active sanctions snapshot canceled pending treasury"
            );
        }
        tx.commit().await?;
    }
    Ok(())
}

/// A current route of the mode on `chain_id` used to screen its treasuries.
fn chain_route(routes: &RouteSet, livemode: bool, chain_id: u64) -> Option<&RouteFile> {
    routes
        .current_in(livemode)
        .find(|route| route.chain.chain_id == chain_id)
}

fn parse_chain_address(chain_id: i64, address: &str) -> Result<(u64, Address), TreasuryError> {
    Ok((
        u64::try_from(chain_id).map_err(|_| TreasuryError::DatabaseInvariant)?,
        Address::from_str(address).map_err(|_| TreasuryError::DatabaseInvariant)?,
    ))
}

/// How [`apply`] announces a change.
enum Announcement<'a> {
    /// `treasury.created`: the treasury applies as it is submitted.
    Created,
    /// `treasury.updated` with `pending`, its representation until now: its time-lock ended.
    Updated(&'a serde_json::Value),
    /// Audited only: a change applied again after a restore, whose events the merchant received
    /// when it first applied. New ones would carry the time of the restore, not of the change.
    Restored,
}

/// Makes treasury `id` its chain's current one at `now`: the former one is replaced, the chain's
/// network of every deposit address of the scope is replaced by a forwarder over it, and the
/// change is audited and announced as `announcement` says, with `treasury.updated` for the replaced
/// one unless it is [`Announcement::Restored`]. The caller holds the scope's exclusive lock.
async fn apply(
    transaction: &mut Transaction<'_, Postgres>,
    routes: &RouteSet,
    scope: Scope,
    id: Uuid,
    now: DateTime<Utc>,
    actor: &Actor,
    announcement: Announcement<'_>,
) -> Result<(), TreasuryError> {
    let (chain_id, address): (i64, String) =
        sqlx::query_as("SELECT chain_id, address FROM treasuries WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut **transaction)
            .await?;
    let chain = u64::try_from(chain_id).map_err(|_| TreasuryError::DatabaseInvariant)?;
    if !routes.chain_ready(chain) {
        return Err(TreasuryError::Unavailable);
    }
    crate::db::rpc::guard_in(transaction, chain).await?;
    let former: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM treasuries \
         WHERE account_id = $1 AND livemode = $2 AND chain_id = $3 \
           AND applied_at IS NOT NULL AND replaced_at IS NULL \
         FOR UPDATE",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let former = match former {
        Some(former) => Some((former, snapshot(transaction, scope, former).await?)),
        None => None,
    };
    if let Some((former, _)) = &former {
        sqlx::query("UPDATE treasuries SET replaced_at = $2 WHERE id = $1")
            .bind(former)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
    }
    sqlx::query("UPDATE treasuries SET applied_at = $2 WHERE id = $1")
        .bind(id)
        .bind(now)
        .execute(&mut **transaction)
        .await?;
    let chain_id = u64::try_from(chain_id).map_err(|_| TreasuryError::DatabaseInvariant)?;
    let treasury = Address::from_str(&address).map_err(|_| TreasuryError::DatabaseInvariant)?;
    let reason = match routes
        .current_in(scope.livemode())
        .find(|route| route.chain.chain_id == chain_id)
    {
        Some(route) => {
            let chain = ChainContracts::of(route).with_treasury(treasury);
            let moved = deposit_addresses::replace_networks(transaction, scope, chain).await?;
            if former.is_none() {
                String::new()
            } else {
                format!(
                    "{moved} deposit address networks on chain {chain_id} now pay it; the \
                     replaced forwarders keep paying the former treasury and are still credited"
                )
            }
        }
        // No route of the mode serves the chain now, so no network is issued there; creation
        // adds them over this treasury when a route returns.
        None => {
            tracing::warn!(
                chain_id,
                "treasury applied on a chain without a current route"
            );
            String::new()
        }
    };
    let replaced = format!("replaced by {}", public_id(id));
    match announcement {
        Announcement::Restored => {
            let reason = format!(
                "applied again after a restore, at {}; {reason}",
                now.to_rfc3339()
            );
            audit_change(transaction, scope, id, actor, "treasury.updated", &reason).await?;
            if let Some((former, _)) = former {
                audit_change(
                    transaction,
                    scope,
                    former,
                    actor,
                    "treasury.updated",
                    &replaced,
                )
                .await?;
            }
        }
        Announcement::Created | Announcement::Updated(_) => {
            let (event_type, before) = match announcement {
                Announcement::Updated(before) => ("treasury.updated", Some(before)),
                _ => ("treasury.created", None),
            };
            record(transaction, scope, id, before, actor, event_type, &reason).await?;
            if let Some((former, before)) = former {
                record(
                    transaction,
                    scope,
                    former,
                    Some(&before),
                    actor,
                    "treasury.updated",
                    &replaced,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// The API representation of the scope's treasury `id`, as an event's `data.object`.
async fn snapshot(
    connection: &mut PgConnection,
    scope: Scope,
    id: Uuid,
) -> Result<serde_json::Value, TreasuryError> {
    let treasury = get_in(connection, scope, id)
        .await?
        .ok_or(TreasuryError::DatabaseInvariant)?;
    Ok(crate::db::to_object(&crate::api::treasury_object(
        &treasury,
    ))?)
}

/// Cancels pending treasury `id` for `reason`, with its audit row and `treasury.canceled`.
async fn mark_canceled(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
    reason: CancellationReason,
    actor: &Actor,
    note: &str,
) -> Result<(), TreasuryError> {
    sqlx::query(
        "UPDATE treasuries SET canceled_at = now(), cancellation_reason = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(reason.code())
    .execute(&mut **transaction)
    .await?;
    record(
        transaction,
        scope,
        id,
        None,
        actor,
        "treasury.canceled",
        note,
    )
    .await
}

/// The audit row of a change to treasury `id`.
async fn audit_change(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
    actor: &Actor,
    action: &str,
    reason: &str,
) -> Result<(), TreasuryError> {
    audit::insert(
        &mut **transaction,
        &audit::Entry {
            account_id: Some(scope.account_id()),
            actor,
            action,
            subject: &format!("treasury:{}", public_id(id)),
            reason,
        },
    )
    .await?;
    Ok(())
}

/// The audit row and event of a change to treasury `id`; `before` is its representation before
/// an update, for `previous_attributes`.
async fn record(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
    before: Option<&serde_json::Value>,
    actor: &Actor,
    event_type: &str,
    reason: &str,
) -> Result<(), TreasuryError> {
    audit_change(transaction, scope, id, actor, event_type, reason).await?;
    let object = snapshot(transaction, scope, id).await?;
    let event = crate::db::NewOutboxEvent::new(
        event_type,
        scope,
        crate::db::EventObject::Treasury(id),
        actor,
    );
    crate::db::enqueue_rendered_in(
        transaction,
        &event,
        &crate::db::event_data(object, before),
        None,
        true,
    )
    .await?;
    Ok(())
}

/// Serializes treasury changes of the scope (exclusive) against issuers of forwarders, which read
/// the current treasuries (shared), so no forwarder is issued over a treasury being replaced.
pub(crate) async fn lock(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    exclusive: bool,
) -> Result<(), sqlx::Error> {
    let query = if exclusive {
        "SELECT pg_advisory_xact_lock(hashtextextended('treasury:' || $1, 0))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('treasury:' || $1, 0))"
    };
    sqlx::query(query)
        .bind(format!("{}:{}", scope.account_id(), scope.livemode()))
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn origin() -> MessageOrigin {
        MessageOrigin::new(
            &crate::api::PublicOrigin::parse("https://api.example:8443").expect("origin"),
        )
    }

    #[test]
    fn the_challenge_is_an_eip4361_message_naming_the_account_mode_and_chain() {
        let issued = DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let message = render_message(
            &origin(),
            statement("acct_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10", true),
            1,
            address!("0x6Da01670d8fc844e736095918bbE11fE8D564163"),
            "0123456789abcdef0123456789abcdef",
            issued,
            issued + CHALLENGE_TTL,
        )
        .expect("message");
        assert_eq!(
            message,
            "api.example:8443 wants you to sign in with your Ethereum account:\n\
             0x6Da01670d8fc844e736095918bbE11fE8D564163\n\
             \n\
             Set this address as the live mode treasury of acct_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10 \
             on Phala Pay.\n\
             \n\
             URI: https://api.example:8443\n\
             Version: 1\n\
             Chain ID: 1\n\
             Nonce: 0123456789abcdef0123456789abcdef\n\
             Issued At: 2026-09-28T12:00:00.000Z\n\
             Expiration Time: 2026-09-28T12:10:00.000Z"
        );
        let parsed = siwe::Message::from_str(&message).expect("parses");
        assert_eq!(parsed.to_string(), message);
        // The personal_sign hash Alloy computes over the message is the one EIP-4361 defines.
        assert_eq!(
            eip191_hash_message(message.as_bytes()).0,
            parsed.eip191_hash().expect("hash")
        );
    }

    #[tokio::test]
    async fn an_erc6492_wrapped_signature_is_refused_before_any_chain_read() {
        let challenge = Challenge {
            nonce: "0123456789abcdef0123456789abcdef".to_owned(),
            chain_id: 1,
            address: Address::repeat_byte(0x11),
            message: "message".to_owned(),
            expires_at: Utc::now(),
        };
        let mut wrapped = vec![0xab; 96];
        wrapped.extend_from_slice(&ERC6492_MAGIC_SUFFIX);
        assert!(matches!(
            verify_signature(&UnavailableContractSignatures, &challenge, &wrapped).await,
            Err(TreasuryError::Erc6492)
        ));
    }
}
