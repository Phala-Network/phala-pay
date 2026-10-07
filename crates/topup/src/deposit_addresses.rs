//! Deposit addresses (docs/design/multi-tenant.md "Deposit addresses"): a customer's persistent,
//! rotatable forwarder address, one for every chain and every supported asset (D16, amended
//! 2026-09-28 by the owner's decision: one address per customer, as exchanges give).
//!
//! A deposit address is the same forwarder a quote gets, `CREATE2` over the treasury and a salt,
//! but its salt is derived from the customer and a version and names no chain or asset
//! ([`topup_core::address::deposit_address_salt`]), so the merchant recomputes it offline. The
//! factory and implementation have one address on every chain, so the forwarder is the same
//! address on every chain whose treasury is the same address; a chain whose treasury differs has
//! its own address, which the address's network for that chain shows.
//!
//! Each chain's forwarder is an `addresses` row owned by the deposit address (a network). It
//! carries no price: a transfer of any supported token of the chain to it, active or retired, is
//! credited at spot through the quote pipeline (fast credit, reversal, events, sweeps, refunds,
//! reconciliation), which reads the forwarder's row and finds no quote; an unsupported token is
//! rejected as for a quote.
//!
//! Creation is idempotent per customer and mode: it returns the active address, adding a network
//! for every issuable chain that has none. Rotation retires the address and issues the next
//! version on every issuable chain. Retired addresses, and networks superseded by a treasury
//! change, keep being scanned and credited, and keep paying the treasury they were issued for.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use alloy_primitives::{Address as EvmAddress, B256};
use chrono::{DateTime, Utc};
use sqlx::types::Json;
use sqlx::{Acquire, FromRow, PgConnection, PgPool, Postgres, QueryBuilder, Transaction};
use topup_core::address::{deposit_address_salt, forwarder_address};
use topup_core::route::RouteFile;
use uuid::Uuid;

use crate::api::error::ApiError;
use crate::api::metadata::MetadataUpdate;
use crate::audit::{self, Actor};
use crate::db::{Account, Customer};
use crate::tenancy::Scope;

/// Rotations one customer may make in a rolling hour.
pub const MAX_ROTATIONS_PER_HOUR: i64 = 10;

/// The columns of [`DepositAddressRow`]; callers append the `WHERE` clause.
const SELECT: &str = r#"
    SELECT deposit_address.id, account.public_id AS account_public_id, deposit_address.livemode,
           customer.client_reference_id, deposit_address.version, deposit_address.status,
           deposit_address.created_at, deposit_address.retired_at, deposit_address.metadata
    FROM deposit_addresses AS deposit_address
    JOIN customers AS customer ON customer.id = deposit_address.customer_id
    JOIN accounts AS account ON account.id = deposit_address.account_id
"#;

/// Lifecycle of a deposit address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// The address creation returns for the customer.
    Active,
    /// Replaced by a newer version; payments to it are still credited.
    Retired,
}

impl Status {
    /// The stable API and database code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }

    fn parse(value: &str) -> Result<Self, DepositAddressError> {
        match value {
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            _ => Err(DepositAddressError::DatabaseInvariant),
        }
    }
}

/// A chain's forwarder contracts: where a deposit address may get a network. The treasury a new
/// forwarder there pays is the account's current treasury of the chain, read when it is issued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainContracts {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Forwarder factory, at the same address on every chain.
    pub factory: EvmAddress,
    /// Forwarder implementation, at the same address on every chain.
    pub implementation: EvmAddress,
}

impl ChainContracts {
    /// The contracts of `route`'s chain.
    #[must_use]
    pub const fn of(route: &RouteFile) -> Self {
        Self {
            chain_id: route.chain.chain_id,
            factory: route.chain.contracts.forwarder_factory,
            implementation: route.chain.contracts.implementation,
        }
    }

    /// The chain with the treasury its new forwarders pay.
    #[must_use]
    pub const fn with_treasury(self, treasury: EvmAddress) -> Chain {
        Chain {
            chain_id: self.chain_id,
            factory: self.factory,
            implementation: self.implementation,
            treasury,
        }
    }
}

/// A chain a deposit address gets a forwarder on: the `CREATE2` contracts and the treasury a new
/// forwarder there pays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Chain {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Forwarder factory, at the same address on every chain.
    pub factory: EvmAddress,
    /// Forwarder implementation, at the same address on every chain.
    pub implementation: EvmAddress,
    /// The treasury new forwarders on the chain pay.
    pub treasury: EvmAddress,
}

/// A deposit address's current forwarder on one chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Network {
    /// The forwarder's `addresses` row.
    pub address_id: Uuid,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Forwarder address on the chain.
    pub address: EvmAddress,
    /// The treasury the forwarder pays, its clone argument.
    pub treasury: EvmAddress,
}

/// A customer's deposit address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositAddress {
    /// Deposit address identifier.
    pub id: Uuid,
    /// Mode.
    pub livemode: bool,
    /// The customer's `client_reference_id`.
    pub client_reference_id: String,
    /// Version among the customer's addresses, from 1.
    pub version: u64,
    /// CREATE2 salt, the same on every chain.
    pub salt: B256,
    /// Lifecycle status.
    pub status: Status,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Retirement time.
    pub retired_at: Option<DateTime<Utc>>,
    /// The merchant's metadata (`crate::api` validates it).
    pub metadata: BTreeMap<String, String>,
    /// The current forwarder on each chain it was issued on, by chain id. Networks superseded by
    /// a treasury change are not listed; they are still credited.
    pub networks: Vec<Network>,
}

