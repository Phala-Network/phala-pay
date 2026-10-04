use std::collections::BTreeSet;

use sqlx::{PgPool, Postgres, Row, Transaction};
use topup_core::screening::PauseScopes;
use uuid::Uuid;

use crate::audit::{self, Actor};
use crate::tenancy::Scope;

pub(crate) struct PauseScopeSources {
    pub(crate) customer: PauseScopes,
    pub(crate) account: PauseScopes,
    pub(crate) route: PauseScopes,
    /// `settlement` while crediting of the deposit's treasury is paused.
    pub(crate) treasury: PauseScopes,
    pub(crate) effective: PauseScopes,
}

impl PauseScopeSources {
    pub(crate) fn from_codes(
        customer_codes: &[String],
        account_codes: &[String],
        route_codes: &[String],
        treasury_codes: &[String],
    ) -> Result<Self, sqlx::Error> {
        Ok(Self {
            customer: parse_codes(customer_codes)?,
            account: parse_codes(account_codes)?,
            route: parse_codes(route_codes)?,
            treasury: parse_codes(treasury_codes)?,
            effective: parse_codes(
                &customer_codes
                    .iter()
                    .chain(account_codes)
                    .chain(route_codes)
                    .chain(treasury_codes)
                    .collect::<Vec<_>>(),
            )?,
        })
    }
}

/// The pause scopes that apply to a customer's deposits on `route` to the forwarder `address_id`:
/// the customer's, the account's, the route's, and `settlement` while crediting of the treasury
/// the forwarder pays is paused.
pub(crate) async fn customer_pause_scopes(
    pool: &PgPool,
    customer_id: Uuid,
    route: &str,
    address_id: Uuid,
) -> Result<Option<PauseScopeSources>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT
            customer.paused_scopes AS customer_scopes,
            account.paused_scopes AS account_scopes,
            COALESCE(route_pause.paused_scopes, '{}'::text[]) AS route_scopes,
            CASE WHEN EXISTS (
                SELECT 1
                FROM addresses AS address
                JOIN treasuries AS treasury
                  ON treasury.account_id = address.account_id
                 AND treasury.livemode = address.livemode
                 AND treasury.chain_id = address.chain_id
                 AND treasury.address = address.treasury
                WHERE address.id = $3 AND cardinality(treasury.crediting_paused_by) > 0
            ) THEN ARRAY['settlement'] ELSE '{}'::text[] END AS treasury_scopes
        FROM customers AS customer
        JOIN accounts AS account ON account.id = customer.account_id
        LEFT JOIN route_pauses AS route_pause ON route_pause.route = $2
        WHERE customer.id = $1
        "#,
    )
    .bind(customer_id)
    .bind(route)
    .bind(address_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let customer_codes: Vec<String> = row.try_get("customer_scopes")?;
    let account_codes: Vec<String> = row.try_get("account_scopes")?;
    let route_codes: Vec<String> = row.try_get("route_scopes")?;
    let treasury_codes: Vec<String> = row.try_get("treasury_scopes")?;
    PauseScopeSources::from_codes(
        &customer_codes,
        &account_codes,
        &route_codes,
        &treasury_codes,
    )
    .map(Some)
}

fn parse_codes<S: AsRef<str>>(codes: &[S]) -> Result<PauseScopes, sqlx::Error> {
    PauseScopes::from_codes(codes).map_err(|error| sqlx::Error::Decode(error.to_string().into()))
}

/// Whose pause a mutation edits: of a whole account, the operator's (`paused_scopes`) or the
/// merchant's own (`self_paused_scopes`, design §12); of a treasury's crediting, the owner's entry
/// in `treasuries.crediting_paused_by`. Both apply; neither lifts the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PauseOwner {
    /// The operator's pauses: `quotes`, `settlement`, `refunds`.
    Operator,
    /// The merchant's own pause, through `POST /v1/account/pause`: `quotes` only.
    Merchant,
}

impl PauseOwner {
    /// The owner's code in `treasuries.crediting_paused_by`.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Merchant => "merchant",
        }
    }
}

