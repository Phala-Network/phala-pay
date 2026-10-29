//! Durable alert instances shared by all existing tracing alert producers.
//!
//! A bounded queue keeps database I/O off the logging/Sentry path. Failed persistence is
//! observable and never prevents the existing Sentry sink from receiving an event.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use prometheus::{IntCounterVec, Opts, core::Collector, proto::MetricFamily};
use sentry::protocol::Event;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Subscriber;
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};
use uuid::Uuid;

type Group = (String, String);
type Silences = BTreeMap<Group, (Uuid, DateTime<Utc>)>;
static SILENCES: OnceLock<RwLock<Silences>> = OnceLock::new();
static RECORDER: OnceLock<mpsc::Sender<Observation>> = OnceLock::new();
static FAILURES: OnceLock<Result<IntCounterVec, prometheus::Error>> = OnceLock::new();
static SILENCE_CHANGES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// Serialize commits and cache publication in the sole lease-owning service process.
pub(crate) async fn silence_change_lock() -> tokio::sync::MutexGuard<'static, ()> {
    SILENCE_CHANGES.lock().await
}

fn failures() -> Result<&'static IntCounterVec, prometheus::Error> {
    FAILURES
        .get_or_init(|| {
            let counter = IntCounterVec::new(
                Opts::new(
                    "topup_alert_persistence_failures_total",
                    "Alert observations not persisted.",
                ),
                &["reason"],
            )?;
            for reason in ["queue_full", "database", "shutdown", "cardinality"] {
                counter.with_label_values(&[reason]);
            }
            Ok(counter)
        })
        .as_ref()
        .map_err(|error| prometheus::Error::Msg(error.to_string()))
}

fn failed(reason: &'static str) {
    if let Ok(counter) = failures() {
        counter.with_label_values(&[reason]).inc();
    }
}

pub(crate) fn collect_metrics() -> Result<Vec<MetricFamily>, prometheus::Error> {
    Ok(failures()?.collect())
}

fn grouping_key(tags: &BTreeMap<String, String>) -> String {
    // Severity changes update an instance rather than creating another group. The Sentry
    // fingerprint retains all of its existing dimensions, including severity.
    let dimensions: BTreeMap<_, _> = tags
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "alert" | "severity" | "runbook"))
        .collect();
    serde_json::json!(dimensions).to_string()
}

fn silences() -> &'static RwLock<Silences> {
    SILENCES.get_or_init(RwLock::default)
}

pub(crate) fn update_silence(name: &str, key: &str, id: Uuid, until: Option<DateTime<Utc>>) {
    if let Ok(mut entries) = silences().write() {
        let group = (name.to_owned(), key.to_owned());
        if let Some(until) = until.filter(|until| *until > Utc::now()) {
            entries.insert(group, (id, until));
        } else if entries
            .get(&group)
            .is_some_and(|(current, _)| *current == id)
        {
            entries.remove(&group);
        }
    }
}

pub(crate) fn is_silenced(event: &Event<'_>) -> bool {
    let Some(name) = event.tags.get("alert") else {
        return false;
    };
    silences().read().ok().is_some_and(|entries| {
        entries
            .get(&(name.clone(), grouping_key(&event.tags)))
            .is_some_and(|(_, until)| *until > Utc::now())
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub(crate) name: String,
    pub(crate) tags: BTreeMap<String, String>,
    pub(crate) severity: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) resolved: bool,
}

impl Observation {
    fn from_event(event: &Event<'_>) -> Option<Self> {
        Some(Self {
            name: event.tags.get("alert")?.clone(),
            tags: event.tags.clone(),
            severity: event
                .tags
                .get("severity")
                .cloned()
                .unwrap_or_else(|| event.level.to_string()),
            at: DateTime::<Utc>::from(event.timestamp),
            resolved: false,
        })
    }
}

fn enqueue(observation: Observation) {
    if let Some(sender) = RECORDER.get() {
        try_enqueue(sender, observation);
    }
}

fn try_enqueue(sender: &mpsc::Sender<Observation>, observation: Observation) {
    if let Err(error) = sender.try_send(observation) {
        failed(if matches!(error, mpsc::error::TrySendError::Full(_)) {
            "queue_full"
        } else {
            "shutdown"
        });
    }
}

/// Observation of an existing detector's decision, including healthy observations after restart.
pub(crate) fn condition(name: &str, component: &str, severity: Option<&str>, now: i64) {
    let Some(at) = DateTime::from_timestamp(now, 0) else {
        return;
    };
    let tags = BTreeMap::from([
        ("alert".to_owned(), name.to_owned()),
        ("component".to_owned(), component.to_owned()),
        (
            "severity".to_owned(),
            severity.unwrap_or("warning").to_owned(),
        ),
    ]);
    enqueue(Observation {
        name: name.to_owned(),
        tags,
        severity: severity.unwrap_or("warning").to_owned(),
        at,
        resolved: severity.is_none(),
    });
}

/// Captures existing tagged WARN/ERROR events even when Sentry is disabled.
pub(crate) struct AlertLayer;

impl<S> Layer<S> for AlertLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        if RECORDER.get().is_none() || event.metadata().fields().field("tags.alert").is_none() {
            return;
        }
        let event = sentry::integrations::tracing::event_from_event(event, &context);
        if let Some(observation) = Observation::from_event(&event) {
            enqueue(observation);
        }
    }
}

