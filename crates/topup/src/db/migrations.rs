//! Recovery for interrupted RPC queue index builds under the existing migration advisory lock.

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::pool::PoolConnection;
use sqlx::{Executor, PgConnection, PgPool, Postgres};

struct QueueIndex {
    version: i64,
    name: &'static str,
    definition: &'static str,
    drop_sql: &'static str,
}

const QUEUE_INDEXES: [QueueIndex; 10] = [
    QueueIndex {
        version: 20261028000000,
        name: "rpc_window_reviews_due_idx",
        definition: "CREATE INDEX rpc_window_reviews_due_idx ON public.rpc_window_reviews USING btree (chain_id, epoch, COALESCE(replayed_at, created_at), from_block, id) WHERE (reviewed_at IS NULL)",
        drop_sql: "DROP INDEX CONCURRENTLY public.rpc_window_reviews_due_idx",
    },
    QueueIndex {
        version: 20261028000002,
        name: "rpc_reorg_ranges_pending_idx",
        definition: "CREATE INDEX rpc_reorg_ranges_pending_idx ON public.rpc_reorg_ranges USING btree (chain_id, epoch, COALESCE((replayed_through + 1), from_block)) WHERE (COALESCE(replayed_through, (from_block - 1)) < to_block)",
        drop_sql: "DROP INDEX CONCURRENTLY public.rpc_reorg_ranges_pending_idx",
    },
    QueueIndex {
        version: 20261029030001,
        name: "addresses_chain_page_idx",
        definition: "CREATE INDEX addresses_chain_page_idx ON public.addresses USING btree (chain_id, id)",
        drop_sql: "DROP INDEX CONCURRENTLY public.addresses_chain_page_idx",
    },
    QueueIndex {
        version: 20261029030002,
        name: "deposits_credit_page_idx",
        definition: "CREATE INDEX deposits_credit_page_idx ON public.deposits USING btree (id) WHERE ((credit_minor IS NOT NULL) AND (price_scaled IS NOT NULL) AND (route IS NOT NULL) AND (route_version IS NOT NULL))",
        drop_sql: "DROP INDEX CONCURRENTLY public.deposits_credit_page_idx",
    },
    QueueIndex {
        version: 20261029030003,
        name: "addresses_chain_created_idx",
        definition: "CREATE INDEX addresses_chain_created_idx ON public.addresses USING btree (chain_id, created_block)",
        drop_sql: "DROP INDEX CONCURRENTLY public.addresses_chain_created_idx",
    },
    QueueIndex {
        version: 20261029030004,
        name: "deposits_custody_page_idx",
        definition: "CREATE INDEX deposits_custody_page_idx ON public.deposits USING btree (address_id, asset_contract, block_number)",
        drop_sql: "DROP INDEX CONCURRENTLY public.deposits_custody_page_idx",
    },
    QueueIndex {
        version: 20261029030005,
        name: "deposits_flush_page_idx",
        definition: "CREATE INDEX deposits_flush_page_idx ON public.deposits USING btree (id) WHERE ((state = 'credited'::text) AND (final_at IS NOT NULL))",
        drop_sql: "DROP INDEX CONCURRENTLY public.deposits_flush_page_idx",
    },
    QueueIndex {
        version: 20261029040000,
        name: "quotes_scope_created_idx",
        definition: "CREATE INDEX quotes_scope_created_idx ON public.quotes USING btree (account_id, livemode, created_at, id)",
        drop_sql: "DROP INDEX CONCURRENTLY public.quotes_scope_created_idx",
    },
    QueueIndex {
        version: 20261029040001,
        name: "refunds_scope_created_idx",
        definition: "CREATE INDEX refunds_scope_created_idx ON public.refunds USING btree (account_id, livemode, created_at, id)",
        drop_sql: "DROP INDEX CONCURRENTLY public.refunds_scope_created_idx",
    },
    QueueIndex {
        version: 20261029040002,
        name: "addresses_scope_page_idx",
        definition: "CREATE INDEX addresses_scope_page_idx ON public.addresses USING btree (account_id, livemode, id)",
        drop_sql: "DROP INDEX CONCURRENTLY public.addresses_scope_page_idx",
    },
];

pub(crate) fn unlocked_migrator() -> Migrator {
    let mut migrator = Migrator::with_migrations(super::MIGRATOR.iter().cloned().collect());
    migrator.set_locking(false);
    // Only used after locked_connection validates the compatibility ledger. SQLx still
    // validates every known checksum and rejects dirty migrations.
    migrator.set_ignore_missing(true);
    migrator
}