/// Deposit address failure mapped by the API boundary.
#[derive(Debug, thiserror::Error)]
pub enum DepositAddressError {
    /// The address does not exist in the scope.
    #[error("deposit address not found")]
    NotFound,
    /// The address was already rotated; rotate the customer's active address instead.
    #[error("deposit address is retired")]
    Retired,
    /// The account's cap on active deposit addresses in this mode is reached.
    #[error("the account has {0} active deposit addresses, its cap")]
    CapReached(i64),
    /// The customer rotated too often in the last hour; a rotation is admitted again after
    /// `retry_after` seconds.
    #[error("deposit address rotation limit exceeded")]
    RateLimited {
        /// Seconds until the oldest counted rotation leaves the hour.
        retry_after: u64,
    },
    /// No chain accepts new networks (every chain of the mode is frozen or paused), so no address
    /// can be issued.
    #[error("no chain accepts new deposit addresses")]
    NoChain,
    /// The chain's permanent pilot quota counts every address ever issued.
    #[error("permanent chain address capacity reached")]
    AddressCapacityReached,
    /// The chain's dual coverage is not ready for issuance yet.
    #[error("chain is not ready")]
    ChainUnavailable,
    /// The account has no treasury on any chain that accepts new networks.
    #[error("no treasury is set on a chain that accepts new deposit addresses")]
    NoTreasury,
    /// Request input is invalid.
    #[error("{0}")]
    InvalidInput(&'static str),
    /// The merged metadata breaks a limit.
    #[error("invalid metadata")]
    Metadata(ApiError),
    /// Persisted data violated an internal invariant.
    #[error("deposit address database invariant failed")]
    DatabaseInvariant,
    /// PostgreSQL rejected or failed the operation.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
}

/// Returns `customer`'s active address, issuing one on `chains` when there is none.
///
/// Networks are issued over the account's current treasury of each chain; a chain without one gets
/// no network. An existing address gets a network on each of `chains` it has none on, and a new
/// network on each chain whose treasury is no longer the one its network pays (the old network is
/// superseded, and still credited). `chains` are the issuable chains of the mode; an existing
/// address's networks on other chains are kept.
///
/// `metadata`, the request's, is merged into the returned address's, as an update would.
///
/// Returns the address and whether it was issued by this call.
pub async fn create<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    account: &Account,
    customer: &Customer,
    chains: &[ChainContracts],
    metadata: Option<&MetadataUpdate>,
) -> Result<(DepositAddress, bool), DepositAddressError> {
    if customer.account_id != account.id {
        return Err(DepositAddressError::NotFound);
    }
    let scope = Scope::new(account.id, customer.livemode);
    let mut transaction = db.begin().await?;
    lock_customer(&mut transaction, customer).await?;
    let issuable = chains;
    let chains = with_treasuries(&mut transaction, scope, issuable).await?;
    let merge = |current: BTreeMap<String, String>| match metadata {
        Some(update) => update.apply(current).map_err(DepositAddressError::Metadata),
        None => Ok(current),
    };
    let active = sqlx::query_as::<_, (Uuid, i64, Json<BTreeMap<String, String>>)>(
        "SELECT id, version, metadata FROM deposit_addresses \
         WHERE customer_id = $1 AND status = 'active' FOR UPDATE",
    )
    .bind(customer.id)
    .fetch_optional(&mut *transaction)
    .await?;
    let (id, issued) = match active {
        Some((id, version, Json(current))) => {
            let merged = merge(current.clone())?;
            if merged != current {
                sqlx::query("UPDATE deposit_addresses SET metadata = $2 WHERE id = $1")
                    .bind(id)
                    .bind(Json(&merged))
                    .execute(&mut *transaction)
                    .await?;
            }
            let version =
                u64::try_from(version).map_err(|_| DepositAddressError::DatabaseInvariant)?;
            let salt = deposit_address_salt(
                &account.public_id,
                customer.livemode,
                &customer.client_reference_id,
                version,
            );
            sync_networks(&mut transaction, scope, id, salt, &chains).await?;
            (id, false)
        }
        None => {
            let merged = merge(BTreeMap::new())?;
            require_chains(issuable, &chains)?;
            check_cap(&mut transaction, scope).await?;
            let id = issue(
                &mut transaction,
                scope,
                &account.public_id,
                customer,
                &chains,
                &merged,
                Uuid::new_v4(),
            )
            .await?;
            (id, true)
        }
    };
    let address = get_in(&mut transaction, scope, id)
        .await?
        .ok_or(DepositAddressError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok((address, issued))
}

/// Retires the scope's active address `id` and issues the next version for the same customer on
/// `chains`, the issuable chains of the mode. The new version carries the retired one's metadata.
pub async fn rotate<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    account: &Account,
    scope: Scope,
    actor: &Actor,
    id: Uuid,
    chains: &[ChainContracts],
) -> Result<DepositAddress, DepositAddressError> {
    let mut transaction = db.begin().await?;
    let customer = sqlx::query_as::<_, (Uuid, String, Vec<String>)>(
        r#"
        SELECT customer.id, customer.client_reference_id, customer.paused_scopes
        FROM deposit_addresses AS deposit_address
        JOIN customers AS customer ON customer.id = deposit_address.customer_id
        WHERE deposit_address.id = $1 AND deposit_address.account_id = $2
          AND deposit_address.livemode = $3
        "#,
    )
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(&mut *transaction)
    .await?
    .map(|(id, client_reference_id, paused_scopes)| Customer {
        id,
        account_id: scope.account_id(),
        livemode: scope.livemode(),
        client_reference_id,
        paused_scopes,
    })
    .ok_or(DepositAddressError::NotFound)?;
    // Creations and rotations of one customer are serialized by its row lock.
    lock_customer(&mut transaction, &customer).await?;
    let issuable = chains;
    let chains = with_treasuries(&mut transaction, scope, issuable).await?;
    let current = sqlx::query_as::<_, (String, Json<BTreeMap<String, String>>)>(
        "SELECT status, metadata FROM deposit_addresses WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *transaction)
    .await?;
    let (status, Json(metadata)) = current;
    if Status::parse(&status)? == Status::Retired {
        return Err(DepositAddressError::Retired);
    }
    let recent: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM deposit_addresses
        WHERE customer_id = $1 AND status = 'retired' AND retired_at >= now() - interval '1 hour'
        "#,
    )
    .bind(customer.id)
    .fetch_one(&mut *transaction)
    .await?;
    if recent >= MAX_ROTATIONS_PER_HOUR {
        // Admitted again when the `MAX_ROTATIONS_PER_HOUR`-th newest rotation leaves the hour.
        let seconds: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT GREATEST(1, ceil(extract(epoch FROM retired_at + interval '1 hour' - now())))::bigint
            FROM deposit_addresses
            WHERE customer_id = $1 AND status = 'retired'
              AND retired_at >= now() - interval '1 hour'
            ORDER BY retired_at DESC
            OFFSET $2 LIMIT 1
            "#,
        )
        .bind(customer.id)
        .bind(MAX_ROTATIONS_PER_HOUR - 1)
        .fetch_optional(&mut *transaction)
        .await?;
        return Err(DepositAddressError::RateLimited {
            retry_after: seconds
                .and_then(|seconds| u64::try_from(seconds).ok())
                .unwrap_or(3600),
        });
    }
    require_chains(issuable, &chains)?;
    retire(&mut transaction, id).await?;
    let issued = issue(
        &mut transaction,
        scope,
        &account.public_id,
        &customer,
        &chains,
        &metadata,
        Uuid::new_v4(),
    )
    .await?;
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(scope.account_id()),
            actor,
            action: "deposit_address.rotate",
            subject: &format!("deposit_address:{}", public_id(id)),
            reason: &format!("API request; replaced by {}", public_id(issued)),
        },
    )
    .await?;
    let address = get_in(&mut transaction, scope, issued)
        .await?
        .ok_or(DepositAddressError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok(address)
}

