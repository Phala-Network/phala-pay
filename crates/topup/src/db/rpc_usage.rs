//! Durable UTC-day usage flushed off the transport path; monitoring only, never RPC admission.

use chrono::{NaiveDate, Utc};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::metrics::{
    RpcAccountingError, RpcCallRecorder, rpc_accounting_failed,
};
use uuid::Uuid;

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

type Key = (String, i64, &'static str);
#[derive(Clone, Debug, PartialEq)]
struct Day {
    epoch: Uuid,
    calls: BTreeMap<Key, i64>,
}

/// Synchronous counters only on the call path; one batched durable snapshot every 30 seconds.
/// Epochs make retries idempotent even if a committed write loses its acknowledgement.
#[derive(Debug, Default)]
pub struct Recorder {
    days: Mutex<BTreeMap<NaiveDate, Day>>,
}

impl Recorder {
    /// Buffer one packet at an explicit UTC day. No database I/O, asynchronous work or task creation.
    pub fn record_on(&self, day: NaiveDate, provider: &str, chain: i64, methods: &[&'static str]) {
        let mut days = self.days.lock().unwrap_or_else(PoisonError::into_inner);
        let day = days.entry(day).or_insert_with(|| Day {
            epoch: Uuid::new_v4(),
            calls: BTreeMap::new(),
        });
        for method in methods {
            let count = day
                .calls
                .entry((provider.to_owned(), chain, method))
                .or_default();
            *count = count.saturating_add(1);
        }
    }

    /// Flush a cumulative snapshot with one upsert. Failed writes leave every count in memory.
    pub async fn flush(&self, pool: &PgPool) -> Result<(), sqlx::Error> {
        let snapshot = self
            .days
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut dates = Vec::new();
        let mut epochs = Vec::new();
        let mut providers = Vec::new();
        let mut chains = Vec::new();
        let mut methods = Vec::new();
        let mut counts = Vec::new();
        for (date, day) in &snapshot {
            for ((provider, chain, method), count) in &day.calls {
                dates.push(*date);
                epochs.push(day.epoch);
                providers.push(provider.as_str());
                chains.push(*chain);
                methods.push(*method);
                counts.push(*count);
            }
        }
        if dates.is_empty() {
            return Ok(());
        }
        sqlx::query("INSERT INTO rpc_daily_usage(day,epoch,provider,chain_id,method,calls) \
            SELECT * FROM unnest($1::date[],$2::uuid[],$3::text[],$4::bigint[],$5::text[],$6::bigint[]) \
            ORDER BY 1,2,3,4,5 ON CONFLICT(day,epoch,provider,chain_id,method) \
            DO UPDATE SET calls=GREATEST(rpc_daily_usage.calls,EXCLUDED.calls)")
            .bind(dates).bind(epochs).bind(providers).bind(chains).bind(methods).bind(counts)
            .execute(pool).await?;
        // Keep today's cumulative epoch; drop old epochs only when no calls arrived during I/O.
        // A late/backwards-clock old-day call then gets a fresh epoch, preserving its count too.
        let today = Utc::now().date_naive();
        let mut days = self.days.lock().unwrap_or_else(PoisonError::into_inner);
        days.retain(|date, day| *date >= today || snapshot.get(date) != Some(day));
        Ok(())
    }

    /// Flush every 30 seconds and once at shutdown. Failure retains pending counters for retry.
    /// Crash loss is at most one interval with a healthy DB; outages can extend that window.
    pub async fn run(&self, pool: PgPool, cancellation: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let stopping = tokio::select! {
                () = cancellation.cancelled() => true,
                _ = interval.tick() => false,
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(1), self.flush(&pool)).await,
                Ok(Ok(()))
            ) {
                rpc_accounting_failed();
            }
            if stopping {
                return;
            }
        }
    }
}

impl RpcCallRecorder for Recorder {
    fn record(
        &self,
        provider: &str,
        chain: Option<u64>,
        methods: &[&'static str],
    ) -> Result<(), RpcAccountingError> {
        let chain = chain
            .and_then(|c| i64::try_from(c).ok())
            .ok_or(RpcAccountingError)?;
        self.record_on(Utc::now().date_naive(), provider, chain, methods);
        Ok(())
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
