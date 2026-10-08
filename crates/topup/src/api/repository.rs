//! API-specific PostgreSQL queries.
//!
//! Merchant queries take the request's [`Scope`] and filter every tenant table on its account and
//! mode, so another tenant's row answers like a missing one. The admin functions below them act
//! for the operator across accounts and are reachable only through the admin-key router.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use alloy_primitives::{Address as EvmAddress, B256, U256};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{Acquire, FromRow, PgPool, Postgres, Row};
use topup_core::money::AtomicAmount;
use topup_core::refund::{RefundDeposit, refund_eligibility};
use topup_core::route::RouteFile;
use uuid::Uuid;

use crate::api_keys::{self, IssuedKey};
use crate::audit::{self, Actor};
use crate::db::Customer;
use crate::routes::RouteSet;
use crate::tenancy::Scope;

use super::auth::VerifiedSignature;
use super::error::ApiError;
use super::models::{
    DailyReportResponse, DepositAdmin, DepositEventDelivery, DepositTransition,
    FailingWebhookEndpoint, NudgeResponse, ReconciliationBlockLiftResponse,
    ReconciliationBlockReport, RouteDailyReport,
};

/// Environment-wide hard bounds on refund verification load, shared by all accounts and modes.
pub const MAX_ATTACHED_PENDING_REFUNDS: i64 = 2;
/// New attachments in a rolling 24-hour window, including refunds already resolved.
pub const MAX_REFUND_ATTACHMENTS_PER_DAY: i64 = 1;

/// Records a verified request signature exactly once within the acceptance window.
pub async fn record_signature(
    pool: &PgPool,
    signature: &VerifiedSignature,
) -> Result<(), ApiError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM seen_signatures WHERE created < now() - interval '5 minutes'")
        .execute(&mut *transaction)
        .await?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO seen_signatures (kid, signature_hash, created)
        VALUES ($1, $2, $3)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(&signature.kid)
    .bind(signature.signature_hash.as_slice())
    .bind(signature.created)
    .execute(&mut *transaction)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(ApiError::signature_replayed());
    }
    transaction.commit().await?;
    Ok(())
}

/// An account as the admin API shows it.
#[derive(FromRow)]
pub struct AdminAccount {
    /// Account id.
    pub id: Uuid,
    /// `acct_…`.
    pub public_id: String,
    /// Display name.
    pub name: String,
    /// `{name, email}`.
    pub contact: Value,
    /// `{reference, reviewed_at, reviewed_by}`.
    pub due_diligence: Value,
    /// Live mode.
    pub charges_enabled: bool,
    /// Restricted for review.
    pub restricted: bool,
    /// Account-level pause scopes.
    pub paused_scopes: Vec<String>,
    /// Cap, in cents and per mode, on the credit of deposits credited before they are final.
    pub max_unfinalized_credit: i64,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// The columns of [`AdminAccount`].
macro_rules! admin_account_columns {
    () => {
        "id, public_id, name, contact, due_diligence, charges_enabled, restricted, \
         paused_scopes, max_unfinalized_credit, created_at"
    };
}

/// An account with the keys the admin request issued, each with its secret.
pub struct IssuedAccount {
    /// The account.
    pub account: AdminAccount,
    /// Its effective caps, test mode's then live mode's.
    pub limits: [crate::limits::Limits; 2],
    /// Keys issued by the request.
    pub api_keys: Vec<IssuedKey>,
}

/// A new account's values (design D8).
pub struct NewAccount<'a> {
    /// Display name.
    pub name: &'a str,
    /// `{name, email}`.
    pub contact: Value,
    /// `{reference, reviewed_at, reviewed_by}`.
    pub due_diligence: Value,
    /// Live mode.
    pub charges_enabled: bool,
}

/// Creates an account with the first secret key of test mode and, with `charges_enabled`, of
/// live mode, and the audit rows and `api_key.created` events, in one transaction. The merchant
/// registers its webhook endpoints itself (`/v1/webhook_endpoints`).
pub async fn create_account(
    pool: &PgPool,
    account: &NewAccount<'_>,
    actor: &Actor,
    reason: &str,
) -> Result<IssuedAccount, ApiError> {
    let mut transaction = pool.begin().await?;
    let created = sqlx::query_as::<_, AdminAccount>(concat!(
        "INSERT INTO accounts (id, name, contact, due_diligence, charges_enabled) \
         VALUES ($1, $2, $3, $4, $5) RETURNING ",
        admin_account_columns!()
    ))
    .bind(Uuid::new_v4())
    .bind(account.name)
    .bind(&account.contact)
    .bind(&account.due_diligence)
    .bind(account.charges_enabled)
    .fetch_one(&mut *transaction)
    .await?;
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(created.id),
            actor,
            action: "account.create",
            subject: &format!("account:{}", created.public_id),
            reason: &serde_json::json!({
                "reason": reason,
                "charges_enabled": account.charges_enabled,
                "due_diligence": account.due_diligence,
            })
            .to_string(),
        },
    )
    .await?;
    let modes: &[bool] = if account.charges_enabled {
        &[false, true]
    } else {
        &[false]
    };
    let mut issued = Vec::with_capacity(modes.len());
    for &livemode in modes {
        issued.push(
            api_keys::create_in(
                &mut transaction,
                Scope::new(created.id, livemode),
                "",
                actor,
                "the account's first key",
            )
            .await
            .map_err(super::keys::map_error)?,
        );
    }
    let limits = account_limits(&mut transaction, created.id).await?;
    transaction.commit().await?;
    Ok(IssuedAccount {
        account: created,
        limits,
        api_keys: issued,
    })
}

/// An account and its effective caps, for the operator's `GET /v1/admin/accounts/{account}`.
pub async fn admin_account(pool: &PgPool, account_id: Uuid) -> Result<IssuedAccount, ApiError> {
    let mut connection = pool.acquire().await?;
    let account = sqlx::query_as::<_, AdminAccount>(concat!(
        "SELECT ",
        admin_account_columns!(),
        " FROM accounts WHERE id = $1"
    ))
    .bind(account_id)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(ApiError::not_found)?;
    let limits = account_limits(&mut connection, account_id).await?;
    Ok(IssuedAccount {
        account,
        limits,
        api_keys: Vec::new(),
    })
}

/// An admin update of an account; absent fields stay.
pub struct AccountChanges {
    /// Live mode.
    pub charges_enabled: Option<bool>,
    /// Restricted for review.
    pub restricted: Option<bool>,
    /// `{name, email}`.
    pub contact: Option<Value>,
    /// Cap on unfinalized credit, in cents.
    pub max_unfinalized_credit: Option<i64>,
    /// A change to the caps of one mode.
    pub limits: Option<(bool, crate::limits::LimitsChange)>,
}

