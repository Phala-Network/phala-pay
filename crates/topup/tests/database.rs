//! PostgreSQL integration tests for the C1 database boundary.

mod support;

use std::collections::BTreeMap;
use std::env;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgPool, Row};
use topup::db::{
    self, ApplyTransitionResult, EventObject, NewDeposit, OutboxEvent, TransitionUpdate,
};
use topup::reconciler::{CheckName, Reconciler, ReconciliationChain, ReconciliationError};
use topup::{heartbeat, restore};
use topup_adapters::chain::evm::TransferLog;
use topup_core::deposit::{DepositState, StepOutcome, WaitReason, next};
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use uuid::Uuid;

use support::seed::{self, NewAccount, NewAddress, NewCustomer};
use support::with_database;

fn migrate(url: &str) -> Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_topup"))
        .arg("migrate")
        .env("DATABASE_URL", url)
        .output()
        .context("run topup migrate")
}

#[tokio::test]
async fn migrations_apply_from_scratch_and_are_idempotent() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            assert_session_budgets(&context.owner_pool, ("5min", "30s", "5min")).await?;
            assert_session_budgets(&context.app_pool, ("30s", "5s", "1min")).await?;
            db::migrate(&context.owner_pool).await?;
            let output = migrate(&context.owner_url)?;
            ensure!(
                output.status.success(),
                "topup migrate failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            // The application login is refused before it touches the schema.
            let output = migrate(&context.app_url)?;
            let text = String::from_utf8_lossy(&output.stdout);
            ensure!(
                !output.status.success() && text.contains("migrate requires the database owner"),
                "migrate must refuse the application login: {text}"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn migrate_refuses_a_pre_cutover_database_without_changing_it() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.owner_pool;
            let issued = seed_account(pool, 90).await?;
            // The historical down migrations reproduce the schema an older image left behind,
            // including its issued address and quote, without changing migration history.
            db::MIGRATOR.undo(pool, 20261023000000).await?;
            let before: Value = sqlx::query_scalar(
                "SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM _sqlx_migrations m",
            )
            .fetch_one(pool)
            .await?;
            let address_before: Value =
                sqlx::query_scalar("SELECT to_jsonb(a) FROM addresses a WHERE id = $1")
                    .bind(issued.address_id)
                    .fetch_one(pool)
                    .await?;
            let output = migrate(&context.owner_url)?;
            ensure!(!output.status.success());
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            ensure!(
                text.contains(
                    "payment settings cutover is incomplete; upgrade through 0.9.x first"
                ),
                "{text}"
            );
            let after: Value = sqlx::query_scalar(
                "SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM _sqlx_migrations m",
            )
            .fetch_one(pool)
            .await?;
            ensure!(after == before, "migrate changed the migration records");
            let address_after: Value =
                sqlx::query_scalar("SELECT to_jsonb(a) FROM addresses a WHERE id = $1")
                    .bind(issued.address_id)
                    .fetch_one(pool)
                    .await?;
            ensure!(
                address_after == address_before,
                "migrate changed the issued address"
            );
            let has_cutover: bool = sqlx::query_scalar(
                "SELECT to_regclass('public.payment_settings_cutover') IS NOT NULL",
            )
            .fetch_one(pool)
            .await?;
            ensure!(!has_cutover, "migrate applied the cutover migration");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn migrate_refuses_an_unfinished_cutover_before_applying_later_migrations() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.owner_pool;
            db::MIGRATOR.undo(pool, 20261024000000).await?;
            sqlx::query("UPDATE payment_settings_cutover SET recording_resumed_at = NULL")
                .execute(pool)
                .await?;
            let before: Value = sqlx::query_scalar(
                "SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM _sqlx_migrations m",
            )
            .fetch_one(pool)
            .await?;
            let cutover_before: Value =
                sqlx::query_scalar("SELECT to_jsonb(c) FROM payment_settings_cutover c")
                    .fetch_one(pool)
                    .await?;
            let output = migrate(&context.owner_url)?;
            ensure!(!output.status.success());
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            ensure!(
                text.contains(
                    "payment settings cutover is incomplete; upgrade through 0.9.x first"
                ),
                "{text}"
            );
            let after: Value = sqlx::query_scalar(
                "SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM _sqlx_migrations m",
            )
            .fetch_one(pool)
            .await?;
            ensure!(after == before, "migrate applied later migrations");
            let cutover_after: Value =
                sqlx::query_scalar("SELECT to_jsonb(c) FROM payment_settings_cutover c")
                    .fetch_one(pool)
                    .await?;
            ensure!(
                cutover_after == cutover_before,
                "migrate changed the unfinished cutover"
            );
            Ok(())
        })
    })
    .await
}

/// The schema starts from one squashed migration that builds every table on an empty database:
/// staging is reset rather than migrated (design §14, §16 PR 11). Later migrations are additive.
#[tokio::test]
async fn the_migrations_build_the_schema_on_an_empty_database() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let first = db::MIGRATOR
                .iter()
                .find(|migration| migration.migration_type.is_up_migration())
                .map(|migration| migration.version);
            ensure!(
                first == Some(20_261_004_000_000),
                "expected the squashed migration first, found {first:?}"
            );
            // `with_database` migrated a database created empty; every documented table exists
            // and nothing else does.
            let tables: Vec<String> = sqlx::query_scalar(
                "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY 1",
            )
            .fetch_all(&context.owner_pool)
            .await?;
            let mut documented: Vec<&str> =
                DOCUMENTED_GRANTS.iter().map(|(table, _)| *table).collect();
            documented.sort_unstable();
            ensure!(tables == documented, "tables={tables:?}");
            // The migration refuses a database that already holds the pre-tenancy schema: it
            // runs only on an empty one.
            let rerun = sqlx::raw_sql(include_str!(
                "../migrations/20261004000000_multi_tenant.up.sql"
            ))
            .execute(&context.owner_pool)
            .await
            .err();
            assert_sqlstate(rerun, "42723")?;
            Ok(())
        })
    })
    .await
}