/// How far past a customer's latest restored version [`reissue`] goes: it derives addresses this
/// far to find one, and brings back a version at most this far past it, so one call issues at most
/// this many versions. A customer further behind is re-issued in steps (32, 64, ...).
pub const REISSUE_VERSIONS_AHEAD: u64 = 32;

/// The version of a customer's deposit address [`reissue`] brings back: `version`, or the version
/// whose address over a treasury of the account in force since the restore point is `address`;
/// with both, they must agree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReissueTarget {
    /// The version.
    pub version: Option<u64>,
    /// An address of the version.
    pub address: Option<EvmAddress>,
}

/// Re-issues after a restore the deposit address of the customer `client_reference_id` that was given out after the restore
/// point and lost with it (docs/design/multi-tenant.md §13). The salt is derived from the account,
/// mode, customer, and version, so the address is the one the merchant holds over the treasury its
/// network paid: `target.address` is looked for over every treasury of the account in force since
/// `restore_point`, less `crate::treasuries::IN_FORCE_TOLERANCE` (`crate::treasuries::in_force`; a
/// restore without one is refused). Each version
/// the call brings back, the one of `target.version` alone included, gets a network over every
/// such treasury of each chain, superseded when it is no longer the chain's current one, so a
/// payment to any address the merchant was given for it since the restore point is credited.
/// Versions between the restored latest one and `target` are issued retired, as the rotations that
/// issued them left them; the active one is retired. A version more than
/// [`REISSUE_VERSIONS_AHEAD`] past the latest one is refused. Each new network is backfilled from the
/// chain's cursor in `backfill_from` (the restored cursor), so the rescan finds payments made to it
/// since. `id` keeps the `da_` id the merchant holds, and
/// `client_secret_hash` the SHA-256 of a client secret of it the caller checked, so the payer's
/// page reads the address again.
///
/// A version the customer already has is returned as it is, with `false`, and takes the secret
/// too. The account's cap, pauses, and the rotation limit do not apply: nothing new is given
/// out.
#[allow(clippy::too_many_arguments)]
pub async fn reissue(
    pool: &PgPool,
    account: &Account,
    livemode: bool,
    client_reference_id: &str,
    chains: &[ChainContracts],
    target: ReissueTarget,
    id: Option<Uuid>,
    client_secret_hash: Option<&[u8; 32]>,
    restore_point: Option<DateTime<Utc>>,
    backfill_from: &BTreeMap<u64, u64>,
    actor: &Actor,
    reason: &str,
) -> Result<(DepositAddress, bool), DepositAddressError> {
    let restore_point = restore_point.ok_or(DepositAddressError::InvalidInput(
        "the restore has no restore point (the restored database has no heartbeat): escalate",
    ))?;
    let scope = Scope::new(account.id, livemode);
    let mut transaction = pool.begin().await?;
    // In this transaction: a re-issue refused below rolls the customer back with it.
    let customer =
        &crate::db::ensure_customer_in(&mut transaction, account.id, livemode, client_reference_id)
            .await?;
    let issuable = chains;
    let chains = with_treasuries(&mut transaction, scope, issuable).await?;
    require_chains(issuable, &chains)?;
    let in_force: Vec<Chain> = crate::treasuries::in_force(
        &mut transaction,
        scope,
        Some(restore_point - crate::treasuries::IN_FORCE_TOLERANCE),
        Utc::now(),
    )
    .await
    .map_err(|error| match error {
        crate::treasuries::TreasuryError::Database(error) => DepositAddressError::Database(error),
        _ => DepositAddressError::DatabaseInvariant,
    })?
    .into_iter()
    .filter_map(|(chain_id, treasury)| {
        issuable
            .iter()
            .find(|chain| chain.chain_id == chain_id)
            .map(|chain| chain.with_treasury(treasury))
    })
    .collect();
    let latest: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM deposit_addresses WHERE customer_id = $1")
            .bind(customer.id)
            .fetch_one(&mut *transaction)
            .await?;
    let latest =
        u64::try_from(latest.unwrap_or(0)).map_err(|_| DepositAddressError::DatabaseInvariant)?;
    let salt = |version| {
        deposit_address_salt(
            &account.public_id,
            customer.livemode,
            &customer.client_reference_id,
            version,
        )
    };
    let found = match target.address {
        None => None,
        Some(address) => {
            let recorded: Option<i64> = sqlx::query_scalar(
                r#"
                SELECT deposit_address.version
                FROM addresses AS address
                JOIN deposit_addresses AS deposit_address
                  ON deposit_address.id = address.deposit_address_id
                WHERE deposit_address.customer_id = $1 AND address.address = $2
                LIMIT 1
                "#,
            )
            .bind(customer.id)
            .bind(format!("{address:#x}"))
            .fetch_optional(&mut *transaction)
            .await?;
            Some(match recorded {
                Some(version) => {
                    u64::try_from(version).map_err(|_| DepositAddressError::DatabaseInvariant)?
                }
                None => (1..=latest.saturating_add(REISSUE_VERSIONS_AHEAD))
                    .find(|version| {
                        let salt = salt(*version);
                        in_force.iter().any(|chain| {
                            forwarder_address(
                                chain.factory,
                                chain.implementation,
                                chain.treasury,
                                salt,
                            ) == address
                        })
                    })
                    .ok_or(DepositAddressError::InvalidInput(
                        "the address is not one of the customer's deposit addresses over the \
                         account's treasuries in force since the restore point",
                    ))?,
            })
        }
    };
    let version = match (target.version, found) {
        (Some(0), _) => {
            return Err(DepositAddressError::InvalidInput(
                "version must be at least 1",
            ));
        }
        (Some(version), Some(found)) if version != found => {
            return Err(DepositAddressError::InvalidInput(
                "the address is not the customer's address of this version",
            ));
        }
        (Some(version), _) | (None, Some(version)) => version,
        (None, None) => {
            return Err(DepositAddressError::InvalidInput(
                "send the address, its version, or both",
            ));
        }
    };
    if version > latest.saturating_add(REISSUE_VERSIONS_AHEAD) {
        return Err(DepositAddressError::InvalidInput(
            "version is more than 32 past the customer's latest one: re-issue it in steps of 32 \
             versions (32, 64, ...)",
        ));
    }
    if version <= latest {
        let existing: Uuid = sqlx::query_scalar(
            "SELECT id FROM deposit_addresses WHERE customer_id = $1 AND version = $2",
        )
        .bind(customer.id)
        .bind(i64::try_from(version).map_err(|_| DepositAddressError::DatabaseInvariant)?)
        .fetch_one(&mut *transaction)
        .await?;
        if id.is_some_and(|id| id != existing) {
            return Err(DepositAddressError::InvalidInput(
                "id is not the customer's deposit address of this version",
            ));
        }
        keep_networks(
            &mut transaction,
            scope,
            &[(existing, salt(version))],
            &chains,
            &in_force,
            backfill_from,
        )
        .await?;
        let address = get_in(&mut transaction, scope, existing)
            .await?
            .ok_or(DepositAddressError::DatabaseInvariant)?;
        if let Some(secret_hash) = client_secret_hash {
            restore_client_secret(
                &mut transaction,
                scope,
                existing,
                secret_hash,
                actor,
                reason,
            )
            .await?;
        }
        transaction.commit().await?;
        return Ok((address, false));
    }
    if let Some(id) = id {
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM deposit_addresses WHERE id = $1)")
                .bind(id)
                .fetch_one(&mut *transaction)
                .await?;
        if taken {
            return Err(DepositAddressError::InvalidInput(
                "id belongs to another deposit address",
            ));
        }
    }
    let active = sqlx::query_as::<_, (Uuid, Json<BTreeMap<String, String>>)>(
        "SELECT id, metadata FROM deposit_addresses \
         WHERE customer_id = $1 AND status = 'active' FOR UPDATE",
    )
    .bind(customer.id)
    .fetch_optional(&mut *transaction)
    .await?;
    let metadata = match active {
        Some((active, Json(metadata))) => {
            retire(&mut transaction, active).await?;
            metadata
        }
        None => BTreeMap::new(),
    };
    let count = version
        .checked_sub(latest)
        .ok_or(DepositAddressError::DatabaseInvariant)?;
    let mut issued = Vec::new();
    let mut salts = Vec::new();
    for step in 1..=count {
        let last = step == count;
        let next = issue(
            &mut transaction,
            scope,
            &account.public_id,
            customer,
            &chains,
            &metadata,
            id.filter(|_| last).unwrap_or_else(Uuid::new_v4),
        )
        .await?;
        if !last {
            retire(&mut transaction, next).await?;
        }
        issued.push(next);
        salts.push((next, salt(latest + step)));
    }
    let reissued = *issued
        .last()
        .ok_or(DepositAddressError::DatabaseInvariant)?;
    keep_networks(
        &mut transaction,
        scope,
        &salts,
        &chains,
        &in_force,
        backfill_from,
    )
    .await?;
    for (chain_id, block) in backfill_from {
        sqlx::query(
            "UPDATE addresses SET created_block = LEAST(created_block, $3) \
             WHERE deposit_address_id = ANY($1) AND chain_id = $2",
        )
        .bind(&issued)
        .bind(i64::try_from(*chain_id).map_err(|_| DepositAddressError::InvalidInput("chain_id"))?)
        .bind(i64::try_from(*block).map_err(|_| DepositAddressError::DatabaseInvariant)?)
        .execute(&mut *transaction)
        .await?;
    }
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(scope.account_id()),
            actor,
            action: "deposit_address.reissue",
            subject: &format!("deposit_address:{}", public_id(reissued)),
            reason,
        },
    )
    .await?;
    if let Some(secret_hash) = client_secret_hash {
        restore_client_secret(
            &mut transaction,
            scope,
            reissued,
            secret_hash,
            actor,
            reason,
        )
        .await?;
    }
    let address = get_in(&mut transaction, scope, reissued)
        .await?
        .ok_or(DepositAddressError::DatabaseInvariant)?;
    transaction.commit().await?;
    Ok((address, true))
}

