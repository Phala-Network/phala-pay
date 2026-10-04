//! Durable-window restart, concurrent insertion and expand-only privileges.
mod support;
use alloy_primitives::{B256, U256};
use anyhow::{Result, ensure};
use topup::db::pricing::TwapStore;
use topup_adapters::pricing::{
    PriceError,
    uniswap_v2::{ObservationStore, Sample, average},
};
use topup_core::price::TwapConfig;

#[tokio::test]
async fn postgres_window_survives_restart_and_refused_samples_are_not_inserted() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let policy = TwapConfig::default();
            let ratio = U256::from(2)
                .checked_shl(112)
                .ok_or_else(|| anyhow::anyhow!("fixture ratio"))?;
            let sample = |i: u64| -> Result<Sample> {
                let elapsed = i
                    .checked_mul(60)
                    .ok_or_else(|| anyhow::anyhow!("fixture elapsed"))?;
                Ok(Sample {
                    block: 100_u64
                        .checked_add(i)
                        .ok_or_else(|| anyhow::anyhow!("fixture block"))?,
                    hash: B256::repeat_byte(1),
                    timestamp: 10_000_u64
                        .checked_add(elapsed)
                        .ok_or_else(|| anyhow::anyhow!("fixture timestamp"))?,
                    spot: ratio,
                    cumulative: ratio
                        .checked_mul(U256::from(elapsed))
                        .ok_or_else(|| anyhow::anyhow!("fixture cumulative"))?,
                })
            };
            let store = TwapStore(database.app_pool.clone());
            for i in 0..=29 {
                let s = sample(i)?;
                let h = store.record(&s, &policy).await?;
                ensure!(matches!(
                    average(&h, &s, s.timestamp, &policy),
                    Err(PriceError::Feed {
                        class: "twap_history",
                        ..
                    })
                ));
            }
            drop(store);
            let restarted = TwapStore(database.app_pool.clone());
            let s = sample(30)?;
            let (a, b) = tokio::join!(restarted.record(&s, &policy), restarted.record(&s, &policy));
            let h = a?;
            b?;
            ensure!(h.len() == 31);
            ensure!(average(&h, &s, s.timestamp, &policy)?.0 == ratio);
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM price_twap_observations")
                .fetch_one(&database.app_pool)
                .await?;
            ensure!(count == 31);
            let jumped = Sample {
                spot: ratio
                    .checked_mul(U256::from(2))
                    .ok_or_else(|| anyhow::anyhow!("fixture jump"))?,
                ..sample(31)?
            };
            ensure!(matches!(
                restarted.record(&jumped, &policy).await,
                Err(PriceError::Feed {
                    class: "twap_sample_jump",
                    ..
                })
            ));
            ensure!(restarted.latest(&policy).await? == Some(s));
            let after_gap = sample(40)?;
            let h = restarted.record(&after_gap, &policy).await?;
            ensure!(matches!(
                average(&h, &after_gap, after_gap.timestamp, &policy),
                Err(PriceError::Feed {
                    class: "twap_history",
                    ..
                })
            ));
            let changed = TwapConfig {
                window_s: 3600,
                ..policy
            };
            ensure!(
                restarted.record(&after_gap, &changed).await?.len() == 1,
                "policy changes cannot reuse looser history"
            );
            // The application cannot overwrite or erase history; binary rollback retains it.
            ensure!(
                sqlx::query("DELETE FROM price_twap_observations")
                    .execute(&database.app_pool)
                    .await
                    .is_err()
            );
            ensure!(
                sqlx::query("UPDATE price_twap_observations SET sample='{}'")
                    .execute(&database.app_pool)
                    .await
                    .is_err()
            );
            sqlx::raw_sql(include_str!(
                "../migrations/20261029000000_uniswap_twap.down.sql"
            ))
            .execute(&database.owner_pool)
            .await?;
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM price_twap_observations")
                .fetch_one(&database.app_pool)
                .await?;
            ensure!(count == 33);
            Ok(())
        })
    })
    .await
}