/// Composite keys tie every tenant row to its parent's account and mode, so no write can join
/// rows of two accounts or two modes (design D13).
#[tokio::test]
async fn tenant_rows_cannot_join_another_accounts_or_modes_rows() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.app_pool;
            let seed = seed_account(pool, 90).await?;
            let other = seed_account_without_address(pool, 91).await?;
            // A quote of the seeded customer that has no address yet.
            let quote = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO quotes (id, account_id, livemode, customer_id, route, \
                 amount_atomic, price_scaled, credit_minor, expires_at, route_version, \
                 settings_revision_id, terms) \
                 SELECT $1, $2, true, $3, 'r', 1, 1, 1, now(), 1, current_revision_id, $4 \
                 FROM payment_settings_state WHERE account_id = $2 AND livemode",
            )
            .bind(quote)
            .bind(seed.account_id)
            .bind(seed.customer_id)
            .bind(sqlx::types::Json(seed::fixture_terms()))
            .execute(pool)
            .await?;

            // An address for another account's quote, or for the quote in the other mode.
            for (account, livemode) in [(other.account_id, true), (seed.account_id, false)] {
                assert_sqlstate(
                    sqlx::query(
                        "INSERT INTO addresses (id, account_id, livemode, chain_id, quote_id, \
                         salt, treasury, address) VALUES ($1, $2, $3, 1, $4, $5, $6, $7)",
                    )
                    .bind(Uuid::new_v4())
                    .bind(account)
                    .bind(livemode)
                    .bind(quote)
                    .bind(format!("{:#x}", b256(92)))
                    .bind(format!("{:#x}", evm_address(92)))
                    .bind(format!("{:#x}", evm_address(93)))
                    .execute(pool)
                    .await
                    .err(),
                    "23503",
                )?;
            }
            // A quote for another account's customer.
            assert_sqlstate(
                sqlx::query(
                    "INSERT INTO quotes (id, account_id, livemode, customer_id, route, \
                     amount_atomic, price_scaled, credit_minor, expires_at, route_version, \
                     settings_revision_id, terms) \
                     SELECT $1, $2, true, $3, 'r', 1, 1, 1, now(), 1, current_revision_id, $4 \
                     FROM payment_settings_state WHERE account_id = $2 AND livemode",
                )
                .bind(Uuid::new_v4())
                .bind(other.account_id)
                .bind(seed.customer_id)
                .bind(sqlx::types::Json(seed::fixture_terms()))
                .execute(pool)
                .await
                .err(),
                "23503",
            )?;
            // A deposit is recorded under its address's account, mode, and customer, whatever
            // the caller holds.
            let deposit_id = insert_numbered_deposit(pool, &seed, 94).await?;
            let deposit = db::get_deposit(pool, deposit_id)
                .await?
                .context("deposit")?;
            ensure!(deposit.account_id == seed.account_id && deposit.livemode);
            ensure!(deposit.customer_id == seed.customer_id);
            // A refund of that deposit filed under another account.
            assert_sqlstate(
                sqlx::query(
                    "INSERT INTO refunds (id, account_id, livemode, chain_id, deposit_id, \
                     amount_atomic, destination_address, status) \
                     VALUES ($1, $2, true, 1, $3, 1, $4, 'pending')",
                )
                .bind(Uuid::new_v4())
                .bind(other.account_id)
                .bind(deposit_id)
                .bind(format!("{:#x}", evm_address(95)))
                .execute(pool)
                .await
                .err(),
                "23503",
            )?;
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn restore_check_rejects_failed_rpc_with_current_schema_and_fresh_heartbeat() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let restore_pool = db::connect(&context.owner_url, "restore-check", 4).await?;
            assert_session_budgets(&restore_pool, ("5min", "30s", "5min")).await?;
            let heartbeat = heartbeat::record(&context.app_pool).await?;

            let reconciler = restore_reconciler(&restore_pool)?;
            let expectations = restore_expectations(&heartbeat);
            let report = restore::check(&restore_pool, &expectations, &reconciler)
                .await
                .map_err(anyhow::Error::msg)?;
            ensure!(report.status == "incomplete");
            // The check records the restore, which freezes the service until the operator
            // reconciles and unfreezes it.
            let restore = topup::restore_mode::active(&context.app_pool)
                .await?
                .context("restore-check freezes the service")?;
            ensure!(report.restore_id == restore.id.to_string());
            ensure!(restore.detected_by == "restore_check");
            let latest = db::MIGRATOR
                .iter()
                .map(|migration| migration.version)
                .max()
                .context("embedded migrations")?;
            ensure!(report.latest_migration == latest);
            ensure!(report.measured_rpo_seconds == Some(0));
            ensure!(report.rpo_basis == "heartbeat_and_lsn");
            ensure!(report.wal_bytes_behind == Some(0));
            ensure!(report.expected_lsn.as_deref() == Some(heartbeat.wal_lsn.as_str()));
            ensure!(report.row_counts.get("heartbeat") == Some(&1));
            ensure!(report.post_restore_reconciliation.status == "incomplete");
            ensure!(
                !report
                    .post_restore_reconciliation
                    .findings
                    .iter()
                    .any(|finding| finding.incomplete)
            );
            restore_pool.close().await;
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn restore_check_without_a_source_lsn_flags_heartbeat_only_rpo() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let heartbeat = heartbeat::record(&context.app_pool).await?;
            let reconciler = restore_reconciler(&context.owner_pool)?;
            let expectations = restore::RestoreExpectations {
                failure_at: Some(heartbeat.recorded_at),
                expected_lsn: None,
            };
            let report = restore::check(&context.owner_pool, &expectations, &reconciler)
                .await
                .map_err(anyhow::Error::msg)?;
            ensure!(report.status == "incomplete");
            ensure!(report.rpo_basis == "heartbeat_only");
            let encoded = serde_json::to_value(&report)?;
            ensure!(encoded["failure_at"] == serde_json::to_value(heartbeat.recorded_at)?);
            ensure!(report.failure_at == Some(heartbeat.recorded_at));
            ensure!(report.expected_lsn.is_none());
            ensure!(report.wal_bytes_behind.is_none());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn restore_check_at_boot_reports_an_unanchored_rpo() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let heartbeat = heartbeat::record(&context.app_pool).await?;
            let reconciler = restore_reconciler(&context.owner_pool)?;
            let expectations = restore::RestoreExpectations {
                failure_at: None,
                expected_lsn: None,
            };
            let report = restore::check(&context.owner_pool, &expectations, &reconciler)
                .await
                .map_err(anyhow::Error::msg)?;
            ensure!(report.status == "incomplete");
            ensure!(report.rpo_basis == "unanchored");
            ensure!(report.measured_rpo_seconds.is_none());
            ensure!(report.restored_heartbeat_at == heartbeat.recorded_at);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn application_role_can_only_append_heartbeats() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let heartbeat = heartbeat::record(&context.app_pool).await?;
            ensure!(heartbeat.wal_lsn.contains('/'));
            let update = sqlx::query("UPDATE heartbeat SET recorded_at = now() WHERE id = $1")
                .bind(heartbeat.id)
                .execute(&context.app_pool)
                .await
                .err();
            assert_sqlstate(update, "42501")?;
            let delete = sqlx::query("DELETE FROM heartbeat WHERE id = $1")
                .bind(heartbeat.id)
                .execute(&context.app_pool)
                .await
                .err();
            assert_sqlstate(delete, "42501")?;
            let truncate = sqlx::query("TRUNCATE heartbeat")
                .execute(&context.app_pool)
                .await
                .err();
            assert_sqlstate(truncate, "42501")?;
            Ok(())
        })
    })
    .await
}

