//! Reference-rate pricing adapters.

mod decimal;

pub mod binance;
pub mod chainlink;
pub mod kraken;
pub mod snapshot;
pub mod uniswap_v2;

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Response;

use crate::redaction::{Redacted, RedactedTransportError};

pub use topup_core::valuation::Observation;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

async fn response_bytes(
    response: Response,
    endpoint: &Redacted,
    operation: &'static str,
) -> Result<Vec<u8>, PriceError> {
    if response
        .content_length()
        .is_some_and(|size| size > u64::try_from(MAX_RESPONSE_BYTES).unwrap_or(u64::MAX))
    {
        return Err(PriceError::MalformedResponse("body too large"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| PriceError::Request(endpoint.request_error(operation, &error)))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(PriceError::MalformedResponse("body too large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Valuation and independent agreement prices from one fetch and observation timestamp.
#[derive(Clone, Debug)]
pub struct PriceQuote {
    /// Merchant valuation, which may conservatively differ from the current market price.
    pub valuation: Observation,
    /// Current market price used for agreement with an independent company.
    pub agreement_price: topup_core::money::ScaledPrice,
    /// Order-book spread relative to mid, rounded up to whole basis points when available.
    pub spread_bps: Option<u64>,
    /// Latest Unix timestamp at which this observation may be reused; never serialized.
    pub reuse_until: Option<topup_core::valuation::UnixSeconds>,
    /// Sanitized source-specific audit evidence.
    pub evidence: serde_json::Value,
}

/// A timestamped USD price observation provider.
#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Fetches one current price observation.
    async fn observe(&self) -> Result<Observation, PriceError>;
    /// Confirmation fetches fresh evidence, sharing only work completed after arrival.
    async fn quote_fresh(&self) -> Result<PriceQuote, PriceError> {
        self.quote().await
    }
    /// Fresh confirmation evidence completed after one valuation request arrived.
    async fn quote_since(&self, _arrived: tokio::time::Instant) -> Result<PriceQuote, PriceError> {
        self.quote_fresh().await
    }
    /// Sample once for a shared scheduled tick, without consuming quote snapshot capacity.
    async fn sample_since(&self, _arrived: tokio::time::Instant) -> Result<PriceQuote, PriceError> {
        self.sample().await
    }
    /// A scheduled sample is independent of quote capacity.
    async fn sample(&self) -> Result<PriceQuote, PriceError> {
        self.quote_fresh().await
    }
    /// Fetch both valuation and agreement prices together; ordinary spot feeds use one price.
    async fn quote(&self) -> Result<PriceQuote, PriceError> {
        let valuation = self.observe().await?;
        Ok(PriceQuote {
            agreement_price: valuation.price,
            spread_bps: None,
            valuation,
            evidence: serde_json::Value::Null,
            reuse_until: None,
        })
    }
}

/// Price adapter construction, transport, or response failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PriceError {
    /// The UTC daily cap on fresh quote snapshots has been reached.
    #[error("daily price snapshot budget exhausted")]
    SnapshotBudgetExhausted,
    /// The HTTP client could not be configured.
    #[error("price HTTP client configuration failed")]
    ClientConfiguration,
    /// A configured endpoint was not a valid URL.
    #[error("price endpoint URL is invalid")]
    InvalidUrl,
    /// The provider request failed or timed out.
    #[error("{0}")]
    Request(RedactedTransportError),
    /// The provider returned a non-success status.
    #[error("price provider returned HTTP {0}")]
    HttpStatus(u16),
    /// The provider response did not match its documented schema.
    #[error("price provider response has invalid `{0}`")]
    MalformedResponse(&'static str),
    /// A decimal price was invalid, zero, or outside the supported range.
    #[error("price provider returned an invalid price")]
    InvalidPrice,
    /// The observation timestamp was outside the supported Unix range.
    #[error("price provider returned an invalid timestamp")]
    InvalidTimestamp,
    /// The bounded observation deadline expired.
    #[error("price source timeout")]
    Timeout,
    /// Existing RPC group execution failed; details remain sanitized in RPC telemetry.
    #[error("price RPC group unavailable")]
    RpcUnavailable,
    /// Sanitized numeric feed evidence for a failed round or A/B disagreement.
    #[error("on-chain price rejected: {class}")]
    Feed {
        /// Static failure classification, never an upstream message.
        class: &'static str,
        /// Numeric round/value/timestamp metadata, never raw RPC bodies.
        evidence: serde_json::Value,
    },
    /// Feed is outside its pinned freshness window.
    #[error("stale price source")]
    Stale,
    /// Independent RPC groups disagree; failover must not mask this.
    #[error("price RPC groups disagree")]
    Disagreement,
    /// Base sequencer is down or recovering.
    #[error("sequencer unavailable or in grace")]
    SequencerUnavailable,
}

impl From<decimal::DecimalPriceError> for PriceError {
    fn from(_: decimal::DecimalPriceError) -> Self {
        Self::InvalidPrice
    }
}

fn http_client() -> Result<reqwest::Client, PriceError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| PriceError::ClientConfiguration)
}

fn unix_now() -> Result<topup_core::valuation::UnixSeconds, PriceError> {
    let timestamp = chrono::Utc::now().timestamp();
    u64::try_from(timestamp)
        .map(topup_core::valuation::UnixSeconds::new)
        .map_err(|_| PriceError::InvalidTimestamp)
}

// Shared public endpoint quota, including all markets and route versions.
async fn admit(source: &'static str) {
    use std::sync::OnceLock;
    static KRAKEN: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    static BINANCE: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    let slot = if source == "kraken" {
        &KRAKEN
    } else {
        &BINANCE
    };
    let mut next = slot
        .get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now()))
        .lock()
        .await;
    tokio::time::sleep_until(*next).await;
    *next = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(1))
        .unwrap_or_else(tokio::time::Instant::now);
}

