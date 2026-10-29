//! The price snapshot cap survives concurrent workers and resets only on the next UTC day.
mod support;

use anyhow::{Result, ensure};
use chrono::NaiveDate;
use support::with_database;
use topup::db::daily_budgets::{QUOTE_SNAPSHOTS_PER_DAY, claim_on};
use topup_adapters::pricing::snapshot::SnapshotBudget;

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
            // The internal pilot probe and exporter share this persisted snapshot. A fresh
            // observation after restart still sees the exhausted day, and UTC rollover selects
            // the new day's rows rather than carrying yesterday's exhaustion forward.
            for (observed_day, expected) in [(day, 60), (day.succ_opt().unwrap(), 1)] {
                let now = observed_day.and_hms_opt(0, 0, 0).unwrap().and_utc();
                let text =
                    topup::observability::metrics::render_chain_reads_at(&context.app_pool, now)
                        .await?;
                ensure!(text.lines().any(|line| line
                    == format!("topup_daily_budget_used{{name=\"price:1\"}} {expected}")));
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn configured_snapshot_budget_is_atomic_and_config_changes_preserve_usage() -> Result<()> {
    for limit in [1, 150] {
        with_database(|context| {
            Box::pin(async move {
                let budget = std::sync::Arc::new(topup::db::pricing::PriceBudget {
                    pool: context.app_pool.clone(),
                    limit,
                });
                let mut workers = tokio::task::JoinSet::new();
                for _ in 0..limit + 20 {
                    let budget = budget.clone();
                    workers.spawn(async move { budget.claim(1).await });
                }
                let mut admitted = 0;
                while let Some(result) = workers.join_next().await {
                    admitted += i32::from(result??);
                }
                ensure!(
                    admitted == limit,
                    "configured snapshot cap must hold across concurrent workers"
                );
                ensure!(!budget.claim(1).await?);
                ensure!(
                    budget.claim(8453).await?,
                    "price chains have independent budgets"
                );
                let restarted = topup::db::pricing::PriceBudget {
                    pool: context.app_pool.clone(),
                    limit,
                };
                ensure!(!restarted.claim(1).await?, "restart cannot reset usage");
                let lower = topup::db::pricing::PriceBudget {
                    pool: context.app_pool.clone(),
                    limit: 1,
                };
                ensure!(
                    !lower.claim(1).await?,
                    "lowering the cap cannot reset usage"
                );
                let higher = topup::db::pricing::PriceBudget {
                    pool: context.app_pool.clone(),
                    limit: limit + 1,
                };
                ensure!(higher.claim(1).await?);
                ensure!(!higher.claim(1).await?);
                Ok(())
            })
        })
        .await?;
    }
    Ok(())
}
