//! Durable UTC-day RPC accounting and deposit observation-day regressions.

mod support;

use anyhow::{Result, ensure};
use chrono::NaiveDate;
use topup::db::rpc_usage::{Recorder, snapshot};

#[tokio::test]
async fn observation_index_resumes_completed_concurrent_build() -> Result<()> {
    let Some(database) = support::TestDatabase::create_with_migrations(false).await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.owner_pool;
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut previous = sqlx::migrate::Migrator::new(directory.as_path()).await?;
        previous.migrations.to_mut().retain(|m| m.version < 20261111000001);
        previous.run(pool).await?;
        sqlx::query(include_str!("../migrations/20261111000001_deposits_observed_day.up.sql"))
            .execute(pool).await?;
        let index = || async {
            sqlx::query_as::<_, (i64, bool)>(
                "SELECT c.oid::bigint,i.indisvalid FROM pg_class c \
                 JOIN pg_index i ON i.indexrelid=c.oid WHERE c.relname='deposits_observed_day_idx'",
            ).fetch_one(pool).await
        };
        let before = index().await?;
        ensure!(before.1);
        topup::db::migrate(pool).await?;
        topup::db::migrate(pool).await?;
        ensure!(index().await? == before, "completed observation index was rebuilt");
        let floor: i64 = sqlx::query_scalar(
            "SELECT compatibility_floor FROM topup_migration_compatibility WHERE version=20261111000001",
        ).fetch_one(pool).await?;
        ensure!(floor == 20261105000000, "quota monitoring raised the rollback floor");
        Ok::<_, anyhow::Error>(())
    }.await;
    result.and(database.cleanup().await)
}

#[tokio::test]
async fn durable_packet_usage_counts_batch_members_retries_and_utc_days() -> Result<()> {
    support::with_database(|context| {
        Box::pin(async move {
            let day = chrono::Utc::now().date_naive();
            let recorder = std::sync::Arc::new(Recorder::default());
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..10 {
                let recorder = recorder.clone();
                workers.spawn(async move {
                    recorder.record_on(
                        day,
                        "ankr-mainnet",
                        1,
                        &["eth_getLogs", "eth_call", "eth_call"],
                    );
                });
            }
            while let Some(result) = workers.join_next().await {
                result?;
            }
            recorder.record_on(day, "ankr-mainnet", 1, &["eth_getLogs"]); // Transport retry.
            recorder.record_on(day, "ankr-base-mainnet", 8453, &["eth_getLogs"]);
            recorder.record_on(day.succ_opt().unwrap(), "ankr-mainnet", 1, &["eth_call"]);
            ensure!(
                snapshot(&context.app_pool, day).await?.is_empty(),
                "call path must not write DB"
            );
            // Failed background persistence retains every buffered call.
            sqlx::query("REVOKE INSERT ON rpc_daily_usage FROM topup_app")
                .execute(&context.owner_pool)
                .await?;
            let failures = topup_adapters::chain::evm::metrics::rpc_accounting_failures();
            let cancellation = tokio_util::sync::CancellationToken::new();
            let worker = tokio::spawn({
                let recorder = recorder.clone();
                let pool = context.app_pool.clone();
                let cancellation = cancellation.clone();
                async move { recorder.run(pool, cancellation).await }
            });
            let failed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while topup_adapters::chain::evm::metrics::rpc_accounting_failures() == failures {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
            sqlx::query("GRANT INSERT ON rpc_daily_usage TO topup_app")
                .execute(&context.owner_pool)
                .await?;
            cancellation.cancel();
            worker.await?; // The shutdown flush retries the failed snapshot.
            failed?;
            recorder.flush(&context.app_pool).await?; // Lost acknowledgements/retries cannot double count.
            let restarted = Recorder::default();
            restarted.record_on(day, "ankr-mainnet", 1, &["eth_getLogs"]);
            restarted.flush(&context.app_pool).await?;
            let counts = snapshot(&context.app_pool, day).await?;
            ensure!(
                counts.iter().any(|r| r.provider == "ankr-mainnet"
                    && r.method == "eth_call"
                    && r.calls == 20)
            );
            ensure!(counts.iter().any(|r| r.provider == "ankr-mainnet"
                && r.method == "eth_getLogs"
                && r.calls == 12));
            ensure!(
                counts
                    .iter()
                    .any(|r| r.provider == "ankr-base-mainnet" && r.calls == 1)
            );
            let tomorrow = snapshot(&context.app_pool, day.succ_opt().unwrap()).await?;
            ensure!(tomorrow.len() == 1 && tomorrow[0].calls == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn deposits_count_observation_day_once() -> Result<()> {
    support::with_database(|context| Box::pin(async move {
        let mut connection = context.app_pool.acquire().await?;
        sqlx::raw_sql("SET TIME ZONE 'America/Los_Angeles';
            CREATE TEMP TABLE deposits (id int PRIMARY KEY, created_at timestamptz, block_time timestamptz, updated_at timestamptz);
            INSERT INTO deposits VALUES (1,'2026-10-31 00:00Z','2026-10-01 00:00Z',now()),
                (2,'2026-10-31 23:59:59Z','2026-10-30 00:00Z',now()),
                (3,'2026-11-01 00:00Z','2026-10-31 00:00Z',now());
            INSERT INTO deposits VALUES (1,now(),now(),now()) ON CONFLICT DO NOTHING;
            UPDATE deposits SET updated_at=now() WHERE id=2;")
            .execute(&mut *connection).await?;
        let day = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        ensure!(topup::db::rpc_usage::deposits_on(&mut *connection, day).await? == 2);
        ensure!(topup::db::rpc_usage::deposits_on(&mut *connection, day.succ_opt().unwrap()).await? == 1);
        Ok(())
    })).await
}
