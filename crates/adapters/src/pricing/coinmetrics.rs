//! Coin Metrics `ReferenceRateUSD` adapter.

use std::fmt::{self, Debug, Formatter};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use topup_core::valuation::{Observation, SourceId, UnixSeconds};

use crate::redaction::Redacted;

use super::decimal::parse_scaled;
use super::{PriceError, PriceSource, http_client, response_bytes};

const ENDPOINT: &str = "https://community-api.coinmetrics.io/v4/timeseries/asset-metrics";
/// Reference-rate metric required by the valuation policy (docs/architecture.md §8).
const METRIC: &str = "ReferenceRateUSD";
/// Sampling frequency of [`METRIC`].
const FREQUENCY: &str = "1m";

/// Coin Metrics `ReferenceRateUSD` one-minute observation source.
pub struct CoinMetrics {
    client: reqwest::Client,
    endpoint: Redacted,
    asset: String,
}

impl Debug for CoinMetrics {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoinMetrics")
            .field("endpoint", &self.endpoint)
            .field("asset", &self.asset)
            .finish_non_exhaustive()
    }
}

impl CoinMetrics {
    /// Creates a source using the keyless community endpoint.
    pub fn new(asset: String) -> Result<Self, PriceError> {
        Self::with_endpoint(asset, ENDPOINT)
    }

    fn with_endpoint(asset: String, endpoint: &str) -> Result<Self, PriceError> {
        let endpoint = Redacted::parse(endpoint).map_err(|_| PriceError::InvalidUrl)?;
        Ok(Self {
            client: http_client()?,
            endpoint,
            asset,
        })
    }

    fn parse_response(&self, body: &[u8]) -> Result<Observation, PriceError> {
        let response: Response =
            serde_json::from_slice(body).map_err(|_| PriceError::MalformedResponse("body"))?;
        let row = response
            .data
            .into_iter()
            .next()
            .ok_or(PriceError::MalformedResponse("data"))?;
        if row.asset != self.asset {
            return Err(PriceError::MalformedResponse("data.asset"));
        }
        let raw_price = row
            .metrics
            .get(METRIC)
            .and_then(Value::as_str)
            .ok_or(PriceError::MalformedResponse("data.metric"))?;
        let observed_at = DateTime::parse_from_rfc3339(&row.time)
            .map_err(|_| PriceError::MalformedResponse("data.time"))?
            .with_timezone(&Utc)
            .timestamp();
        Ok(Observation {
            source: SourceId::new("coinmetrics"),
            price: parse_scaled(raw_price)?,
            observed_at: UnixSeconds::new(
                u64::try_from(observed_at).map_err(|_| PriceError::InvalidTimestamp)?,
            ),
        })
    }
}

#[async_trait]
impl PriceSource for CoinMetrics {
    async fn observe(&self) -> Result<Observation, PriceError> {
        let request = self.client.get(self.endpoint.expose().clone()).query(&[
            ("assets", self.asset.as_str()),
            ("metrics", METRIC),
            ("frequency", FREQUENCY),
            ("limit_per_asset", "1"),
            ("paging_from", "end"),
        ]);
        let response = request.send().await.map_err(|error| {
            PriceError::Request(self.endpoint.request_error("coinmetrics fetch", &error))
        })?;
        if !response.status().is_success() {
            return Err(PriceError::HttpStatus(response.status().as_u16()));
        }
        let body = response_bytes(response, &self.endpoint, "coinmetrics body").await?;
        self.parse_response(&body)
    }
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    asset: String,
    time: String,
    #[serde(flatten)]
    metrics: serde_json::Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recorded_response() {
        let source = CoinMetrics::new("pha".to_owned()).expect("source");
        let observation = source
            .parse_response(include_bytes!(
                "../../tests/fixtures/pricing/coinmetrics.json"
            ))
            .expect("fixture parses");
        assert_eq!(observation.price.value(), 12_345_678);
        assert_eq!(observation.observed_at.value(), 1_790_035_200);
    }

    #[test]
    fn rejects_recorded_malformed_response() {
        let source = CoinMetrics::new("pha".to_owned()).expect("source");
        assert!(
            source
                .parse_response(include_bytes!(
                    "../../tests/fixtures/pricing/coinmetrics-malformed.json"
                ))
                .is_err()
        );
    }
}
