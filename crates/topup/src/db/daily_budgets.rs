//! Atomic UTC daily limits shared by quote workers and future transaction submissions.

use chrono::{NaiveDate, Utc};
use sqlx::PgPool;

/// Fresh quote snapshots allowed per price chain and UTC day in this environment.
pub const QUOTE_SNAPSHOTS_PER_DAY: i32 = 60;

/// Claims one unit without exceeding `limit`; a failed persistence operation is never admission.
pub async fn claim(pool: &PgPool, name: &str, limit: i32) -> Result<bool, sqlx::Error> {
    claim_on(pool, Utc::now().date_naive(), name, limit).await
}

/// Claims at an explicit UTC day, for deterministic rollover and concurrency tests.
pub async fn claim_on(
    pool: &PgPool,
    day: NaiveDate,
    name: &str,
    limit: i32,
) -> Result<bool, sqlx::Error> {
    if name.is_empty() || name.len() > 80 || limit <= 0 {
        return Err(sqlx::Error::Protocol("invalid daily budget".to_owned()));
    }
    Ok(sqlx::query(
        "INSERT INTO daily_budgets(day,name,used) VALUES($1,$2,1) \
         ON CONFLICT(day,name) DO UPDATE SET used=daily_budgets.used+1 \
         WHERE daily_budgets.used < $3",
    )
    .bind(day)
    .bind(name)
    .bind(limit)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}
