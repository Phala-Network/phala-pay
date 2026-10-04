//! Reference-rate pricing adapters.

mod decimal;

pub mod binance;
pub mod chainlink;
pub mod kraken;

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

/// A timestamped USD price observation provider.
#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Fetches one current price observation.
    async fn observe(&self) -> Result<Observation, PriceError>;
    /// Observation plus sanitized source-specific audit evidence.
    async fn evidence(&self) -> Result<(Observation, serde_json::Value), PriceError> {
        self.observe().await.map(|o| (o, serde_json::Value::Null))
    }
}

/// Price adapter construction, transport, or response failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PriceError {
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
    #[error("Chainlink feed rejected: {class}")]
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
