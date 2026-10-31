//! Durable UTC-day RPC accounting and deposit observation-day regressions.

mod support;

use anyhow::{Result, ensure};
use chrono::NaiveDate;
use topup::db::rpc_usage::{Recorder, snapshot};

#[tokio::test]
async fn durable_packet_usage_counts_batch_members_retries_and_utc_days() -> Result<()> {
    support::with_database(|context| {
        Box::pin(async move {
            let day = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
            let recorder = Recorder(context.app_pool.clone());
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..10 {
                let recorder = Recorder(context.app_pool.clone());
                workers.spawn(async move {
                    recorder
                        .record_on(
                            day,
                            "ankr-mainnet",
                            1,
                            &["eth_getLogs", "eth_call", "eth_call"],
                        )
                        .await
                });
            }
            while let Some(result) = workers.join_next().await {
                result??;
            }
            // An identical retried packet is billed again, and a new recorder cannot reset usage.
            let restarted = Recorder(context.app_pool.clone());
            restarted
                .record_on(day, "ankr-mainnet", 1, &["eth_getLogs"])
                .await?;
            recorder
                .record_on(day, "ankr-base-mainnet", 8453, &["eth_getLogs"])
                .await?;
            recorder
                .record_on(day.succ_opt().unwrap(), "ankr-mainnet", 1, &["eth_call"])
                .await?;
            let counts = snapshot(&context.app_pool, day).await?;
            ensure!(
                counts.iter().any(|r| r.provider == "ankr-mainnet"
                    && r.method == "eth_call"
                    && r.calls == 20)
            );
            ensure!(counts.iter().any(|r| r.provider == "ankr-mainnet"
                && r.method == "eth_getLogs"
                && r.calls == 11));
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
