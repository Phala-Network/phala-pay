//! PostgreSQL schema migration and repository functions.

mod accounts;
mod addresses;
pub mod chain_reads;
pub mod daily_budgets;
mod deposits;
pub(crate) mod migrations;
mod outbox;
pub(crate) mod pending;
pub mod pricing;
pub mod rpc;
pub(crate) mod scanner;
pub(crate) mod sweeps;
mod types;

use sqlx::PgPool;
use sqlx::migrate::Migrator;

pub use accounts::{Account, Customer, ensure_customer_in, get_account, get_customer};
pub(crate) use addresses::chain_address_page;
pub use addresses::{Address, get_address, list_chain_addresses};
pub(crate) use deposits::deposits_by_ids;
pub use deposits::{
    ApplyTransitionError, ApplyTransitionResult, CanonicalEvidence, ClaimedDeposit, Deposit,
    Evidence, LockConsumption, NewDeposit, OutboxEvent, StoredValuation, TransitionEffects,
    TransitionUpdate, TransitionWrites, UnfinalizedCredit, apply_transition, claim_deposit,
    claim_deposit_at, get_deposit, insert_deposit, insert_deposit_in, release_deposit_lease,
    unfinalized_credit,
};
pub use outbox::{
    EventObject, NewOutboxEvent, Notice, SYSTEM_ACTOR, enqueue_in, enqueue_rendered_in, event_data,
    is_account_event, previous_attributes, render, to_object,
};
pub use pending::{
    HeadCommit, NewPendingTransfer, PendingTransfer, commit_head_scan, list_address_pending,
    list_addresses_pending,
};
pub(crate) use scanner::find_scan_address;
pub use scanner::{
    ADDRESS_PAGE_SIZE, CoveredAddress, ScanAddress, ScanCommit, commit_confirmed_scan,
    coverage_addresses, get_confirmed_cursor, get_cursor, initialize_cursor,
    insert_scanned_deposit_in, list_scan_addresses, scan_address_page,
};
pub(crate) use sweeps::mark_swept;
pub use sweeps::{FactoryCommit, commit_factory_logs};

/// Embedded SQL migrations for the service database.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Applies every pending embedded migration.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    migrations::run(pool).await
}

pub(crate) fn state_code(state: topup_core::deposit::DepositState) -> &'static str {
    use topup_core::deposit::DepositState;

    match state {
        DepositState::Detected => "detected",
        DepositState::Confirmed => "confirmed",
        DepositState::Credited => "credited",
        DepositState::Swept => "swept",
        DepositState::Rejected => "rejected",
        DepositState::Reversed => "reversed",
    }
}

pub(crate) fn parse_state(value: &str) -> Result<topup_core::deposit::DepositState, sqlx::Error> {
    use topup_core::deposit::DepositState;

    match value {
        "detected" => Ok(DepositState::Detected),
        "confirmed" => Ok(DepositState::Confirmed),
        "credited" => Ok(DepositState::Credited),
        "swept" => Ok(DepositState::Swept),
        "rejected" => Ok(DepositState::Rejected),
        "reversed" => Ok(DepositState::Reversed),
        other => Err(sqlx::Error::Decode(
            format!("unknown deposit state `{other}`").into(),
        )),
    }
}

pub(crate) fn parse_reason(
    value: Option<&str>,
) -> Result<Option<topup_core::deposit::RejectReason>, sqlx::Error> {
    use topup_core::deposit::RejectReason;

    value
        .map(|reason| match reason {
            "unsupported_asset" => Ok(RejectReason::UnsupportedAsset),
            "below_minimum" => Ok(RejectReason::BelowMinimum),
            "out_of_range" => Ok(RejectReason::OutOfRange),
            "sanctioned" => Ok(RejectReason::Sanctioned),
            "out_of_bounds" => Ok(RejectReason::OutOfBounds),
            "asset_not_accepted" => Ok(RejectReason::AssetNotAccepted),
            other => Err(sqlx::Error::Decode(
                format!("unknown rejection reason `{other}`").into(),
            )),
        })
        .transpose()
}

/// Commits `transaction` after a success and rolls it back after an error, before the caller
/// answers. Begun in the caller's transaction (a handler's, from `Idempotent::begin`), it is a
/// savepoint: its commit keeps its changes for the caller to commit, and its rollback undoes them
/// and releases the row locks taken since, while the caller's transaction goes on. Dropping it
/// instead only queues the rollback until its connection is next used, so its row locks would
/// outlive the answer, and a worker that skips locked rows would pass them over.
pub async fn settle<T, E: From<sqlx::Error>>(
    transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<T, E>,
) -> Result<T, E> {
    match result {
        Ok(value) => {
            transaction.commit().await?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

/// Connects with the production session budgets for a runtime command.
pub async fn connect(
    url: &str,
    command: &str,
    max_connections: u32,
) -> Result<PgPool, sqlx::Error> {
    use std::time::Duration;
    let (statement, lock, idle) = if command == "migrate" || command == "restore-check" {
        (
            Duration::from_secs(300),
            Duration::from_secs(30),
            Duration::from_secs(300),
        )
    } else {
        (
            Duration::from_secs(30),
            Duration::from_secs(5),
            Duration::from_secs(60),
        )
    };
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .after_connect(move |connection, _| {
            Box::pin(async move {
                sqlx::query("SELECT set_config('statement_timeout', $1, false), set_config('lock_timeout', $2, false), set_config('idle_in_transaction_session_timeout', $3, false)")
                    .bind(format!("{}ms", statement.as_millis()))
                    .bind(format!("{}ms", lock.as_millis()))
                    .bind(format!("{}ms", idle.as_millis()))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
}
