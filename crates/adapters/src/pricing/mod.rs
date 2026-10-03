//! Reference-rate pricing adapters.

mod decimal;

pub mod binance;
pub mod coinmetrics;
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
    /// No Coin Metrics source is configured for the requested asset identifier.
    #[error("no Coin Metrics source for `{0}`")]
    UnconfiguredAsset(String),
}

impl From<decimal::DecimalPriceError> for PriceError {
    fn from(_: decimal::DecimalPriceError) -> Self {
        Self::InvalidPrice
    }
}

fn http_client() -> Result<reqwest::Client, PriceError> {
    reqwest::Client::builder()
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
