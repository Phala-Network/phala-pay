//! Durable, immutable service-recorded PHA/WETH observations, shared by quote and credit paths.
use async_trait::async_trait;
use sqlx::{PgPool, types::Json};
use topup_adapters::pricing::{
    PriceError,
    uniswap_v2::{ObservationStore, SAMPLE_INTERVAL_S, Sample, check_sample},
};
use topup_core::price::TwapConfig;

/// PostgreSQL storage scoped by the complete safety policy. Policy changes start a new window.
pub struct TwapStore {
    pool: PgPool,
    interval_s: u64,
}
impl TwapStore {
    /// Original one-minute observation cadence and default guard limits.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            interval_s: 60,
        }
    }
    /// Approved staging-only 300-second cadence; limits remain route configuration.
    pub fn staging(pool: PgPool) -> Self {
        Self {
            pool,
            interval_s: SAMPLE_INTERVAL_S,
        }
    }
}
fn storage_error(_: sqlx::Error) -> PriceError {
    tracing::error!("TWAP observation persistence failed");
    topup_adapters::pricing::uniswap_v2::refusal("twap_storage", serde_json::Value::Null)
}
fn policy_key(policy: &TwapConfig) -> Result<String, PriceError> {
    serde_json::to_string(policy).map_err(|_| PriceError::InvalidPrice)
}
#[async_trait]
impl ObservationStore for TwapStore {
    async fn history(&self, policy: &TwapConfig) -> Result<Vec<Sample>, PriceError> {
        let samples: Vec<Json<Sample>> = sqlx::query_scalar("SELECT sample FROM price_twap_observations WHERE policy=$1 AND block_timestamp >= (SELECT max(block_timestamp) FROM price_twap_observations WHERE policy=$1) - $2 ORDER BY block_timestamp")
            .bind(policy_key(policy)?)
            .bind(i64::try_from(policy.window_s.saturating_add(policy.max_sample_age_s).saturating_add(self.interval_s)).map_err(|_| PriceError::InvalidTimestamp)?)
            .fetch_all(&self.pool).await.map_err(storage_error)?;
        Ok(samples.into_iter().map(|sample| sample.0).collect())
    }
    async fn latest(&self, policy: &TwapConfig) -> Result<Option<Sample>, PriceError> {
        sqlx::query_scalar::<_, Json<Sample>>("SELECT sample FROM price_twap_observations WHERE policy=$1 ORDER BY block_timestamp DESC LIMIT 1")
            .bind(policy_key(policy)?).fetch_optional(&self.pool).await.map(|s| s.map(|s| s.0)).map_err(storage_error)
    }
    async fn record(
        &self,
        sample: &Sample,
        policy: &TwapConfig,
    ) -> Result<Vec<Sample>, PriceError> {
        let key = policy_key(policy)?;
        let block = i64::try_from(sample.block).map_err(|_| PriceError::InvalidTimestamp)?;
        let timestamp =
            i64::try_from(sample.timestamp).map_err(|_| PriceError::InvalidTimestamp)?;
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        // Concurrent quote/credit/sampler workers serialize validation and insertion per policy.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 26120450))")
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        let previous: Option<Json<Sample>> = sqlx::query_scalar("SELECT sample FROM price_twap_observations WHERE policy=$1 ORDER BY block_timestamp DESC LIMIT 1")
            .bind(&key).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        if let Some(previous) = &previous
            && let Err(error) = check_sample(&previous.0, sample, policy)
        {
            tx.rollback().await.map_err(storage_error)?;
            return Err(error);
        }
        let insert = previous
            .as_ref()
            .is_none_or(|p| sample.timestamp.saturating_sub(p.0.timestamp) >= self.interval_s);
        if insert {
            sqlx::query("INSERT INTO price_twap_observations (policy, block_number, block_timestamp, sample) VALUES ($1,$2,$3,$4)")
                .bind(&key).bind(block).bind(timestamp).bind(Json(sample)).execute(&mut *tx).await.map_err(storage_error)?;
        }
        let floor = sample
            .timestamp
            .saturating_sub(policy.window_s)
            .saturating_sub(policy.max_sample_age_s)
            .saturating_sub(self.interval_s);
        let history: Vec<Json<Sample>> = sqlx::query_scalar("SELECT sample FROM price_twap_observations WHERE policy=$1 AND block_timestamp >= $2 ORDER BY block_timestamp")
            .bind(&key).bind(i64::try_from(floor).map_err(|_| PriceError::InvalidTimestamp)?).fetch_all(&mut *tx).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(history.into_iter().map(|s| s.0).collect())
    }
}

/// Atomic environment-local UTC quote snapshot budget, independent of sampler and confirm.
pub struct PriceBudget(pub PgPool);
#[async_trait]
impl topup_adapters::pricing::snapshot::SnapshotBudget for PriceBudget {
    async fn claim(&self, chain_id: u64) -> Result<bool, PriceError> {
        super::daily_budgets::claim(
            &self.0,
            &format!("price:{chain_id}"),
            super::daily_budgets::QUOTE_SNAPSHOTS_PER_DAY,
        )
        .await
        .map_err(storage_error)
    }
}
