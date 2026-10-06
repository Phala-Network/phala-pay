//! Head-state load semantics and single-statement coverage with a global subscriber.
mod support;
use alloy_primitives::B256;
use anyhow::{Result, ensure};
use topup::db;
use topup_adapters::chain::evm::group::{HeadAnchor, WatermarkStore};

#[tokio::test]
async fn head_state_matches_load_semantics_in_one_statement() -> Result<()> {
    use std::sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    };
    use topup_adapters::chain::evm::group::Failure;
    use tracing::Instrument;
    use tracing_subscriber::{
        Layer, layer::Context as TraceContext, prelude::*, registry::LookupSpan,
    };
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    static SUBSCRIBER: OnceLock<()> = OnceLock::new();
    struct QueryCount;
    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for QueryCount {
        fn on_event(&self, event: &tracing::Event<'_>, ctx: TraceContext<'_, S>) {
            if event.metadata().target() == "sqlx::query"
                && ctx
                    .event_scope(event)
                    .is_some_and(|mut scope| scope.any(|span| span.name() == "head_state_probe"))
            {
                COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    async fn compare(
        state: &db::rpc::RpcState,
        backend_pid: i32,
        chain: u64,
        group: &str,
        tag: &str,
    ) -> Result<()> {
        let current_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&state.pool)
            .await?;
        ensure!(
            current_pid == backend_pid,
            "statement counter connection changed"
        );
        let expected = async {
            Ok((
                state.load(chain, group, tag).await?,
                state.load(chain, group, "cursor").await?,
            ))
        }
        .await;
        COUNT.store(0, Ordering::SeqCst);
        let actual = state
            .head_state(chain, group, tag)
            .instrument(tracing::info_span!("head_state_probe"))
            .await;
        ensure!(
            actual == expected,
            "head_state differs from load for {chain}/{group}/{tag}: {actual:?} vs {expected:?}"
        );
        let current_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&state.pool)
            .await?;
        ensure!(
            current_pid == backend_pid,
            "statement counter connection changed"
        );
        ensure!(
            COUNT.load(Ordering::SeqCst) == 1,
            "head_state must execute exactly one SQL statement on backend {backend_pid}; counted {} for {chain}/{group}/{tag}",
            COUNT.load(Ordering::SeqCst)
        );
        Ok(())
    }
    SUBSCRIBER.get_or_init(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(QueryCount))
            .expect("install head_state statement counter subscriber");
    });
    support::with_database(|database| {
        Box::pin(async move {
            // Count the real WatermarkStore method on one dedicated, preconnected backend.
            // The shared runtime pool can open another connection during head_state and log its
            // after_connect session-budget SQL inside the counting subscriber. This pool has no
            // lifecycle SQL hooks and never expires its sole connection; no other task uses it.
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .max_lifetime(None)
                .idle_timeout(None)
                .connect(&database.app_url)
                .await?;
            let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&pool)
                .await?;
            let state = db::rpc::RpcState {
                pool,
                config_digest: "head-state-test".into(),
            };
            let result = async {
                // Missing state defaults to epoch 0 and unfrozen, including with epoch-0 rows.
                compare(&state, backend_pid, 1, "a", "latest").await?;
                let head = HeadAnchor {
                    number: 100,
                    hash: B256::repeat_byte(1).to_string(),
                    parent_hash: B256::repeat_byte(2).to_string(),
                };
                sqlx::query("INSERT INTO rpc_watermarks(chain_id,group_id,tag,epoch,number,hash,parent_hash,member_id,config_digest) VALUES(1,'a','latest',0,$1,$2,$3,'member','test')")
                    .bind(100_i64)
                    .bind(&head.hash)
                    .bind(&head.parent_hash)
                    .execute(&database.owner_pool)
                    .await?;
                compare(&state, backend_pid, 1, "a", "latest").await?;
                ensure!(state.head_state(1, "a", "latest").await? == (Some(head.clone()), None));
                state.accept(1, "a", "cursor", "member", &head).await?;
                for tag in ["latest", "safe", "finalized", "cursor", "unknown"] {
                    compare(&state, backend_pid, 1, "a", tag).await?;
                }
                ensure!(state.head_state(1, "a", "latest").await? == (Some(head.clone()), Some(head.clone())));
                ensure!(state.head_state(1, "a", "safe").await? == (None, Some(head.clone())));
                for tag in ["safe", "finalized"] {
                    state.accept(1, "a", tag, "member", &head).await?;
                    compare(&state, backend_pid, 1, "a", tag).await?;
                }
                compare(&state, backend_pid, 1, "other", "latest").await?;
                compare(&state, backend_pid, 2, "a", "latest").await?;
                // load does not check awaiting_anchor; preserve its distinction from blocked().
                sqlx::query("UPDATE rpc_chain_state SET awaiting_anchor=true WHERE chain_id=1")
                    .execute(&database.owner_pool)
                    .await?;
                compare(&state, backend_pid, 1, "a", "latest").await?;
                ensure!(state.blocked(1).await == Err(Failure::Unavailable));
                sqlx::query("UPDATE rpc_chain_state SET epoch=1,awaiting_anchor=false WHERE chain_id=1")
                    .execute(&database.owner_pool)
                    .await?;
                compare(&state, backend_pid, 1, "a", "latest").await?;
                ensure!(state.head_state(1, "a", "latest").await? == (None, None));
                let current = HeadAnchor {number: 101, ..head.clone()};
                state.accept(1, "a", "latest", "member", &current).await?;
                compare(&state, backend_pid, 1, "a", "latest").await?;
                ensure!(state.head_state(1, "a", "latest").await? == (Some(current.clone()), None));
                state.accept(1, "a", "cursor", "member", &current).await?;
                compare(&state, backend_pid, 1, "a", "latest").await?;
                ensure!(state.head_state(1, "a", "latest").await? == (Some(current.clone()), Some(current)));
                state.freeze(1).await?;
                for group in ["a", "missing"] {
                    compare(&state, backend_pid, 1, group, "latest").await?;
                    ensure!(state.head_state(1, group, "latest").await == Err(Failure::Fork));
                }
                Ok(())
            }.await;
            state.pool.close().await;
            result
        })
    }).await
}