pub(crate) async fn locked_connection(
    pool: &PgPool,
) -> Result<PoolConnection<Postgres>, MigrateError> {
    let mut connection = pool.acquire().await?;
    // Retain the repository's migration lock through the cutover and concurrent index phases.
    // Cancellation closes the session rather than returning a migration lock to the pool.
    connection.close_on_drop();
    // A blocking pg_advisory_lock query holds an old snapshot that concurrent expression-index
    // builds wait for, producing a deadlock with another migrator. Each nonblocking try ends its
    // snapshot before sleeping. Bound the wait by the pool's existing acquisition timeout.
    tokio::time::timeout(pool.options().get_acquire_timeout(), async {
        loop {
            let acquired: bool = sqlx::query_scalar(
                "SELECT pg_try_advisory_lock(hashtextextended('payment-settings-migration', 0))",
            )
            .fetch_one(&mut *connection)
            .await?;
            if acquired {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)??;
    validate_compatibility(&mut connection).await?;
    Ok(connection)
}

pub(crate) async fn finish(mut connection: PoolConnection<Postgres>) -> Result<(), MigrateError> {
    let unlock = sqlx::query_scalar::<_, bool>(
        "SELECT pg_advisory_unlock(hashtextextended('payment-settings-migration', 0))",
    )
    .fetch_one(&mut *connection)
    .await;
    let close = connection.close().await;
    let unlocked = unlock?;
    close?;
    if !unlocked {
        return Err(sqlx::Error::Protocol("migration lock was not held".to_owned()).into());
    }
    Ok(())
}

pub(crate) async fn run_in(connection: &mut PgConnection) -> Result<(), MigrateError> {
    recover_queue_indexes(connection).await?;
    // The outer lock covers inspection, cleanup, and the unmodified SQLx migrations.
    unlocked_migrator().run(&mut *connection).await?;
    record_compatibility(connection).await
}

pub(super) async fn run(pool: &PgPool) -> Result<(), MigrateError> {
    let mut connection = locked_connection(pool).await?;
    let result = run_in(&mut connection).await;
    let finish = finish(connection).await;
    result?;
    finish
}

async fn recover_queue_indexes(connection: &mut PgConnection) -> Result<(), MigrateError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    for index in QUEUE_INDEXES {
        let recorded: bool = exists
            && sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM public._sqlx_migrations WHERE version=$1)",
            )
            .bind(index.version)
            .fetch_one(&mut *connection)
            .await?;
        if recorded {
            continue;
        }
        let record: Option<(Option<bool>, Option<String>)> = sqlx::query_as(
            "SELECT i.indisvalid, CASE WHEN c.relkind='i' THEN pg_get_indexdef(c.oid) END FROM pg_class c \
             JOIN pg_namespace n ON n.oid=c.relnamespace \
             LEFT JOIN pg_index i ON i.indexrelid=c.oid \
             WHERE n.nspname='public' AND c.relname=$1",
        )
        .bind(index.name)
        .fetch_optional(&mut *connection)
        .await?;
        let Some((valid, definition)) = record else {
            continue;
        };
        // Only an unapplied queue migration following the RPC epoch schema owns this recovery.
        // Fail closed for a dirty or inconsistent history, before CREATE IF NOT EXISTS can run.
        let eligible: bool = exists && sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM public._sqlx_migrations WHERE version=20261027000000 AND success) \
             AND NOT EXISTS (SELECT 1 FROM public._sqlx_migrations WHERE version > $1 OR NOT success)",
        )
        .bind(index.version)
        .fetch_one(&mut *connection)
        .await?;
        if !eligible {
            return Err(sqlx::Error::Protocol(format!(
                "public.{} has inconsistent migration history; refusing recovery",
                index.name,
            ))
            .into());
        }
        if definition.as_deref() != Some(index.definition) || valid.is_none() {
            return Err(sqlx::Error::Protocol(format!(
                "public.{} does not match pending migration {}; refusing recovery",
                index.name, index.version,
            ))
            .into());
        }
        if valid == Some(true) {
            // CREATE IF NOT EXISTS may resume bookkeeping only after this exact-definition check.
            tracing::info!(
                index = index.name,
                migration = index.version,
                "resuming completed unrecorded queue index migration"
            );
            continue;
        }
        tracing::warn!(
            index = index.name,
            migration = index.version,
            "removing invalid index from interrupted queue migration"
        );
        connection.execute(index.drop_sql).await?;
    }
    Ok(())
}

// Schema capability of the first release implementing this protocol. Future expand-only releases
// keep the immediately previous release's maximum migration here. A breaking release raises it
// to its own maximum and declares "no rollback; restore required" in CHANGELOG.
const COMPATIBILITY_FLOOR: i64 = 20261029030005;

async fn validate_compatibility(connection: &mut PgConnection) -> Result<(), MigrateError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    if !exists {
        return Ok(());
    }
    let known = super::MIGRATOR
        .iter()
        .map(|m| m.version)
        .collect::<Vec<_>>();
    let maximum = known.iter().max().copied().unwrap_or_default();
    let unknown: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT version, checksum, success FROM public._sqlx_migrations WHERE NOT (version = ANY($1))")
        .bind(&known).fetch_all(&mut *connection).await?;
    if unknown.is_empty() {
        return Ok(());
    }
    let ledger: bool = sqlx::query_scalar(
        "SELECT to_regclass('public.topup_migration_compatibility') IS NOT NULL",
    )
    .fetch_one(&mut *connection)
    .await?;
    for (version, checksum, success) in unknown {
        let compatible = version > maximum
            && ledger
            && success
            && sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM public.topup_migration_compatibility \
             WHERE version=$1 AND checksum=$2 AND compatibility_floor <= $3)",
            )
            .bind(version)
            .bind(checksum)
            .bind(maximum)
            .fetch_one(&mut *connection)
            .await?;
        if !compatible {
            return Err(MigrateError::VersionMissing(version));
        }
    }
    Ok(())
}

async fn record_compatibility(connection: &mut PgConnection) -> Result<(), MigrateError> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS public.topup_migration_compatibility \
         (version bigint PRIMARY KEY, checksum bytea NOT NULL, compatibility_floor bigint NOT NULL)")
        .await?;
    // Default privileges may grant the app writes on owner-created tables: revoke explicitly.
    connection
        .execute("REVOKE ALL ON public.topup_migration_compatibility FROM PUBLIC, topup_app")
        .await?;
    for migration in super::MIGRATOR.iter() {
        sqlx::query(
            "INSERT INTO public.topup_migration_compatibility (version, checksum, compatibility_floor) \
             SELECT version, checksum, $2 FROM public._sqlx_migrations WHERE version=$1 AND success \
             ON CONFLICT (version) DO NOTHING")
            .bind(migration.version).bind(COMPATIBILITY_FLOOR).execute(&mut *connection).await?;
    }
    Ok(())
}