/// RPC fixtures shared by adapter and service tests.
#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub mod test_rpc {
    use crate::chain::evm::EvmClient;
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use std::sync::Arc;

    /// Owns a typed RPC client and its disposable server.
    pub struct RpcFixture {
        /// Client connected to the fixture server.
        pub client: Arc<EvmClient>,
        /// Number of actual requests received by the fixture.
        pub sends: Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for RpcFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    /// Serves replies through the same typed endpoint path as production adapters.
    pub async fn rpc(
        id: &str,
        reply: impl Fn(Value) -> Value + Clone + Send + Sync + 'static,
    ) -> RpcFixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received = sends.clone();
        let handler = move |Json(request): Json<Value>| {
            let reply = reply.clone();
            let received = received.clone();
            async move {
                received.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let id = request["id"].clone();
                let result = if request["method"] == "eth_call" {
                    use crate::chain::evm::{
                        SnapshotResult, aggregate3Call, getBlockHashCall,
                        getCurrentBlockTimestampCall,
                    };
                    use alloy_sol_types::SolCall;
                    let input = request["params"][0]["input"]
                        .as_str()
                        .or_else(|| request["params"][0]["data"].as_str())
                        .unwrap();
                    let bytes = hex::decode(input.trim_start_matches("0x")).unwrap();
                    if let Ok(aggregate) = aggregate3Call::abi_decode_validate(&bytes) {
                        assert_eq!(request["params"][1]["requireCanonical"], true);
                        let head = reply(
                            json!({"method":"eth_getBlockByNumber","params":["latest",false]}),
                        );
                        if request["params"][1]["blockHash"] != head["hash"] {
                            return Json(
                                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"block is not canonical"}}),
                            );
                        }
                        let results: Vec<_> = aggregate.calls.into_iter().map(|call| {
                            let output = if getCurrentBlockTimestampCall::abi_decode_validate(&call.callData).is_ok() {
                                let timestamp = u64::from_str_radix(head["timestamp"].as_str().unwrap().trim_start_matches("0x"),16).unwrap();
                                getCurrentBlockTimestampCall::abi_encode_returns(&alloy_primitives::U256::from(timestamp))
                            } else if let Ok(call) = getBlockHashCall::abi_decode_validate(&call.callData) {
                                let header = reply(json!({"method":"eth_getBlockByNumber","params":[format!("0x{:x}",call.blockNumber),false]}));
                                getBlockHashCall::abi_encode_returns(&header["hash"].as_str().unwrap().parse().unwrap())
                            } else {
                                let mut inner = request.clone();
                                inner["params"][0]["to"] = json!(call.target);
                                inner["params"][0]["input"] = json!(format!("0x{}",hex::encode(call.callData)));
                                let result = reply(inner);
                                hex::decode(result.as_str().unwrap().trim_start_matches("0x")).unwrap()
                            };
                            SnapshotResult {success:true,returnData:output.into()}
                        }).collect();
                        json!(format!(
                            "0x{}",
                            hex::encode(aggregate3Call::abi_encode_returns(&results))
                        ))
                    } else {
                        reply(request)
                    }
                } else {
                    reply(request)
                };
                Json(json!({"jsonrpc":"2.0","id":id,"result":result}))
            }
        };
        let app = Router::new().route("/", post(handler));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        RpcFixture {
            client: Arc::new(
                EvmClient::new(&url)
                    .unwrap()
                    .with_provider(id)
                    .with_chain_id(1),
            ),
            sends,
            task,
        }
    }
    /// Serves a pinned pair and ETH round, counting every actual RPC send.
    pub async fn uniswap_v2(
        id: &str,
        anchor: (u64, alloy_primitives::B256),
        state: super::uniswap_v2::PairState,
        eth_answer: i64,
        now: u64,
        sends: Arc<std::sync::atomic::AtomicUsize>,
    ) -> RpcFixture {
        use super::uniswap_v2::*;
        use alloy_primitives::{B256, U256};
        use alloy_sol_types::SolCall;
        let (head, hash) = anchor;
        rpc(id, move |request| {
            sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                        request["params"][1],
                        json!({"blockHash":hash,"requireCanonical":true}),
                        "all pair and ETH/USD values must use the canonical verify pin"
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
                        "0x313ce567" => super::chainlink::decimalsCall::abi_encode_returns(&8),
                        "0xfeaf968c" => super::chainlink::latestRoundDataCall::abi_encode_returns(
                            &super::chainlink::latestRoundDataReturn {
                                roundId: alloy_primitives::Uint::<80, 2>::from(20),
                                answer: alloy_primitives::I256::try_from(
                                    if request["params"][0]["to"].as_str().is_some_and(|to| {
                                        to.eq_ignore_ascii_case(
                                            topup_core::price::feed("ETH_USD", 1).unwrap().address,
                                        )
                                    }) {
                                        eth_answer
                                    } else {
                                        100_000_000
                                    },
                                )
                                .unwrap(),
                                startedAt: U256::from(now),
                                updatedAt: U256::from(now),
                                answeredInRound: alloy_primitives::Uint::<80, 2>::from(20),
                            },
                        ),
                        _ => panic!("unexpected selector"),
                    };
                    json!(format!("0x{}", hex::encode(data)))
                }
                _ => panic!("unexpected RPC"),
            }
        })
        .await
    }
    /// Serves a Chainlink round over the shared typed RPC fixture.
    pub async fn chainlink(
        id: &str,
        answer: i64,
        round_id: u128,
        complete: u128,
        updated: u64,
        malformed: bool,
    ) -> RpcFixture {
        use super::chainlink::{decimalsCall, latestRoundDataCall, latestRoundDataReturn};
        use alloy_sol_types::SolCall;
        rpc(id, move |request: Value| {
            if request["method"] == "eth_getBlockByNumber" {
                let mut head = serde_json::to_value(alloy::rpc::types::Block::<
                    alloy::rpc::types::Transaction,
                >::default())
                .unwrap();
                head["number"] = if request["params"][0] == "latest" {
                    json!("0x64")
                } else {
                    request["params"][0].clone()
                };
                head["hash"] = json!(format!("0x{}", "11".repeat(32)));
                head["parentHash"] = json!(format!("0x{}", "22".repeat(32)));
                head["timestamp"] = json!(format!("0x{updated:x}"));
                return head;
            }
            if request["method"] == "eth_blockNumber" {
                return json!("0x64");
            }
            assert_eq!(
                request["params"][1],
                json!({"blockHash":format!("0x{}","11".repeat(32)),"requireCanonical":true})
            );
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
            json!(result)
        })
        .await
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use axum::{
        Router,
        http::{StatusCode, header::LOCATION},
        routing::get,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Server {
        url: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn server(
        status: StatusCode,
        body: String,
        delay: Duration,
        calls: Arc<AtomicUsize>,
    ) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let location = url.clone();
        let app = Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                let location = location.clone();
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    (status, [(LOCATION, location)], body)
                }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Server { url, task }
    }
    #[tokio::test]
    async fn exchange_http_faults_are_bounded_and_redacted() {
        // The same shared HTTP boundary protects each exchange adapter.
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::FOUND,
            StatusCode::OK,
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let fixture = server(
                status,
                "secret-canary-not-json".into(),
                Duration::ZERO,
                calls.clone(),
            )
            .await;
            let endpoint = Redacted::parse(&fixture.url).unwrap();
            let response = http_client()
                .unwrap()
                .get(endpoint.expose().clone())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "redirect must not be followed"
            );
            if status == StatusCode::OK {
                let body = response_bytes(response, &endpoint, "fixture")
                    .await
                    .unwrap();
                let k = kraken::Kraken::new("PHAUSD".into()).unwrap();
                let b = binance::Binance::new("PHAUSDT".into()).unwrap();
                assert!(k.parse_response(&body).is_err());
                assert!(b.parse_response(&body).is_err());
            }
        }
        let fixture = server(
            StatusCode::OK,
            "x".repeat(MAX_RESPONSE_BYTES + 1),
            Duration::ZERO,
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        let endpoint = Redacted::parse(&fixture.url).unwrap();
        let response = http_client()
            .unwrap()
            .get(endpoint.expose().clone())
            .send()
            .await
            .unwrap();
        assert_eq!(
            response_bytes(response, &endpoint, "fixture")
                .await
                .err()
                .unwrap(),
            PriceError::MalformedResponse("body too large")
        );
        let fixture = server(
            StatusCode::OK,
            "secret-canary".into(),
            Duration::from_secs(1),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        let endpoint = Redacted::parse(&fixture.url).unwrap();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let error = client
            .get(endpoint.expose().clone())
            .send()
            .await
            .unwrap_err();
        let safe = PriceError::Request(endpoint.request_error("fixture", &error)).to_string();
        assert!(!safe.contains("secret-canary"));
        assert!(!safe.contains(&fixture.url));
    }
}
