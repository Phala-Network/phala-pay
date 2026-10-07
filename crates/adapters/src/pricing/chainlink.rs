//! Chainlink AggregatorV3 reader over the existing bounded A/B RPC clients.
use super::{PriceError, PriceQuote, PriceSource, unix_now};
use crate::chain::evm::EvmClient;
use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;
use std::sync::Arc;
use topup_core::{
    money::{PRICE_SCALE, ScaledPrice},
    price::Feed,
    valuation::{Observation, SourceId, UnixSeconds},
};

sol! {
    function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
    function decimals() external view returns (uint8);
}
/// Validated round evidence, safe to audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Round {
    /// Round identifier.
    pub id: u128,
    /// Signed answer.
    pub answer: alloy_primitives::I256,
    /// Recovery/round start.
    pub started_at: u64,
    /// Feed update timestamp.
    pub updated_at: u64,
    /// Completed round identifier.
    pub answered_in_round: u128,
}
/// A single block agreed by both groups, including timestamp and canonical hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceBlock {
    /// Ethereum block height.
    pub number: u64,
    /// Canonical block hash.
    pub hash: B256,
    /// Full Unix timestamp (the pair uses its low 32 bits).
    pub timestamp: u64,
}
/// Shared independent group clients; no alternate direct HTTP endpoint.
pub struct Chainlink {
    snapshots: Arc<super::snapshot::Snapshots>,
    feed: Feed,
}
impl Chainlink {
    /// Constructs a single-feed snapshot reader, primarily for isolated adapter tests.
    pub fn new(a: Arc<EvmClient>, b: Arc<EvmClient>, feed: Feed) -> Result<Self, PriceError> {
        let calls = calls(feed)?;
        Ok(Self {
            snapshots: Arc::new(super::snapshot::Snapshots::new(
                feed.chain_id,
                a,
                b,
                calls,
                None,
            )),
            feed,
        })
    }
    /// Uses the complete shared snapshot of all configured feeds on this chain.
    pub fn with_snapshots(snapshots: Arc<super::snapshot::Snapshots>, feed: Feed) -> Self {
        Self { snapshots, feed }
    }
    /// Current quote round, pinned by verify and independently derived on both endpoints.
    pub async fn round(&self) -> Result<Round, PriceError> {
        let snapshot = self
            .snapshots
            .fetch(super::snapshot::SnapshotUse::Quote)
            .await?;
        round(&snapshot, self.feed)
    }
    async fn quote_for(
        &self,
        purpose: super::snapshot::SnapshotUse,
        arrived: tokio::time::Instant,
    ) -> Result<PriceQuote, PriceError> {
        let snapshot = self.snapshots.fetch_since(purpose, arrived).await?;
        let r = round(&snapshot, self.feed)?;
        let o = validate_round(&r, self.feed, unix_now()?.value()).map_err(|error| {
            PriceError::Feed {
                class: if error == PriceError::Stale {
                    "stale"
                } else {
                    "malformed"
                },
                evidence: round_evidence(&r, self.feed),
            }
        })?;
        Ok(PriceQuote {
            agreement_price: o.price,
            spread_bps: None,
            valuation: o,
            evidence: round_evidence(&r, self.feed),
            reuse_until: Some(UnixSeconds::new(
                r.updated_at
                    .saturating_add(self.feed.heartbeat_s)
                    .saturating_add(self.feed.margin_s),
            )),
        })
    }
    /// Halts on sequencer down, malformed uptime or recovery grace.
    pub async fn sequencer(&self, grace_s: u64) -> Result<serde_json::Value, PriceError> {
        self.sequencer_since(
            grace_s,
            super::snapshot::SnapshotUse::Quote,
            tokio::time::Instant::now(),
        )
        .await
    }
    /// Screen sequencer uptime from the same snapshot as this request's other price reads.
    pub async fn sequencer_since(
        &self,
        grace_s: u64,
        purpose: super::snapshot::SnapshotUse,
        arrived: tokio::time::Instant,
    ) -> Result<serde_json::Value, PriceError> {
        let snapshot = self.snapshots.fetch_since(purpose, arrived).await?;
        let round = round(&snapshot, self.feed)?;
        let evidence =
            serde_json::json!({"round":round_evidence(&round, self.feed),"grace_s":grace_s});
        validate_sequencer(&round, unix_now()?.value(), grace_s).map_err(|_| PriceError::Feed {
            class: "sequencer_down",
            evidence: evidence.clone(),
        })?;
        Ok(evidence)
    }
}
fn round_evidence(r: &Round, feed: Feed) -> serde_json::Value {
    serde_json::json!({"feed":feed.name,"chain_id":feed.chain_id,"address":feed.address,"decimals":feed.decimals,"round":r.id.to_string(),"answer":r.answer.to_string(),"answered_in_round":r.answered_in_round.to_string(),"started_at":r.started_at,"updated_at":r.updated_at,"heartbeat_s":feed.heartbeat_s,"margin_s":feed.margin_s,"age_s":unix_now().ok().and_then(|now|now.value().checked_sub(r.updated_at))})
}