/// Chain double for a restore check run without chain access; only alert-only checks use it.
struct UnavailableChain;

impl UnavailableChain {
    fn error<T>() -> Result<T, ReconciliationError> {
        Err(ReconciliationError::Chain("chain unavailable".to_owned()))
    }
}

#[async_trait]
impl ReconciliationChain for UnavailableChain {
    async fn finalized_head(&self) -> Result<u64, ReconciliationError> {
        Self::error()
    }

    async fn transfer_logs_to(
        &self,
        _addresses: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<TransferLog>, ReconciliationError> {
        Self::error()
    }

    async fn token_balances(
        &self,
        _token: Address,
        _addresses: &[Address],
        _block: u64,
    ) -> Result<Vec<U256>, ReconciliationError> {
        Self::error()
    }

    async fn factory_addresses(
        &self,
        _factory: Address,
        _treasury: Address,
        _salts: &[B256],
    ) -> Result<Vec<Address>, ReconciliationError> {
        Self::error()
    }
}

fn restore_reconciler(pool: &PgPool) -> Result<Reconciler> {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.rpc_providers = vec![
        "http://127.0.0.1:8546".to_owned(),
        "http://localhost:8546".to_owned(),
    ];
    let chain_id = route.chain.chain_id;
    Ok(Reconciler::with_dependencies(
        pool.clone(),
        Arc::new(topup::routes::RouteSet::new(vec![route]).map_err(anyhow::Error::msg)?),
        BTreeMap::from([(
            chain_id,
            Arc::new(UnavailableChain) as Arc<dyn ReconciliationChain>,
        )]),
    ))
}

#[tokio::test]
async fn restore_check_asks_the_product_nothing_and_keeps_recorded_credits() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let heartbeat = heartbeat::record(&context.app_pool).await?;
            let seed = seed_account(&context.app_pool, 3).await?;
            let credited = insert_numbered_deposit(&context.app_pool, &seed, 3).await?;
            sqlx::query(
                "UPDATE deposits SET state = 'credited', valuation_at = now(), \
                 price_scaled = 25000000, price_source = 'spot', credit_minor = 250 WHERE id = $1",
            )
            .bind(credited)
            .execute(&context.owner_pool)
            .await?;
            let reconciler = restore_reconciler(&context.owner_pool)?;
            let expectations = restore_expectations(&heartbeat);

            let report = restore::check(&context.owner_pool, &expectations, &reconciler)
                .await
                .map_err(anyhow::Error::msg)?;
            ensure!(
                report.status == "incomplete",
                "failed RPC must block acceptance: {:?}",
                report.failures
            );
            ensure!(report.post_restore_reconciliation.status == "incomplete");
            // Critical chain checks fail closed, while the recorded credit stays unchanged.
            ensure!(
                report
                    .post_restore_reconciliation
                    .failed_checks
                    .contains(&CheckName::CustodyBalance)
            );
            ensure!(
                report
                    .failures
                    .iter()
                    .any(|failure| failure.contains("custody_balance"))
            );
            let deposit = db::get_deposit(&context.app_pool, credited)
                .await?
                .context("credited deposit")?;
            ensure!(deposit.state == DepositState::Credited);
            ensure!(deposit.credit_minor.map(|value| value.value()) == Some(250));
            Ok(())
        })
    })
    .await
}

/// A deposit without its transaction's origin, or a deposit event without its snapshot's identity,
/// fails its write: only a reversed deposit restored from a delivery lacks its origin.
#[tokio::test]
async fn rows_keep_the_invariants_the_service_reads() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 1).await?;
            let deposit = new_deposit(seed.address_id, 1, 1, 0);
            let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
            ensure!(db::insert_deposit(&context.app_pool, &deposit).await?);
            for statement in [
                "UPDATE deposits SET tx_from = NULL, tx_nonce = NULL WHERE id = $1",
                "UPDATE deposits SET tx_nonce = NULL WHERE id = $1",
            ] {
                assert_sqlstate(
                    sqlx::query(statement)
                        .bind(id)
                        .execute(&context.owner_pool)
                        .await
                        .err(),
                    "23514",
                )?;
            }

            let insert_event = |object_type: &'static str, data: Value| {
                let pool = context.app_pool.clone();
                async move {
                    sqlx::query(
                        "INSERT INTO events (id, account_id, livemode, type, object_type, \
                         object_id, actor, data) VALUES ($1, $2, true, $3, $4, $5, 'system', $6)",
                    )
                    .bind(Uuid::new_v4())
                    .bind(seed.account_id)
                    .bind(format!("{object_type}.test"))
                    .bind(object_type)
                    .bind(Uuid::new_v4())
                    .bind(data)
                    .execute(&pool)
                    .await
                }
            };
            let identity = json!({"object": {"receipt_log_index": 0, "revision": 0,
                "block_hash": format!("{:#x}", b256(1)), "block_time": 1_790_000_000}});
            insert_event("deposit", identity.clone()).await?;
            insert_event("quote", json!({"object": {}})).await?;
            let mut without_block = identity;
            without_block["object"]["block_hash"] = Value::Null;
            for (object_type, data) in [
                ("deposit", json!({"object": {}})),
                ("deposit", without_block),
                ("quote", json!({})),
            ] {
                assert_sqlstate(insert_event(object_type, data).await.err(), "23514")?;
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn application_role_can_append_and_read_history_but_cannot_mutate_it() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 1).await?;
            let deposit = new_deposit(seed.address_id, 1, 1, 0);
            let deposit_id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
            ensure!(db::insert_deposit(&context.app_pool, &deposit).await?);

            let transition_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO transitions (id, deposit_id, from_state, to_state, attempt, evidence) VALUES ($1, $2, 'detected', 'detected', 0, '{}'::jsonb)",
            )
            .bind(transition_id)
            .bind(deposit_id)
            .execute(&context.app_pool)
            .await?;
            let audit_id = Uuid::new_v4();
            insert_audit(&context.app_pool, audit_id, "permission test").await?;

            let transition_count: i64 = sqlx::query("SELECT count(*) FROM transitions")
                .fetch_one(&context.app_pool)
                .await?
                .try_get(0)?;
            let audit_count: i64 = sqlx::query("SELECT count(*) FROM audit")
                .fetch_one(&context.app_pool)
                .await?
                .try_get(0)?;
            ensure!(transition_count == 1 && audit_count == 1);

            for statement in [
                "UPDATE transitions SET evidence = '{}'::jsonb",
                "DELETE FROM transitions",
                "TRUNCATE transitions",
                "UPDATE audit SET reason = 'changed'",
                "DELETE FROM audit",
                "TRUNCATE audit",
            ] {
                assert_sqlstate(
                    sqlx::query(statement)
                        .execute(&context.app_pool)
                        .await
                        .err(),
                    "42501",
                )?;
            }
            assert_sqlstate(
                sqlx::query("TRUNCATE events")
                    .execute(&context.app_pool)
                    .await
                    .err(),
                "42501",
            )?;
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn application_role_can_read_but_cannot_mutate_migration_history() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let versions: Vec<i64> =
                sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(&context.app_pool)
                    .await?;
            let expected: Vec<i64> = db::MIGRATOR
                .iter()
                .filter(|migration| migration.migration_type.is_up_migration())
                .map(|migration| migration.version)
                .collect();
            ensure!(versions == expected);

            for statement in [
                "UPDATE _sqlx_migrations SET version = version",
                "DELETE FROM _sqlx_migrations",
            ] {
                assert_sqlstate(
                    sqlx::query(statement)
                        .execute(&context.app_pool)
                        .await
                        .err(),
                    "42501",
                )?;
            }
            Ok(())
        })
    })
    .await
}