/// Gives each re-issued deposit address of `versions` (its id and salt) a network over every
/// treasury of `in_force` that is not its chain's current one in `current` (issuance gave it that
/// one): superseded, as a treasury change leaves a network, so payments to it are still credited.
async fn keep_networks(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    versions: &[(Uuid, B256)],
    current: &[Chain],
    in_force: &[Chain],
    backfill_from: &BTreeMap<u64, u64>,
) -> Result<(), DepositAddressError> {
    for chain in in_force.iter().filter(|chain| !current.contains(chain)) {
        for (id, salt) in versions {
            keep_network(transaction, scope, *id, *salt, *chain, backfill_from).await?;
        }
    }
    Ok(())
}

/// Gives deposit address `id` (of `salt`) a superseded network over `chain`'s treasury unless it
/// has one, backfilled from the chain's cursor in `backfill_from`.
async fn keep_network(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
    salt: B256,
    chain: Chain,
    backfill_from: &BTreeMap<u64, u64>,
) -> Result<(), DepositAddressError> {
    let chain_id =
        i64::try_from(chain.chain_id).map_err(|_| DepositAddressError::InvalidInput("chain_id"))?;
    let restored_block = backfill_from
        .get(&chain.chain_id)
        .map(|block| i64::try_from(*block))
        .transpose()
        .map_err(|_| DepositAddressError::DatabaseInvariant)?;
    let address = forwarder_address(chain.factory, chain.implementation, chain.treasury, salt);
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM addresses WHERE deposit_address_id=$1 AND chain_id=$2 AND address=$3)")
        .bind(id).bind(chain_id).bind(format!("{address:#x}")).fetch_one(&mut **transaction).await?;
    if exists {
        return Ok(());
    }
    match crate::db::chain_reads::admit_address(transaction, chain.chain_id).await? {
        crate::db::chain_reads::AddressAdmission::Admitted => {}
        crate::db::chain_reads::AddressAdmission::NotReady => {
            return Err(DepositAddressError::ChainUnavailable);
        }
        crate::db::chain_reads::AddressAdmission::CapacityReached => {
            return Err(DepositAddressError::AddressCapacityReached);
        }
    }
    sqlx::query(
        r#"
        INSERT INTO addresses (
            id, account_id, livemode, chain_id, deposit_address_id, salt, treasury, address,
            created_block, superseded_at
        )
        SELECT $1, $2, $3, $4, $5, $6, $7, $8,
               LEAST(cursor.block, COALESCE($9::bigint, cursor.block)),
               now()
        FROM (
            SELECT (SELECT through_block FROM chain_coverage WHERE chain_id = $4) AS block
        ) AS cursor
        WHERE NOT EXISTS (
            SELECT 1 FROM addresses
            WHERE deposit_address_id = $5 AND chain_id = $4 AND address = $8
        )
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .bind(id)
    .bind(format!("{salt:#x}"))
    .bind(format!("{:#x}", chain.treasury))
    .bind(format!("{address:#x}"))
    .bind(restored_block)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Keeps a client secret of the re-issued address `id` the merchant holds, audited once added.
async fn restore_client_secret(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
    secret_hash: &[u8; 32],
    actor: &Actor,
    reason: &str,
) -> Result<(), DepositAddressError> {
    let added = sqlx::query(
        "INSERT INTO deposit_address_client_secrets (secret_hash, deposit_address_id) \
         VALUES ($1, $2) ON CONFLICT (secret_hash) DO NOTHING",
    )
    .bind(secret_hash.as_slice())
    .bind(id)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        > 0;
    if added {
        audit::insert(
            &mut **transaction,
            &audit::Entry {
                account_id: Some(scope.account_id()),
                actor,
                action: "deposit_address.client_secret_restore",
                subject: &format!("deposit_address:{}", public_id(id)),
                reason,
            },
        )
        .await?;
    }
    Ok(())
}

/// The public id of a deposit address, `da_` and the hex of its id.
#[must_use]
pub fn public_id(id: Uuid) -> String {
    crate::ids::format(crate::ids::DEPOSIT_ADDRESS, id)
}

/// Loads the scope's deposit address `id`.
pub async fn get(
    pool: &PgPool,
    scope: Scope,
    id: Uuid,
) -> Result<Option<DepositAddress>, DepositAddressError> {
    let mut connection = pool.acquire().await?;
    get_in(&mut connection, scope, id).await
}

/// Filters and cursor of [`list`].
#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    /// Only this customer's addresses.
    pub client_reference_id: Option<String>,
    /// Only addresses in this status.
    pub status: Option<Status>,
    /// The page after this address (older), or before it (newer) when `before`.
    pub cursor: Option<Uuid>,
    /// Whether `cursor` is `ending_before`.
    pub before: bool,
    /// Page size.
    pub limit: i64,
}