/// The effective caps of `account_id`, test mode's then live mode's.
async fn account_limits(
    connection: &mut sqlx::PgConnection,
    account_id: Uuid,
) -> Result<[crate::limits::Limits; 2], ApiError> {
    let mut limits = [crate::limits::DEFAULT_TEST, crate::limits::DEFAULT_LIVE];
    for (livemode, slot) in [false, true].into_iter().zip(limits.iter_mut()) {
        *slot = crate::limits::load(connection, Scope::new(account_id, livemode))
            .await
            .map_err(limits_error)?;
    }
    Ok(limits)
}

fn limits_error(error: crate::limits::LimitsError) -> ApiError {
    match error {
        crate::limits::LimitsError::OutOfRange(field) => ApiError::invalid_param(
            format!("limits.{field}"),
            format!("{field} is out of range"),
        ),
        crate::limits::LimitsError::Invalid => {
            tracing::error!("account_limits holds an invalid value");
            ApiError::internal()
        }
        crate::limits::LimitsError::Database(error) => ApiError::from(error),
    }
}

/// Applies `changes`, with an audit row and an `account.updated` event per enabled mode, in one
/// transaction; an update that changes nothing writes nothing. Enabling live mode for an account
/// without a live key issues its first live key.
pub async fn update_account(
    pool: &PgPool,
    routes: &RouteSet,
    account_id: Uuid,
    changes: &AccountChanges,
    actor: &Actor,
    reason: &str,
) -> Result<IssuedAccount, ApiError> {
    let mut transaction = pool.begin().await?;
    let before = sqlx::query_as::<_, AdminAccount>(concat!(
        "SELECT ",
        admin_account_columns!(),
        " FROM accounts WHERE id = $1 FOR UPDATE"
    ))
    .bind(account_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(ApiError::not_found)?;
    let object = crate::db::EventObject::Account(account_id);
    let mut previous = Vec::with_capacity(2);
    for livemode in [false, true] {
        let scope = Scope::new(account_id, livemode);
        previous.push(crate::db::render(&mut transaction, routes, scope, object).await?);
    }
    let after = sqlx::query_as::<_, AdminAccount>(concat!(
        "UPDATE accounts SET charges_enabled = COALESCE($2, charges_enabled), \
         restricted = COALESCE($3, restricted), contact = COALESCE($4, contact), \
         max_unfinalized_credit = COALESCE($5, max_unfinalized_credit) \
         WHERE id = $1 RETURNING ",
        admin_account_columns!()
    ))
    .bind(account_id)
    .bind(changes.charges_enabled)
    .bind(changes.restricted)
    .bind(&changes.contact)
    .bind(changes.max_unfinalized_credit)
    .fetch_one(&mut *transaction)
    .await?;
    let limits_before = account_limits(&mut transaction, account_id).await?;
    if let Some((livemode, change)) = &changes.limits {
        crate::limits::update(&mut transaction, Scope::new(account_id, *livemode), change)
            .await
            .map_err(limits_error)?;
    }
    let limits = account_limits(&mut transaction, account_id).await?;
    let unchanged = after.charges_enabled == before.charges_enabled
        && after.restricted == before.restricted
        && after.contact == before.contact
        && after.max_unfinalized_credit == before.max_unfinalized_credit
        && limits == limits_before;
    if unchanged {
        // Nothing an operator reads changed; a row the update created holds the defaults.
        transaction.commit().await?;
        return Ok(IssuedAccount {
            account: after,
            limits,
            api_keys: Vec::new(),
        });
    }
    let modes: &[bool] = if after.charges_enabled {
        &[false, true]
    } else {
        &[false]
    };
    let mut issued = Vec::new();
    if after.charges_enabled && !before.charges_enabled {
        let has_live_key: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM api_keys \
             WHERE account_id = $1 AND livemode AND revoked_at IS NULL)",
        )
        .bind(account_id)
        .fetch_one(&mut *transaction)
        .await?;
        if !has_live_key {
            issued.push(
                api_keys::create_in(
                    &mut transaction,
                    Scope::new(account_id, true),
                    "",
                    actor,
                    "the account's first live key",
                )
                .await
                .map_err(super::keys::map_error)?,
            );
        }
    }
    audit::insert(
        &mut *transaction,
        &audit::Entry {
            account_id: Some(account_id),
            actor,
            action: "account.update",
            subject: &format!("account:{}", after.public_id),
            reason: &serde_json::json!({
                "reason": reason,
                "replaced": {
                    "charges_enabled": before.charges_enabled,
                    "restricted": before.restricted,
                    "contact": before.contact,
                    "max_unfinalized_credit": before.max_unfinalized_credit,
                    "limits": {"test": limits_before[0], "live": limits_before[1]},
                },
            })
            .to_string(),
        },
    )
    .await?;
    for &livemode in modes {
        let scope = Scope::new(account_id, livemode);
        let event = crate::db::NewOutboxEvent::new("account.updated", scope, object, actor);
        let before = &previous[usize::from(livemode)];
        crate::db::enqueue_in(&mut transaction, routes, &event, Some(before)).await?;
    }
    transaction.commit().await?;
    Ok(IssuedAccount {
        account: after,
        limits,
        api_keys: issued,
    })
}

/// Finds or creates the customer `client_reference_id` of `scope`.
pub async fn ensure_customer(
    pool: &PgPool,
    scope: Scope,
    client_reference_id: &str,
) -> Result<Customer, ApiError> {
    sqlx::query(
        r#"
        INSERT INTO customers (id, account_id, livemode, client_reference_id)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, livemode, client_reference_id) DO NOTHING
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(client_reference_id)
    .execute(pool)
    .await?;
    find_customer(pool, scope, client_reference_id)
        .await?
        .ok_or_else(ApiError::internal)
}