/// Mirrors the grants table in `crates/topup/migrations/README.md`; grants come from default
/// privileges, so a new table must be listed here with its intended privileges.
const DOCUMENTED_GRANTS: &[(&str, &[&str])] = &[
    ("transitions", &["SELECT", "INSERT"]),
    ("audit", &["SELECT", "INSERT"]),
    ("reconciliation_findings", &["SELECT", "INSERT"]),
    ("heartbeat", &["SELECT", "INSERT"]),
    ("reconciliation_blocks", &["SELECT", "INSERT", "DELETE"]),
    (
        "deposit_address_client_secrets",
        &["SELECT", "INSERT", "DELETE"],
    ),
    ("flushed", &["SELECT", "INSERT"]),
    ("flush_failures", &["SELECT", "INSERT"]),
    (
        "reconciliation_deposit_cursors",
        &["SELECT", "INSERT", "UPDATE"],
    ),
    ("restore_timeline", &["SELECT", "UPDATE"]),
    ("restores", &["SELECT", "INSERT", "UPDATE"]),
    ("restore_delivered_events", &["SELECT", "INSERT"]),
    ("restore_deposit_tombstones", &["SELECT", "INSERT"]),
    // Plus `UPDATE` of its discard columns only, checked below.
    ("restore_delivered_credits", &["SELECT", "INSERT"]),
    ("rpc_config_acceptances", &["SELECT", "INSERT"]),
    ("rpc_member_validations", &["SELECT", "INSERT"]),
    ("rpc_chain_state", &["SELECT", "INSERT"]),
    ("rpc_window_reviews", &["SELECT", "INSERT"]),
    ("rpc_watermarks", &["SELECT", "INSERT", "UPDATE"]),
    ("rpc_recoveries", &["SELECT"]),
    ("rpc_role_bindings", &["SELECT", "INSERT"]),
    ("rpc_reorg_ranges", &["SELECT", "INSERT"]),
    ("_sqlx_migrations", &["SELECT"]),
    ("topup_migration_compatibility", &[]),
    ("price_twap_observations", &["SELECT", "INSERT"]),
    ("chain_checkpoints", &["SELECT", "INSERT", "UPDATE"]),
    ("chain_coverage", &["SELECT", "INSERT", "UPDATE"]),
    ("daily_budgets", &["SELECT", "INSERT", "UPDATE"]),
    ("accounts", OPERATIONAL),
    ("payment_settings_revisions", &["SELECT", "INSERT"]),
    ("payment_settings_state", OPERATIONAL),
    ("payment_settings_cutover", &["SELECT", "UPDATE"]),
    ("account_limits", OPERATIONAL),
    ("api_keys", OPERATIONAL),
    ("treasuries", OPERATIONAL),
    ("treasury_challenges", OPERATIONAL),
    ("route_pauses", OPERATIONAL),
    ("seen_signatures", OPERATIONAL),
    ("customers", OPERATIONAL),
    ("quotes", OPERATIONAL),
    ("deposit_addresses", OPERATIONAL),
    ("addresses", OPERATIONAL),
    ("cursors", OPERATIONAL),
    ("scan_address_sweeps", OPERATIONAL),
    ("reconciliation_work_cursors", OPERATIONAL),
    ("pending_transfers", OPERATIONAL),
    ("deposits", OPERATIONAL),
    ("refunds", OPERATIONAL),
    ("webhook_endpoints", OPERATIONAL),
    ("events", &["SELECT", "INSERT"]),
    ("webhook_deliveries", OPERATIONAL),
    ("idempotency_keys", OPERATIONAL),
    ("retiring_webhook_keys", OPERATIONAL),
];
const OPERATIONAL: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE"];

