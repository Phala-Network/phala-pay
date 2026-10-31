//! Durable UTC-day transport usage; monitoring only, never RPC admission.

use chrono::{NaiveDate, Utc};
use sqlx::PgPool;
use topup_adapters::chain::evm::metrics::{RpcAccountingError, RpcCallRecorder};

/// One bounded endpoint/method/day tally, independent of process lifetime.
#[derive(Debug, sqlx::FromRow)]
pub struct Usage {
    /// Configured endpoint id, never a URL.
    pub provider: String,
    /// Bounded JSON-RPC method label.
    pub method: String,
    /// Number of attempted calls, including failed/retried requests.
    pub calls: i64,
}

/// A dedicated small pool avoids waiting for a connection held by the RPC caller's transaction.
#[derive(Debug)]
pub struct Recorder(pub PgPool);

impl Recorder {
    /// Persist one packet atomically, grouping duplicate batch methods into a single upsert.
    pub async fn record_on(
        &self,
        day: NaiveDate,
        provider: &str,
        chain: i64,
        methods: &[&str],
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO rpc_daily_usage(day,provider,chain_id,method,calls) \
             SELECT $1,$2,$3,method,count(*) FROM unnest($4::text[]) AS methods(method) GROUP BY method \
             ORDER BY method ON CONFLICT(day,provider,chain_id,method) \
             DO UPDATE SET calls=rpc_daily_usage.calls+EXCLUDED.calls",
        )
        .bind(day)
        .bind(provider)
        .bind(chain)
        .bind(methods)
        .execute(&self.0)
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl RpcCallRecorder for Recorder {
    async fn record(
        &self,
        provider: &str,
        chain: Option<u64>,
        methods: &[&'static str],
    ) -> Result<(), RpcAccountingError> {
        let chain = chain
            .and_then(|c| i64::try_from(c).ok())
            .ok_or(RpcAccountingError)?;
        self.record_on(Utc::now().date_naive(), provider, chain, methods)
            .await
            .map_err(|_| RpcAccountingError)
    }
}

/// Read the persisted day's endpoint/method counts, including calls before a restart.
pub async fn snapshot(pool: &PgPool, day: NaiveDate) -> Result<Vec<Usage>, sqlx::Error> {
    sqlx::query_as(
        "SELECT provider,method,sum(calls)::bigint AS calls FROM rpc_daily_usage \
        WHERE day=$1 GROUP BY provider,method",
    )
    .bind(day)
    .fetch_all(pool)
    .await
}

/// Count each newly observed deposit once, including rejected deposits, regardless of retries.
/// The persisted insertion timestamp makes this restart-safe and distinct from chain block time.
pub async fn deposits_on<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    day: NaiveDate,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM deposits WHERE created_at >= \
        ($1::date::timestamp AT TIME ZONE 'UTC') AND created_at < \
        (($1::date + 1)::timestamp AT TIME ZONE 'UTC')",
    )
    .bind(day)
    .fetch_one(executor)
    .await
}