/// Finds the customer `client_reference_id` of `scope`.
pub async fn find_customer(
    pool: &PgPool,
    scope: Scope,
    client_reference_id: &str,
) -> Result<Option<Customer>, ApiError> {
    let row = sqlx::query_as::<_, CustomerRow>(
        r#"
        SELECT id, account_id, livemode, client_reference_id, paused_scopes
        FROM customers
        WHERE account_id = $1 AND livemode = $2 AND client_reference_id = $3
        "#,
    )
    .bind(scope.account_id())
    .bind(scope.livemode())
    .bind(client_reference_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// A refund request: the deposit, destination, and amount (the unrefunded remainder when absent).
pub struct NewRefund<'a> {
    /// The request's scope.
    pub scope: Scope,
    /// Deposit to refund.
    pub deposit_id: Uuid,
    /// The deposit's route, or the fallback route of an unrouted deposit.
    pub route: &'a RouteFile,
    /// Customer-controlled destination, already screened.
    pub destination: EvmAddress,
    /// Requested amount; `None` refunds the remainder.
    pub amount: Option<AtomicAmount>,
    /// The refund's validated metadata.
    pub metadata: &'a super::metadata::Metadata,
    /// Audit actor.
    pub actor: &'a Actor,
}

/// Creates a `pending` refund after every policy check and returns its id: the deposit is final
/// and refundable, nothing pauses refunds, and the amount fits the deposit's remainder after its
/// pending and succeeded refunds, which it then reserves. Audited, and announced as
/// `refund.created`.
pub async fn request_refund<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &RouteSet,
    refund: &NewRefund<'_>,
) -> Result<Uuid, ApiError> {
    let mut transaction = db.begin().await?;
    let result = request_refund_in(&mut transaction, routes, refund).await;
    crate::db::settle(transaction, result).await
}

async fn request_refund_in(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    routes: &RouteSet,
    refund: &NewRefund<'_>,
) -> Result<Uuid, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT deposit.amount_atomic::text AS amount_atomic, deposit.state, deposit.reason,
               deposit.chain_id, deposit.final_at IS NOT NULL AS is_final,
               deposit.sanctions_hit_at IS NOT NULL AS sanctions_hit,
               customer.paused_scopes AS customer_scopes,
               account.paused_scopes AS account_scopes,
               COALESCE(route_pause.paused_scopes, '{}') AS route_scopes
        FROM deposits AS deposit
        JOIN customers AS customer ON customer.id = deposit.customer_id
        JOIN accounts AS account ON account.id = deposit.account_id
        LEFT JOIN route_pauses AS route_pause ON route_pause.route = COALESCE(deposit.route, $4)
        WHERE deposit.id = $1 AND deposit.account_id = $2 AND deposit.livemode = $3
        FOR UPDATE OF deposit, customer
        "#,
    )
    .bind(refund.deposit_id)
    .bind(refund.scope.account_id())
    .bind(refund.scope.livemode())
    .bind(&refund.route.route)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| ApiError::not_found().with_param("deposit"))?;

    let customer_scopes: Vec<String> = row.try_get("customer_scopes")?;
    let account_scopes: Vec<String> = row.try_get("account_scopes")?;
    let route_scopes: Vec<String> = row.try_get("route_scopes")?;
    if [&customer_scopes, &account_scopes, &route_scopes]
        .into_iter()
        .any(|scopes| scopes.iter().any(|scope| scope == "refunds"))
    {
        return Err(ApiError::paused("refund requests are paused"));
    }

    // A sanctions hit on a delivered credit preserves its outcome, but not refund eligibility.
    if row.try_get::<bool, _>("sanctions_hit")? {
        return Err(ApiError::deposit_not_refundable());
    }

    let deposit_amount = parse_atomic(row.try_get::<String, _>("amount_atomic")?)?;
    // The dust floor of the terms that govern the deposit, or the route's default for one no
    // terms govern (docs/design/payment-settings.md §9).
    let min_refund =
        crate::payment_config::refund_floor(transaction, routes, refund.deposit_id, refund.route)
            .await?;
    refund_eligibility(refund_deposit_from_row(&row, min_refund)?)
        .map_err(|_| ApiError::deposit_not_refundable())?;
    // Nothing is paid back for a deposit that could still be reversed.
    if !row.try_get::<bool, _>("is_final")? {
        return Err(ApiError::deposit_not_final());
    }
    let chain_id: i64 = row.try_get("chain_id")?;

    let reserved = sqlx::query_scalar::<_, String>(
        r#"
        SELECT COALESCE(sum(amount_atomic), 0)::text
        FROM refunds
        WHERE deposit_id = $1 AND status IN ('pending', 'succeeded')
        "#,
    )
    .bind(refund.deposit_id)
    .fetch_one(&mut **transaction)
    .await?;
    let remaining = deposit_amount
        .checked_sub(parse_atomic(reserved)?)
        .ok_or_else(ApiError::internal)?;
    let amount = refund.amount.map_or(remaining, AtomicAmount::value);
    if amount.is_zero() {
        return Err(ApiError::amount_too_small(
            "amount_atomic",
            "nothing is left to refund",
        ));
    }
    if amount > remaining {
        return Err(ApiError::amount_too_large(
            "amount_atomic",
            format!("at most {remaining} base units are left to refund"),
        ));
    }

    let refund_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO refunds
            (id, account_id, livemode, chain_id, deposit_id, amount_atomic, destination_address,
             status, metadata)
        VALUES ($1, $2, $3, $4, $5, $6::text::numeric, $7, 'pending', $8)
        "#,
    )
    .bind(refund_id)
    .bind(refund.scope.account_id())
    .bind(refund.scope.livemode())
    .bind(chain_id)
    .bind(refund.deposit_id)
    .bind(amount.to_string())
    .bind(format!("{:#x}", refund.destination))
    .bind(sqlx::types::Json(refund.metadata))
    .execute(&mut **transaction)
    .await?;
    insert_audit_tx(
        transaction,
        Some(refund.scope.account_id()),
        refund.actor,
        "refund.create",
        &refund_subject(refund_id),
    )
    .await?;
    let event = crate::db::NewOutboxEvent::new(
        "refund.created",
        refund.scope,
        crate::db::EventObject::Refund(refund_id),
        refund.actor,
    );
    crate::db::enqueue_in(transaction, routes, &event, None).await?;
    Ok(refund_id)
}

fn refund_subject(refund_id: Uuid) -> String {
    format!(
        "refund:{}",
        crate::ids::format(crate::ids::REFUND, refund_id)
    )
}

/// Attaches the merchant's refund transaction to a pending refund; the verification worker checks
/// it at finality. Repeating the same transaction is a no-op; another one is
/// `refund_unexpected_state`, since only the verification outcome ends a refund with a
/// transaction attached. Audited, and announced as `refund.updated`.
pub async fn mark_refund_paid<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &RouteSet,
    scope: Scope,
    refund_id: Uuid,
    tx_hash: B256,
    receipt_log_index: Option<u64>,
    actor: &Actor,
) -> Result<(), ApiError> {
    let mut transaction = db.begin().await?;
    let result = mark_refund_paid_in(
        &mut transaction,
        routes,
        scope,
        refund_id,
        tx_hash,
        receipt_log_index,
        actor,
    )
    .await;
    crate::db::settle(transaction, result).await
}