/// A page of the scope's deposit addresses, newest first, and whether more follow in the
/// direction of the page. A cursor outside the scope is `NotFound`.
pub async fn list(
    pool: &PgPool,
    scope: Scope,
    filter: &ListFilter,
) -> Result<(Vec<DepositAddress>, bool), DepositAddressError> {
    let mut connection = pool.acquire().await?;
    let mut builder = QueryBuilder::new(SELECT);
    push_scope(&mut builder, scope);
    if let Some(reference) = &filter.client_reference_id {
        builder
            .push(" AND customer.client_reference_id = ")
            .push_bind(reference.clone());
    }
    if let Some(status) = filter.status {
        builder
            .push(" AND deposit_address.status = ")
            .push_bind(status.code());
    }
    if let Some(cursor) = filter.cursor {
        let found: Option<(DateTime<Utc>, Uuid)> = sqlx::query_as(
            "SELECT created_at, id FROM deposit_addresses \
             WHERE id = $1 AND account_id = $2 AND livemode = $3",
        )
        .bind(cursor)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(&mut *connection)
        .await?;
        let (created_at, id) = found.ok_or(DepositAddressError::NotFound)?;
        builder
            .push(if filter.before {
                " AND (deposit_address.created_at, deposit_address.id) > ("
            } else {
                " AND (deposit_address.created_at, deposit_address.id) < ("
            })
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder.push(if filter.before {
        " ORDER BY deposit_address.created_at ASC, deposit_address.id ASC LIMIT "
    } else {
        " ORDER BY deposit_address.created_at DESC, deposit_address.id DESC LIMIT "
    });
    builder.push_bind(filter.limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<DepositAddressRow>()
        .fetch_all(&mut *connection)
        .await?;
    let limit =
        usize::try_from(filter.limit).map_err(|_| DepositAddressError::DatabaseInvariant)?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    if filter.before {
        rows.reverse();
    }
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let mut networks = load_networks(&mut connection, &ids).await?;
    let addresses = rows
        .into_iter()
        .map(|row| {
            let own = networks.remove(&row.id).unwrap_or_default();
            row.into_address(own)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((addresses, has_more))
}

fn push_scope(builder: &mut QueryBuilder<Postgres>, scope: Scope) {
    builder
        .push(" WHERE deposit_address.account_id = ")
        .push_bind(scope.account_id())
        .push(" AND deposit_address.livemode = ")
        .push_bind(scope.livemode());
}

/// The scope's deposit address `id`, read on `connection`.
pub(crate) async fn get_in(
    connection: &mut PgConnection,
    scope: Scope,
    id: Uuid,
) -> Result<Option<DepositAddress>, DepositAddressError> {
    let mut builder = QueryBuilder::new(SELECT);
    push_scope(&mut builder, scope);
    builder.push(" AND deposit_address.id = ").push_bind(id);
    let Some(row) = builder
        .build_query_as::<DepositAddressRow>()
        .fetch_optional(&mut *connection)
        .await?
    else {
        return Ok(None);
    };
    let networks = load_networks(connection, &[row.id])
        .await?
        .remove(&row.id)
        .unwrap_or_default();
    row.into_address(networks).map(Some)
}

/// The current networks of `ids`, by deposit address and then chain id.
async fn load_networks(
    connection: &mut PgConnection,
    ids: &[Uuid],
) -> Result<BTreeMap<Uuid, Vec<Network>>, DepositAddressError> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, i64, String, String)>(
        r#"
        SELECT deposit_address_id, id, chain_id, address, treasury
        FROM addresses
        WHERE deposit_address_id = ANY($1) AND superseded_at IS NULL
        ORDER BY deposit_address_id, chain_id
        "#,
    )
    .bind(ids)
    .fetch_all(connection)
    .await?;
    let mut networks = BTreeMap::<Uuid, Vec<Network>>::new();
    for (deposit_address_id, address_id, chain_id, address, treasury) in rows {
        networks
            .entry(deposit_address_id)
            .or_default()
            .push(Network {
                address_id,
                chain_id: u64::try_from(chain_id)
                    .map_err(|_| DepositAddressError::DatabaseInvariant)?,
                address: EvmAddress::from_str(&address)
                    .map_err(|_| DepositAddressError::DatabaseInvariant)?,
                treasury: EvmAddress::from_str(&treasury)
                    .map_err(|_| DepositAddressError::DatabaseInvariant)?,
            });
    }
    Ok(networks)
}

async fn retire(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<(), DepositAddressError> {
    let updated = sqlx::query(
        "UPDATE deposit_addresses SET status = 'retired', retired_at = now() \
         WHERE id = $1 AND status = 'active'",
    )
    .bind(id)
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(DepositAddressError::DatabaseInvariant);
    }
    Ok(())
}

/// Refuses a new address that would take the account's active addresses in `scope` past its cap.
/// The advisory lock serializes issuance per account and mode from this check to commit.
async fn check_cap(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
) -> Result<(), DepositAddressError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('deposit-address-cap:' || $1, 0))")
        .bind(format!("{}:{}", scope.account_id(), scope.livemode()))
        .execute(&mut **transaction)
        .await?;
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM deposit_addresses \
         WHERE account_id = $1 AND livemode = $2 AND status = 'active'",
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_one(&mut **transaction)
    .await?;
    let cap = crate::limits::load(transaction, scope)
        .await
        .map_err(|error| match error {
            crate::limits::LimitsError::Database(error) => DepositAddressError::Database(error),
            _ => DepositAddressError::DatabaseInvariant,
        })?
        .max_active_deposit_addresses;
    let cap = i64::try_from(cap).map_err(|_| DepositAddressError::DatabaseInvariant)?;
    if active >= cap {
        return Err(DepositAddressError::CapReached(cap));
    }
    Ok(())
}

