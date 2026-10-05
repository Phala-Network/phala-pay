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

/// Request-admission pause owned by one running API process. A new process starts paused until
/// its first successful health probe; explicit deployment pauses have a monotonic deadline.
/// Process exit discards its lease, so a replacement or rollback never inherits maintenance.
pub(crate) struct InstancePause {
    lease: tokio::sync::Mutex<InstanceLease>,
}

struct InstanceLease {
    owner: String,
    deadline: tokio::time::Instant,
    expires_at: i64,
    booting: bool,
}

impl Default for InstancePause {
    fn default() -> Self {
        Self {
            lease: tokio::sync::Mutex::new(InstanceLease {
                owner: String::new(),
                deadline: tokio::time::Instant::now(),
                expires_at: chrono::Utc::now().timestamp(),
                booting: false,
            }),
        }
    }
}

impl InstancePause {
    pub(crate) fn booting() -> Self {
        Self {
            lease: tokio::sync::Mutex::new(InstanceLease {
                owner: "startup".to_owned(),
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(900),
                expires_at: chrono::Utc::now().timestamp().saturating_add(900),
                booting: true,
            }),
        }
    }

    pub(crate) async fn snapshot(&self) -> (Vec<String>, String, i64) {
        let lease = self.lease.lock().await;
        let scopes = if lease.deadline > tokio::time::Instant::now() {
            vec!["mutations".to_owned()]
        } else {
            vec![]
        };
        (scopes, lease.owner.clone(), lease.expires_at)
    }

    pub(crate) async fn mutations_paused(&self) -> bool {
        self.lease.lock().await.deadline > tokio::time::Instant::now()
    }

    /// Health lifts only the startup pause. Old-process health probes never lift a deployment's
    /// explicit drain, and all already admitted requests finish independently of this lease.
    pub(crate) async fn healthy_boot(&self) {
        let mut lease = self.lease.lock().await;
        if lease.booting {
            lease.deadline = tokio::time::Instant::now();
            lease.expires_at = chrono::Utc::now().timestamp();
            lease.booting = false;
        }
    }

    /// Zero duration resumes. Serialize ownership, audit commit and admission together: an audit
    /// failure leaves the lease unchanged, and stale cleanup cannot lift a newer active owner.
    pub(crate) async fn mutate(
        &self,
        pool: &PgPool,
        owner: &str,
        duration: i64,
        actor: &Actor,
        reason: &str,
    ) -> Result<bool, sqlx::Error> {
        let mut lease = self.lease.lock().await;
        if lease.deadline > tokio::time::Instant::now() && lease.owner != owner {
            return Ok(false);
        }
        let seconds = u64::try_from(duration)
            .map_err(|_| sqlx::Error::Protocol("invalid pause duration".to_owned()))?;
        // Admission shares this lock with mutations. Bound audit persistence so a stalled
        // database cannot prevent requests from observing lease expiry indefinitely.
        let record = async {
            let mut tx = pool.begin().await?;
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
            tx.commit().await
        };
        // The audit commit may already have landed when this deadline expires. Its outcome is
        // unknown, so this must not be classified as a pool acquisition timeout.
        tokio::time::timeout(std::time::Duration::from_secs(5), record)
            .await
            .map_err(|_| sqlx::Error::Io(std::io::ErrorKind::TimedOut.into()))??;
        *lease = InstanceLease {
            owner: owner.to_owned(),
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(seconds),
            expires_at: chrono::Utc::now().timestamp().saturating_add(duration),
            booting: false,
        };
        Ok(true)
    }
}

#[cfg(test)]
mod instance_tests {
    use super::*;
    use axum::response::IntoResponse;

    #[tokio::test(start_paused = true)]
    async fn startup_pause_expires_without_a_health_probe_or_worker() {
        let pause = InstancePause::booting();
        assert!(pause.mutations_paused().await);
        tokio::time::advance(std::time::Duration::from_secs(901)).await;
        assert!(!pause.mutations_paused().await);
        assert!(pause.snapshot().await.0.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn failed_audit_releases_admission_and_does_not_set_a_pause() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(60))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("test connection URL");
        let pause = InstancePause::default();
        let started = tokio::time::Instant::now();
        let result = pause
            .mutate(
                &pool,
                "deploy-1",
                900,
                &Actor::admin("admin/test"),
                "test upgrade",
            )
            .await;
        let error = result.unwrap_err();
        assert!(
            matches!(&error, sqlx::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        let error = crate::api::error::ApiError::from(error);
        assert_eq!(error.code(), "internal_error");
        let response = error.into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(
            response
                .extensions()
                .get::<crate::api::error::NotExecuted>()
                .is_none()
        );
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::RETRY_AFTER)
        );
        assert!(started.elapsed() <= std::time::Duration::from_secs(5));
        assert!(!pause.mutations_paused().await);
        pool.close().await;
    }
}