async fn mark_refund_paid_in(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    routes: &RouteSet,
    scope: Scope,
    refund_id: Uuid,
    tx_hash: B256,
    receipt_log_index: Option<u64>,
    actor: &Actor,
) -> Result<(), ApiError> {
    let clear = crate::refunds::lock_refund_deposit(transaction, scope, refund_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if !clear {
        return Err(ApiError::deposit_not_refundable());
    }
    let (status, current_hash, current_log) = locked_refund(transaction, scope, refund_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let tx_hash = format!("{tx_hash:#x}");
    let receipt_log_index = receipt_log_index
        .map(|index| {
            i64::try_from(index).map_err(|_| {
                ApiError::invalid_param("receipt_log_index", "receipt_log_index is too large")
            })
        })
        .transpose()?;
    if let Some(current) = current_hash {
        let same =
            current == tx_hash && receipt_log_index.is_none_or(|index| current_log == Some(index));
        if same && status != "canceled" {
            return Ok(());
        }
        return Err(ApiError::refund_unexpected_state(if status == "pending" {
            "already marked paid with another transaction".to_owned()
        } else {
            status
        }));
    }
    if status != "pending" {
        return Err(ApiError::refund_unexpected_state(status));
    }
    // Serialize admission across this environment. Idempotent attachments returned above do
    // not claim quota. Count after acquiring the lock so competing transactions see commits.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('refund-attachment-budget', 0))")
        .execute(&mut **transaction)
        .await?;
    let (pending, recent): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'pending' AND tx_hash IS NOT NULL), \
         count(*) FILTER (WHERE paid_at > statement_timestamp() - interval '24 hours') FROM refunds",
    )
    .fetch_one(&mut **transaction)
    .await?;
    if pending >= MAX_ATTACHED_PENDING_REFUNDS {
        return Err(ApiError::refund_attachment_limit_exceeded(
            "the environment's attached-pending refund limit is reached; contact the operator",
        ));
    }
    if recent >= MAX_REFUND_ATTACHMENTS_PER_DAY {
        return Err(ApiError::refund_attachment_limit_exceeded(
            "the environment's rolling 24-hour refund attachment limit is reached; contact the operator",
        ));
    }
    let object = crate::db::EventObject::Refund(refund_id);
    let before = crate::db::render(transaction, routes, scope, object).await?;
    let updated = sqlx::query(
        r#"
        UPDATE refunds
        SET tx_hash = $2, receipt_log_index = $3, paid_at = clock_timestamp(), next_check_at = now(),
            updated_at = now()
        WHERE id = $1
        "#,
    )
    .bind(refund_id)
    .bind(&tx_hash)
    .bind(receipt_log_index)
    .execute(&mut **transaction)
    .await;
    match updated {
        Ok(_) => {}
        Err(sqlx::Error::Database(error))
            if error.constraint() == Some("refunds_transfer_unique") =>
        {
            return Err(ApiError::transfer_already_used());
        }
        Err(error) => return Err(error.into()),
    }
    insert_audit_tx_with_reason(
        transaction,
        Some(scope.account_id()),
        actor,
        "refund.mark_paid",
        &refund_subject(refund_id),
        &tx_hash,
    )
    .await?;
    let event = crate::db::NewOutboxEvent::new("refund.updated", scope, object, actor);
    crate::db::enqueue_in(transaction, routes, &event, Some(&before)).await?;
    Ok(())
}

/// Cancels a pending refund without a transaction attached, releasing its reservation; canceling a
/// canceled refund is a no-op. Once `mark_paid` attached a transaction, the refund stays reserved
/// until verification ends it, so that the merchant cannot pay the deposit back twice. Audited,
/// and announced as `refund.updated`.
pub async fn cancel_refund<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &RouteSet,
    scope: Scope,
    refund_id: Uuid,
    actor: &Actor,
) -> Result<(), ApiError> {
    let mut transaction = db.begin().await?;
    let result = cancel_refund_in(&mut transaction, routes, scope, refund_id, actor).await;
    crate::db::settle(transaction, result).await
}