/// The issuable chains with the scope's current treasury of each, leaving out chains without
/// one. The shared treasury lock, held to commit, keeps a treasury change from applying meanwhile,
/// so no network is issued over a treasury being replaced.
async fn with_treasuries(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    chains: &[ChainContracts],
) -> Result<Vec<Chain>, DepositAddressError> {
    crate::treasuries::lock(transaction, scope, false).await?;
    let treasuries = crate::treasuries::current(transaction, scope)
        .await
        .map_err(|error| match error {
            crate::treasuries::TreasuryError::Database(error) => {
                DepositAddressError::Database(error)
            }
            _ => DepositAddressError::DatabaseInvariant,
        })?;
    Ok(chains
        .iter()
        .filter_map(|chain| {
            treasuries
                .get(&chain.chain_id)
                .map(|treasury| chain.with_treasury(*treasury))
        })
        .collect())
}

/// Refuses a new address when no chain can take it: `NoChain` when no chain is issuable, and
/// `NoTreasury` when none of them has a treasury.
fn require_chains(
    issuable: &[ChainContracts],
    chains: &[Chain],
) -> Result<(), DepositAddressError> {
    match (issuable.is_empty(), chains.is_empty()) {
        (true, _) => Err(DepositAddressError::NoChain),
        (false, true) => Err(DepositAddressError::NoTreasury),
        (false, false) => Ok(()),
    }
}