#[tokio::test]
async fn application_role_privileges_match_the_documented_grants() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let tables: Vec<String> = sqlx::query_scalar(
                "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY 1",
            )
            .fetch_all(&context.owner_pool)
            .await?;
            let documented: BTreeMap<&str, &[&str]> = DOCUMENTED_GRANTS.iter().copied().collect();
            ensure!(documented.len() == DOCUMENTED_GRANTS.len(), "duplicate documented table");
            let mut listed: Vec<&str> = documented.keys().copied().collect();
            listed.sort_unstable();
            ensure!(
                tables == listed,
                "public tables differ from the documented grants: tables={tables:?} documented={listed:?}"
            );

            for table in &tables {
                let expected = documented
                    .get(table.as_str())
                    .context("documented table")?;
                for privilege in [
                    "SELECT",
                    "INSERT",
                    "UPDATE",
                    "DELETE",
                    "TRUNCATE",
                    "REFERENCES",
                    "TRIGGER",
                ] {
                    let granted: bool =
                        sqlx::query_scalar("SELECT has_table_privilege('topup_app', $1, $2)")
                            .bind(format!("public.{table}"))
                            .bind(privilege)
                            .fetch_one(&context.owner_pool)
                            .await?;
                    ensure!(
                        granted == expected.contains(&privilege),
                        "topup_app {privilege} on {table}: granted={granted}"
                    );
                }
            }
            for (table, columns) in [
                ("rpc_member_validations", vec![("validated_at",true),("genesis_hash",false)]),
                ("rpc_chain_state", vec![("frozen",true),("awaiting_anchor",true),("epoch",false),("recovery_pending",false)]),
                ("rpc_window_reviews", vec![("reviewed_at",true),("reviewed_by",true),("replayed_at",true),("epoch",false),("request",false),("end_hash",false)]),
                ("rpc_reorg_ranges", vec![("replayed_through",true),("from_block",false),("epoch",false)]),
            ] {
                for (column, expected) in columns {
                    let granted: bool = sqlx::query_scalar("SELECT has_column_privilege('topup_app', $1, $2, 'UPDATE')")
                        .bind(format!("public.{table}")).bind(column).fetch_one(&context.owner_pool).await?;
                    ensure!(granted == expected, "UPDATE of {table}.{column}: granted={granted}");
                }
            }
            // A delivered credit is recorded once; only its discard is ever written.
            for (column, expected) in [
                ("discarded_at", true),
                ("discarded_by", true),
                ("discard_reason", true),
                ("credit_minor", false),
                ("price_scaled", false),
            ] {
                let granted: bool = sqlx::query_scalar(
                    "SELECT has_column_privilege('topup_app', \
                     'public.restore_delivered_credits', $1, 'UPDATE')",
                )
                .bind(column)
                .fetch_one(&context.owner_pool)
                .await?;
                ensure!(granted == expected, "UPDATE of {column}: granted={granted}");
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn owner_side_history_mutation_is_rejected_by_defense_in_depth_triggers() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 2).await?;
            let deposit = new_deposit(seed.address_id, 1, 2, 0);
            let deposit_id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
            db::insert_deposit(&context.app_pool, &deposit).await?;
            let transition_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO transitions (id, deposit_id, from_state, to_state, attempt, evidence) VALUES ($1, $2, 'detected', 'detected', 0, '{}'::jsonb)",
            )
            .bind(transition_id)
            .bind(deposit_id)
            .execute(&context.app_pool)
            .await?;
            let audit_id = Uuid::new_v4();
            insert_audit(&context.app_pool, audit_id, "trigger test").await?;
            for (statement, id) in [
                ("UPDATE transitions SET evidence = '{}'::jsonb WHERE id = $1", transition_id),
                ("DELETE FROM transitions WHERE id = $1", transition_id),
                ("UPDATE audit SET reason = 'changed' WHERE id = $1", audit_id),
                ("DELETE FROM audit WHERE id = $1", audit_id),
            ] {
                assert_sqlstate(
                    sqlx::query(statement)
                        .bind(id)
                        .execute(&context.owner_pool)
                        .await
                        .err(),
                    "55000",
                )?;
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn accounts_and_customers_enforce_identity_uniqueness_and_only_pause_mutates() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let first_account =
                seed::create_account(&context.app_pool, &NewAccount::named("a")).await?;
            let second_account =
                seed::create_account(&context.app_pool, &NewAccount::named("b")).await?;
            ensure!(
                first_account.public_id
                    == topup::ids::format(topup::ids::ACCOUNT, first_account.id)
            );

            seed::set_account_paused_scopes(
                &context.app_pool,
                first_account.id,
                &["quotes".to_owned()],
            )
            .await?;
            let stored_account = db::get_account(&context.app_pool, first_account.id)
                .await?
                .context("account must exist")?;
            ensure!(stored_account.paused_scopes == ["quotes"]);

            let first_customer = NewCustomer {
                id: Uuid::new_v4(),
                account_id: first_account.id,
                livemode: true,
                client_reference_id: "workspace".to_owned(),
                paused_scopes: Vec::new(),
            };
            seed::create_customer(&context.app_pool, &first_customer).await?;
            assert_unique(
                seed::create_customer(
                    &context.app_pool,
                    &NewCustomer {
                        id: Uuid::new_v4(),
                        ..first_customer.clone()
                    },
                )
                .await
                .err(),
            )?;
            // The same reference is another customer in the other mode or another account.
            for (account_id, livemode) in [(first_account.id, false), (second_account.id, true)] {
                seed::create_customer(
                    &context.app_pool,
                    &NewCustomer {
                        id: Uuid::new_v4(),
                        account_id,
                        livemode,
                        ..first_customer.clone()
                    },
                )
                .await?;
            }
            seed::set_customer_paused_scopes(
                &context.app_pool,
                first_customer.id,
                &["settlement".to_owned()],
            )
            .await?;
            let stored_customer = db::get_customer(&context.app_pool, first_customer.id)
                .await?
                .context("customer must exist")?;
            ensure!(stored_customer.account_id == first_customer.account_id);
            ensure!(stored_customer.client_reference_id == first_customer.client_reference_id);
            ensure!(stored_customer.paused_scopes == ["settlement"]);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn addresses_are_canonical_and_unique_per_chain() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let first_account = seed_account_without_address(&context.app_pool, 20).await?;
            let second = seed_account_without_address(&context.app_pool, 21).await?;
            let checksum = Address::from_str("0x52908400098527886E0F7030069857D2E4169EE7")?;
            let lowercase = Address::from_str("0x52908400098527886e0f7030069857d2e4169ee7")?;
            ensure!(checksum == lowercase);
            let first = new_address(first_account.customer_id, 1, checksum, 20);
            seed::insert_address(&context.app_pool, &first).await?;
            assert_unique(
                seed::insert_address(
                    &context.app_pool,
                    &new_address(second.customer_id, 1, lowercase, 22),
                )
                .await
                .err(),
            )?;

            // One customer may hold several addresses on a chain.
            seed::insert_address(
                &context.app_pool,
                &new_address(first_account.customer_id, 1, evm_address(23), 23),
            )
            .await?;
            seed::insert_address(
                &context.app_pool,
                &new_address(second.customer_id, 2, lowercase, 24),
            )
            .await?;

            let before: i64 = sqlx::query("SELECT count(*) FROM addresses")
                .fetch_one(&context.app_pool)
                .await?
                .try_get(0)?;
            ensure!(Address::from_str("not-an-address").is_err());
            ensure!(B256::from_str("not-a-hash").is_err());
            let after: i64 = sqlx::query("SELECT count(*) FROM addresses")
                .fetch_one(&context.app_pool)
                .await?
                .try_get(0)?;
            ensure!(before == after, "malformed input reached the database");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn deposits_are_idempotent_and_concurrent_claimers_get_different_rows() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 30).await?;
            let first = new_deposit(seed.address_id, 1, 30, 0);
            ensure!(db::insert_deposit(&context.app_pool, &first).await?);
            ensure!(!db::insert_deposit(&context.app_pool, &first).await?);
            ensure!(
                db::insert_deposit(
                    &context.app_pool,
                    &NewDeposit {
                        chain_id: 2,
                        ..first.clone()
                    },
                )
                .await?
            );
            db::insert_deposit(&context.app_pool, &new_deposit(seed.address_id, 1, 31, 0)).await?;

            let (first_claim, second_claim) = tokio::join!(
                db::claim_deposit(&context.app_pool, Uuid::new_v4()),
                db::claim_deposit(&context.app_pool, Uuid::new_v4())
            );
            let first_claim = first_claim?.context("first claim must return a row")?;
            let second_claim = second_claim?.context("second claim must return a row")?;
            ensure!(first_claim.id != second_claim.id);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn attempts_survive_claim_and_wait_then_reset_on_advance() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 40).await?;
            let deposit = new_deposit(seed.address_id, 1, 40, 0);
            let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
            db::insert_deposit(&context.app_pool, &deposit).await?;
            sqlx::query("UPDATE deposits SET attempt = 3 WHERE id = $1")
                .bind(id)
                .execute(&context.app_pool)
                .await?;

            let claimed = db::claim_deposit(&context.app_pool, Uuid::new_v4())
                .await?
                .context("deposit must be claimable")?;
            ensure!(claimed.attempt == 3);
            let wait = next(
                DepositState::Detected,
                &StepOutcome::Wait {
                    reason: WaitReason::Paused,
                },
            )?;
            let mut transaction = context.app_pool.begin().await?;
            let result = db::apply_transition(
                &mut transaction,
                &topup::routes::RouteSet::default(),
                id,
                DepositState::Detected,
                claimed.lease_token.context("claim must have a token")?,
                TransitionUpdate {
                    transition: wait,
                    rejection_reason: None,
                    attempt: 3,
                    next_attempt_at: Utc::now() - Duration::seconds(1),
                },
                db::TransitionWrites {
                    evidence: &json!({"wait": "paused"}),
                    effects: &db::TransitionEffects::default(),
                    outbox_events: &[],
                },
            )
            .await?;
            ensure!(result == ApplyTransitionResult::Applied);
            transaction.commit().await?;
            ensure!(
                db::get_deposit(&context.app_pool, id)
                    .await?
                    .context("deposit")?
                    .attempt
                    == 3
            );

            let claimed = db::claim_deposit(&context.app_pool, Uuid::new_v4())
                .await?
                .context("deposit must be claimable again")?;
            let advance = next(DepositState::Detected, &StepOutcome::Advance)?;
            let mut transaction = context.app_pool.begin().await?;
            db::apply_transition(
                &mut transaction,
                &topup::routes::RouteSet::default(),
                id,
                DepositState::Detected,
                claimed.lease_token.context("claim must have a token")?,
                TransitionUpdate {
                    transition: advance,
                    rejection_reason: None,
                    attempt: 0,
                    next_attempt_at: Utc::now(),
                },
                db::TransitionWrites {
                    evidence: &json!({"advance": true}),
                    effects: &db::TransitionEffects::default(),
                    outbox_events: &[],
                },
            )
            .await?;
            transaction.commit().await?;
            let stored = db::get_deposit(&context.app_pool, id)
                .await?
                .context("deposit must exist")?;
            ensure!(stored.state == DepositState::Confirmed && stored.attempt == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn transition_cas_and_outbox_are_atomic() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 50).await?;
            let deposit = new_deposit(seed.address_id, 1, 50, 0);
            let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
            db::insert_deposit(&context.app_pool, &deposit).await?;
            let claimed = db::claim_deposit(&context.app_pool, Uuid::new_v4())
                .await?
                .context("deposit must be claimable")?;
            let transition = next(DepositState::Detected, &StepOutcome::Advance)?;
            let update = TransitionUpdate {
                transition,
                rejection_reason: None,
                attempt: 0,
                next_attempt_at: Utc::now(),
            };

            let mut transaction = context.app_pool.begin().await?;
            ensure!(
                db::apply_transition(
                    &mut transaction,
                    &topup::routes::RouteSet::default(),
                    id,
                    DepositState::Detected,
                    Uuid::new_v4(),
                    update,
                    db::TransitionWrites {
                        evidence: &json!({}),
                        effects: &db::TransitionEffects::default(),
                        outbox_events: &[],
                    },
                )
                .await?
                    == ApplyTransitionResult::Stale
            );
            transaction.commit().await?;

            // The second event cannot be stored (its account does not exist), after the state,
            // transition, and first event were written: none of them may survive.
            let first_event = Uuid::new_v4();
            let events = [
                OutboxEvent {
                    id: first_event,
                    event_type: "deposit.rejected".to_owned(),
                    account_id: seed.account_id,
                    livemode: true,
                    object: EventObject::Deposit(id),
                    next_attempt_at: Utc::now(),
                    actor: topup::db::SYSTEM_ACTOR.to_owned(),
                    request: None,
                    signing_key_version: None,
                },
                OutboxEvent {
                    id: Uuid::new_v4(),
                    event_type: "deposit.rejected".to_owned(),
                    account_id: Uuid::new_v4(),
                    livemode: true,
                    object: EventObject::Deposit(id),
                    next_attempt_at: Utc::now(),
                    actor: topup::db::SYSTEM_ACTOR.to_owned(),
                    request: None,
                    signing_key_version: None,
                },
            ];
            let mut transaction = context.app_pool.begin().await?;
            ensure!(
                db::apply_transition(
                    &mut transaction,
                    &topup::routes::RouteSet::default(),
                    id,
                    DepositState::Detected,
                    claimed.lease_token.context("claim must have a token")?,
                    update,
                    db::TransitionWrites {
                        evidence: &json!({"atomic": true}),
                        effects: &db::TransitionEffects::default(),
                        outbox_events: &events,
                    },
                )
                .await
                .is_err()
            );
            transaction.rollback().await?;
            let stored = db::get_deposit(&context.app_pool, id)
                .await?
                .context("deposit must exist")?;
            ensure!(stored.state == DepositState::Detected);
            ensure!(count_where(&context.app_pool, "transitions", "deposit_id", id).await? == 0);
            ensure!(count_where(&context.app_pool, "events", "id", first_event).await? == 0);

            // A repeated event id is written once: deterministic ids make re-emission a no-op.
            let repeated = Uuid::new_v4();
            let events = [id, Uuid::new_v4()].map(|object| OutboxEvent {
                id: repeated,
                event_type: "deposit.credited".to_owned(),
                account_id: seed.account_id,
                livemode: true,
                object: EventObject::Deposit(object),
                next_attempt_at: Utc::now(),
                actor: topup::db::SYSTEM_ACTOR.to_owned(),
                request: None,
                signing_key_version: None,
            });
            let mut transaction = context.app_pool.begin().await?;
            ensure!(
                db::apply_transition(
                    &mut transaction,
                    &topup::routes::RouteSet::default(),
                    id,
                    DepositState::Detected,
                    claimed.lease_token.context("claim must have a token")?,
                    update,
                    db::TransitionWrites {
                        evidence: &json!({}),
                        effects: &db::TransitionEffects::default(),
                        outbox_events: &events,
                    },
                )
                .await?
                    == ApplyTransitionResult::Applied
            );
            transaction.commit().await?;
            let objects: Vec<Uuid> =
                sqlx::query_scalar("SELECT object_id FROM events WHERE id = $1")
                    .bind(repeated)
                    .fetch_all(&context.app_pool)
                    .await?;
            ensure!(objects == [id]);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn rate_lock_consumption_is_unique_but_unconsumed_locks_are_independent() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 80).await?;
            let first_deposit = insert_numbered_deposit(&context.app_pool, &seed, 80).await?;
            let second_deposit = insert_numbered_deposit(&context.app_pool, &seed, 81).await?;
            let first_lock = insert_lock_address(&context.app_pool, seed.customer_id, 80).await?;
            let second_lock = insert_lock_address(&context.app_pool, seed.customer_id, 81).await?;
            let third_lock = insert_lock_address(&context.app_pool, seed.customer_id, 82).await?;
            let fourth_lock = insert_lock_address(&context.app_pool, seed.customer_id, 83).await?;

            insert_rate_lock(&context.app_pool, first_lock, Some(first_deposit)).await?;
            assert_unique(
                insert_rate_lock(&context.app_pool, second_lock, Some(first_deposit))
                    .await
                    .err(),
            )?;
            insert_rate_lock(&context.app_pool, third_lock, None).await?;
            insert_rate_lock(&context.app_pool, fourth_lock, Some(second_deposit)).await?;
            Ok(())
        })
    })
    .await
}

