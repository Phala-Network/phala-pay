//! One dual-source Multicall3 snapshot of every configured state read on a price chain.
use super::{PriceError, chainlink::PriceBlock};
use crate::chain::evm::{
    EvmClient, MULTICALL3, aggregate3Call, getBlockHashCall, getCurrentBlockTimestampCall,
};
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Snapshot purpose: only quote fetches consume the quote snapshot cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotUse {
    /// A fresh quote may reuse a snapshot for at most twelve seconds.
    Quote,
    /// Confirmation requires a snapshot completed after the request arrived.
    Confirm,
    /// The independent staging sampler does not consume quote capacity.
    Sample,
}
/// Persistence boundary: a failed claim never permits a fresh quote price.
#[async_trait]
pub trait SnapshotBudget: Send + Sync {
    /// Atomically claim one fresh quote snapshot for this price chain and UTC day.
    async fn claim(&self, chain_id: u64) -> Result<bool, PriceError>;
}
/// Independently agreed bytes and the canonical pin that produced them.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Verify's current-state pin.
    pub block: PriceBlock,
    values: BTreeMap<(Address, Vec<u8>), Bytes>,
}
impl Snapshot {
    /// Retrieve one configured call without issuing another RPC request.
    pub fn get<C: SolCall>(&self, target: Address, call: C) -> Result<&Bytes, PriceError> {
        self.values
            .get(&(target, call.abi_encode()))
            .ok_or(PriceError::MalformedResponse("snapshot call missing"))
    }
}
/// Shared snapshot state of exactly one price chain and independent endpoint pair.
pub struct Snapshots {
    chain_id: u64,
    read: Arc<EvmClient>,
    verify: Arc<EvmClient>,
    calls: Vec<(Address, Bytes)>,
    budget: Option<Arc<dyn SnapshotBudget>>,
    latest: Mutex<Option<(Instant, Arc<Snapshot>)>>,
    guards: Vec<(
        topup_core::price::TwapConfig,
        Arc<dyn super::uniswap_v2::ObservationStore>,
    )>,
}
impl Snapshots {
    /// Calls are collected once from the validated route set; none are chosen by client input.
    pub fn new(
        chain_id: u64,
        read: Arc<EvmClient>,
        verify: Arc<EvmClient>,
        mut calls: Vec<(Address, Bytes)>,
        budget: Option<Arc<dyn SnapshotBudget>>,
    ) -> Self {
        calls.sort();
        calls.dedup();
        calls.push((
            MULTICALL3,
            getCurrentBlockTimestampCall {}.abi_encode().into(),
        ));
        Self {
            chain_id,
            read,
            verify,
            calls,
            budget,
            latest: Mutex::new(None),
            guards: Vec::new(),
        }
    }
    /// Add the persisted TWAP baselines to the same Multicall3 snapshot.
    pub fn with_twap_guard(
        mut self,
        policy: topup_core::price::TwapConfig,
        store: Arc<dyn super::uniswap_v2::ObservationStore>,
    ) -> Self {
        self.guards.push((policy, store));
        self
    }
    /// Re-pin after any error on the next caller attempt; errors never become cached prices.
    pub async fn fetch(&self, purpose: SnapshotUse) -> Result<Arc<Snapshot>, PriceError> {
        self.fetch_since(purpose, Instant::now()).await
    }
    /// One valuation request shares a snapshot across sequential feeds and its sequencer gate.
    pub async fn fetch_since(
        &self,
        purpose: SnapshotUse,
        arrived: Instant,
    ) -> Result<Arc<Snapshot>, PriceError> {
        if !self.read.contract_ready() || !self.verify.contract_ready() {
            return Err(PriceError::RpcUnavailable);
        }
        let mut latest = self.latest.lock().await;
        if let Some((completed, snapshot)) = &*latest {
            let usable = if purpose == SnapshotUse::Quote {
                completed.elapsed() < Duration::from_secs(12)
            } else {
                *completed >= arrived
            };
            if usable {
                return Ok(Arc::clone(snapshot));
            }
        }
        if purpose == SnapshotUse::Quote
            && let Some(budget) = &self.budget
            && !budget.claim(self.chain_id).await?
        {
            return Err(PriceError::SnapshotBudgetExhausted);
        }
        // Verify supplies both head and header. Read's only request is the multicall.
        let (number, hash, timestamp) = self
            .verify
            .current_pin()
            .await
            .map_err(|_| PriceError::RpcUnavailable)?;
        let mut calls = self.calls.clone();
        for (policy, store) in &self.guards {
            for previous in store.history(policy).await? {
                if previous.block < number {
                    calls.push((
                        MULTICALL3,
                        getBlockHashCall {
                            blockNumber: alloy_primitives::U256::from(previous.block),
                        }
                        .abi_encode()
                        .into(),
                    ));
                }
            }
        }
        calls.sort();
        calls.dedup();
        let (read, verify) = tokio::try_join!(
            self.read.multicall(calls.clone(), hash),
            self.verify.multicall(calls.clone(), hash)
        )
        .map_err(|_| PriceError::RpcUnavailable)?;
        if read != verify {
            self.read.disagreement("eth_call");
            self.verify.disagreement("eth_call");
            return Err(PriceError::Disagreement);
        }
        let values = aggregate3Call::abi_decode_returns_validate(&read)
            .map_err(|_| PriceError::MalformedResponse("snapshot multicall"))?;
        if values.len() != calls.len() || values.iter().any(|v| !v.success) {
            return Err(PriceError::MalformedResponse("incomplete snapshot"));
        }
        let values: BTreeMap<_, _> = calls
            .iter()
            .zip(values)
            .map(|((target, data), value)| ((*target, data.to_vec()), value.returnData))
            .collect();
        let clock = values
            .get(&(MULTICALL3, getCurrentBlockTimestampCall {}.abi_encode()))
            .ok_or(PriceError::InvalidTimestamp)?;
        let clock = getCurrentBlockTimestampCall::abi_decode_returns_validate(clock)
            .map_err(|_| PriceError::InvalidTimestamp)?;
        if clock != alloy_primitives::U256::from(timestamp) {
            return Err(PriceError::Disagreement);
        }
        let snapshot = Arc::new(Snapshot {
            block: PriceBlock {
                number,
                hash,
                timestamp,
            },
            values,
        });
        *latest = Some((Instant::now(), Arc::clone(&snapshot)));
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_rpc::chainlink;
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Budget {
        available: AtomicBool,
        claims: AtomicUsize,
    }
    #[async_trait]
    impl SnapshotBudget for Budget {
        async fn claim(&self, _: u64) -> Result<bool, PriceError> {
            self.claims.fetch_add(1, Ordering::SeqCst);
            Ok(self.available.load(Ordering::SeqCst))
        }
    }
    #[tokio::test]
    async fn quote_cap_never_serves_a_stale_snapshot_and_confirm_is_fresh() {
        let a = chainlink("budget-read", 100_000_000, 20, 20, 1000, false).await;
        let b = chainlink("budget-verify", 100_000_000, 20, 20, 1000, false).await;
        let budget = Arc::new(Budget {
            available: AtomicBool::new(true),
            claims: AtomicUsize::new(0),
        });
        let snapshots = Snapshots::new(
            1,
            a.client.clone(),
            b.client.clone(),
            Vec::new(),
            Some(budget.clone()),
        );
        let first = snapshots.fetch(SnapshotUse::Quote).await.unwrap();
        assert_eq!(a.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            b.sends.load(Ordering::SeqCst),
            3,
            "one snapshot costs 3 × 80 = 240 verify credits"
        );
        budget.available.store(false, Ordering::SeqCst);
        let cached = snapshots.fetch(SnapshotUse::Quote).await.unwrap();
        assert!(Arc::ptr_eq(&first, &cached));
        assert_eq!(budget.claims.load(Ordering::SeqCst), 1);
        snapshots.latest.lock().await.as_mut().unwrap().0 =
            Instant::now() - Duration::from_secs(13);
        assert!(matches!(
            snapshots.fetch(SnapshotUse::Quote).await,
            Err(PriceError::SnapshotBudgetExhausted)
        ));
        assert_eq!(
            a.sends.load(Ordering::SeqCst),
            1,
            "exhausted quote performs no RPC"
        );
        let fresh = snapshots.fetch(SnapshotUse::Confirm).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert_eq!(a.sends.load(Ordering::SeqCst), 2);
        assert_eq!(b.sends.load(Ordering::SeqCst), 6);
        assert_eq!(
            budget.claims.load(Ordering::SeqCst),
            2,
            "confirm consumes no quote budget"
        );
    }

    #[tokio::test]
    async fn noncanonical_state_waits_and_the_next_attempt_uses_a_fresh_verify_pin() {
        use crate::pricing::test_rpc;
        use serde_json::json;
        let changed = Arc::new(AtomicBool::new(false));
        let node = |read: bool| {
            let changed = changed.clone();
            test_rpc::rpc(
                if read {
                    "canonical-read"
                } else {
                    "canonical-verify"
                },
                move |request| {
                    if request["method"] == "eth_blockNumber" {
                        return json!("0x64");
                    }
                    assert_eq!(request["method"], "eth_getBlockByNumber");
                    let mut header = serde_json::to_value(alloy::rpc::types::Block::<
                        alloy::rpc::types::Transaction,
                    >::default())
                    .unwrap();
                    header["number"] = json!("0x62");
                    header["timestamp"] = json!("0x3e8");
                    header["hash"] = json!(alloy_primitives::B256::repeat_byte(
                        if read || changed.load(Ordering::SeqCst) {
                            22
                        } else {
                            11
                        }
                    ));
                    header
                },
            )
        };
        let read = node(true).await;
        let verify = node(false).await;
        let snapshots = Snapshots::new(
            1,
            read.client.clone(),
            verify.client.clone(),
            Vec::new(),
            None,
        );
        assert!(matches!(
            snapshots.fetch(SnapshotUse::Confirm).await,
            Err(PriceError::RpcUnavailable)
        ));
        changed.store(true, Ordering::SeqCst);
        let agreed = snapshots.fetch(SnapshotUse::Confirm).await.unwrap();
        assert_eq!(agreed.block.hash, alloy_primitives::B256::repeat_byte(22));
        assert_eq!(
            verify.sends.load(Ordering::SeqCst),
            6,
            "failed pin is never cached"
        );
    }
}