async fn cancel_refund_in(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    routes: &RouteSet,
    scope: Scope,
    refund_id: Uuid,
    actor: &Actor,
) -> Result<(), ApiError> {
    let (status, tx_hash, _) = locked_refund(transaction, scope, refund_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    match status.as_str() {
        "canceled" => return Ok(()),
        "pending" if tx_hash.is_some() => {
            return Err(ApiError::refund_unexpected_state(
                "marked paid: its reservation remains until finalized verification resolves it",
            ));
        }
        "pending" => {}
        _ => return Err(ApiError::refund_unexpected_state(status)),
    }
    let object = crate::db::EventObject::Refund(refund_id);
    let before = crate::db::render(transaction, routes, scope, object).await?;
    sqlx::query("UPDATE refunds SET status = 'canceled', updated_at = now() WHERE id = $1")
        .bind(refund_id)
        .execute(&mut **transaction)
        .await?;
    insert_audit_tx(
        transaction,
        Some(scope.account_id()),
        actor,
        "refund.cancel",
        &refund_subject(refund_id),
    )
    .await?;
    let event = crate::db::NewOutboxEvent::new("refund.updated", scope, object, actor);
    crate::db::enqueue_in(transaction, routes, &event, Some(&before)).await?;
    Ok(())
}

/// The scope's refund `refund_id`, locked: its status, transaction, and log.
async fn locked_refund(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    scope: Scope,
    refund_id: Uuid,
) -> Result<Option<(String, Option<String>, Option<i64>)>, ApiError> {
    Ok(sqlx::query_as(
        r#"
        SELECT status, tx_hash, receipt_log_index
        FROM refunds
        WHERE id = $1 AND account_id = $2 AND livemode = $3
        FOR UPDATE
        "#,
    )
    .bind(refund_id)
    .bind(scope.account_id())
    .bind(scope.livemode())
    .fetch_optional(&mut **transaction)
    .await?)
}

/// A deposit's account, mode, and internals with its transitions and events, for the operator:
/// `None` for an unknown deposit.
pub async fn admin_deposit(
    pool: &PgPool,
    deposit_id: Uuid,
) -> Result<Option<(Scope, DepositAdmin)>, ApiError> {
    let row = sqlx::query_as::<_, DepositAdminRow>(
        r#"
        SELECT deposit.account_id, deposit.livemode, account.public_id AS account,
               deposit.state, deposit.route, deposit.route_version, deposit.receipt_log_index,
               deposit.block_time, deposit.final_at, deposit.price_scaled::text AS price_scaled,
               deposit.updated_at, deposit.settings_revision_id, deposit.settings_hold_id
        FROM deposits AS deposit
        JOIN accounts AS account ON account.id = deposit.account_id
        WHERE deposit.id = $1
        "#,
    )
    .bind(deposit_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let transitions = sqlx::query_as::<_, DepositTransitionRow>(
        r#"
        SELECT id, from_state, to_state, attempt, evidence, created_at
        FROM transitions
        WHERE deposit_id = $1
        ORDER BY created_at, id
        "#,
    )
    .bind(deposit_id)
    .fetch_all(pool)
    .await?;
    let events = sqlx::query_as::<_, DepositEventRow>(
        r#"
        SELECT event.id, event.type AS event_type, event.created AS created_at,
               (SELECT CASE WHEN bool_and(delivery.delivered_at IS NOT NULL)
                            THEN max(delivery.delivered_at) END
                FROM webhook_deliveries AS delivery
                WHERE delivery.event_id = event.id) AS delivered_at
        FROM events AS event
        WHERE event.object_type = 'deposit' AND event.object_id = $1
        ORDER BY event.created, event.id
        "#,
    )
    .bind(deposit_id)
    .fetch_all(pool)
    .await?;
    let admin = DepositAdmin {
        account: row.account,
        state: row.state,
        route: row.route,
        route_version: row
            .route_version
            .map(u64::try_from)
            .transpose()
            .map_err(|_| ApiError::internal())?,
        settings_revision: row
            .settings_revision_id
            .map(|id| crate::ids::format("psrev_", id)),
        settings_hold: row.settings_hold_id,
        receipt_log_index: u64::try_from(row.receipt_log_index)
            .map_err(|_| ApiError::internal())?,
        block_time: row.block_time,
        final_at: row.final_at,
        price_scaled: row.price_scaled,
        updated_at: row.updated_at,
        transitions: transitions.into_iter().map(Into::into).collect(),
        events: events.into_iter().map(Into::into).collect(),
    };
    Ok(Some((Scope::new(row.account_id, row.livemode), admin)))
}

/// Makes a deposit the pump processes (`detected` or `confirmed`) immediately claimable without
/// changing its state; any other state is `400 deposit_unexpected_state`, since the pump never
/// claims it.
pub async fn nudge_deposit(
    pool: &PgPool,
    deposit_id: Uuid,
    actor: &Actor,
) -> Result<NudgeResponse, ApiError> {
    let mut transaction = pool.begin().await?;
    let state: String = sqlx::query_scalar("SELECT state FROM deposits WHERE id = $1 FOR UPDATE")
        .bind(deposit_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if !matches!(state.as_str(), "detected" | "confirmed") {
        // Awaited, so the deposit is unlocked before the refusal is answered.
        transaction.rollback().await?;
        return Err(ApiError::deposit_unexpected_state(&state));
    }
    let (next_attempt_at, account_id) = sqlx::query_as::<_, (DateTime<Utc>, Uuid)>(
        r#"
        UPDATE deposits SET next_attempt_at = now() WHERE id = $1
        RETURNING next_attempt_at, account_id
        "#,
    )
    .bind(deposit_id)
    .fetch_one(&mut *transaction)
    .await?;
    insert_audit_tx(
        &mut transaction,
        Some(account_id),
        actor,
        "deposit_nudged",
        &format!(
            "deposit:{}",
            crate::ids::format(crate::ids::DEPOSIT, deposit_id)
        ),
    )
    .await?;
    transaction.commit().await?;
    Ok(NudgeResponse {
        deposit_id: crate::ids::format(crate::ids::DEPOSIT, deposit_id),
        next_attempt_at,
    })
}

/// Lifts a reconciliation block and appends an audit row, carrying the block, in the same
/// transaction.
///
/// Chain lifts require a fresh passing dual contract check before the audited lift. A repeated
/// lift of a lifted block returns the first lift without another audit row.
pub async fn lift_reconciliation_block(
    pool: &PgPool,
    routes: &RouteSet,
    block_key: &str,
    actor: &Actor,
    reason: &str,
) -> Result<ReconciliationBlockLiftResponse, ApiError> {
    let mut transaction = pool.begin().await?;
    let subject = format!("reconciliation_block:{block_key}");
    crate::db::rpc::lock_reconciliation_in(&mut transaction, block_key).await?;
    let block=sqlx::query_as::<_,ReconciliationBlockRow>("SELECT block_key,scope,chain_id,address_id,check_name,reason,created_at FROM reconciliation_blocks WHERE block_key=$1")
        .bind(block_key).fetch_optional(&mut *transaction).await?;
    if let Some(block) = block
        && block.scope == "chain"
    {
        let chain = u64::try_from(block.chain_id).map_err(|_| ApiError::chain_frozen())?;
        let a = routes
            .provider(chain, 0)
            .map_err(|_| ApiError::chain_frozen())?;
        let b = routes
            .provider(chain, 1)
            .map_err(|_| ApiError::chain_frozen())?;
        let checked = crate::contracts::check_pair(a, b, chain, routes.routes()).await;
        let passed = matches!(checked, Ok(crate::contracts::ContractCheck::Pass));
        a.contract_checked(passed);
        b.contract_checked(passed);
        if !passed {
            return Err(ApiError::chain_frozen());
        }
    }
    let lifted = sqlx::query_as::<_, ReconciliationBlockRow>(
        r#"
        DELETE FROM reconciliation_blocks
        WHERE block_key = $1
        RETURNING block_key, scope, chain_id, address_id, check_name, reason, created_at
        "#,
    )
    .bind(block_key)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(block) = lifted {
        let evidence = serde_json::json!({
            "reason": reason,
            "block": {
                "scope": block.scope,
                "chain_id": block.chain_id,
                "address_id": block.address_id,
                "check": block.check_name,
                "reason": block.reason,
                "created_at": block.created_at,
            },
        });
        insert_audit_tx_with_reason(
            &mut transaction,
            None,
            actor,
            "reconciliation_block.lift",
            &subject,
            &evidence.to_string(),
        )
        .await?;
    }
    let lifted_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"
        SELECT created_at FROM audit
        WHERE action = 'reconciliation_block.lift' AND subject = $1
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(&subject)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(ApiError::not_found)?;
    transaction.commit().await?;
    Ok(ReconciliationBlockLiftResponse {
        block_key: block_key.to_owned(),
        lifted_at,
    })
}

/// Adds or removes a customer's pause scopes and appends an audit row in the same transaction.
pub async fn mutate_customer_scopes(
    pool: &PgPool,
    customer: &Customer,
    requested: &[String],
    pause: bool,
    actor: &Actor,
) -> Result<Vec<String>, ApiError> {
    let mut transaction = pool.begin().await?;
    let current: Vec<String> =
        sqlx::query_scalar("SELECT paused_scopes FROM customers WHERE id = $1 FOR UPDATE")
            .bind(customer.id)
            .fetch_one(&mut *transaction)
            .await?;
    let updated = updated_scopes(current, requested, pause);
    sqlx::query("UPDATE customers SET paused_scopes = $2 WHERE id = $1")
        .bind(customer.id)
        .bind(&updated)
        .execute(&mut *transaction)
        .await?;
    insert_audit_tx(
        &mut transaction,
        Some(customer.account_id),
        actor,
        if pause { "pause" } else { "resume" },
        &format!("customer:{}", customer.id),
    )
    .await?;
    transaction.commit().await?;
    Ok(updated)
}

/// Adds or removes route pause scopes and appends the required administrative audit row.
pub async fn mutate_route_scopes(
    pool: &PgPool,
    route: &str,
    requested: &[String],
    pause: bool,
    actor: &Actor,
) -> Result<Vec<String>, ApiError> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO route_pauses (route, paused_scopes) VALUES ($1, '{}') ON CONFLICT DO NOTHING",
    )
    .bind(route)
    .execute(&mut *transaction)
    .await?;
    let current: Vec<String> =
        sqlx::query_scalar("SELECT paused_scopes FROM route_pauses WHERE route = $1 FOR UPDATE")
            .bind(route)
            .fetch_one(&mut *transaction)
            .await?;
    let updated = updated_scopes(current, requested, pause);
    sqlx::query("UPDATE route_pauses SET paused_scopes = $2 WHERE route = $1")
        .bind(route)
        .bind(&updated)
        .execute(&mut *transaction)
        .await?;
    insert_audit_tx(
        &mut transaction,
        None,
        actor,
        if pause { "pause" } else { "resume" },
        &format!("route:{route}"),
    )
    .await?;
    transaction.commit().await?;
    Ok(updated)
}

/// Returns active pause scopes for a route, or an empty set when it has no pause row.
pub async fn route_paused_scopes(pool: &PgPool, route: &str) -> Result<Vec<String>, ApiError> {
    Ok(
        sqlx::query_scalar("SELECT paused_scopes FROM route_pauses WHERE route = $1")
            .bind(route)
            .fetch_optional(pool)
            .await?
            .unwrap_or_default(),
    )
}

#[derive(FromRow)]
struct CustomerRow {
    id: Uuid,
    account_id: Uuid,
    livemode: bool,
    client_reference_id: String,
    paused_scopes: Vec<String>,
}

impl From<CustomerRow> for Customer {
    fn from(row: CustomerRow) -> Self {
        Self {
            id: row.id,
            account_id: row.account_id,
            livemode: row.livemode,
            client_reference_id: row.client_reference_id,
            paused_scopes: row.paused_scopes,
        }
    }
}

#[derive(FromRow)]
struct DepositAdminRow {
    account_id: Uuid,
    livemode: bool,
    account: String,
    state: String,
    route: Option<String>,
    route_version: Option<i64>,
    receipt_log_index: i64,
    block_time: DateTime<Utc>,
    final_at: Option<DateTime<Utc>>,
    price_scaled: Option<String>,
    updated_at: DateTime<Utc>,
    settings_revision_id: Option<Uuid>,
    settings_hold_id: Option<Uuid>,
}

#[derive(FromRow)]
struct DepositTransitionRow {
    id: Uuid,
    from_state: String,
    to_state: String,
    attempt: i32,
    evidence: Value,
    created_at: DateTime<Utc>,
}

impl From<DepositTransitionRow> for DepositTransition {
    fn from(row: DepositTransitionRow) -> Self {
        Self {
            id: row.id,
            from_state: row.from_state,
            to_state: row.to_state,
            attempt: row.attempt,
            evidence: row.evidence,
            created_at: row.created_at,
        }
    }
}

#[derive(FromRow)]
struct DepositEventRow {
    id: Uuid,
    event_type: String,
    created_at: DateTime<Utc>,
    delivered_at: Option<DateTime<Utc>>,
}

impl From<DepositEventRow> for DepositEventDelivery {
    fn from(row: DepositEventRow) -> Self {
        Self {
            id: crate::outbox::webhook_id(row.id),
            event_type: row.event_type,
            created_at: row.created_at,
            delivered_at: row.delivered_at,
        }
    }
}

#[derive(FromRow)]
struct ReconciliationBlockRow {
    block_key: String,
    scope: String,
    chain_id: i64,
    address_id: Option<Uuid>,
    check_name: String,
    reason: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<ReconciliationBlockRow> for ReconciliationBlockReport {
    type Error = ApiError;

    fn try_from(row: ReconciliationBlockRow) -> Result<Self, Self::Error> {
        Ok(Self {
            block_key: row.block_key,
            scope: row.scope,
            chain_id: count_u64(row.chain_id)?,
            address_id: row.address_id,
            check: row.check_name,
            reason: row.reason,
            created_at: row.created_at,
        })
    }
}

/// Computes the daily finance report entirely from persisted integer values.
pub async fn daily_report(
    pool: &PgPool,
    routes: &[RouteFile],
    generated_at: DateTime<Utc>,
    failing_for_hours: u32,
) -> Result<DailyReportResponse, ApiError> {
    let mut reports = BTreeMap::<String, RouteDailyReport>::new();
    for route in routes {
        reports
            .entry(route.route.clone())
            .or_insert_with(|| empty_route_report(route));
    }
    for row in
        sqlx::query("SELECT DISTINCT chain_id, asset_contract FROM deposits WHERE route IS NULL")
            .fetch_all(pool)
            .await?
    {
        let chain_id = count_u64(row.try_get("chain_id")?)?;
        let asset_contract: String = row.try_get("asset_contract")?;
        let key = unrouted_key(chain_id, &asset_contract);
        reports
            .entry(key.clone())
            .or_insert_with(|| empty_unrouted_report(key, chain_id, asset_contract));
    }

    for row in sqlx::query(
        r#"
        SELECT COALESCE(route, 'unrouted:' || chain_id::text || ':' || asset_contract) AS report_key,
               state, count(*)::bigint AS count
        FROM deposits
        GROUP BY report_key, state
        "#,
    )
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("report_key")?;
        let state: String = row.try_get("state")?;
        let count = count_u64(row.try_get("count")?)?;
        if let Some(report) = reports.get_mut(&route) {
            report.deposits_by_state.insert(state, count);
        }
    }

    for row in sqlx::query(
        r#"
        WITH held AS (
            SELECT COALESCE(route, 'unrouted:' || chain_id::text || ':' || asset_contract)
                       AS report_key,
                   chain_id, asset_contract, sum(amount_atomic) AS deposited
            FROM deposits
            WHERE state <> 'reversed'
            GROUP BY report_key, chain_id, asset_contract
        ), swept AS (
            SELECT chain_id, token, sum(amount_atomic) AS flushed
            FROM flushed
            GROUP BY chain_id, token
        )
        SELECT held.report_key,
               GREATEST(sum(held.deposited - COALESCE(swept.flushed, 0)), 0)::text AS amount
        FROM held
        LEFT JOIN swept
            ON swept.chain_id = held.chain_id AND swept.token = held.asset_contract
        GROUP BY held.report_key
        "#,
    )
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("report_key")?;
        if let Some(report) = reports.get_mut(&route) {
            report.unflushed_balance_atomic = row.try_get("amount")?;
        }
    }

    for row in sqlx::query(
        r#"
        SELECT route, COALESCE(sum(amount_atomic), 0)::text AS amount
        FROM quotes
        WHERE consumed_by IS NULL AND expires_at > $1
        GROUP BY route
        "#,
    )
    .bind(generated_at)
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("route")?;
        if let Some(report) = reports.get_mut(&route) {
            report.open_rate_lock_exposure_atomic = row.try_get("amount")?;
        }
    }

    for row in sqlx::query(
        r#"
        WITH succeeded AS (
            SELECT deposit_id, sum(amount_atomic) AS amount
            FROM refunds
            WHERE status = 'succeeded'
            GROUP BY deposit_id
        )
        SELECT COALESCE(
                   deposit.route,
                   'unrouted:' || deposit.chain_id::text || ':' || deposit.asset_contract
               ) AS report_key,
               COALESCE(sum(GREATEST(deposit.amount_atomic - COALESCE(succeeded.amount, 0), 0)), 0)::text AS amount
        FROM deposits AS deposit
        LEFT JOIN succeeded ON succeeded.deposit_id = deposit.id
        WHERE deposit.state = 'rejected'
        GROUP BY report_key
        "#,
    )
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("report_key")?;
        if let Some(report) = reports.get_mut(&route) {
            report.rejected_holds_atomic = row.try_get("amount")?;
        }
    }

    for row in sqlx::query(
        r#"
        SELECT COALESCE(
                   deposit.route,
                   'unrouted:' || deposit.chain_id::text || ':' || deposit.asset_contract
               ) AS report_key,
               count(DISTINCT event.id)::bigint AS count,
               COALESCE(
                   max(extract(epoch FROM ($1 - event.created)))::bigint, 0
               ) AS max_age_seconds
        FROM events AS event
        JOIN webhook_deliveries AS delivery ON delivery.event_id = event.id
        JOIN deposits AS deposit ON deposit.id = event.object_id
        WHERE event.type = 'deposit.credited' AND delivery.delivered_at IS NULL
        GROUP BY report_key
        "#,
    )
    .bind(generated_at)
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("report_key")?;
        let count = count_u64(row.try_get("count")?)?;
        let max_age = count_u64(row.try_get::<i64, _>("max_age_seconds")?.max(0))?;
        if let Some(report) = reports.get_mut(&route) {
            report.credited_undelivered = count;
            report.credited_undelivered_max_age_seconds = max_age;
        }
    }

    for row in sqlx::query(
        r#"
        SELECT COALESCE(
                   deposit.route,
                   'unrouted:' || deposit.chain_id::text || ':' || deposit.asset_contract
               ) AS report_key,
               refund.status, count(*)::bigint AS count
        FROM refunds AS refund
        JOIN deposits AS deposit ON deposit.id = refund.deposit_id
        GROUP BY report_key, refund.status
        "#,
    )
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("report_key")?;
        let status: String = row.try_get("status")?;
        let count = count_u64(row.try_get("count")?)?;
        if let Some(report) = reports.get_mut(&route) {
            report.refunds_by_status.insert(status, count);
        }
    }

    for row in sqlx::query(
        r#"
        SELECT COALESCE(
                   deposit.route,
                   'unrouted:' || deposit.chain_id::text || ':' || deposit.asset_contract
               ) AS report_key,
               deposit.state,
               max(GREATEST(
                   0,
                   floor(extract(epoch FROM ($1 - COALESCE(state_entry.entered_at, deposit.created_at))))
               ))::bigint AS age_seconds
        FROM deposits AS deposit
        LEFT JOIN LATERAL (
            SELECT max(transition.created_at) AS entered_at
            FROM transitions AS transition
            WHERE transition.deposit_id = deposit.id
              AND transition.to_state = deposit.state
              AND transition.from_state <> transition.to_state
        ) AS state_entry ON true
        GROUP BY report_key, deposit.state
        "#,
    )
    .bind(generated_at)
    .fetch_all(pool)
    .await?
    {
        let route: String = row.try_get("report_key")?;
        let state: String = row.try_get("state")?;
        let age = count_u64(row.try_get("age_seconds")?)?;
        if let Some(report) = reports.get_mut(&route) {
            report.age_in_state_max_seconds.insert(state, age);
        }
    }

    let exposure_minor = sqlx::query_scalar(
        r#"
        SELECT COALESCE(sum(credit_minor), 0)::text
        FROM quotes
        WHERE status = 'open' AND exposure_reserved
        "#,
    )
    .fetch_one(pool)
    .await?;

    let reconciliation_blocks = sqlx::query_as::<_, ReconciliationBlockRow>(
        r#"
        SELECT block_key, scope, chain_id, address_id, check_name, reason, created_at
        FROM reconciliation_blocks
        ORDER BY block_key
        "#,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect::<Result<_, _>>()?;

    let failing_webhook_endpoints =
        failing_webhook_endpoints(pool, generated_at, failing_for_hours).await?;

    Ok(DailyReportResponse {
        sanctions_snapshot: crate::sanctions::active(pool).await?,
        sanctions_manual_entries: sqlx::query_scalar(
            "SELECT count(*) FROM sanctions_manual_entries WHERE removed_at IS NULL",
        )
        .fetch_one(pool)
        .await?,
        generated_at,
        exposure_minor,
        routes: reports.into_values().collect(),
        reconciliation: None,
        reconciliation_blocks,
        failing_for_hours,
        failing_webhook_endpoints,
    })
}