#[derive(Clone, Copy)]
struct Seed {
    account_id: Uuid,
    customer_id: Uuid,
    address_id: Uuid,
}

#[derive(Clone, Copy)]
struct AccountSeed {
    account_id: Uuid,
    customer_id: Uuid,
}

async fn seed_account(pool: &PgPool, number: u8) -> Result<Seed> {
    let account = seed_account_without_address(pool, number).await?;
    let address = new_address(account.customer_id, 1, evm_address(number), number);
    seed::insert_address(pool, &address).await?;
    Ok(Seed {
        account_id: account.account_id,
        customer_id: account.customer_id,
        address_id: address.id,
    })
}

async fn seed_account_without_address(pool: &PgPool, number: u8) -> Result<AccountSeed> {
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            webhook_url: format!("https://product-{number}.test/webhooks"),
            ..NewAccount::named(&format!("product-{number}"))
        },
        &format!("workspace-{number}"),
    )
    .await?;
    Ok(AccountSeed {
        account_id: account.id,
        customer_id: customer.id,
    })
}

fn new_address(customer_id: Uuid, chain_id: u64, address: Address, salt_byte: u8) -> NewAddress {
    NewAddress {
        id: Uuid::new_v4(),
        customer_id,
        chain_id,
        route: "ethereum-pha".to_owned(),
        salt: b256(salt_byte),
        address,
    }
}

