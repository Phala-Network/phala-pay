use std::future::Future;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use tokio::time::timeout;
use topup_adapters::chain::evm::{ChainError, ChainReader, FinalizedReader, TransferLog};

use crate::chain_retry::backing_off;

use super::ReconciliationError;

/// Bounded chain reads needed by reconciliation.
#[async_trait]
pub trait ReconciliationChain: Send + Sync {
    /// Returns the reviewed finalized block.
    async fn finalized_head(&self) -> Result<u64, ReconciliationError>;

    /// Returns finalized ERC-20 transfers to tracked recipients.
    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ReconciliationError>;

    /// Token state at one EIP-1898 canonical hash; no numeric or latest fallback.
    async fn token_balances_pinned(
        &self,
        _token: Address,
        _addresses: &[Address],
        _hash: B256,
    ) -> Result<Vec<U256>, ReconciliationError> {
        Err(ReconciliationError::Invariant(
            "canonical balance pin unavailable",
        ))
    }
    /// Returns token balances at one block in bounded JSON-RPC batches.
    async fn token_balances(
        &self,
        token: Address,
        addresses: &[Address],
        block: u64,
    ) -> Result<Vec<U256>, ReconciliationError>;

    /// Returns factory-derived forwarder addresses of `treasury` in bounded batches.
    async fn factory_addresses(
        &self,
        factory: Address,
        treasury: Address,
        salts: &[B256],
    ) -> Result<Vec<Address>, ReconciliationError>;
}

/// Production reconciliation reads: finalized logs through the reconciler's own reader, and
/// balances and derived addresses through the shared client.
///
/// Every read backs off and retries while the provider refuses it for now (see
/// [`backing_off`]); each attempt keeps the client's request timeout. A round's reads are
/// sequential, but every task of the service starts at once and shares provider A, so the first
/// round after a restart meets that startup burst.
#[async_trait]
impl ReconciliationChain for FinalizedReader {
    async fn finalized_head(&self) -> Result<u64, ReconciliationError> {
        let head = backing_off(|| {
            bounded(
                self,
                "finalized head fetch",
                ChainReader::finalized_head(self),
            )
        })
        .await?;
        Ok(head.number)
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ReconciliationError> {
        Ok(backing_off(|| {
            bounded(
                self,
                "transfer log fetch",
                ChainReader::transfer_logs_to(self, addresses, from_block, to_block),
            )
        })
        .await?)
    }

    async fn token_balances_pinned(
        &self,
        token: Address,
        addresses: &[Address],
        hash: B256,
    ) -> Result<Vec<U256>, ReconciliationError> {
        Ok(self
            .client()
            .token_balances(token, addresses, alloy::eips::BlockId::hash_canonical(hash))
            .await?)
    }
    async fn token_balances(
        &self,
        token: Address,
        addresses: &[Address],
        block: u64,
    ) -> Result<Vec<U256>, ReconciliationError> {
        Ok(backing_off(|| {
            self.client()
                .token_balances(token, addresses, BlockNumberOrTag::Number(block))
        })
        .await?)
    }

    async fn factory_addresses(
        &self,
        factory: Address,
        treasury: Address,
        salts: &[B256],
    ) -> Result<Vec<Address>, ReconciliationError> {
        Ok(backing_off(|| self.client().factory_addresses(factory, treasury, salts)).await?)
    }
}

/// Bounds one reader call as a whole by the request timeout: a window's logs and the reads that
/// complete them, each of which the reader also bounds.
async fn bounded<T>(
    reader: &FinalizedReader,
    operation: &'static str,
    read: impl Future<Output = Result<T, ChainError>>,
) -> Result<T, ChainError> {
    timeout(reader.client().request_timeout(), read)
        .await
        .map_err(|_| ChainError::Transport(reader.client().endpoint().timeout_error(operation)))?
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use topup_adapters::chain::evm::EvmClient;

    use super::*;

    #[tokio::test]
    async fn balance_read_transport_failure_does_not_format_the_provider_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let secret = "rpc-secret-token";
        let chain = FinalizedReader::new(Arc::new(
            EvmClient::with_timeout(
                &format!("http://user:{secret}@{address}/rpc?api_key={secret}"),
                Duration::from_secs(5),
            )
            .expect("production adapter accepts URL"),
        ));

        let error = chain
            .token_balances(Address::ZERO, &[Address::ZERO], 1)
            .await
            .expect_err("closed connections fail the balance read");
        server.abort();

        let message = error.to_string();
        assert!(message.contains("[REDACTED URL]"), "{message}");
        assert!(!message.contains(secret) && !message.contains("api_key"));
    }
}
