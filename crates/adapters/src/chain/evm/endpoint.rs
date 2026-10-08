//! Endpoint readiness and the single Alloy retry policy; no endpoint selection or failover.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use alloy::transports::{RpcError, TransportError, layers::RetryPolicy};
use chrono::Utc;

/// Health of one independently configured endpoint.
#[derive(Debug, Default)]
pub struct EndpointState {
    ready: AtomicBool,
    consecutive_failures: AtomicU32,
    contract_pending: AtomicBool,
    quota_until: AtomicI64,
    verify: AtomicBool,
}

impl EndpointState {
    /// Marks this endpoint as the independent Infura verifier.
    pub fn set_verify(&self) {
        self.verify.store(true, Ordering::Relaxed);
    }
    /// An exhausted Infura credential cannot send again before the next UTC day.
    pub fn quota_exhausted(&self) -> bool {
        Utc::now().timestamp() < self.quota_until.load(Ordering::Relaxed)
    }
    /// Three consecutive final request failures suspend readiness; success recovers immediately.
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed) && !self.quota_exhausted() && self.contract_ready()
    }
    /// Code identity is checked independently of transport success.
    pub fn contract_ready(&self) -> bool {
        !self.contract_pending.load(Ordering::Relaxed)
    }
    /// A missing or failed dual code check cannot be cleared by an unrelated successful RPC.
    pub fn contract_checked(&self, passed: bool) {
        self.contract_pending.store(!passed, Ordering::Relaxed);
    }
    /// Records a fully decoded successful operation.
    pub fn succeeded(&self) {
        if !self.quota_exhausted() {
            self.consecutive_failures.store(0, Ordering::Relaxed);
            self.ready.store(true, Ordering::Relaxed);
        }
    }
    /// Count final operation outcomes, never individual Alloy retry attempts.
    pub fn failed(&self) {
        let previous = self
            .consecutive_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            })
            .unwrap_or(u32::MAX);
        if previous >= 2 {
            self.ready.store(false, Ordering::Relaxed);
        }
    }
    /// Records HTTP status before Alloy's HTTP transport decodes a JSON-RPC error body.
    pub fn http_status(&self, status: u16) {
        if self.verify.load(Ordering::Relaxed) && status == 402 {
            let now = Utc::now();
            if let Some(next) = now
                .date_naive()
                .succ_opt()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
            {
                self.quota_until
                    .store(next.and_utc().timestamp(), Ordering::Relaxed);
            }
        }
    }
}

/// Only HTTP overload and JSON-RPC throughput errors are retried, never a quota exhaustion.
#[derive(Clone, Debug)]
pub(super) struct EndpointRetry(pub(super) Arc<EndpointState>);

impl RetryPolicy for EndpointRetry {
    fn should_retry(&self, error: &TransportError) -> bool {
        if self.0.quota_exhausted() {
            return false;
        }
        match error {
            RpcError::Transport(kind) => kind
                .as_http_error()
                .is_some_and(|http| matches!(http.status, 429 | 503)),
            RpcError::ErrorResp(payload) => payload.code == -32005,
            _ => false,
        }
    }
    fn backoff_hint(&self, error: &TransportError) -> Option<Duration> {
        match error {
            RpcError::Transport(kind) => kind.retry_after(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::evm::EvmClient;
    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::{Value, json};
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn whole_hint_call_limits_include_transport_retries_and_stop_both_sides() {
        for (read_limit, verify_limit) in [(12, 0), (0, 8)] {
            let count = Arc::new(AtomicUsize::new(0));
            let received = count.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, Router::new().route("/", post(move |Json(request): Json<Value>| {
                    let received = received.clone();
                    async move {
                        // Every first attempt overloads, so logical calls and billed calls differ.
                        let attempt = received.fetch_add(1, Ordering::SeqCst);
                        if attempt.is_multiple_of(2) {
                            (StatusCode::TOO_MANY_REQUESTS, Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32005,"message":"busy"}})))
                        } else {
                            (StatusCode::OK, Json(json!({"jsonrpc":"2.0","id":request["id"],"result":"0x1"})))
                        }
                    }
                }))).await.unwrap();
            });
            let read = EvmClient::new(&url).unwrap().with_provider("hint-read");
            let verify = EvmClient::new(&url).unwrap().with_provider("hint-verify");
            read.hint_call_limits(&verify, async {
                let side = if read_limit > 0 { &read } else { &verify };
                let limit = read_limit + verify_limit;
                for _ in 0..limit / 2 {
                    assert_eq!(side.latest_head().await.unwrap(), 1);
                }
                assert!(read.latest_head().await.is_err());
                assert!(verify.latest_head().await.is_err());
                assert_eq!(count.load(Ordering::SeqCst), limit);
                assert!(
                    side.ready(),
                    "task exhaustion must not change endpoint readiness"
                );
            })
            .await;
            // Scope termination restores the other callers' normal transport behavior.
            assert_eq!(read.latest_head().await.unwrap(), 1);
            assert_eq!(count.load(Ordering::SeqCst), read_limit + verify_limit + 2);
            server.abort();
        }
    }

    #[tokio::test]
    async fn overload_retries_and_infura_quota_does_not_send_again() {
        for status in [429_u16, 503, 402] {
            let count = Arc::new(AtomicUsize::new(0));
            let received = count.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app=Router::new().route("/",post(move |Json(request):Json<Value>| {
                let received=received.clone();
                async move {
                    let attempt=received.fetch_add(1,Ordering::SeqCst);
                    if attempt==0 { (StatusCode::from_u16(status).unwrap(),Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"refused"}}))) }
                    else { (StatusCode::OK,Json(json!({"jsonrpc":"2.0","id":request["id"],"result":"0x1"}))) }
                }
            }));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = EvmClient::new(&url).unwrap().as_verify();
            let result = client.latest_head().await;
            if status == 402 {
                assert!(result.is_err());
                assert!(!client.ready());
                assert!(client.latest_head().await.is_err());
                assert_eq!(count.load(Ordering::SeqCst), 1);
                assert!(client.state.quota_exhausted());
                let next = Utc::now()
                    .date_naive()
                    .succ_opt()
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp();
                assert_eq!(client.state.quota_until.load(Ordering::Relaxed), next);
                // Simulate crossing UTC midnight: the endpoint can be tested and made ready again.
                client
                    .state
                    .quota_until
                    .store(Utc::now().timestamp() - 1, Ordering::Relaxed);
                assert_eq!(client.latest_head().await.unwrap(), 1);
                assert!(client.ready());
            } else {
                assert_eq!(result.unwrap(), 1);
                assert_eq!(count.load(Ordering::SeqCst), 2);
                assert!(client.ready());
            }
            server.abort();
        }
    }
}