async fn insert_audit(pool: &PgPool, id: Uuid, reason: &str) -> Result<()> {
    topup::audit::insert_with_id(
        pool,
        id,
        &topup::audit::Entry {
            account_id: None,
            actor: &topup::audit::Actor::admin("admin/v1"),
            action: "pause",
            subject: "route:test",
            reason,
        },
    )
    .await?;
    Ok(())
}

fn new_deposit(address_id: Uuid, chain_id: u64, number: u8, log_index: u64) -> NewDeposit {
    NewDeposit {
        chain_id,
        tx_hash: b256(number),
        log_index,
        receipt_log_index: log_index,
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        is_final: true,
        block_number: 100 + u64::from(number),
        block_hash: b256(number.wrapping_add(1)),
        block_time: Utc::now(),
        address_id,
        route: Some("ethereum-pha".to_owned()),
        route_version: Some(1),
        asset_contract: evm_address(200),
        from_address: evm_address(number.wrapping_add(100)),
        amount_atomic: atomic(1_000),
        state: DepositState::Detected,
        reason: None,
        next_attempt_at: Utc::now() - Duration::seconds(1),
    }
}

async fn insert_numbered_deposit(pool: &PgPool, seed: &Seed, number: u8) -> Result<Uuid> {
    let deposit = new_deposit(seed.address_id, 1, number, 0);
    let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
    ensure!(db::insert_deposit(pool, &deposit).await?);
    Ok(id)
}

/// A failure instant coinciding with the sampled source heartbeat.
fn restore_expectations(heartbeat: &heartbeat::Heartbeat) -> restore::RestoreExpectations {
    restore::RestoreExpectations {
        failure_at: Some(heartbeat.recorded_at),
        expected_lsn: Some(heartbeat.wal_lsn.clone()),
    }
}

async fn insert_lock_address(pool: &PgPool, customer_id: Uuid, number: u8) -> Result<Uuid> {
    let id = Uuid::new_v4();
    seed::insert_address(
        pool,
        &NewAddress {
            id,
            customer_id,
            chain_id: 1,
            route: "ethereum-pha".to_owned(),
            salt: b256(number),
            address: evm_address(number.wrapping_add(100)),
        },
    )
    .await?;
    Ok(id)
}

async fn insert_rate_lock(
    pool: &PgPool,
    address_id: Uuid,
    consumed_by: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let status = if consumed_by.is_some() {
        "consumed"
    } else {
        "open"
    };
    sqlx::query(
        r#"
        UPDATE quotes
        SET amount_atomic = 1000, price_scaled = 25000000, credit_minor = 250,
            expires_at = now() + interval '15 minutes', consumed_by = $2, status = $3,
            closed_at = CASE WHEN $3 = 'open' THEN NULL ELSE now() END
        WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
        "#,
    )
    .bind(address_id)
    .bind(consumed_by)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

async fn count_where(pool: &PgPool, table: &str, column: &str, id: Uuid) -> Result<i64> {
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE {column} = $1"
    )))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get(0)?)
}

fn atomic(value: u64) -> AtomicAmount {
    AtomicAmount::new(U256::from(value))
}

fn evm_address(byte: u8) -> Address {
    Address::from([byte; 20])
}

fn b256(byte: u8) -> B256 {
    B256::from([byte; 32])
}

fn assert_unique(error: Option<sqlx::Error>) -> Result<()> {
    assert_sqlstate(error, "23505")
}

fn assert_sqlstate(error: Option<sqlx::Error>, expected: &str) -> Result<()> {
    let error = error.context("expected a database error")?;
    let database_error = error
        .as_database_error()
        .context("expected a database error")?;
    ensure!(
        database_error.code().as_deref() == Some(expected),
        "expected SQLSTATE {expected}, got {error}"
    );
    Ok(())
}

async fn assert_session_budgets(pool: &PgPool, expected: (&str, &str, &str)) -> Result<()> {
    let actual: (String, String, String) = sqlx::query_as(
        "SELECT current_setting('statement_timeout'), current_setting('lock_timeout'), \
         current_setting('idle_in_transaction_session_timeout')",
    )
    .fetch_one(pool)
    .await?;
    ensure!(
        (actual.0.as_str(), actual.1.as_str(), actual.2.as_str()) == expected,
        "unexpected session budgets: {actual:?}"
    );
    Ok(())
}