/// The service's single bounded persistence worker. Read-only restore instances never start it.
pub struct AlertRecorder {
    pool: PgPool,
    receiver: mpsc::Receiver<Observation>,
}

impl AlertRecorder {
    /// Load durable silences before any alert-producing workers or admin handlers start.
    pub async fn initialize(pool: PgPool) -> Result<Self, anyhow::Error> {
        let entries: Vec<(String, String, Uuid, DateTime<Utc>)> = sqlx::query_as(
            "SELECT name, grouping_key, id, silenced_until FROM alerts \
             WHERE resolved_at IS NULL AND silenced_until > now()",
        )
        .fetch_all(&pool)
        .await?;
        {
            let mut cache = silences()
                .write()
                .map_err(|_| anyhow::anyhow!("alert silence cache unavailable"))?;
            *cache = entries
                .into_iter()
                .map(|(name, key, id, until)| ((name, key), (id, until)))
                .collect();
        }
        let (sender, receiver) = mpsc::channel(1024);
        RECORDER
            .set(sender)
            .map_err(|_| anyhow::anyhow!("alert recorder already initialized"))?;
        Ok(Self { pool, receiver })
    }

    /// Persist observations and prune at most 1,000 resolved instances per minute.
    pub async fn run(mut self, cancellation: CancellationToken) {
        let mut pruning = tokio::time::interval(Duration::from_secs(60));
        pruning.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => {
                    self.receiver.close();
                    // TaskGroup bounds shutdown; drain accepted observations while time remains.
                    while let Some(observation) = self.receiver.recv().await {
                        self.persist(&observation).await;
                    }
                    return;
                }
                observation = self.receiver.recv() => match observation {
                    Some(observation) => self.persist(&observation).await,
                    None => return,
                },
                _ = pruning.tick() => {
                    if let Err(error) = prune(&self.pool).await {
                        tracing::error!(%error, "resolved alert retention failed");
                    }
                    if let Ok(mut entries) = silences().write() {
                        entries.retain(|_, (_, until)| *until > Utc::now());
                    }
                }
            }
        }
    }

    async fn persist(&self, observation: &Observation) {
        if let Err(error) = record(&self.pool, observation).await {
            failed("database");
            tracing::error!(%error, "alert persistence failed; Sentry emission continues");
        }
    }
}

pub(crate) async fn record(pool: &PgPool, observation: &Observation) -> Result<(), sqlx::Error> {
    let _guard = silence_change_lock().await;
    let key = grouping_key(&observation.tags);
    if observation.resolved {
        let id: Option<Uuid> = sqlx::query_scalar(
            "UPDATE alerts SET resolved_at=$3 WHERE name=$1 AND grouping_key=$2 \
             AND resolved_at IS NULL AND last_seen_at <= $3 RETURNING id",
        )
        .bind(&observation.name)
        .bind(&key)
        .bind(observation.at)
        .fetch_optional(pool)
        .await?;
        if let Some(id) = id {
            update_silence(&observation.name, &key, id, None);
        }
        return Ok(());
    }
    let tags = serde_json::json!(observation.tags);
    // The lease owner and silence_change_lock serialize admission with every record/update.
    // Existing instances keep receiving updates even after a name reaches its group budget.
    let (active_count, existing): (i64, bool) = sqlx::query_as(
        "SELECT count(*),COALESCE(bool_or(grouping_key=$2),false) FROM alerts \
         WHERE name=$1 AND resolved_at IS NULL",
    )
    .bind(&observation.name)
    .bind(&key)
    .fetch_one(pool)
    .await?;
    if !existing && (active_count >= 100 || key.len() > 512 || tags.to_string().len() > 2048) {
        failed("cardinality");
        return Ok(());
    }
    let runbook = crate::observability::alert_runbook(&observation.name, &observation.tags);
    sqlx::query(
        "INSERT INTO alerts (id,name,grouping_key,severity,tags,runbook,first_seen_at,last_seen_at) \
         SELECT $1,$2,$3,$4,$5,$6,$7,$7 WHERE NOT EXISTS \
           (SELECT 1 FROM alerts WHERE name=$2 AND grouping_key=$3 AND resolved_at >= $7) \
         ON CONFLICT (name,grouping_key) WHERE resolved_at IS NULL DO UPDATE SET \
           severity=EXCLUDED.severity,tags=EXCLUDED.tags,runbook=EXCLUDED.runbook, \
           last_seen_at=EXCLUDED.last_seen_at WHERE alerts.last_seen_at <= EXCLUDED.last_seen_at",
    ).bind(Uuid::new_v4()).bind(&observation.name).bind(key).bind(&observation.severity)
        .bind(tags).bind(runbook).bind(observation.at)
        .execute(pool).await?;
    Ok(())
}

