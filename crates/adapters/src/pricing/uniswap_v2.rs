//! Pinned PHA/WETH Uniswap V2 cumulative oracle, with durable service observations.
use super::{
    Observation, PriceError, PriceQuote, PriceSource,
    chainlink::{Chainlink, PriceBlock, agreed_block, confirm_block, validate_round},
    unix_now,
};
use crate::chain::evm::EvmClient;
use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, Bytes, U256, U512, address};
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use topup_core::{
    money::ScaledPrice,
    price::{TwapConfig, feed},
    valuation::{SourceId, UnixSeconds},
};

/// The sole reviewed pool; arbitrary addresses cannot be configured.
pub const PAIR: Address = address!("8867f20c1c63baccec7617626254a060eeb0e61e");
/// Mainnet PHA, with 18 decimals.
pub const PHA: Address = address!("6c5ba91642f10282b576d91922ae6448c9d52f4e");
/// Mainnet WETH, with 18 decimals.
pub const WETH: Address = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
/// Sample no more often than once per minute, shared across quote/credit workers.
pub const SAMPLE_INTERVAL_S: u64 = 60;
sol! {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function price0CumulativeLast() external view returns (uint256);
    function price1CumulativeLast() external view returns (uint256);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
}
/// Complete pair state at one agreed block, before counterfactual accumulation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairState {
    /// First token; determines the cumulative direction.
    pub token0: Address,
    /// Second token.
    pub token1: Address,
    /// First reserve in atomic units.
    pub reserve0: U256,
    /// Second reserve in atomic units.
    pub reserve1: U256,
    /// Pair's uint32 timestamp.
    pub timestamp_last: u32,
    /// Stored price0 cumulative.
    pub cumulative0: U256,
    /// Stored price1 cumulative.
    pub cumulative1: U256,
}
/// Counterfactual service sample; integers serialize losslessly, including wrapping counters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// Numbered block height.
    pub block: u64,
    /// Agreed canonical hash.
    pub hash: B256,
    /// Full block timestamp.
    pub timestamp: u64,
    /// Counterfactual WETH/PHA cumulative in UQ112x112 seconds.
    pub cumulative: U256,
    /// Current WETH/PHA spot in UQ112x112.
    pub spot: U256,
}
/// Persistence boundary implemented by PostgreSQL in the service.
#[async_trait]
pub trait ObservationStore: Send + Sync {
    /// Last accepted observation, checked for canonicality before appending after a restart.
    async fn latest(&self, policy: &TwapConfig) -> Result<Option<Sample>, PriceError>;
    /// Atomically check the last sample's jump, append at most once/minute, and return
    /// chronological history covering window + freshness. Failures never become prices.
    async fn record(&self, sample: &Sample, policy: &TwapConfig)
    -> Result<Vec<Sample>, PriceError>;
}
/// Stable, distinct alert classification with sanitized numerical evidence.
pub fn refusal(class: &'static str, evidence: Value) -> PriceError {
    PriceError::Feed { class, evidence }
}
/// Match UniswapV2OracleLibrary: uint32 elapsed and uint256 cumulative wrap are intentional.
pub fn counterfactual(state: &PairState, block: PriceBlock) -> Result<(Sample, U256), PriceError> {
    let (pha, weth, cumulative) = if state.token0 == PHA && state.token1 == WETH {
        (state.reserve0, state.reserve1, state.cumulative0)
    } else if state.token0 == WETH && state.token1 == PHA {
        (state.reserve1, state.reserve0, state.cumulative1)
    } else {
        return Err(refusal(
            "twap_token_order",
            json!({"token0":state.token0,"token1":state.token1}),
        ));
    };
    let max_reserve = U256::from(1)
        .checked_shl(112)
        .ok_or(PriceError::InvalidPrice)?
        .wrapping_sub(U256::from(1));
    if pha.is_zero() || weth.is_zero() || pha > max_reserve || weth > max_reserve {
        return Err(refusal(
            "twap_liquidity",
            json!({"pha_reserve":pha,"weth_reserve":weth}),
        ));
    }
    let spot = weth
        .checked_shl(112)
        .ok_or(PriceError::InvalidPrice)?
        .checked_div(pha)
        .ok_or(PriceError::InvalidPrice)?;
    let timestamp = u32::try_from(block.timestamp & u64::from(u32::MAX))
        .map_err(|_| PriceError::InvalidTimestamp)?;
    let elapsed = timestamp.wrapping_sub(state.timestamp_last);
    let cumulative = cumulative.wrapping_add(spot.wrapping_mul(U256::from(elapsed)));
    Ok((
        Sample {
            block: block.number,
            hash: block.hash,
            timestamp: block.timestamp,
            cumulative,
            spot,
        },
        weth,
    ))
}
/// Full-width products avoid loss of precision and overflow when converting Q112 to USD.
pub fn usd_price(ratio: U256, eth_usd: ScaledPrice) -> Result<ScaledPrice, PriceError> {
    let value = U512::from(ratio)
        .checked_mul(U512::from(eth_usd.value()))
        .ok_or(PriceError::InvalidPrice)?
        .wrapping_shr(112); // Deliberately truncate fractional USD units; fixed shift < 512.
    let value: u64 = value.try_into().map_err(|_| PriceError::InvalidPrice)?;
    ScaledPrice::new(value, 8).map_err(|_| PriceError::InvalidPrice)
}
/// Lending-style conservative valuation: falls follow spot immediately; rises wait for TWAP.
pub fn valuation_price(
    twap: U256,
    spot: U256,
    eth_usd: ScaledPrice,
) -> Result<ScaledPrice, PriceError> {
    usd_price(twap.min(spot), eth_usd)
}
fn exceeds(value: U256, reference: U256, bps: u16) -> bool {
    let diff = value.abs_diff(reference);
    U512::from(diff).saturating_mul(U512::from(10_000))
        > U512::from(reference).saturating_mul(U512::from(bps))
}
/// Guard current liquidity with the same pinned ETH/USD price used for valuation.
pub fn liquidity(weth: U256, eth_usd: ScaledPrice, policy: &TwapConfig) -> Result<(), PriceError> {
    let value = U512::from(weth)
        .checked_mul(U512::from(eth_usd.value()))
        .ok_or(PriceError::InvalidPrice)?;
    let floor = U512::from(policy.min_weth_reserve_usd)
        .checked_mul(U512::from(100_000_000_u64))
        .and_then(|v| v.checked_mul(U512::from(1_000_000_000_000_000_000_u64)))
        .ok_or(PriceError::InvalidPrice)?;
    if value < floor {
        return Err(refusal(
            "twap_liquidity",
            json!({"weth_reserve":weth,"minimum_usd":policy.min_weth_reserve_usd}),
        ));
    }
    Ok(())
}
/// Check before insertion under the database lock; refused jumps never poison the baseline.
pub fn check_sample(
    previous: &Sample,
    sample: &Sample,
    policy: &TwapConfig,
) -> Result<(), PriceError> {
    if sample.block < previous.block
        || sample.timestamp < previous.timestamp
        || (sample.block == previous.block && sample != previous)
        || (sample.block > previous.block && sample.timestamp == previous.timestamp)
    {
        return Err(refusal(
            "twap_sample_order",
            json!({"previous_block":previous.block,"block":sample.block}),
        ));
    }
    if exceeds(
        sample.spot,
        previous.spot,
        policy.max_sample_jump_bps.value(),
    ) {
        return Err(refusal(
            "twap_sample_jump",
            json!({"previous_spot":previous.spot,"spot":sample.spot,"limit_bps":policy.max_sample_jump_bps}),
        ));
    }
    Ok(())
}
/// Require a continuous persisted window, a recent anchor and current spot/TWAP agreement.
pub fn average<'a>(
    history: &'a [Sample],
    current: &Sample,
    now: u64,
    policy: &TwapConfig,
) -> Result<(U256, &'a Sample, &'a Sample), PriceError> {
    let end = history
        .last()
        .ok_or_else(|| refusal("twap_history", json!({"window_s":policy.window_s})))?;
    if now
        .checked_sub(end.timestamp)
        .is_none_or(|age| age > policy.max_sample_age_s)
    {
        return Err(refusal(
            "twap_stale_sample",
            json!({"timestamp":end.timestamp,"now":now}),
        ));
    }
    let target = end
        .timestamp
        .checked_sub(policy.window_s)
        .ok_or_else(|| refusal("twap_history", json!({"window_s":policy.window_s})))?;
    let start_index = history
        .iter()
        .rposition(|s| s.timestamp <= target)
        .ok_or_else(|| {
            refusal(
                "twap_history",
                json!({"window_s":policy.window_s,"samples":history.len()}),
            )
        })?;
    let start = history
        .get(start_index)
        .ok_or(PriceError::InvalidTimestamp)?;
    if target.saturating_sub(start.timestamp) > policy.max_sample_age_s {
        return Err(refusal(
            "twap_history",
            json!({"anchor":start.timestamp,"target":target}),
        ));
    }
    let window = history
        .get(start_index..)
        .ok_or(PriceError::InvalidTimestamp)?;
    for pair in window.windows(2) {
        if let [a, b] = pair {
            if b.timestamp
                .checked_sub(a.timestamp)
                .is_none_or(|gap| gap == 0 || gap > policy.max_sample_age_s)
            {
                return Err(refusal(
                    "twap_history",
                    json!({"gap_from":a.timestamp,"gap_to":b.timestamp}),
                ));
            }
            check_sample(a, b, policy)?;
        }
    }
    let elapsed = end
        .timestamp
        .checked_sub(start.timestamp)
        .ok_or(PriceError::InvalidTimestamp)?;
    let twap = end
        .cumulative
        .wrapping_sub(start.cumulative)
        .checked_div(U256::from(elapsed))
        .ok_or(PriceError::InvalidPrice)?;
    if twap.is_zero() {
        return Err(PriceError::InvalidPrice);
    }
    if exceeds(current.spot, twap, policy.max_spot_deviation_bps.value()) {
        return Err(refusal(
            "twap_spot_divergence",
            json!({"twap":twap,"spot":current.spot,"limit_bps":policy.max_spot_deviation_bps}),
        ));
    }
    Ok((twap, start, end))
}
/// On-chain composite source using the existing A/B transports and shared durable history.
pub struct UniswapV2 {
    a: Arc<EvmClient>,
    b: Arc<EvmClient>,
    eth: Chainlink,
    policy: TwapConfig,
    store: Arc<dyn ObservationStore>,
}
impl UniswapV2 {
    /// Restrict observation clients to independent Ethereum groups.
    pub fn new(
        a: Arc<EvmClient>,
        b: Arc<EvmClient>,
        policy: TwapConfig,
        store: Arc<dyn ObservationStore>,
    ) -> Result<Self, PriceError> {
        policy.validate().map_err(|_| PriceError::InvalidPrice)?;
        let (ga, gb) = (
            a.group().ok_or(PriceError::RpcUnavailable)?,
            b.group().ok_or(PriceError::RpcUnavailable)?,
        );
        if ga.chain != 1
            || gb.chain != 1
            || ga.id == gb.id
            || ga
                .members
                .iter()
                .any(|a| gb.members.iter().any(|b| a.company == b.company))
        {
            return Err(PriceError::Disagreement);
        }
        let eth = Chainlink::new(
            a.clone(),
            b.clone(),
            feed("ETH_USD", 1).ok_or(PriceError::InvalidPrice)?,
        );
        Ok(Self {
            a,
            b,
            eth,
            policy,
            store,
        })
    }
    async fn read(&self, client: &EvmClient, block: u64) -> Result<PairState, PriceError> {
        async fn call<C: SolCall>(
            client: &EvmClient,
            call: C,
            block: u64,
        ) -> Result<C::Return, PriceError> {
            let data = client
                .call(
                    "Uniswap V2 price",
                    PAIR,
                    Bytes::from(call.abi_encode()),
                    Some(BlockId::number(block)),
                )
                .await
                .map_err(|_| PriceError::RpcUnavailable)?;
            C::abi_decode_returns_validate(&data)
                .map_err(|_| PriceError::MalformedResponse("pair state"))
        }
        let (t0, t1, c0, c1, reserves) = tokio::try_join!(
            call(client, token0Call {}, block),
            call(client, token1Call {}, block),
            call(client, price0CumulativeLastCall {}, block),
            call(client, price1CumulativeLastCall {}, block),
            call(client, getReservesCall {}, block)
        )?;
        Ok(PairState {
            token0: t0,
            token1: t1,
            cumulative0: c0,
            cumulative1: c1,
            reserve0: U256::from(reserves.reserve0),
            reserve1: U256::from(reserves.reserve1),
            timestamp_last: reserves.blockTimestampLast,
        })
    }
    async fn fetch(&self) -> Result<PriceQuote, PriceError> {
        let block = agreed_block(&self.a, &self.b).await?;
        let now = unix_now()?.value();
        if now
            .checked_sub(block.timestamp)
            .is_none_or(|age| age > self.policy.max_sample_age_s)
        {
            return Err(refusal(
                "twap_stale_sample",
                json!({"block":block.number,"timestamp":block.timestamp,"now":now}),
            ));
        }
        let (a, b, round) = tokio::try_join!(
            self.read(&self.a, block.number),
            self.read(&self.b, block.number),
            self.eth.round_at(block.number)
        )?;
        if a != b {
            return Err(PriceError::Disagreement);
        }
        if confirm_block(&self.a, &self.b, block.number).await? != block {
            return Err(PriceError::Disagreement);
        }
        let eth = validate_round(
            &round,
            feed("ETH_USD", 1).ok_or(PriceError::InvalidPrice)?,
            now,
        )?;
        let (sample, weth) = counterfactual(&a, block)?;
        liquidity(weth, eth.price, &self.policy)?;
        if let Some(previous) = self.store.latest(&self.policy).await? {
            check_sample(&previous, &sample, &self.policy)?;
            if confirm_block(&self.a, &self.b, previous.block).await?.hash != previous.hash {
                return Err(refusal(
                    "twap_reorg",
                    json!({"block":previous.block,"stored_hash":previous.hash}),
                ));
            }
        }
        let mut history = self.store.record(&sample, &self.policy).await?;
        // Quotes between scheduled samples still end their TWAP at the very same pinned
        // block as ETH/USD. The current endpoint need not create an extra database row.
        if history
            .last()
            .is_some_and(|end| end.timestamp < sample.timestamp)
        {
            history.push(sample.clone());
        }
        let (ratio, start, end) = average(&history, &sample, now, &self.policy)?;
        // Detect reorged persisted endpoints, including after a restart. Never silently rebase.
        for sample in [start, end] {
            if confirm_block(&self.a, &self.b, sample.block).await?.hash != sample.hash {
                return Err(refusal(
                    "twap_reorg",
                    json!({"block":sample.block,"stored_hash":sample.hash}),
                ));
            }
        }
        let price = valuation_price(ratio, sample.spot, eth.price)?;
        let agreement_price = usd_price(sample.spot, eth.price)?;
        let twap_price = usd_price(ratio, eth.price)?;
        let evidence = json!({"pair":PAIR,"token0":a.token0,"token1":a.token1,"block":block.number,"block_hash":block.hash,"window_start_block":start.block,"window_end_block":end.block,"window_s":end.timestamp.saturating_sub(start.timestamp),"cumulative_start":start.cumulative,"cumulative_end":end.cumulative,"twap_q112":ratio,"spot_q112":sample.spot,"weth_reserve":weth,"eth_usd_scaled":eth.price.value().to_string(),"eth_round":round.id.to_string(),"eth_updated_at":round.updated_at,"policy":self.policy,
            "valuation_rule":"min(twap,spot)",
            "twap_usd_scaled":twap_price.value().to_string(),
            "spot_usd_scaled":agreement_price.value().to_string(),
            "valuation_usd_scaled":price.value().to_string()});
        Ok(PriceQuote {
            valuation: Observation {
                source: SourceId::new("uniswap_v2_twap"),
                price,
                observed_at: UnixSeconds::new(end.timestamp),
            },
            agreement_price,
            evidence,
        })
    }
}
#[async_trait]
impl PriceSource for UniswapV2 {
    async fn observe(&self) -> Result<Observation, PriceError> {
        self.fetch().await.map(|q| q.valuation)
    }
    async fn quote(&self) -> Result<PriceQuote, PriceError> {
        self.fetch().await
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_rpc::{self, RpcFixture};
    use super::*;
    use topup_core::money::Bps;
    fn q(value: u64) -> U256 {
        U256::from(value) << 112_usize
    }
    fn sample(timestamp: u64, spot: U256, cumulative: U256) -> Sample {
        Sample {
            block: timestamp,
            hash: B256::repeat_byte(1),
            timestamp,
            spot,
            cumulative,
        }
    }
    fn class<T: std::fmt::Debug>(result: Result<T, PriceError>, expected: &str) {
        assert!(
            matches!(result, Err(PriceError::Feed { class, .. }) if class == expected),
            "expected {expected}"
        );
    }
    fn history(spot: U256) -> Vec<Sample> {
        (0..=30_u64)
            .map(|i| sample(10_000 + i * 60, spot, spot * U256::from(i * 60)))
            .collect()
    }
    #[test]
    fn recorded_mainnet_cumulative_math_and_token_order() {
        // Actual mainnet blocks 26,120,450 and 26,120,610, not synthetic reserves.
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/uniswap-v2-pha-mainnet.json"
        ))
        .unwrap();
        let mut samples = Vec::new();
        for raw in fixture["samples"].as_array().unwrap() {
            let bytes = |name: &str| {
                hex::decode(raw[name].as_str().unwrap().strip_prefix("0x").unwrap()).unwrap()
            };
            let reserves =
                getReservesCall::abi_decode_returns_validate(&bytes("getReserves")).unwrap();
            for name in ["pha_decimals", "weth_decimals"] {
                assert_eq!(
                    super::super::chainlink::decimalsCall::abi_decode_returns_validate(&bytes(
                        name
                    ))
                    .unwrap(),
                    18
                );
            }
            let state = PairState {
                token0: token0Call::abi_decode_returns_validate(&bytes("token0")).unwrap(),
                token1: token1Call::abi_decode_returns_validate(&bytes("token1")).unwrap(),
                reserve0: U256::from(reserves.reserve0),
                reserve1: U256::from(reserves.reserve1),
                timestamp_last: reserves.blockTimestampLast,
                cumulative0: price0CumulativeLastCall::abi_decode_returns_validate(&bytes(
                    "price0CumulativeLast",
                ))
                .unwrap(),
                cumulative1: price1CumulativeLastCall::abi_decode_returns_validate(&bytes(
                    "price1CumulativeLast",
                ))
                .unwrap(),
            };
            assert_eq!(state.token0, PHA);
            assert_eq!(state.token1, WETH);
            let block = PriceBlock {
                number: raw["block_number"].as_u64().unwrap(),
                hash: raw["block_hash"].as_str().unwrap().parse().unwrap(),
                timestamp: raw["timestamp"].as_u64().unwrap(),
            };
            let (s, _) = counterfactual(&state, block).unwrap();
            assert_eq!(
                s.cumulative,
                raw["expected_counterfactual0"]
                    .as_str()
                    .unwrap()
                    .parse::<U256>()
                    .unwrap()
            );
            // The opposite layout must use price1, without averaging the reciprocal price0.
            let reversed = PairState {
                token0: WETH,
                token1: PHA,
                reserve0: state.reserve1,
                reserve1: state.reserve0,
                cumulative0: state.cumulative1,
                cumulative1: state.cumulative0,
                ..state.clone()
            };
            assert_eq!(counterfactual(&reversed, block).unwrap().0, s);
            let other_direction = PairState {
                cumulative0: state.cumulative1,
                reserve0: state.reserve1,
                reserve1: state.reserve0,
                ..state.clone()
            };
            assert_eq!(
                counterfactual(&other_direction, block)
                    .unwrap()
                    .0
                    .cumulative,
                raw["expected_counterfactual1"]
                    .as_str()
                    .unwrap()
                    .parse::<U256>()
                    .unwrap()
            );
            class(
                counterfactual(
                    &PairState {
                        token1: Address::ZERO,
                        ..state
                    },
                    block,
                ),
                "twap_token_order",
            );
            samples.push(s);
        }
        let [start, end] = samples.as_slice() else {
            panic!("fixture must have two blocks")
        };
        let ratio = end.cumulative.wrapping_sub(start.cumulative)
            / U256::from(end.timestamp - start.timestamp);
        assert_eq!(
            ratio,
            fixture["expected"]["twap_q112"]
                .as_str()
                .unwrap()
                .parse::<U256>()
                .unwrap()
        );
        let eth_round = super::super::chainlink::latestRoundDataCall::abi_decode_returns_validate(
            &hex::decode(
                fixture["samples"][1]["eth_usd_round"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("0x")
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let eth = ScaledPrice::new(eth_round.answer.try_into().unwrap(), 8).unwrap();
        assert_eq!(
            usd_price(ratio, eth).unwrap().value().to_string(),
            fixture["expected"]["pha_usd_scaled"].as_str().unwrap()
        );
    }
    #[test]
    fn uint32_timestamp_and_uint256_cumulative_wrap() {
        let state = PairState {
            token0: PHA,
            token1: WETH,
            reserve0: U256::from(1),
            reserve1: U256::from(2),
            timestamp_last: u32::MAX - 9,
            cumulative0: U256::MAX - q(2) * U256::from(5),
            cumulative1: U256::ZERO,
        };
        let block = PriceBlock {
            number: 10,
            hash: B256::ZERO,
            timestamp: u64::from(u32::MAX) + 11,
        };
        let s = counterfactual(&state, block).unwrap().0;
        assert_eq!(
            s.cumulative.wrapping_sub(state.cumulative0),
            q(2) * U256::from(20)
        );
        let mut h = history(q(2));
        for s in &mut h {
            s.cumulative = s.cumulative.wrapping_add(U256::MAX - q(2) * U256::from(20));
        }
        assert_eq!(
            average(&h, h.last().unwrap(), 11800, &TwapConfig::default())
                .unwrap()
                .0,
            q(2)
        );
    }
    #[test]
    fn window_freshness_liquidity_and_single_sample_jump() {
        let policy = TwapConfig::default();
        let h = history(q(2));
        let end = h.last().unwrap();
        assert_eq!(average(&h, end, 11800, &policy).unwrap().0, q(2));
        class(average(&h[1..], end, 11800, &policy), "twap_history");
        class(average(&h, end, 11981, &policy), "twap_stale_sample");
        let mut missing = h.clone();
        missing.drain(10..14);
        class(average(&missing, end, 11800, &policy), "twap_history");
        let eth = ScaledPrice::new(200_000_000_000, 8).unwrap();
        assert!(liquidity(U256::from(50_000_000_000_000_000_000_u128), eth, &policy).is_ok());
        class(
            liquidity(U256::from(49_999_999_999_999_999_999_u128), eth, &policy),
            "twap_liquidity",
        );
        let jump = Sample {
            spot: q(3),
            ..end.clone()
        };
        class(check_sample(&h[29], &jump, &policy), "twap_sample_jump");
        let spike = Sample {
            spot: q(3),
            ..sample(11860, q(3), end.cumulative)
        };
        class(average(&h, &spike, 11800, &policy), "twap_spot_divergence");
    }
    #[test]
    fn manipulation_spike_and_gradual_sustained_skew() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/uniswap-v2-manipulation.json"
        ))
        .unwrap();
        let policy = TwapConfig::default();
        let h = history(q(fixture["baseline_spot_units"].as_u64().unwrap()));
        let end = h.last().unwrap();
        // A one-block 50% pump cannot move the thirty-minute average into agreement.
        let spike_spot = q(fixture["spike_spot_units"].as_u64().unwrap());
        let duration = fixture["spike_duration_s"].as_u64().unwrap();
        let spike = sample(
            end.timestamp + duration,
            spike_spot,
            end.cumulative + spike_spot * U256::from(duration),
        );
        class(check_sample(end, &spike, &policy), "twap_sample_jump");
        class(average(&h, &spike, 11812, &policy), "twap_spot_divergence");
        // A 2%/minute ramp evades the jump limit but still exceeds spot/TWAP divergence.
        let step = fixture["sample_interval_s"].as_u64().unwrap();
        let mut ramp = vec![sample(
            10000,
            q(fixture["baseline_spot_units"].as_u64().unwrap()),
            U256::ZERO,
        )];
        for value in fixture["ramp_spot_units"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
        {
            let previous = ramp.last().unwrap();
            let spot = q(value.as_u64().unwrap());
            let next = sample(
                previous.timestamp + step,
                spot,
                previous.cumulative + previous.spot * U256::from(step),
            );
            assert!(check_sample(previous, &next, &policy).is_ok());
            ramp.push(next);
        }
        class(
            average(&ramp, ramp.last().unwrap(), 11800, &policy),
            "twap_spot_divergence",
        );
        // Sustained flat skew can pass these guard rails: the independent Kraken check and
        // credit exposure cap remain mandatory; TWAP alone is not a security guarantee.
        let skew = history(q(fixture["sustained_skew_spot_units"].as_u64().unwrap()));
        assert_eq!(
            average(&skew, skew.last().unwrap(), 11800, &policy)
                .unwrap()
                .0,
            q(150)
        );
        let strict = TwapConfig {
            max_spot_deviation_bps: Bps::new(1).unwrap(),
            ..policy
        };
        assert!(average(&skew, skew.last().unwrap(), 11800, &strict).is_ok());
    }
    struct Memory(tokio::sync::Mutex<Vec<Sample>>);
    #[async_trait]
    impl ObservationStore for Memory {
        async fn latest(&self, _: &TwapConfig) -> Result<Option<Sample>, PriceError> {
            Ok(self.0.lock().await.last().cloned())
        }
        async fn record(&self, s: &Sample, p: &TwapConfig) -> Result<Vec<Sample>, PriceError> {
            let mut h = self.0.lock().await;
            if let Some(prev) = h.last() {
                check_sample(prev, s, p)?;
            }
            if h.last()
                .is_none_or(|prev| s.timestamp.saturating_sub(prev.timestamp) >= 60)
            {
                h.push(s.clone());
            }
            Ok(h.clone())
        }
    }
    async fn rpc(
        id: &str,
        head: u64,
        hash: B256,
        state: PairState,
        eth_answer: i64,
        now: u64,
    ) -> RpcFixture {
        test_rpc::rpc(id, move |request| {
            match request["method"].as_str().unwrap() {
                "eth_blockNumber" => json!(format!("0x{head:x}")),
                "eth_getBlockByNumber" => {
                    let mut h = serde_json::to_value(alloy::rpc::types::Block::<
                        alloy::rpc::types::Transaction,
                    >::default())
                    .unwrap();
                    h["number"] = if request["params"][0] == "latest" {
                        json!(format!("0x{head:x}"))
                    } else {
                        request["params"][0].clone()
                    };
                    h["hash"] = json!(hash);
                    h["parentHash"] = json!(B256::repeat_byte(3));
                    h["timestamp"] = json!(format!("0x{now:x}"));
                    h
                }
                "eth_call" => {
                    assert_eq!(
                        request["params"][1], "0x62",
                        "all pair AND ETH/USD values must be pinned to min(100,102)-2"
                    );
                    let input = request["params"][0]["input"]
                        .as_str()
                        .or_else(|| request["params"][0]["data"].as_str())
                        .unwrap();
                    let data = match &input[..10] {
                        "0x0dfe1681" => token0Call::abi_encode_returns(&state.token0),
                        "0xd21220a7" => token1Call::abi_encode_returns(&state.token1),
                        "0x5909c0d5" => {
                            price0CumulativeLastCall::abi_encode_returns(&state.cumulative0)
                        }
                        "0x5a3d5493" => {
                            price1CumulativeLastCall::abi_encode_returns(&state.cumulative1)
                        }
                        "0x0902f1ac" => getReservesCall::abi_encode_returns(&getReservesReturn {
                            reserve0: alloy_primitives::Uint::<112, 2>::from(
                                state.reserve0.to::<u128>(),
                            ),
                            reserve1: alloy_primitives::Uint::<112, 2>::from(
                                state.reserve1.to::<u128>(),
                            ),
                            blockTimestampLast: state.timestamp_last,
                        }),
                        "0x313ce567" => {
                            super::super::chainlink::decimalsCall::abi_encode_returns(&8)
                        }
                        "0xfeaf968c" => {
                            super::super::chainlink::latestRoundDataCall::abi_encode_returns(
                                &super::super::chainlink::latestRoundDataReturn {
                                    roundId: alloy_primitives::Uint::<80, 2>::from(20),
                                    answer: alloy_primitives::I256::try_from(eth_answer).unwrap(),
                                    startedAt: U256::from(now),
                                    updatedAt: U256::from(now),
                                    answeredInRound: alloy_primitives::Uint::<80, 2>::from(20),
                                },
                            )
                        }
                        _ => panic!("unexpected selector"),
                    };
                    json!(format!("0x{}", hex::encode(data)))
                }
                _ => panic!("unexpected RPC"),
            }
        })
        .await
    }
    #[tokio::test]
    async fn quote_between_samples_ends_at_the_eth_usd_block_without_an_extra_row() {
        let now = unix_now().unwrap().value();
        let hash = B256::repeat_byte(1);
        let state = PairState {
            token0: PHA,
            token1: WETH,
            reserve0: U256::from(100_000_000_000_000_000_000_000_u128),
            reserve1: U256::from(100_000_000_000_000_000_000_u128),
            timestamp_last: u32::try_from(now).unwrap(),
            cumulative0: U256::ZERO,
            cumulative1: U256::ZERO,
        };
        let ratio = counterfactual(
            &state,
            PriceBlock {
                number: 98,
                hash,
                timestamp: now,
            },
        )
        .unwrap()
        .0
        .spot;
        for percent in [98_u64, 100, 102] {
            let state = PairState {
                reserve1: state.reserve1 * U256::from(percent) / U256::from(100),
                cumulative0: ratio * U256::from(1800),
                ..state.clone()
            };
            let spot = counterfactual(
                &state,
                PriceBlock {
                    number: 98,
                    hash,
                    timestamp: now,
                },
            )
            .unwrap()
            .0
            .spot;
            let mut h: Vec<Sample> = (0..30_u64)
                .map(|i| Sample {
                    block: 67 + i,
                    hash,
                    timestamp: now - 1800 + i * 60,
                    spot: ratio,
                    cumulative: ratio * U256::from(i * 60),
                })
                .collect();
            h.push(Sample {
                block: 97,
                hash,
                timestamp: now - 30,
                spot: ratio,
                cumulative: ratio * U256::from(1770),
            });
            let store = Arc::new(Memory(tokio::sync::Mutex::new(h)));
            let a = rpc(
                "twap-success-a",
                100,
                hash,
                state.clone(),
                200_000_000_000,
                now,
            )
            .await;
            let b = rpc("twap-success-b", 102, hash, state, 200_000_000_000, now).await;
            let reader = UniswapV2::new(
                a.client.clone(),
                b.client.clone(),
                TwapConfig::default(),
                store.clone(),
            )
            .unwrap();
            let quote = reader.quote().await.unwrap();
            let o = quote.valuation;
            let evidence = quote.evidence;
            let eth = ScaledPrice::new(200_000_000_000, 8).unwrap();
            assert_eq!(quote.agreement_price, usd_price(spot, eth).unwrap());
            assert_eq!(o.source.as_str(), "uniswap_v2_twap");
            assert_eq!(o.observed_at.value(), now);
            assert_eq!(o.price, valuation_price(ratio, spot, eth).unwrap());
            assert_eq!(evidence["window_end_block"], 98);
            assert_eq!(
                store.0.lock().await.len(),
                31,
                "no extra persisted quote sample"
            );
        }
    }
    #[tokio::test]
    async fn pinned_ab_pair_header_and_eth_disagreement_fail_closed() {
        let now = unix_now().unwrap().value();
        let hash = B256::repeat_byte(1);
        let state = PairState {
            token0: PHA,
            token1: WETH,
            reserve0: U256::from(100_000_000_000_000_000_000_000_u128),
            reserve1: U256::from(100_000_000_000_000_000_000_u128),
            timestamp_last: u32::try_from(now).unwrap(),
            cumulative0: U256::ZERO,
            cumulative1: U256::ZERO,
        };
        let a = rpc("twap-a", 100, hash, state.clone(), 200_000_000_000, now).await;
        for fault in ["none", "header", "cumulative", "reserves", "tokens", "eth"] {
            let mut bstate = state.clone();
            let mut bhash = hash;
            let mut eth = 200_000_000_000;
            match fault {
                "header" => bhash = B256::repeat_byte(2),
                "cumulative" => bstate.cumulative0 = U256::from(1),
                "reserves" => bstate.reserve1 += U256::from(1),
                "tokens" => bstate.token0 = WETH,
                "eth" => eth += 1,
                _ => {}
            }
            let b = rpc("twap-b", 102, bhash, bstate, eth, now).await;
            let store = Arc::new(Memory(tokio::sync::Mutex::new(Vec::new())));
            let reader = UniswapV2::new(
                a.client.clone(),
                b.client.clone(),
                TwapConfig::default(),
                store.clone(),
            )
            .unwrap();
            let result = reader.quote().await;
            if fault == "none" {
                class(result, "twap_history");
                assert_eq!(store.0.lock().await.len(), 1);
            } else {
                assert!(
                    matches!(
                        result,
                        Err(PriceError::Disagreement)
                            | Err(PriceError::Feed {
                                class: "divergent",
                                ..
                            })
                    ),
                    "{fault}: {result:?}"
                );
                assert!(store.0.lock().await.is_empty());
            }
        }
    }
}