/// Pure completeness/freshness validation; preserves the feed update time.
pub fn validate_round(round: &Round, feed: Feed, now: u64) -> Result<Observation, PriceError> {
    if round.id == 0
        || round.answered_in_round < round.id
        || round.updated_at == 0
        || round.started_at > round.updated_at
        || round.answer <= alloy_primitives::I256::ZERO
    {
        return Err(PriceError::MalformedResponse("incomplete round"));
    }
    let age = now
        .checked_sub(round.updated_at)
        .ok_or(PriceError::InvalidTimestamp)?;
    if age > feed.heartbeat_s.saturating_add(feed.margin_s) {
        return Err(PriceError::Stale);
    }
    let value: u64 = round
        .answer
        .try_into()
        .map_err(|_| PriceError::InvalidPrice)?;
    let price = ScaledPrice::new(value, PRICE_SCALE).map_err(|_| PriceError::InvalidPrice)?;
    Ok(Observation {
        source: SourceId::new("chainlink"),
        price,
        observed_at: UnixSeconds::new(round.updated_at),
    })
}
/// Uptime feeds use zero for up; they must never pass price-answer validation.
pub fn validate_sequencer(round: &Round, now: u64, grace_s: u64) -> Result<(), PriceError> {
    if round.id == 0
        || round.answered_in_round < round.id
        || round.answer != alloy_primitives::I256::ZERO
        || round.started_at == 0
        || round.updated_at < round.started_at
        || round.updated_at > now
        || now
            .checked_sub(round.started_at)
            .is_none_or(|age| age <= grace_s)
    {
        return Err(PriceError::SequencerUnavailable);
    }
    Ok(())
}
/// The feed's two reads are combined with all other configured price state in one multicall.
pub fn calls(feed: Feed) -> Result<Vec<(Address, Bytes)>, PriceError> {
    let address = feed.address.parse().map_err(|_| PriceError::InvalidPrice)?;
    Ok(vec![
        (address, latestRoundDataCall {}.abi_encode().into()),
        (address, decimalsCall {}.abi_encode().into()),
    ])
}
/// Decode a complete independently agreed round and its configured precision.
pub fn round(snapshot: &super::snapshot::Snapshot, feed: Feed) -> Result<Round, PriceError> {
    let address = feed.address.parse().map_err(|_| PriceError::InvalidPrice)?;
    let precision =
        decimalsCall::abi_decode_returns_validate(snapshot.get(address, decimalsCall {})?)
            .map_err(|_| PriceError::MalformedResponse("decimals"))?;
    if precision != feed.decimals {
        return Err(PriceError::MalformedResponse("decimals"));
    }
    let r = latestRoundDataCall::abi_decode_returns_validate(
        snapshot.get(address, latestRoundDataCall {})?,
    )
    .map_err(|_| PriceError::MalformedResponse("round"))?;
    Ok(Round {
        id: r.roundId.to::<u128>(),
        answer: r.answer,
        started_at: r
            .startedAt
            .try_into()
            .map_err(|_| PriceError::InvalidTimestamp)?,
        updated_at: r
            .updatedAt
            .try_into()
            .map_err(|_| PriceError::InvalidTimestamp)?,
        answered_in_round: r.answeredInRound.to::<u128>(),
    })
}
#[async_trait]
impl PriceSource for Chainlink {
    async fn quote_since(&self, arrived: tokio::time::Instant) -> Result<PriceQuote, PriceError> {
        self.quote_for(super::snapshot::SnapshotUse::Confirm, arrived)
            .await
    }
    async fn observe(&self) -> Result<Observation, PriceError> {
        self.quote().await.map(|q| q.valuation)
    }
    async fn quote(&self) -> Result<PriceQuote, PriceError> {
        self.quote_for(
            super::snapshot::SnapshotUse::Quote,
            tokio::time::Instant::now(),
        )
        .await
    }
    async fn quote_fresh(&self) -> Result<PriceQuote, PriceError> {
        self.quote_for(
            super::snapshot::SnapshotUse::Confirm,
            tokio::time::Instant::now(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_rpc::chainlink as rpc;
    use super::*;
    #[tokio::test]
    async fn round_send_count_with_stationary_head() {
        use std::sync::atomic::Ordering;
        let now = unix_now().unwrap().value();
        let a = rpc("round-count-a", 100_000_000, 20, 20, now, false).await;
        let b = rpc("round-count-b", 100_000_000, 20, 20, now, false).await;
        let reader = Chainlink::new(
            a.client.clone(),
            b.client.clone(),
            topup_core::price::feed("USDC_USD", 1).unwrap(),
        )
        .unwrap();
        reader.round().await.unwrap();
        let sends = a.sends.load(Ordering::SeqCst) + b.sends.load(Ordering::SeqCst);
        println!("Chainlink::round stationary-head sends: {sends}");
        assert_eq!(sends, 4);
    }
    fn round() -> Round {
        Round {
            id: 20,
            answered_in_round: 20,
            answer: alloy_primitives::I256::try_from(100_000_000_i64).unwrap(),
            started_at: 100,
            updated_at: 100,
        }
    }
    #[test]
    fn pinned_heartbeat_margin_bound_and_incomplete_rounds() {
        let feed = topup_core::price::feed("USDC_USD", 1).unwrap();
        let r = round();
        for name in ["USDC_USD", "USDT_USD"] {
            let feed = topup_core::price::feed(name, 1).unwrap();
            for publication_delay in [36, 61, 600] {
                assert!(
                    validate_round(&r, feed, 100 + feed.heartbeat_s + publication_delay).is_ok()
                );
            }
            assert_eq!(
                validate_round(&r, feed, 100 + feed.heartbeat_s + 601),
                Err(PriceError::Stale)
            );
        }
        assert!(validate_round(&r, feed, 100 + feed.heartbeat_s + feed.margin_s).is_ok());
        assert_eq!(
            validate_round(&r, feed, 101 + feed.heartbeat_s + feed.margin_s),
            Err(PriceError::Stale)
        );
        for bad in [
            Round {
                answered_in_round: 19,
                ..r.clone()
            },
            Round {
                updated_at: 0,
                ..r.clone()
            },
            Round {
                answer: alloy_primitives::I256::ZERO,
                ..r.clone()
            },
            Round {
                answer: alloy_primitives::I256::MINUS_ONE,
                ..r.clone()
            },
            Round {
                updated_at: 201,
                ..r.clone()
            },
        ] {
            assert!(validate_round(&bad, feed, 200).is_err());
        }
    }
    #[test]
    fn sequencer_down_and_exact_grace_boundary_halt() {
        let up = Round {
            answer: alloy_primitives::I256::ZERO,
            ..round()
        };
        assert!(validate_sequencer(&up, 3700, 3600).is_err());
        assert!(validate_sequencer(&up, 3701, 3600).is_ok());
        let down = Round {
            answer: alloy_primitives::I256::try_from(1_i64).unwrap(),
            ..up.clone()
        };
        assert!(validate_sequencer(&down, 10000, 3600).is_err());
        assert!(
            validate_sequencer(
                &Round {
                    started_at: 0,
                    ..up
                },
                10000,
                3600
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn typed_group_round_agreement_and_fault_injection() {
        let now = unix_now().unwrap().value();
        let a = rpc("price-test-a", 100_000_000, 20, 20, now, false).await;
        for (answer, round_id, complete, updated, malformed, healthy) in [
            (100_000_000, 20, 20, now, false, true),
            (101_000_000, 20, 20, now, false, false),
            (100_000_000, 21, 21, now, false, false),
            (100_000_000, 20, 19, now, false, false),
            (100_000_000, 20, 20, now, true, false),
        ] {
            let b = rpc(
                "price-test-b",
                answer,
                round_id,
                complete,
                updated,
                malformed,
            )
            .await;
            let reader = Chainlink::new(
                a.client.clone(),
                b.client.clone(),
                topup_core::price::feed("USDC_USD", 1).unwrap(),
            )
            .unwrap();
            let observation = reader.quote().await;
            assert_eq!(observation.is_ok(), healthy, "{observation:?}");
        }
        let old = now.saturating_sub(90000);
        let a = rpc("price-test-old-a", 100_000_000, 20, 20, old, false).await;
        let b = rpc("price-test-old-b", 100_000_000, 20, 20, old, false).await;
        let reader = Chainlink::new(
            a.client.clone(),
            b.client.clone(),
            topup_core::price::feed("USDC_USD", 1).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            reader.quote().await,
            Err(PriceError::Feed { class: "stale", .. })
        ));
    }
}