pub(crate) async fn prune(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM alerts WHERE id IN (SELECT id FROM alerts \
        WHERE resolved_at < now() - interval '30 days' ORDER BY resolved_at \
        LIMIT 1000 FOR UPDATE SKIP LOCKED)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context as _, Result, ensure};

    fn observation(name: &str, at: DateTime<Utc>, severity: &str) -> Observation {
        Observation {
            name: name.to_owned(),
            tags: BTreeMap::from([
                ("component".to_owned(), "database".to_owned()),
                ("severity".to_owned(), severity.to_owned()),
            ]),
            severity: severity.to_owned(),
            at,
            resolved: false,
        }
    }

    #[tokio::test]
    async fn firing_updates_one_instance_recovery_and_recurrence_reset_operator_state() -> Result<()>
    {
        crate::test_support::with_database(|database| Box::pin(async move {
            let pool = &database.app_pool;
            let now = Utc::now();
            let mut fired = observation("TopupHeartbeatStale", now, "warning");
            record(pool, &fired).await?;
            let (id, first): (Uuid, DateTime<Utc>) = sqlx::query_as("SELECT id,first_seen_at FROM alerts").fetch_one(pool).await?;
            sqlx::query("UPDATE alerts SET acknowledged_at=now(),acknowledged_by='admin:test',silenced_until=now()+interval '1 day' WHERE id=$1")
                .bind(id).execute(pool).await?;
            fired.at = now + chrono::TimeDelta::minutes(1);
            fired.severity = "critical".to_owned();
            fired.tags.insert("severity".to_owned(), "critical".to_owned());
            record(pool, &fired).await?;
            record(pool, &fired).await?;
            let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts").fetch_one(pool).await?;
            ensure!(count == 1);
            let (last, severity, acknowledged): (DateTime<Utc>, String, Option<String>) =
                sqlx::query_as("SELECT last_seen_at,severity,acknowledged_by FROM alerts WHERE id=$1")
                .bind(id).fetch_one(pool).await?;
            ensure!(last.timestamp() == fired.at.timestamp() && first.timestamp() == now.timestamp());
            ensure!(severity == "critical" && acknowledged.as_deref() == Some("admin:test"));
            // A fresh detector (no in-memory active entries) still resolves durable state.
            fired.resolved = true;
            fired.at = now + chrono::TimeDelta::minutes(2);
            record(pool, &fired).await?;
            let resolved: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT resolved_at FROM alerts WHERE id=$1")
                .bind(id).fetch_one(pool).await?;
            ensure!(resolved.is_some());
            // A delayed queued observation cannot reopen a manually/automatically resolved incident.
            let mut stale = fired.clone(); stale.resolved = false; stale.at = now;
            record(pool, &stale).await?;
            fired.resolved = false;
            fired.at = now + chrono::TimeDelta::minutes(3);
            record(pool, &fired).await?;
            let (new_id, ack, silence): (Uuid, Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
                sqlx::query_as("SELECT id,acknowledged_at,silenced_until FROM alerts WHERE resolved_at IS NULL")
                .fetch_one(pool).await?;
            ensure!(new_id != id && ack.is_none() && silence.is_none());
            Ok(())
        })).await
    }

    #[tokio::test]
    async fn retention_prunes_only_old_resolved_instances_and_migration_reverses() -> Result<()> {
        crate::test_support::with_database(|database| {
            Box::pin(async move {
                let pool = &database.app_pool;
                let now = Utc::now();
                for (name, days, resolved) in [
                    ("old", 31, true),
                    ("recent", 29, true),
                    ("active", 40, false),
                ] {
                    let mut sample =
                        observation(name, now - chrono::TimeDelta::days(days), "warning");
                    record(pool, &sample).await?;
                    if resolved {
                        sample.resolved = true;
                        record(pool, &sample).await?;
                    }
                }
                prune(pool).await?;
                let names: Vec<String> =
                    sqlx::query_scalar("SELECT name FROM alerts ORDER BY name")
                        .fetch_all(pool)
                        .await?;
                ensure!(names == ["active", "recent"]);
                sqlx::raw_sql(include_str!("../migrations/20261105000000_alerts.down.sql"))
                    .execute(&database.owner_pool)
                    .await?;
                let absent: bool = sqlx::query_scalar("SELECT to_regclass('alerts') IS NULL")
                    .fetch_one(&database.owner_pool)
                    .await?;
                ensure!(absent);
                sqlx::raw_sql(include_str!("../migrations/20261105000000_alerts.up.sql"))
                    .execute(&database.owner_pool)
                    .await?;
                record(pool, &observation("restored", now, "warning"))
                    .await
                    .context("application grant after up migration")?;
                Ok(())
            })
        })
        .await
    }

    #[test]
    fn silence_uses_grouping_not_severity_expires_and_is_instance_scoped() {
        let name = "TopupSilenceUnitTest";
        let tags = BTreeMap::from([
            ("alert".to_owned(), name.to_owned()),
            ("component".to_owned(), "database".to_owned()),
            ("severity".to_owned(), "critical".to_owned()),
        ]);
        let key = grouping_key(&tags);
        let id = Uuid::new_v4();
        let event = Event {
            tags,
            ..Event::default()
        };
        update_silence(
            name,
            &key,
            id,
            Some(Utc::now() + chrono::TimeDelta::hours(1)),
        );
        assert!(is_silenced(&event));
        update_silence(name, &key, Uuid::new_v4(), None);
        assert!(
            is_silenced(&event),
            "an older instance cannot clear a new instance's silence"
        );
        update_silence(
            name,
            &key,
            id,
            Some(Utc::now() - chrono::TimeDelta::seconds(1)),
        );
        assert!(!is_silenced(&event));
        update_silence(
            name,
            &key,
            id,
            Some(Utc::now() + chrono::TimeDelta::hours(1)),
        );
        update_silence(name, &key, id, None);
        assert!(!is_silenced(&event));
    }

    #[tokio::test]
    async fn queue_and_database_failures_are_counted_without_panicking() -> Result<()> {
        let sample = observation("TopupRecorderFailureTest", Utc::now(), "warning");
        let (sender, receiver) = mpsc::channel(1);
        let counter = failures()?;
        let full_before = counter.with_label_values(&["queue_full"]).get();
        try_enqueue(&sender, sample.clone());
        try_enqueue(&sender, sample.clone());
        ensure!(counter.with_label_values(&["queue_full"]).get() == full_before + 1);
        drop(receiver);
        let closed_before = counter.with_label_values(&["shutdown"]).get();
        try_enqueue(&sender, sample.clone());
        ensure!(counter.with_label_values(&["shutdown"]).get() == closed_before + 1);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")?;
        pool.close().await;
        let (_, receiver) = mpsc::channel(1);
        let recorder = AlertRecorder { pool, receiver };
        let failed_before = counter.with_label_values(&["database"]).get();
        recorder.persist(&sample).await;
        ensure!(counter.with_label_values(&["database"]).get() == failed_before + 1);
        Ok(())
    }

    #[tokio::test]
    async fn new_groups_have_cardinality_and_byte_limits_existing_instances_still_update()
    -> Result<()> {
        crate::test_support::with_database(|database| {
            Box::pin(async move {
                let pool = &database.app_pool;
                let now = Utc::now();
                let counter = failures()?.with_label_values(&["cardinality"]);
                let before = counter.get();
                for index in 0..101 {
                    let mut sample = observation("TopupCardinalityTest", now, "warning");
                    sample
                        .tags
                        .insert("component".to_owned(), format!("group-{index}"));
                    record(pool, &sample).await?;
                }
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM alerts WHERE name='TopupCardinalityTest'",
                )
                .fetch_one(pool)
                .await?;
                ensure!(count == 100 && counter.get() == before + 1);
                let mut updated = observation(
                    "TopupCardinalityTest",
                    now + chrono::TimeDelta::seconds(1),
                    "critical",
                );
                updated
                    .tags
                    .insert("component".to_owned(), "group-0".to_owned());
                record(pool, &updated).await?;
                let severity: String = sqlx::query_scalar(
                    "SELECT severity FROM alerts WHERE name=$1 AND grouping_key=$2",
                )
                .bind(&updated.name)
                .bind(grouping_key(&updated.tags))
                .fetch_one(pool)
                .await?;
                ensure!(severity == "critical" && counter.get() == before + 1);
                let mut oversized = observation("TopupByteLimitTest", now, "warning");
                oversized
                    .tags
                    .insert("component".to_owned(), "g".repeat(513));
                record(pool, &oversized).await?;
                oversized
                    .tags
                    .insert("component".to_owned(), "short".to_owned());
                oversized
                    .tags
                    .insert("severity".to_owned(), "w".repeat(2048));
                record(pool, &oversized).await?;
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM alerts WHERE name='TopupByteLimitTest'",
                )
                .fetch_one(pool)
                .await?;
                ensure!(count == 0 && counter.get() == before + 3);
                Ok(())
            })
        })
        .await
    }
}
