//! Durable chain-read observations shared by the metrics exporter and internal pilot alerts.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

pub(super) const ADDRESS_COUNTS_SQL: &str = "SELECT chain_id,count(*) FROM addresses GROUP BY chain_id UNION ALL SELECT chain_id,0 FROM chain_coverage c WHERE NOT EXISTS(SELECT 1 FROM addresses a WHERE a.chain_id=c.chain_id)";

pub(super) struct Snapshot {
    pub coverage: Vec<(i64, i64, i64)>,
    pub budgets: Vec<(String, i32)>,
    pub issued: Vec<(i64, i64)>,
    pub unresolved: Vec<(i64, i64, i64)>,
    pub slow: Vec<(i64, i64, i64)>,
}

pub(super) async fn snapshot(pool: &PgPool, now: DateTime<Utc>) -> Result<Snapshot, sqlx::Error> {
    let (coverage, budgets, issued, unresolved, slow) = tokio::try_join!(
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT c.chain_id,GREATEST(0,extract(epoch FROM $1::timestamptz-c.through_time)::bigint),(SELECT count(*) FROM addresses a WHERE a.chain_id=c.chain_id AND (a.dual_covered_through IS NULL OR a.dual_covered_through < c.through_block)) FROM chain_coverage c").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (String, i32)>("SELECT name,used FROM daily_budgets WHERE day=($1::timestamptz AT TIME ZONE 'UTC')::date").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64)>(ADDRESS_COUNTS_SQL).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT chain_id,count(*) FILTER (WHERE first_unresolved_at IS NOT NULL AND deposit_finality_pending(deposits)),count(*) FILTER (WHERE first_unresolved_at > $1::timestamptz - interval '24 hours') FROM deposits GROUP BY chain_id UNION ALL SELECT chain_id,0,0 FROM chain_checkpoints c WHERE NOT EXISTS(SELECT 1 FROM deposits d WHERE d.chain_id=c.chain_id)").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT chain_id,count(*) FILTER (WHERE first_slow_at IS NOT NULL AND first_unresolved_at IS NULL AND state='detected' AND final_at IS NULL AND confirm_receipt_checks=0),count(*) FILTER (WHERE first_slow_at > $1::timestamptz - interval '24 hours') FROM deposits GROUP BY chain_id UNION ALL SELECT chain_id,0,0 FROM chain_checkpoints c WHERE NOT EXISTS(SELECT 1 FROM deposits d WHERE d.chain_id=c.chain_id)").bind(now).fetch_all(pool)
    )?;
    Ok(Snapshot {
        coverage,
        budgets,
        issued,
        unresolved,
        slow,
    })
}
