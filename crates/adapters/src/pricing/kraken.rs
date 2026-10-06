//! Kraken PHA/USD, USDC/USD and USDT/USD order-book mid adapter.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use topup_core::money::ScaledPrice;
use topup_core::valuation::{Observation, SourceId};

use crate::redaction::Redacted;

use super::decimal::parse_scaled;
use super::{PriceError, PriceQuote, PriceSource, http_client, response_bytes, unix_now};

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

    pub(super) fn parse_response(&self, body: &[u8]) -> Result<PriceQuote, PriceError> {
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
        let ask = ticker
            .ask
            .first()
            .ok_or(PriceError::MalformedResponse("result.a"))?;
        let bid = ticker
            .bid
            .first()
            .ok_or(PriceError::MalformedResponse("result.b"))?;
        let ask_scaled = parse_scaled(ask)
            .map_err(|_| PriceError::MalformedResponse("result.a"))?
            .value();
        let bid_scaled = parse_scaled(bid)
            .map_err(|_| PriceError::MalformedResponse("result.b"))?
            .value();
        let spread = ask_scaled
            .checked_sub(bid_scaled)
            .ok_or(PriceError::MalformedResponse("crossed book"))?;
        let mid = u128::from(bid_scaled)
            .checked_add(u128::from(ask_scaled))
            .and_then(|sum| sum.checked_div(2))
            .ok_or(PriceError::MalformedResponse("book mid"))?;
        let price = ScaledPrice::new(
            u64::try_from(mid).map_err(|_| PriceError::MalformedResponse("book mid"))?,
            8,
        )
        .map_err(|_| PriceError::MalformedResponse("book mid"))?;
        let spread_bps = u128::from(spread)
            .checked_mul(10_000)
            .map(|spread| spread.div_ceil(mid))
            .and_then(|spread| u64::try_from(spread).ok())
            .ok_or(PriceError::MalformedResponse("book spread"))?;
        let last = ticker
            .last_trade
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|trade| trade.first())
            .and_then(Value::as_str)
            .filter(|last| parse_scaled(last).is_ok());
        Ok(PriceQuote {
            valuation: Observation {
                source: SourceId::new("kraken"),
                price,
                observed_at: unix_now()?,
            },
            agreement_price: price,
            spread_bps: Some(spread_bps),
            evidence: json!({"bid":bid,"ask":ask,"last":last,"spread_bps":spread_bps}),
            reuse_until: None,
        })
    }

    async fn fetch(&self) -> Result<PriceQuote, PriceError> {
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

#[async_trait]
impl PriceSource for Kraken {
    async fn observe(&self) -> Result<Observation, PriceError> {
        self.fetch().await.map(|quote| quote.valuation)
    }

    async fn quote(&self) -> Result<PriceQuote, PriceError> {
        self.fetch().await
    }
}

#[derive(Deserialize)]
struct Response {
    error: Vec<String>,
    result: BTreeMap<String, Ticker>,
}

#[derive(Deserialize)]
struct Ticker {
    #[serde(rename = "a", default)]
    ask: Vec<String>,
    #[serde(rename = "b", default)]
    bid: Vec<String>,
    #[serde(rename = "c")]
    last_trade: Option<Value>,
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
        assert_eq!(observation.valuation.price.value(), 100_010_000);
        assert_eq!(observation.spread_bps, Some(2));
        assert_eq!(
            observation.evidence,
            json!({"bid":"1.00000000","ask":"1.00020000","last":"1.00010000","spread_bps":2})
        );
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

    #[test]
    fn prices_all_markets_from_book_mid_without_requiring_a_last_trade() {
        for (pair, result_pair) in [
            ("PHAUSD", "PHAUSD"),
            ("USDCUSD", "USDCUSD"),
            ("USDTUSD", "USDTZUSD"),
        ] {
            let source = Kraken::new(pair.to_owned()).expect("source");
            for last in [json!(["0.07183", "1"]), Value::Null, json!(["invalid"])] {
                let body = serde_json::to_vec(&json!({
                    "error":[],
                    "result":{result_pair:{"a":["0.07297"],"b":["0.07281"],"c":last}}
                }))
                .expect("response");
                let quote = source.parse_response(&body).expect("valid book");
                assert_eq!(quote.valuation.price.value(), 7_289_000);
                assert_eq!(quote.agreement_price, quote.valuation.price);
                assert_eq!(quote.spread_bps, Some(22));
                assert_eq!(quote.evidence["bid"], "0.07281");
                assert_eq!(quote.evidence["ask"], "0.07297");
                assert_eq!(
                    quote.evidence["last"],
                    if last == json!(["0.07183", "1"]) {
                        json!("0.07183")
                    } else {
                        Value::Null
                    }
                );
            }
        }
    }

    #[test]
    fn rejects_missing_invalid_zero_and_crossed_books_as_malformed() {
        let source = Kraken::new("PHAUSD".to_owned()).expect("source");
        for book in [
            json!({"a":["0.07297"]}),
            json!({"b":["0.07281"]}),
            json!({"a":[],"b":["0.07281"]}),
            json!({"a":["0.07297"],"b":[]}),
            json!({"a":["invalid"],"b":["0.07281"]}),
            json!({"a":["0.07297"],"b":["invalid"]}),
            json!({"a":["0"],"b":["0.07281"]}),
            json!({"a":["0.07297"],"b":["0"]}),
            json!({"a":["0.07281"],"b":["0.07297"]}),
        ] {
            let body = serde_json::to_vec(&json!({"error":[],"result":{"PHAUSD":book}}))
                .expect("response");
            assert!(matches!(
                source.parse_response(&body),
                Err(PriceError::MalformedResponse(_))
            ));
        }
    }

    #[test]
    fn rounds_mid_down_and_preserves_fractional_spread_for_the_guard() {
        let source = Kraken::new("PHAUSD".to_owned()).expect("source");
        let quote = source
            .parse_response(
                br#"{"error":[],"result":{"PHAUSD":{"a":["0.00000201"],"b":["0.00000200"]}}}"#,
            )
            .expect("valid book");
        assert_eq!(quote.valuation.price.value(), 200);
        assert_eq!(quote.spread_bps, Some(50));

        let quote = source
            .parse_response(
                br#"{"error":[],"result":{"PHAUSD":{"a":["1.00500001"],"b":["0.99500000"]}}}"#,
            )
            .expect("valid book");
        assert_eq!(quote.valuation.price.value(), 100_000_000);
        assert_eq!(quote.spread_bps, Some(101));
    }
}