#[tokio::test]
async fn restore_check_counts_failure_time_and_does_not_grant_sampling_tolerance() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let heartbeat = heartbeat::record(&context.app_pool).await?;
            let reconciler = restore_reconciler(&context.owner_pool)?;
            let expectations = restore::RestoreExpectations {
                failure_at: Some(heartbeat.recorded_at + chrono::TimeDelta::milliseconds(60_100)),
                expected_lsn: None,
            };
            let report = restore::check(&context.owner_pool, &expectations, &reconciler)
                .await
                .map_err(anyhow::Error::msg)?;
            ensure!(report.measured_rpo_seconds == Some(61));
            ensure!(report.allowed_rpo_seconds == 60);
            ensure!(
                report
                    .failures
                    .iter()
                    .any(|failure| failure.contains("restore RPO exceeded"))
            );
            let active = topup::restore_mode::active(&context.owner_pool)
                .await?
                .context("frozen")?;
            topup::restore_mode::record_validation(&context.owner_pool, &active, "ok", &[]).await?;
            // An early DB failure during a repeated validation must invalidate the old pass.
            let invalid = restore::RestoreExpectations {
                expected_lsn: Some("invalid-lsn".to_owned()),
                ..expectations
            };
            ensure!(
                restore::check(&context.owner_pool, &invalid, &reconciler)
                    .await
                    .is_err()
            );
            let status: String = sqlx::query_scalar(
                "SELECT reason::jsonb ->> 'status' FROM audit WHERE action = 'restore.validation' \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .fetch_one(&context.owner_pool)
            .await?;
            ensure!(status == "incomplete");
            Ok(())
        })
    })
    .await
}

/// Concurrent scale indexes can finish before SQLx records their versions. Migration retries
/// validate and reuse those exact objects, and a down/up round trip leaves old-schema queries
/// available throughout the additive cutover.
#[tokio::test]
async fn scale_indexes_resume_unrecorded_builds_and_round_trip() -> Result<()> {
    with_database(|context| Box::pin(async move {
        let pool=&context.owner_pool;
        db::MIGRATOR.undo(pool,20261028000002).await?;
        for sql in [
            include_str!("../migrations/20261029030001_address_pages.up.sql"),
            include_str!("../migrations/20261029030002_credit_pages.up.sql"),
            include_str!("../migrations/20261029030003_address_created.up.sql"),
            include_str!("../migrations/20261029030004_custody_pages.up.sql"),
            include_str!("../migrations/20261029030005_flush_pages.up.sql"),
        ] {sqlx::query(sql).execute(pool).await?;}
        let objects=|| async {
            anyhow::Ok(sqlx::query_as::<_,(String,i64,bool)>("SELECT c.relname,c.oid::bigint,i.indisvalid FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid WHERE c.relname=ANY($1) ORDER BY c.relname")
                .bind(vec!["addresses_chain_page_idx","deposits_credit_page_idx","addresses_chain_created_idx","deposits_custody_page_idx","deposits_flush_page_idx"]).fetch_all(pool).await?)
        };
        let before=objects().await?;
        ensure!(before.len()==5 && before.iter().all(|(_,_,valid)|*valid));
        db::migrate(pool).await?;
        ensure!(objects().await?==before,"completed index was unnecessarily rebuilt");
        let compatible: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM _sqlx_migrations m \
             JOIN topup_migration_compatibility c ON c.version=m.version AND c.checksum=m.checksum \
             WHERE m.version BETWEEN 20261029030000 AND 20261029030005 \
               AND m.success AND c.compatibility_floor=20261029030005",
        )
        .fetch_one(pool)
        .await?;
        ensure!(compatible == 6, "scale migrations must retain the N-1 compatibility floor");
        db::MIGRATOR.undo(pool,20261028000002).await?;
        ensure!(objects().await?.is_empty());
        // Queries shipped by the previous release still work with the old or new schema.
        db::list_scan_addresses(&context.app_pool,1).await?;
        db::list_chain_addresses(&context.app_pool,1).await?;
        db::migrate(pool).await?;
        db::list_scan_addresses(&context.app_pool,1).await?;
        ensure!(objects().await?.len()==5);
        Ok(())
    })).await
}

/// Concurrent service list and heartbeat indexes reuse completed builds, round trip, and leave existing
/// compatibility ledger entries intact across rollback and migration retries.
#[tokio::test]
async fn service_indexes_resume_unrecorded_builds_and_round_trip() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.owner_pool;
            let ledger: Vec<(i64, Vec<u8>, i64)> = sqlx::query_as(
                "SELECT version, checksum, compatibility_floor FROM topup_migration_compatibility \
                 WHERE version <= 20261029030005 ORDER BY version",
            )
            .fetch_all(pool)
            .await?;
            db::MIGRATOR.undo(pool, 20261029030005).await?;
            for sql in [
                include_str!("../migrations/20261029040000_quote_list.up.sql"),
                include_str!("../migrations/20261029040001_refund_list.up.sql"),
                include_str!("../migrations/20261029040002_forwarder_list.up.sql"),
                include_str!("../migrations/20261029040003_heartbeat_recorded.up.sql"),
            ] {
                sqlx::query(sql).execute(pool).await?;
            }
            let objects = || async {
                sqlx::query_as::<_, (String, i64, bool)>(
                    "SELECT c.relname, c.oid::bigint, i.indisvalid FROM pg_class c \
                     JOIN pg_index i ON i.indexrelid=c.oid WHERE c.relname=ANY($1) \
                     ORDER BY c.relname",
                )
                .bind(vec![
                    "quotes_scope_created_idx",
                    "refunds_scope_created_idx",
                    "addresses_scope_page_idx",
                    "heartbeat_recorded_at_idx",
                ])
                .fetch_all(pool)
                .await
            };
            let before = objects().await?;
            ensure!(before.len() == 4 && before.iter().all(|(_, _, valid)| *valid));
            db::migrate(pool).await?;
            ensure!(objects().await? == before, "completed index was rebuilt");
            let compatible: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM _sqlx_migrations m \
                 JOIN topup_migration_compatibility c ON c.version=m.version AND c.checksum=m.checksum \
                 WHERE m.version BETWEEN 20261029040000 AND 20261029040003 \
                   AND m.success AND c.compatibility_floor=20261029030005",
            )
            .fetch_one(pool)
            .await?;
            ensure!(compatible == 4, "service indexes must retain the N-1 floor");
            db::MIGRATOR.undo(pool, 20261029030005).await?;
            ensure!(objects().await?.is_empty());
            db::migrate(pool).await?;
            ensure!(objects().await?.len() == 4);
            let retained: Vec<(i64, Vec<u8>, i64)> = sqlx::query_as(
                "SELECT version, checksum, compatibility_floor FROM topup_migration_compatibility \
                 WHERE version <= 20261029030005 ORDER BY version",
            )
            .fetch_all(pool)
            .await?;
            ensure!(retained == ledger, "rollback rewrote existing ledger entries");
            Ok(())
        })
    })
    .await
}
