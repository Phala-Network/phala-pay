//! The price snapshot cap survives concurrent workers and resets only on the next UTC day.
mod support;

use anyhow::{Result, ensure};
use chrono::NaiveDate;
use support::with_database;
use topup::db::daily_budgets::{QUOTE_SNAPSHOTS_PER_DAY, claim_on};

#[tokio::test]
async fn snapshot_budget_is_atomic_scoped_and_resets_next_utc_day() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let day = NaiveDate::from_ymd_opt(2026, 10, 7).expect("valid day");
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..150 {
                let pool = context.app_pool.clone();
                workers.spawn(async move {
                    claim_on(&pool, day, "price:1", QUOTE_SNAPSHOTS_PER_DAY).await
                });
            }
            let mut admitted = 0;
            while let Some(result) = workers.join_next().await {
                if result?? {
                    admitted += 1;
                }
            }
            ensure!(
                admitted == 60,
                "concurrent claims must admit exactly 60 snapshots"
            );
            ensure!(!claim_on(&context.app_pool, day, "price:1", QUOTE_SNAPSHOTS_PER_DAY).await?);
            ensure!(
                claim_on(
                    &context.app_pool,
                    day,
                    "price:8453",
                    QUOTE_SNAPSHOTS_PER_DAY
                )
                .await?
            );
            ensure!(
                claim_on(
                    &context.app_pool,
                    day.succ_opt().expect("next day"),
                    "price:1",
                    QUOTE_SNAPSHOTS_PER_DAY
                )
                .await?
            );
            let used: i32 = sqlx::query_scalar(
                "SELECT used FROM daily_budgets WHERE day=$1 AND name='price:1'",
            )
            .bind(day)
            .fetch_one(&context.app_pool)
            .await?;
            ensure!(
                used == 60,
                "refused attempts must leave the counter unchanged"
            );
            Ok(())
        })
    })
    .await
}