/// Enabled webhook endpoints of every account whose oldest undelivered event is older than
/// `hours` at `at`, oldest first: deliveries are retried until delivered and never given up on, so
/// such an endpoint has failed for that long and its merchant has not fixed it (platform health).
async fn failing_webhook_endpoints(
    pool: &PgPool,
    at: DateTime<Utc>,
    hours: u32,
) -> Result<Vec<FailingWebhookEndpoint>, ApiError> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            bool,
            String,
            i64,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
            Option<i32>,
        ),
    >(
        r#"
        SELECT endpoint.id, account.public_id, endpoint.livemode, endpoint.url,
               count(*)::bigint, min(event.created), endpoint.last_attempt_at,
               endpoint.last_attempt_status
        FROM webhook_deliveries AS delivery
        JOIN webhook_endpoints AS endpoint ON endpoint.id = delivery.endpoint_id
        JOIN events AS event ON event.id = delivery.event_id
        JOIN accounts AS account ON account.id = endpoint.account_id
        WHERE delivery.delivered_at IS NULL AND delivery.failed_at IS NULL
          AND delivery.url IS NULL
          AND endpoint.status = 'enabled' AND endpoint.deleted_at IS NULL
        GROUP BY endpoint.id, account.public_id
        HAVING min(event.created) < $1 - make_interval(hours => $2)
        ORDER BY min(event.created), endpoint.id
        "#,
    )
    .bind(at)
    .bind(i32::try_from(hours).map_err(|_| ApiError::internal())?)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, account, livemode, url, pending, oldest, last_attempt_at, status)| {
                FailingWebhookEndpoint {
                    id: crate::ids::format(crate::ids::WEBHOOK_ENDPOINT, id),
                    account,
                    livemode,
                    url,
                    pending_deliveries: pending,
                    oldest_pending_at: oldest,
                    last_attempt_at,
                    last_attempt_status: status.and_then(|status| u16::try_from(status).ok()),
                }
            },
        )
        .collect())
}

