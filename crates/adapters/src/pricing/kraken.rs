//! Kraken PHA/USD, USDC/USD and USDT/USD last-trade adapter.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use topup_core::valuation::{Observation, SourceId};

use crate::redaction::Redacted;

use super::decimal::parse_scaled;
use super::{PriceError, PriceSource, http_client, response_bytes, unix_now};

const ENDPOINT: &str = "https://api.kraken.com/0/public/Ticker";

/// Kraken ticker observation source.
#[derive(Debug)]
pub struct Kraken {
    client: reqwest::Client,
    endpoint: Redacted,
    pair: String,
}

impl Kraken {
    /// Creates a source for one Kraken market pair.
    pub fn new(pair: String) -> Result<Self, PriceError> {
        if !matches!(pair.as_str(), "PHAUSD" | "USDCUSD" | "USDTUSD") {
            return Err(PriceError::MalformedResponse("pair"));
        }
        Ok(Self {
            client: http_client()?,
            endpoint: Redacted::parse(
                &std::env::var("TOPUP_TEST_KRAKEN_ENDPOINT")
                    .unwrap_or_else(|_| ENDPOINT.to_owned()),
            )
            .map_err(|_| PriceError::InvalidUrl)?,
            pair,
        })
    }

    pub(super) fn parse_response(&self, body: &[u8]) -> Result<Observation, PriceError> {
        let mut response: Response =
            serde_json::from_slice(body).map_err(|_| PriceError::MalformedResponse("body"))?;
        if !response.error.is_empty() {
            return Err(PriceError::MalformedResponse("error"));
        }
        if response.result.len() != 1 {
            return Err(PriceError::MalformedResponse("result"));
        }
        let expected = if self.pair == "USDTUSD" {
            "USDTZUSD"
        } else {
            &self.pair
        };
        let ticker = response
            .result
            .remove(expected)
            .ok_or(PriceError::MalformedResponse("result"))?;
        let price = ticker
            .last_trade
            .first()
            .ok_or(PriceError::MalformedResponse("result.c"))?;
        Ok(Observation {
            source: SourceId::new("kraken"),
            price: parse_scaled(price)?,
            observed_at: unix_now()?,
        })
    }
}

#[async_trait]
impl PriceSource for Kraken {
    async fn observe(&self) -> Result<Observation, PriceError> {
        super::admit("kraken").await;
        let response = self
            .client
            .get(self.endpoint.expose().clone())
            .query(&[("pair", self.pair.as_str())])
            .send()
            .await
            .map_err(|error| {
                PriceError::Request(self.endpoint.request_error("kraken fetch", &error))
            })?;
        if !response.status().is_success() {
            return Err(PriceError::HttpStatus(response.status().as_u16()));
        }
        let body = response_bytes(response, &self.endpoint, "kraken body").await?;
        self.parse_response(&body)
    }
}

#[derive(Deserialize)]
struct Response {
    error: Vec<String>,
    result: BTreeMap<String, Ticker>,
}

#[derive(Deserialize)]
struct Ticker {
    #[serde(rename = "c")]
    last_trade: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recorded_response() {
        let source = Kraken::new("USDTUSD".to_owned()).expect("source");
        let observation = source
            .parse_response(include_bytes!("../../tests/fixtures/pricing/kraken.json"))
            .expect("fixture parses");
        assert_eq!(observation.price.value(), 100_010_000);
    }

    #[test]
    fn rejects_recorded_malformed_response() {
        let source = Kraken::new("USDTUSD".to_owned()).expect("source");
        assert!(
            source
                .parse_response(include_bytes!(
                    "../../tests/fixtures/pricing/kraken-malformed.json"
                ))
                .is_err()
        );
    }
}
