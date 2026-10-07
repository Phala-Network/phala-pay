//! JSON-RPC calls counted per provider, chain, and method, for measuring provider usage.
//!
//! Every [`super::EvmClient`] request passes through [`CountingLayer`], so every call is counted
//! once when it is sent, whether it succeeds or not: that is what providers bill. Labels are
//! bounded: provider labels are the configured provider ids (never URLs), chains are the
//! configured chain ids, and methods outside [`METHODS`] count as `other`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll};
use std::time::SystemTime;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportFut};
use tower::{Layer, Service};

tokio::task_local! {
    static HTTP_STATUS: std::cell::Cell<Option<u16>>;
    static TASK_CALLS: TaskCalls;
}

/// Hard transport-level limits of one hint task, including retries and batch items.
#[derive(Clone)]
struct TaskCalls {
    read: CallLabels,
    verify: CallLabels,
    remaining: Arc<Mutex<(usize, usize)>>,
}

impl TaskCalls {
    fn claim(&self, labels: &CallLabels, count: usize) -> bool {
        let mut remaining = self
            .remaining
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if remaining.0 == 0 || remaining.1 == 0 {
            return false;
        }
        let side = if *labels == self.read {
            &mut remaining.0
        } else if *labels == self.verify {
            &mut remaining.1
        } else {
            return false;
        };
        if let Some(left) = side.checked_sub(count) {
            *side = left;
            true
        } else {
            false
        }
    }
}

/// Scope the whole task's actual transport calls; exhaustion sends no further requests.
pub(super) async fn task_call_limits<T>(
    read: CallLabels,
    verify: CallLabels,
    work: impl Future<Output = T>,
) -> T {
    TASK_CALLS
        .scope(
            TaskCalls {
                read,
                verify,
                remaining: Arc::new(Mutex::new((12, 8))),
            },
            work,
        )
        .await
}

/// Preserve a response's HTTP status across Alloy's JSON-RPC error-body decoding.
pub(super) fn observe_http_status(status: u16) {
    let _ = HTTP_STATUS.try_with(|slot| slot.set(Some(status)));
}

/// Methods counted under their own name; every other method counts as `other`.
pub const METHODS: [&str; 16] = [
    "eth_blockNumber",
    "eth_call",
    "eth_chainId",
    "eth_estimateGas",
    "eth_feeHistory",
    "eth_gasPrice",
    "eth_getBalance",
    "eth_getBlockByHash",
    "eth_getBlockByNumber",
    "eth_getCode",
    "eth_getLogs",
    "eth_getTransactionByHash",
    "eth_getTransactionCount",
    "eth_getTransactionReceipt",
    "eth_maxPriorityFeePerGas",
    "eth_sendRawTransaction",
];

/// Label of a client without a configured provider id.
pub const UNLABELED_PROVIDER: &str = "unlabeled";

/// Counter key: provider label, chain id (`None` before a client is bound to a chain), method.
type CallKey = (String, Option<u64>, &'static str);

static CALLS: Mutex<BTreeMap<CallKey, u64>> = Mutex::new(BTreeMap::new());
type ErrorKey = (String, Option<u64>, &'static str, &'static str);
static ERRORS: Mutex<BTreeMap<ErrorKey, u64>> = Mutex::new(BTreeMap::new());
type EndpointKey = (String, u64);
static ENDPOINTS: Mutex<BTreeMap<EndpointKey, Weak<super::endpoint::EndpointState>>> =
    Mutex::new(BTreeMap::new());

/// Error counters use the same configured provider and method labels as billed calls.
pub fn rpc_error_counts() -> Vec<(String, Option<u64>, &'static str, &'static str, u64)> {
    ERRORS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|((provider, chain, method, class), count)| {
            (provider.clone(), *chain, *method, *class, *count)
        })
        .collect()
}

/// Readiness of currently resident configured endpoints, including a halted UTC quota.
pub fn endpoint_readiness() -> Vec<(String, u64, bool)> {
    let mut endpoints = ENDPOINTS.lock().unwrap_or_else(PoisonError::into_inner);
    endpoints.retain(|_, state| state.strong_count() > 0);
    endpoints
        .iter()
        .filter_map(|((provider, chain), state)| {
            state
                .upgrade()
                .map(|state| (provider.clone(), *chain, state.ready()))
        })
        .collect()
}

pub(super) fn register(labels: &CallLabels, state: &Arc<super::endpoint::EndpointState>) {
    if let Some(chain) = labels.chain_id {
        ENDPOINTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((labels.provider.clone(), chain), Arc::downgrade(state));
    }
}

pub(super) fn record_error(labels: &CallLabels, methods: &[&'static str], class: &'static str) {
    let mut counts = ERRORS.lock().unwrap_or_else(PoisonError::into_inner);
    for method in methods {
        let count = counts
            .entry((labels.provider.clone(), labels.chain_id, *method, class))
            .or_default();
        *count = count.saturating_add(1);
    }
}
/// When the first call was counted; rates are the counters over the time since.
static COUNTING_SINCE: OnceLock<SystemTime> = OnceLock::new();

/// When this process counted its first call, if it has.
#[must_use]
pub fn counting_since() -> Option<SystemTime> {
    COUNTING_SINCE.get().copied()
}

/// One counter: the calls sent to `provider` for `chain_id` with `method` since process start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcCallCount {
    /// Configured provider id.
    pub provider: String,
    /// Chain id, when the client is bound to one.
    pub chain_id: Option<u64>,
    /// JSON-RPC method, or `other`.
    pub method: &'static str,
    /// Calls sent.
    pub calls: u64,
}

