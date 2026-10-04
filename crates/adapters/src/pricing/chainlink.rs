//! Chainlink AggregatorV3 reader over the existing bounded A/B RPC clients.
use super::{PriceError, PriceSource, unix_now};
use crate::chain::evm::EvmClient;
use alloy_eips::BlockId;
use alloy_primitives::{Address, Bytes};
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
/// Shared independent group clients; no alternate direct HTTP endpoint.
pub struct Chainlink {
    a: Arc<EvmClient>,
    b: Arc<EvmClient>,
    feed: Feed,
}
impl Chainlink {
    /// Constructs a reader for pinned metadata.
    pub fn new(a: Arc<EvmClient>, b: Arc<EvmClient>, feed: Feed) -> Self {
        Self { a, b, feed }
    }
    async fn read(&self, client: &EvmClient) -> Result<Round, PriceError> {
        let address: Address = self
            .feed
            .address
            .parse()
            .map_err(|_| PriceError::InvalidPrice)?;
        let (data, precision) = tokio::try_join!(
            client.call(
                "price round",
                address,
                Bytes::from(latestRoundDataCall {}.abi_encode()),
                Some(BlockId::latest())
            ),
            client.call(
                "price decimals",
                address,
                Bytes::from(decimalsCall {}.abi_encode()),
                Some(BlockId::latest())
            )
        )
        .map_err(|_| PriceError::RpcUnavailable)?;
        let precision = decimalsCall::abi_decode_returns_validate(&precision)
            .map_err(|_| PriceError::MalformedResponse("decimals"))?;
        if precision != self.feed.decimals {
            return Err(PriceError::MalformedResponse("decimals"));
        }
        let r = latestRoundDataCall::abi_decode_returns_validate(&data)
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
    /// Read both groups and reject different rounds, values or timestamps.
    pub async fn round(&self) -> Result<Round, PriceError> {
        let (a, b) = tokio::try_join!(self.read(&self.a), self.read(&self.b))?;
        if a != b {
            return Err(PriceError::Feed {
                class: "divergent",
                evidence: serde_json::json!({"a":round_evidence(&a,self.feed),"b":round_evidence(&b,self.feed)}),
            });
        }
        Ok(a)
    }
    /// Halts on sequencer down, malformed uptime or recovery grace.
    pub async fn sequencer(&self, grace_s: u64) -> Result<serde_json::Value, PriceError> {
        let round = self.round().await?;
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
#[async_trait]
impl PriceSource for Chainlink {
    async fn observe(&self) -> Result<Observation, PriceError> {
        validate_round(&self.round().await?, self.feed, unix_now()?.value())
    }
    async fn evidence(&self) -> Result<(Observation, serde_json::Value), PriceError> {
        let r = self.round().await?;
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
        Ok((o, round_evidence(&r, self.feed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    struct RpcFixture {
        client: Arc<EvmClient>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for RpcFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn rpc(
        id: &str,
        answer: i64,
        round_id: u128,
        complete: u128,
        updated: u64,
        malformed: bool,
    ) -> RpcFixture {
        use crate::{
            chain::evm::group::{
                GroupPolicy, Member, RpcGroup,
                budget::{BudgetSpec, Budgets},
            },
            redaction::Redacted,
        };
        use axum::{Json, Router, routing::post};
        use serde_json::{Value, json};
        use std::collections::BTreeMap;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<Value>| async move {
                if request["method"] == "eth_getBlockByNumber" {
                    let mut head = serde_json::to_value(alloy::rpc::types::Block::<
                        alloy::rpc::types::Transaction,
                    >::default())
                    .unwrap();
                    head["number"] = json!("0x64");
                    head["hash"] = json!(format!("0x{}", "11".repeat(32)));
                    head["parentHash"] = json!(format!("0x{}", "22".repeat(32)));
                    return Json(json!({"jsonrpc":"2.0","id":request["id"],"result":head}));
                }
                assert_eq!(request["params"][1], "latest");
                let data = request["params"][0]["input"]
                    .as_str()
                    .or_else(|| request["params"][0]["data"].as_str())
                    .unwrap_or("");
                let result = if malformed {
                    "0x1234".to_owned()
                } else if data.starts_with("0x313ce567") {
                    format!("0x{}", hex::encode(decimalsCall::abi_encode_returns(&8u8)))
                } else {
                    let r = latestRoundDataReturn {
                        roundId: alloy_primitives::Uint::<80, 2>::from(round_id),
                        answer: alloy_primitives::I256::try_from(answer).unwrap(),
                        startedAt: alloy_primitives::U256::from(updated),
                        updatedAt: alloy_primitives::U256::from(updated),
                        answeredInRound: alloy_primitives::Uint::<80, 2>::from(complete),
                    };
                    format!(
                        "0x{}",
                        hex::encode(latestRoundDataCall::abi_encode_returns(&r))
                    )
                };
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let budgets = Arc::new(
            Budgets::new(&BTreeMap::from([
                (
                    "account".into(),
                    BudgetSpec {
                        requests_per_second: 100,
                        burst: 100,
                    },
                ),
                (
                    "key".into(),
                    BudgetSpec {
                        requests_per_second: 100,
                        burst: 100,
                    },
                ),
            ]))
            .unwrap(),
        );
        let group = RpcGroup::new(
            id.into(),
            1,
            GroupPolicy::default(),
            vec![Member {
                id: id.into(),
                company: id.into(),
                endpoint: Redacted::parse(&url).unwrap(),
                account: "account".into(),
                key: "key".into(),
                priority: 0,
                weight: 1,
            }],
            budgets,
        )
        .unwrap();
        group.verified(0, true);
        RpcFixture {
            client: Arc::new(EvmClient::from_group(group, None).unwrap()),
            task,
        }
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
            );
            let observation = reader.evidence().await;
            assert_eq!(observation.is_ok(), healthy, "{observation:?}");
        }
        let old = now.saturating_sub(90000);
        let a = rpc("price-test-old-a", 100_000_000, 20, 20, old, false).await;
        let b = rpc("price-test-old-b", 100_000_000, 20, 20, old, false).await;
        let reader = Chainlink::new(
            a.client.clone(),
            b.client.clone(),
            topup_core::price::feed("USDC_USD", 1).unwrap(),
        );
        assert!(matches!(
            reader.evidence().await,
            Err(PriceError::Feed { class: "stale", .. })
        ));
        b.client.group().unwrap().verified(0, false);
        assert!(reader.observe().await.is_err());
    }
}
