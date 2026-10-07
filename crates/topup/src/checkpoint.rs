//! Dual-source finalized checkpoints; a disagreement can never lower or replace durable evidence.
use crate::db::chain_reads::{self, Boundary};
use sqlx::PgPool;
use topup_adapters::chain::evm::ChainReader;

/// Advance only after both endpoints still attest the previous checkpoint and the new boundary.
pub async fn advance<R: ChainReader, V: ChainReader>(
    pool: &PgPool,
    chain: u64,
    read: &R,
    verify: &V,
) -> Result<Boundary, crate::scanner::ScannerError> {
    if let Some(previous) = chain_reads::checkpoint(pool, chain).await? {
        let (a, b) =
            tokio::try_join!(read.header(previous.number), verify.header(previous.number))?;
        if a.0 != previous.hash || b.0 != previous.hash {
            chain_reads::freeze(pool, chain, "finalized_checkpoint_conflict").await?;
            tracing::error!(
                tags.alert = "TopupFinalizedCheckpointConflict",
                chain_id = chain,
                "stored finalized checkpoint hash changed; chain frozen"
            );
            return Err(crate::scanner::ScannerError::Disagreement);
        }
    }
    let ((a, ah), b) = tokio::try_join!(read.finalized_header(), verify.finalized_head())?;
    if a.number > b.number {
        return Err(crate::scanner::ScannerError::Disagreement);
    }
    let bh = verify.header(a.number).await?;
    if ah != bh.0 || bh.1 != a.time {
        chain_reads::freeze(pool, chain, "finalized_checkpoint_conflict").await?;
        tracing::error!(
            tags.alert = "TopupFinalizedCheckpointConflict",
            chain_id = chain,
            "stored finalized checkpoint hash changed; chain frozen"
        );
        return Err(crate::scanner::ScannerError::Disagreement);
    }
    let boundary = Boundary {
        number: a.number,
        hash: ah,
        time: a.time,
    };
    chain_reads::advance_checkpoint(pool, chain, boundary).await?;
    chain_reads::checkpoint(pool, chain)
        .await?
        .ok_or(crate::scanner::ScannerError::Disagreement)
}