/// Returns every counter, ordered by provider, chain, and method.
#[must_use]
pub fn rpc_call_counts() -> Vec<RpcCallCount> {
    CALLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|((provider, chain_id, method), calls)| RpcCallCount {
            provider: provider.clone(),
            chain_id: *chain_id,
            method,
            calls: *calls,
        })
        .collect()
}

/// Returns the calls counted for one provider label, by method, for tests and measurements.
#[must_use]
pub fn provider_call_counts(provider: &str) -> BTreeMap<&'static str, u64> {
    let mut counts = BTreeMap::new();
    for count in rpc_call_counts() {
        if count.provider == provider {
            let calls: &mut u64 = counts.entry(count.method).or_default();
            *calls = calls.saturating_add(count.calls);
        }
    }
    counts
}

fn bounded_method(method: &str) -> &'static str {
    METHODS
        .iter()
        .find(|known| **known == method)
        .copied()
        .unwrap_or("other")
}

pub(super) fn record(labels: &CallLabels, method: &str) {
    COUNTING_SINCE.get_or_init(SystemTime::now);
    let key = (
        labels.provider.clone(),
        labels.chain_id,
        bounded_method(method),
    );
    let mut calls = CALLS.lock().unwrap_or_else(PoisonError::into_inner);
    let count = calls.entry(key).or_default();
    *count = count.saturating_add(1);
}

/// The labels one client's calls are counted under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CallLabels {
    pub(super) provider: String,
    pub(super) chain_id: Option<u64>,
}

impl Default for CallLabels {
    fn default() -> Self {
        Self {
            provider: UNLABELED_PROVIDER.to_owned(),
            chain_id: None,
        }
    }
}

/// Counts every JSON-RPC request, including each request of a batch, under fixed labels.
#[derive(Clone, Debug)]
pub(super) struct CountingLayer {
    labels: Arc<CallLabels>,
    state: Arc<super::endpoint::EndpointState>,
}

impl CountingLayer {
    pub(super) fn new(labels: CallLabels, state: Arc<super::endpoint::EndpointState>) -> Self {
        register(&labels, &state);
        Self {
            labels: Arc::new(labels),
            state,
        }
    }
}

impl<S> Layer<S> for CountingLayer {
    type Service = CountingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CountingService {
            inner,
            labels: Arc::clone(&self.labels),
            state: Arc::clone(&self.state),
        }
    }
}

/// The transport wrapped by [`CountingLayer`].
#[derive(Clone, Debug)]
pub(super) struct CountingService<S> {
    inner: S,
    labels: Arc<CallLabels>,
    state: Arc<super::endpoint::EndpointState>,
}

impl<S> Service<RequestPacket> for CountingService<S>
where
    S: Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        >,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        if self.state.quota_exhausted() {
            return Box::pin(async {
                Err(alloy::transports::TransportErrorKind::non_retryable_str(
                    "UTC daily RPC quota exhausted",
                ))
            });
        }
        let methods: Vec<_> = request.method_names().map(bounded_method).collect();
        if TASK_CALLS.try_with(|budget| budget.claim(&self.labels, methods.len())) == Ok(false) {
            return Box::pin(async {
                Err(alloy::transports::TransportErrorKind::non_retryable_str(
                    "hint task RPC call limit exhausted",
                ))
            });
        }
        for method in &methods {
            record(&self.labels, method);
        }
        let labels = self.labels.clone();
        let future = self.inner.call(request);
        Box::pin(HTTP_STATUS.scope(std::cell::Cell::new(None), async move {
            let response = future.await;
            // Alloy prefers a decoded JSON-RPC error over an HTTP error. Retain the status
            // for the one standard retry layer, even when the error body is valid JSON-RPC.
            let result = match HTTP_STATUS.with(std::cell::Cell::get) {
                Some(status @ (402 | 429 | 503)) => {
                    Err(alloy::transports::TransportErrorKind::http_error(
                        status,
                        "RPC endpoint refused the request".to_owned(),
                    ))
                }
                _ => response,
            };
            let class = match &result {
                Ok(packet) if packet.first_error_code() == Some(-32005) => Some("throughput"),
                Ok(packet) if packet.is_error() => Some("rpc"),
                Err(alloy::transports::RpcError::Transport(kind)) => {
                    Some(match kind.as_http_error().map(|error| error.status) {
                        Some(402) => "quota",
                        Some(429) => "throughput",
                        Some(503) => "unavailable",
                        Some(_) => "http",
                        None => "transport",
                    })
                }
                Err(_) => Some("rpc"),
                _ => None,
            };
            if let Some(class) = class {
                record_error(&labels, &methods, class);
            }
            result
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_methods_share_one_label() {
        assert_eq!(bounded_method("eth_getLogs"), "eth_getLogs");
        assert_eq!(bounded_method("debug_traceTransaction"), "other");
        assert_eq!(bounded_method("attacker-chosen-name"), "other");
    }
}
