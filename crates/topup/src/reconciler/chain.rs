use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use topup_adapters::chain::evm::FinalizedReader;

use super::ReconciliationError;

/// Canonically pinned state reads needed by the remaining reconciliation checks.
#[async_trait]
pub trait ReconciliationChain: Send + Sync {
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
}

/// Endpoint transport retries remain inside Alloy; a failed check waits for the next round.
#[async_trait]
impl ReconciliationChain for FinalizedReader {
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
            .token_balances_pinned(Address::ZERO, &[Address::ZERO], B256::ZERO)
            .await
            .expect_err("closed connections fail the balance read");
        server.abort();

        let message = error.to_string();
        assert!(message.contains("[REDACTED URL]"), "{message}");
        assert!(!message.contains(secret) && !message.contains("api_key"));
    }
}