fn empty_route_report(route: &RouteFile) -> RouteDailyReport {
    RouteDailyReport {
        route: route.route.clone(),
        chain_id: route.chain.chain_id,
        asset_contract: format!("{:#x}", route.asset.contract),
        unflushed_balance_atomic: "0".to_owned(),
        open_rate_lock_exposure_atomic: "0".to_owned(),
        rejected_holds_atomic: "0".to_owned(),
        deposits_by_state: zero_counts(&[
            "detected",
            "confirmed",
            "credited",
            "swept",
            "rejected",
            "reversed",
        ]),
        credited_undelivered: 0,
        credited_undelivered_max_age_seconds: 0,
        refunds_by_status: zero_counts(&["pending", "succeeded", "failed", "canceled"]),
        age_in_state_max_seconds: zero_counts(&[
            "detected",
            "confirmed",
            "credited",
            "swept",
            "rejected",
        ]),
    }
}

fn empty_unrouted_report(route: String, chain_id: u64, asset_contract: String) -> RouteDailyReport {
    RouteDailyReport {
        route,
        chain_id,
        asset_contract,
        unflushed_balance_atomic: "0".to_owned(),
        open_rate_lock_exposure_atomic: "0".to_owned(),
        rejected_holds_atomic: "0".to_owned(),
        deposits_by_state: zero_counts(&[
            "detected",
            "confirmed",
            "credited",
            "swept",
            "rejected",
            "reversed",
        ]),
        credited_undelivered: 0,
        credited_undelivered_max_age_seconds: 0,
        refunds_by_status: zero_counts(&["pending", "succeeded", "failed", "canceled"]),
        age_in_state_max_seconds: zero_counts(&[
            "detected",
            "confirmed",
            "credited",
            "swept",
            "rejected",
        ]),
    }
}