/// Adds (`pause`) or removes `scopes` of the whole account, in the caller's transaction, with an
/// audit row naming `reason` and, when the scopes changed, an `account.updated` event in each mode
/// the account uses (test, and live once enabled). Returns the scopes of `owner`, or `None` for an
/// unknown account.
// The pause's operands and its audit context; a struct of them would only rename the call sites.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mutate_account_scopes_in(
    transaction: &mut Transaction<'_, Postgres>,
    routes: &crate::routes::RouteSet,
    account_id: Uuid,
    owner: PauseOwner,
    scopes: &[&str],
    pause: bool,
    actor: &Actor,
    reason: &str,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    let (select, update) = match owner {
        PauseOwner::Operator => (
            "SELECT paused_scopes, public_id, charges_enabled FROM accounts WHERE id = $1 \
             FOR UPDATE",
            "UPDATE accounts SET paused_scopes = $2 WHERE id = $1",
        ),
        PauseOwner::Merchant => (
            "SELECT self_paused_scopes, public_id, charges_enabled FROM accounts WHERE id = $1 \
             FOR UPDATE",
            "UPDATE accounts SET self_paused_scopes = $2 WHERE id = $1",
        ),
    };
    let Some((current, public_id, charges_enabled)) =
        sqlx::query_as::<_, (Vec<String>, String, bool)>(select)
            .bind(account_id)
            .fetch_optional(&mut **transaction)
            .await?
    else {
        return Ok(None);
    };
    let mut updated = current.iter().cloned().collect::<BTreeSet<_>>();
    for scope in scopes {
        if pause {
            updated.insert((*scope).to_owned());
        } else {
            updated.remove(*scope);
        }
    }
    let updated: Vec<String> = updated.into_iter().collect();
    audit::insert(
        &mut **transaction,
        &audit::Entry {
            account_id: Some(account_id),
            actor,
            action: if pause { "pause" } else { "resume" },
            subject: &format!("account:{public_id}"),
            reason,
        },
    )
    .await?;
    let mut before = current;
    before.sort();
    if updated == before {
        return Ok(Some(updated));
    }
    let modes: &[bool] = if charges_enabled {
        &[false, true]
    } else {
        &[false]
    };
    let object = crate::db::EventObject::Account(account_id);
    let mut previous = Vec::with_capacity(modes.len());
    for &livemode in modes {
        let scope = Scope::new(account_id, livemode);
        previous.push(crate::db::render(transaction, routes, scope, object).await?);
    }
    sqlx::query(update)
        .bind(account_id)
        .bind(&updated)
        .execute(&mut **transaction)
        .await?;
    for (&livemode, before) in modes.iter().zip(&previous) {
        let scope = Scope::new(account_id, livemode);
        let event = crate::db::NewOutboxEvent::new("account.updated", scope, object, actor);
        crate::db::enqueue_in(transaction, routes, &event, Some(before)).await?;
    }
    Ok(Some(updated))
}

/// Effective instance scopes: an expired lease is cleared on read, without a worker or runner.
pub(crate) async fn instance_pause(
    pool: &PgPool,
) -> Result<(Vec<String>, String, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT CASE WHEN expires_at > clock_timestamp() THEN paused_scopes ELSE '{}'::text[] END, \
         owner, extract(epoch FROM expires_at)::bigint FROM instance_pause WHERE singleton",
    )
    .fetch_one(pool)
    .await
}

pub(crate) async fn mutations_paused(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM instance_pause WHERE singleton \
         AND expires_at > clock_timestamp() AND 'mutations' = ANY(paused_scopes))",
    )
    .fetch_one(pool)
    .await
}

/// Changes only the deployment-owned instance pause, never an account/customer/route pause.
/// Zero duration resumes. Compare ownership under a row lock so stale cleanup cannot lift a
/// newer deployment's pause. The audit and lease commit together.
pub(crate) async fn mutate_instance_pause(
    pool: &PgPool,
    owner: &str,
    duration: i64,
    actor: &Actor,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (current_owner, active): (String, bool) = sqlx::query_as(
        "SELECT owner, expires_at > clock_timestamp() AND cardinality(paused_scopes) > 0 \
         FROM instance_pause WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    if active && current_owner != owner {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE instance_pause SET paused_scopes = $1, owner = $2, \
         expires_at = clock_timestamp() + $3::bigint * interval '1 second' WHERE singleton",
    )
    .bind(if duration > 0 {
        vec!["mutations"]
    } else {
        vec![]
    })
    .bind(owner)
    .bind(duration)
    .execute(&mut *tx)
    .await?;
    audit::insert(
        &mut *tx,
        &audit::Entry {
            account_id: None,
            actor,
            action: if duration > 0 { "pause" } else { "resume" },
            subject: "instance",
            reason,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}
