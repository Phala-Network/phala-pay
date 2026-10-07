//! Read-only N-1 safety checks. N never writes the legacy RPC framework's tables.
use super::types::to_i64;
use sqlx::PgPool;

/// Refuse upgrade until N-1 has resolved frozen/anchor/recovery state on configured chains.
pub async fn check_start(pool: &PgPool, chains: &[u64]) -> Result<(), sqlx::Error> {
    let chains = chains
        .iter()
        .map(|chain| to_i64(*chain, "RPC chain"))
        .collect::<Result<Vec<_>, _>>()?;
    let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rpc_chain_state WHERE chain_id=ANY($1) AND (frozen OR awaiting_anchor OR recovery_pending))")
        .bind(chains).fetch_one(pool).await?;
    if blocked {
        return Err(sqlx::Error::Protocol("resolve legacy frozen, awaiting_anchor or recovery_pending state with N-1 before starting N".into()));
    }
    Ok(())
}
/// Serialize ledger writes with chain freezes using the new coverage row.
pub async fn guard_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
) -> Result<(), sqlx::Error> {
    lock_reconciliation_in(tx, &format!("chain:{chain}")).await?;
    let chain = to_i64(chain, "chain id")?;
    sqlx::query("SELECT chain_id FROM chain_coverage WHERE chain_id=$1 FOR UPDATE")
        .bind(chain)
        .fetch_optional(&mut **tx)
        .await?;
    let blocked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reconciliation_blocks WHERE scope='chain' AND chain_id=$1)",
    )
    .bind(chain)
    .fetch_one(&mut **tx)
    .await?;
    if blocked {
        return Err(sqlx::Error::Protocol(
            "chain reconciliation block is active".into(),
        ));
    }
    Ok(())
}
/// Serialize freeze, lift and ledger admission without widening append-only table privileges.
pub async fn lock_reconciliation_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 704202))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Read N-1's sweep epoch without changing its state.
pub async fn sweep_epoch(pool: &PgPool, chain: u64) -> Result<i64, sqlx::Error> {
    Ok(
        sqlx::query_scalar("SELECT epoch FROM rpc_chain_state WHERE chain_id=$1")
            .bind(to_i64(chain, "chain id")?)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0),
    )
}