/// Inserts the customer's next version as `id` with a network on each of `chains`, and returns
/// its id.
async fn issue(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    account_public_id: &str,
    customer: &Customer,
    chains: &[Chain],
    metadata: &BTreeMap<String, String>,
    id: Uuid,
) -> Result<Uuid, DepositAddressError> {
    let latest: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM deposit_addresses WHERE customer_id = $1")
            .bind(customer.id)
            .fetch_one(&mut **transaction)
            .await?;
    let version = latest
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(DepositAddressError::DatabaseInvariant)?;
    let version_u64 = u64::try_from(version).map_err(|_| DepositAddressError::DatabaseInvariant)?;
    sqlx::query(
        r#"
        INSERT INTO deposit_addresses (id, account_id, livemode, customer_id, version, metadata)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(customer.id)
    .bind(version)
    .bind(Json(metadata))
    .execute(&mut **transaction)
    .await?;
    let salt = deposit_address_salt(
        account_public_id,
        scope.livemode(),
        &customer.client_reference_id,
        version_u64,
    );
    sync_networks(transaction, scope, id, salt, chains).await?;
    Ok(id)
}

/// Gives deposit address `id` (of `salt`) a current network on each of `chains` over the chain's
/// treasury: a chain without one gets one, and a chain whose network pays another treasury has it
/// superseded (still watched and credited) by one over the new treasury. A network superseded
/// earlier is made current again when its treasury is the chain's again. Networks on chains not
/// in `chains` are left as they are.
pub async fn sync_networks(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    id: Uuid,
    salt: B256,
    chains: &[Chain],
) -> Result<(), DepositAddressError> {
    let mut seen = BTreeSet::new();
    for chain in chains {
        if !seen.insert(chain.chain_id) {
            return Err(DepositAddressError::DatabaseInvariant);
        }
        let chain_id = i64::try_from(chain.chain_id)
            .map_err(|_| DepositAddressError::InvalidInput("chain_id"))?;
        let address = forwarder_address(chain.factory, chain.implementation, chain.treasury, salt);
        let address_hex = format!("{address:#x}");
        let current: Option<String> = sqlx::query_scalar(
            "SELECT address FROM addresses \
             WHERE deposit_address_id = $1 AND chain_id = $2 AND superseded_at IS NULL \
             FOR UPDATE",
        )
        .bind(id)
        .bind(chain_id)
        .fetch_optional(&mut **transaction)
        .await?;
        match current {
            Some(current) if current == address_hex => continue,
            Some(_) => {
                sqlx::query(
                    "UPDATE addresses SET superseded_at = now() \
                     WHERE deposit_address_id = $1 AND chain_id = $2 AND superseded_at IS NULL",
                )
                .bind(id)
                .bind(chain_id)
                .execute(&mut **transaction)
                .await?;
            }
            None => {}
        }
        // The chain's treasury may be one an earlier network of this address paid: that forwarder
        // is the chain's current one again.
        crate::db::rpc::guard_in(transaction, chain.chain_id).await?;
        let restored = sqlx::query(
            "UPDATE addresses SET superseded_at = NULL \
             WHERE deposit_address_id = $1 AND chain_id = $2 AND address = $3",
        )
        .bind(id)
        .bind(chain_id)
        .bind(&address_hex)
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        if restored > 0 {
            continue;
        }
        match crate::db::chain_reads::admit_address(transaction, chain.chain_id).await? {
            crate::db::chain_reads::AddressAdmission::Admitted => {}
            crate::db::chain_reads::AddressAdmission::NotReady => {
                return Err(DepositAddressError::ChainUnavailable);
            }
            crate::db::chain_reads::AddressAdmission::CapacityReached => {
                return Err(DepositAddressError::AddressCapacityReached);
            }
        }
        // A newly derived forwarder cannot hold earlier payments to this salt and treasury unless
        // someone sent to the address before its network existed (on a chain added later, or
        // before a treasury change); as for quote addresses, the scanner covers it from the
        // chain's committed cursor.
        sqlx::query(
            r#"
            INSERT INTO addresses (
                id, account_id, livemode, chain_id, deposit_address_id, salt, treasury, address,
                created_block
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                (SELECT through_block FROM chain_coverage WHERE chain_id = $4)
            )
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(scope.account_id())
        .bind(scope.livemode())
        .bind(chain_id)
        .bind(id)
        .bind(format!("{salt:#x}"))
        .bind(format!("{:#x}", chain.treasury))
        .bind(&address_hex)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

/// Replaces `chain`'s network of every deposit address of `scope`, active or retired, by the
/// forwarder over `chain.treasury` with the same salt, when a treasury change applies on the chain
/// (design §5a). A replaced network is superseded: still watched and credited, paying the treasury
/// it was issued for. A network the new treasury paid before is made current again. Returns how
/// many networks changed. The caller holds the scope's exclusive treasury lock.
pub(crate) async fn replace_networks(
    transaction: &mut Transaction<'_, Postgres>,
    scope: Scope,
    chain: Chain,
) -> Result<usize, DepositAddressError> {
    let chain_id =
        i64::try_from(chain.chain_id).map_err(|_| DepositAddressError::InvalidInput("chain_id"))?;
    let current = sqlx::query_as::<_, (Uuid, String, String)>(
        r#"
        SELECT deposit_address_id, salt, address
        FROM addresses
        WHERE account_id = $1 AND livemode = $2 AND chain_id = $3
          AND deposit_address_id IS NOT NULL AND superseded_at IS NULL
        FOR UPDATE
        "#,
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .fetch_all(&mut **transaction)
    .await?;
    let mut owners = Vec::new();
    let mut salts = Vec::new();
    let mut addresses = Vec::new();
    for (owner, salt, address) in current {
        let salt = B256::from_str(&salt).map_err(|_| DepositAddressError::DatabaseInvariant)?;
        let next = format!(
            "{:#x}",
            forwarder_address(chain.factory, chain.implementation, chain.treasury, salt)
        );
        if next != address {
            owners.push(owner);
            salts.push(format!("{salt:#x}"));
            addresses.push(next);
        }
    }
    if owners.is_empty() {
        return Ok(0);
    }
    sqlx::query(
        "UPDATE addresses SET superseded_at = now() \
         WHERE deposit_address_id = ANY($1) AND chain_id = $2 AND superseded_at IS NULL",
    )
    .bind(&owners)
    .bind(chain_id)
    .execute(&mut **transaction)
    .await?;
    let restored: BTreeSet<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE addresses AS address SET superseded_at = NULL
        FROM unnest($1::uuid[], $2::text[]) AS next(owner, address)
        WHERE address.deposit_address_id = next.owner AND address.chain_id = $3
          AND address.address = next.address
        RETURNING address.deposit_address_id
        "#,
    )
    .bind(&owners)
    .bind(&addresses)
    .bind(chain_id)
    .fetch_all(&mut **transaction)
    .await?
    .into_iter()
    .collect();
    let mut ids = Vec::new();
    let mut new_owners = Vec::new();
    let mut new_salts = Vec::new();
    let mut new_addresses = Vec::new();
    for ((owner, salt), address) in owners.iter().zip(&salts).zip(&addresses) {
        if !restored.contains(owner) {
            ids.push(Uuid::new_v4());
            new_owners.push(*owner);
            new_salts.push(salt.clone());
            new_addresses.push(address.clone());
        }
    }
    match crate::db::chain_reads::admit_addresses(transaction, chain.chain_id, ids.len()).await? {
        crate::db::chain_reads::AddressAdmission::Admitted => {}
        crate::db::chain_reads::AddressAdmission::NotReady => {
            return Err(DepositAddressError::ChainUnavailable);
        }
        crate::db::chain_reads::AddressAdmission::CapacityReached => {
            return Err(DepositAddressError::AddressCapacityReached);
        }
    }
    // As in `sync_networks`, the scanner covers a new forwarder from the chain's committed cursor.
    sqlx::query(
        r#"
        INSERT INTO addresses (
            id, account_id, livemode, chain_id, deposit_address_id, salt, treasury, address,
            created_block
        )
        SELECT next.id, $5, $6, $7, next.owner, next.salt, $8, next.address,
               (SELECT through_block FROM chain_coverage WHERE chain_id = $7)
        FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::text[])
            AS next(id, owner, salt, address)
        "#,
    )
    .bind(&ids)
    .bind(&new_owners)
    .bind(&new_salts)
    .bind(&new_addresses)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(chain_id)
    .bind(format!("{:#x}", chain.treasury))
    .execute(&mut **transaction)
    .await?;
    Ok(owners.len())
}

async fn lock_customer(
    transaction: &mut Transaction<'_, Postgres>,
    customer: &Customer,
) -> Result<(), DepositAddressError> {
    // `NO KEY UPDATE`, as quote creation takes: it serializes this customer's issuance without
    // blocking the `KEY SHARE` locks of foreign-key checks on scanner deposit inserts.
    let found = sqlx::query(
        "SELECT id FROM customers WHERE id = $1 AND account_id = $2 AND livemode = $3 \
         FOR NO KEY UPDATE",
    )
    .bind(customer.id)
    .bind(customer.account_id)
    .bind(customer.livemode)
    .fetch_optional(&mut **transaction)
    .await?;
    if found.is_none() {
        return Err(DepositAddressError::NotFound);
    }
    Ok(())
}

#[derive(FromRow)]
struct DepositAddressRow {
    id: Uuid,
    account_public_id: String,
    livemode: bool,
    client_reference_id: String,
    version: i64,
    status: String,
    created_at: DateTime<Utc>,
    retired_at: Option<DateTime<Utc>>,
    metadata: Json<BTreeMap<String, String>>,
}

impl DepositAddressRow {
    fn into_address(self, networks: Vec<Network>) -> Result<DepositAddress, DepositAddressError> {
        let version =
            u64::try_from(self.version).map_err(|_| DepositAddressError::DatabaseInvariant)?;
        Ok(DepositAddress {
            id: self.id,
            salt: deposit_address_salt(
                &self.account_public_id,
                self.livemode,
                &self.client_reference_id,
                version,
            ),
            livemode: self.livemode,
            client_reference_id: self.client_reference_id,
            version,
            status: Status::parse(&self.status)?,
            created_at: self.created_at,
            retired_at: self.retired_at,
            metadata: self.metadata.0,
            networks,
        })
    }
}
