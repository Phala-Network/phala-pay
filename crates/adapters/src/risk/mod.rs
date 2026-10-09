//! Risk and compliance data sources.

use alloy_primitives::Address;
use async_trait::async_trait;
use topup_core::screening::SanctionsResult;

/// Injectable sanctions decision source. Verified lists ignore historical block numbers.
#[async_trait]
pub trait SanctionsSource: Send + Sync {
    /// Checks an address; the block number is retained for N-1 screening fixtures.
    async fn sanctions(&self, address: Address, block_number: u64) -> SanctionsResult;
}