fn unrouted_key(chain_id: u64, asset_contract: &str) -> String {
    format!("unrouted:{chain_id}:{asset_contract}")
}

fn zero_counts(codes: &[&str]) -> BTreeMap<String, u64> {
    codes.iter().map(|code| ((*code).to_owned(), 0)).collect()
}

fn count_u64(value: i64) -> Result<u64, ApiError> {
    u64::try_from(value).map_err(|_| ApiError::internal())
}

fn parse_atomic(value: String) -> Result<U256, ApiError> {
    U256::from_str(&value).map_err(|_| ApiError::internal())
}

fn refund_deposit_from_row(
    row: &PgRow,
    min_refund: AtomicAmount,
) -> Result<RefundDeposit, ApiError> {
    let state: String = row.try_get("state")?;
    let reason: Option<String> = row.try_get("reason")?;
    Ok(RefundDeposit {
        state: crate::db::parse_state(&state).map_err(|_| ApiError::internal())?,
        reason: crate::db::parse_reason(reason.as_deref()).map_err(|_| ApiError::internal())?,
        amount: AtomicAmount::new(parse_atomic(row.try_get("amount_atomic")?)?),
        min_refund,
    })
}

fn updated_scopes(current: Vec<String>, requested: &[String], pause: bool) -> Vec<String> {
    let mut scopes = current.into_iter().collect::<BTreeSet<_>>();
    for scope in requested {
        if pause {
            scopes.insert(scope.clone());
        } else {
            scopes.remove(scope);
        }
    }
    scopes.into_iter().collect()
}

async fn insert_audit_tx(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    account_id: Option<Uuid>,
    actor: &Actor,
    action: &str,
    subject: &str,
) -> Result<(), ApiError> {
    insert_audit_tx_with_reason(
        transaction,
        account_id,
        actor,
        action,
        subject,
        "signed API request",
    )
    .await
}

async fn insert_audit_tx_with_reason(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    account_id: Option<Uuid>,
    actor: &Actor,
    action: &str,
    subject: &str,
    reason: &str,
) -> Result<(), ApiError> {
    audit::insert(
        &mut **transaction,
        &audit::Entry {
            account_id,
            actor,
            action,
            subject,
            reason,
        },
    )
    .await?;
    Ok(())
}
