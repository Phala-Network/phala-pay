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
        let (a, b) = tokio::join!(read.header(previous.number), verify.header(previous.number));
        if [a.as_ref().ok(), b.as_ref().ok()]
            .iter()
            .flatten()
            .any(|header| header.0 != previous.hash)
        {
            chain_reads::freeze(pool, chain, "finalized_checkpoint_conflict").await?;
            tracing::error!(
                tags.alert = "TopupFinalizedCheckpointConflict",
                chain_id = chain,
                "stored finalized checkpoint hash changed; chain frozen"
            );
            return Err(crate::scanner::ScannerError::Disagreement);
        }
        a?;
        b?;
    }
    let ((a, ah), (b, bh)) = tokio::try_join!(read.finalized_header(), verify.finalized_header())?;
    // A height difference is normal endpoint lag. Only the ahead endpoint needs a pin read.
    let number = a.number.min(b.number);
    let (a, b) = if a.number > number {
        (read.header(number).await?, (bh, b.time))
    } else if b.number > number {
        ((ah, a.time), verify.header(number).await?)
    } else {
        ((ah, a.time), (bh, b.time))
    };
    if a != b {
        chain_reads::freeze(pool, chain, "finalized_checkpoint_conflict").await?;
        tracing::error!(
            tags.alert = "TopupFinalizedCheckpointConflict",
            chain_id = chain,
            "stored finalized checkpoint hash changed; chain frozen"
        );
        return Err(crate::scanner::ScannerError::Disagreement);
    }
    let boundary = Boundary {
        number,
        hash: a.0,
        time: a.1,
    };
    chain_reads::advance_checkpoint(pool, chain, boundary).await?;
    chain_reads::checkpoint(pool, chain)
        .await?
        .ok_or(crate::scanner::ScannerError::Disagreement)
}
