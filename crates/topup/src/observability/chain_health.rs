//! Durable chain-read observations shared by the metrics exporter and internal pilot alerts.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

pub(super) const ADDRESS_COUNTS_SQL: &str = "SELECT chain_id,count(*) FROM addresses GROUP BY chain_id UNION ALL SELECT chain_id,0 FROM chain_coverage c WHERE NOT EXISTS(SELECT 1 FROM addresses a WHERE a.chain_id=c.chain_id)";
// Match scanner::hot_address_page, including all quote/deposit states. Owner timestamps count
// issuance once; reopening quotes and retiring addresses do not refund the rolling allowance.
const ACTIVITY_SQL: &str = "WITH hot AS (SELECT address_id FROM quotes WHERE created_at >= $2 \
    UNION SELECT address_id FROM deposits WHERE block_time >= $3) \
    SELECT a.chain_id,count(*) FILTER (WHERE h.address_id IS NOT NULL), \
    count(*) FILTER (WHERE COALESCE(q.created_at,da.created_at) > $1::timestamptz - interval '24 hours') \
    FROM addresses a LEFT JOIN hot h ON h.address_id=a.id \
    LEFT JOIN quotes q ON q.id=a.quote_id \
    LEFT JOIN deposit_addresses da ON da.id=a.deposit_address_id GROUP BY a.chain_id";

pub(super) const HOT_ADDRESS_BUDGET: i64 = 2_000;

pub(super) fn issuance_budget(routes: &crate::routes::RouteSet) -> i64 {
    if routes.staging() { 10 } else { 50 }
}

pub(super) struct Snapshot {
    pub coverage: Vec<(i64, i64, i64)>,
    pub budgets: Vec<(String, i32)>,
    pub issued: Vec<(i64, i64)>,
    pub unresolved: Vec<(i64, i64, i64)>,
    pub slow: Vec<(i64, i64, i64)>,
    pub activity: Vec<(i64, i64, i64)>,
    pub ignored: Vec<(i64, i64)>,
}

pub(super) async fn snapshot(
    pool: &PgPool,
    routes: &crate::routes::RouteSet,
    now: DateTime<Utc>,
) -> Result<Snapshot, sqlx::Error> {
    let discovery = routes.discovery();
    let (coverage, budgets, issued, unresolved, slow, activity, ignored) = tokio::try_join!(
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT c.chain_id,GREATEST(0,extract(epoch FROM $1::timestamptz-c.through_time)::bigint),(SELECT count(*) FROM addresses a WHERE a.chain_id=c.chain_id AND (a.dual_covered_through IS NULL OR a.dual_covered_through < c.through_block)) FROM chain_coverage c").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (String, i32)>("SELECT name,used FROM daily_budgets WHERE day=($1::timestamptz AT TIME ZONE 'UTC')::date").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64)>(ADDRESS_COUNTS_SQL).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT chain_id,count(*) FILTER (WHERE first_unresolved_at IS NOT NULL AND deposit_finality_pending(deposits)),count(*) FILTER (WHERE first_unresolved_at > $1::timestamptz - interval '24 hours') FROM deposits GROUP BY chain_id UNION ALL SELECT chain_id,0,0 FROM chain_checkpoints c WHERE NOT EXISTS(SELECT 1 FROM deposits d WHERE d.chain_id=c.chain_id)").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64, i64)>("SELECT chain_id,count(*) FILTER (WHERE first_slow_at IS NOT NULL AND first_unresolved_at IS NULL AND state='detected' AND final_at IS NULL AND confirm_receipt_checks=0),count(*) FILTER (WHERE first_slow_at > $1::timestamptz - interval '24 hours') FROM deposits GROUP BY chain_id UNION ALL SELECT chain_id,0,0 FROM chain_checkpoints c WHERE NOT EXISTS(SELECT 1 FROM deposits d WHERE d.chain_id=c.chain_id)").bind(now).fetch_all(pool),
        sqlx::query_as::<_, (i64, i64, i64)>(ACTIVITY_SQL).bind(now)
            .bind(now - chrono::Duration::hours(i64::from(discovery.quote_window_hours)))
            .bind(now - chrono::Duration::days(i64::from(discovery.deposit_window_days)))
            .fetch_all(pool),
        sqlx::query_as::<_, (i64,i64)>("SELECT c.chain_id,COALESCE(max(d.event_count),0) FROM chain_coverage c \
            LEFT JOIN ignored_transfer_daily d ON d.chain_id=c.chain_id AND d.day=($1::timestamptz AT TIME ZONE 'UTC')::date \
            GROUP BY c.chain_id").bind(now).fetch_all(pool)
    )?;
    Ok(Snapshot {
        coverage,
        budgets,
        issued,
        unresolved,
        slow,
        activity,
        ignored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn activity_counts_distinct_hot_recipients_and_both_issuance_owners()
    -> Result<(), sqlx::Error> {
        use sqlx::Connection as _;
        let Ok(url) = std::env::var("DATABASE_URL") else {
            assert_ne!(
                std::env::var("CI").as_deref(),
                Ok("true"),
                "CI requires DATABASE_URL"
            );
            return Ok(());
        };
        let mut conn = sqlx::PgConnection::connect(&url).await?;
        sqlx::raw_sql("CREATE TEMP TABLE addresses (id bigint,chain_id bigint,quote_id bigint,deposit_address_id bigint); \
            CREATE TEMP TABLE quotes (id bigint,address_id bigint,created_at timestamptz); \
            CREATE TEMP TABLE deposit_addresses (id bigint,created_at timestamptz); \
            CREATE TEMP TABLE deposits (address_id bigint,block_time timestamptz); \
            INSERT INTO addresses VALUES (1,1,1,NULL),(2,1,NULL,2),(3,1,NULL,3),(4,2,4,NULL); \
            INSERT INTO quotes VALUES (1,1,'2026-10-30 12:00Z'),(5,1,'2026-10-30 12:01Z'),(4,4,'2026-10-01 00:00Z'); \
            INSERT INTO deposit_addresses VALUES (2,'2026-10-30 11:00Z'),(3,'2026-10-01 00:00Z'); \
            INSERT INTO deposits VALUES (1,'2026-10-30 12:02Z'),(1,'2026-10-30 12:03Z'),(3,'2026-10-24 13:00Z'),(4,'2026-10-01 00:00Z')")
            .execute(&mut conn).await?;
        let now: DateTime<Utc> = "2026-10-30T13:00:00Z".parse().unwrap();
        let counts: Vec<(i64, i64, i64)> = sqlx::query_as(ACTIVITY_SQL)
            .bind(now)
            .bind(now - chrono::Duration::hours(24))
            .bind(now - chrono::Duration::days(7))
            .fetch_all(&mut conn)
            .await?;
        let counts: std::collections::BTreeMap<_, _> = counts
            .into_iter()
            .map(|(chain, hot, issued)| (chain, (hot, issued)))
            .collect();
        assert_eq!(
            counts[&1],
            (2, 2),
            "duplicate activity is one hot address; idle new address still counts issuance"
        );
        assert_eq!(
            counts[&2],
            (0, 0),
            "cold chain emits zero to resolve alerts"
        );
        let later: Vec<(i64, i64, i64)> = sqlx::query_as(ACTIVITY_SQL)
            .bind(now + chrono::Duration::days(1))
            .bind(now)
            .bind(now)
            .fetch_all(&mut conn)
            .await?;
        assert!(
            later.contains(&(1, 0, 0)),
            "configured shorter windows and rolling expiry"
        );
        Ok(())
    }
}
