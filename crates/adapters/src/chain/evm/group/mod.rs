//! Independent typed RPC groups, shared budgets, bounded retry and persisted head validation.
pub mod budget;
pub mod metrics;
pub mod rules;
pub mod transport;

use super::metrics::{CallLabels, record};
use crate::redaction::Redacted;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use async_trait::async_trait;
use budget::Budgets;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};
use tower::{
    Service, ServiceExt,
    retry::{Policy, Retry},
};

/// Deterministic selection; no hedging or programmable policy callbacks.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// Ascending priority, then configured order.
    #[default]
    Failover,
    /// Smooth weighted round robin across eligible members.
    WeightedRoundRobin,
}
/// Retry bounds for a complete member acceptance/readmission probe.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProbePolicy {
    /// Maximum attempts per RPC, including the first.
    pub attempts: u32,
    /// Total deadline for the complete probe, in milliseconds.
    pub deadline: u64,
}
impl Default for ProbePolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            deadline: 30_000,
        }
    }
}
/// Bounded group policy; millisecond fields avoid ambiguous duration units.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GroupPolicy {
    /// Selection algorithm.
    pub selection: Selection,
    /// First acceptance and readmission probe bounds.
    pub probe: ProbePolicy,
    /// Total operation deadline including admission and retry.
    pub total_deadline_ms: u64,
    /// Per-send timeout.
    pub attempt_timeout_ms: u64,
    /// Maximum member attempts including the first.
    pub max_attempts: u32,
    /// Delay between attempts.
    pub retry_delay_ms: u64,
    /// Consecutive transient failures before cooldown.
    pub failures: u32,
    /// Failure cooldown.
    pub cooldown_ms: u64,
    /// Successful recovery probes before readmission.
    pub recovery_successes: u32,
    /// Reviewed bounded provider-specific error mappings.
    pub rpc_error_rules: Vec<rules::ErrorRule>,
}
impl Default for GroupPolicy {
    fn default() -> Self {
        Self {
            selection: Selection::Failover,
            probe: ProbePolicy::default(),
            total_deadline_ms: 10_000,
            attempt_timeout_ms: 3_000,
            max_attempts: 3,
            retry_delay_ms: 100,
            failures: 3,
            cooldown_ms: 30_000,
            recovery_successes: 2,
            rpc_error_rules: Vec::new(),
        }
    }
}
impl GroupPolicy {
    /// Checks bounds before constructing clients.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=16).contains(&self.probe.attempts)
            || !(1..=120_000).contains(&self.probe.deadline)
            || self.attempt_timeout_ms > self.probe.deadline
            || self.retry_delay_ms >= self.probe.deadline
            || !(1..=16).contains(&self.max_attempts)
            || self.total_deadline_ms == 0
            || self.total_deadline_ms > 120_000
            || self.attempt_timeout_ms == 0
            || self.attempt_timeout_ms > self.total_deadline_ms
            || self.failures == 0
            || self.cooldown_ms == 0
            || self.recovery_successes == 0
            || self.retry_delay_ms >= self.total_deadline_ms
        {
            return Err("invalid RPC policy bounds");
        }
        rules::validate(&self.rpc_error_rules)?;
        Ok(())
    }
}
/// Sanitized typed classification. No raw response message is formatted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// Connection or HTTP 408 timeout.
    #[error("RPC transport failure")]
    Transport,
    /// Temporary server failure.
    #[error("RPC server failure")]
    Server,
    /// Account/key rate limit.
    #[error("RPC throttled")]
    Throttled,
    /// Permanent request error.
    #[error("RPC invalid request")]
    Request,
    /// Contract execution revert.
    #[error("RPC execution reverted")]
    Revert,
    /// Unsupported method.
    #[error("RPC capability unavailable")]
    Capability,
    /// Log range/result too large.
    #[error("RPC log window too large")]
    Range,
    /// HTTP request body too large.
    #[error("RPC request body too large")]
    Body,
    /// Redirect refused.
    #[error("RPC redirect refused")]
    Redirect,
    /// Authorization or chain identity invalid.
    #[error("RPC member identity or credentials invalid")]
    Identity,
    /// Invalid or oversized response.
    #[error("RPC malformed response")]
    Malformed,
    /// Stale head, never an accepted empty window.
    #[error("RPC stale head")]
    Stale,
    /// Finalized branch conflict.
    #[error("RPC finalized fork conflict")]
    Fork,
    /// Durable state cannot be loaded or written.
    #[error("RPC persistence unavailable")]
    Persistence,
    /// Unmapped upstream error.
    #[error("RPC unclassified error")]
    Unclassified,
    /// Whole group unavailable.
    #[error("RPC group unavailable")]
    Unavailable,
    /// Total deadline expired.
    #[error("RPC deadline expired")]
    Deadline,
}
impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Server => "server",
            Self::Throttled => "throttled",
            Self::Request => "request",
            Self::Revert => "revert",
            Self::Capability => "capability",
            Self::Range => "range",
            Self::Body => "body",
            Self::Redirect => "redirect",
            Self::Identity => "identity",
            Self::Malformed => "malformed",
            Self::Stale => "stale",
            Self::Fork => "fork",
            Self::Persistence => "persistence",
            Self::Unclassified => "unclassified",
            Self::Unavailable => "unavailable",
            Self::Deadline => "deadline",
        }
    }
    pub(crate) fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transport
                | Self::Server
                | Self::Throttled
                | Self::Capability
                | Self::Malformed
                | Self::Stale
                | Self::Identity
                | Self::Redirect
        )
    }
}
/// Classifies while HTTP metadata is still available. Messages only participate in bounded matches.
pub fn classify(method: &str, reply: &transport::HttpReply) -> Option<Failure> {
    let status = reply.status;
    if (300..400).contains(&status) {
        return Some(Failure::Redirect);
    }
    if status == 401 || status == 403 {
        return Some(Failure::Identity);
    }
    let error = reply.body.get("error");
    let code = error.and_then(|e| e.get("code")).and_then(Value::as_i64);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(256)
        .collect::<String>()
        .to_ascii_lowercase();
    if matches!(code, Some(-32600 | -32602)) {
        return Some(Failure::Request);
    }
    if message.contains("execution reverted") || message.starts_with("revert") {
        return Some(Failure::Revert);
    }
    if code == Some(-32601) {
        return Some(if method == "eth_sendRawTransaction" {
            Failure::Request
        } else {
            Failure::Capability
        });
    }
    if status == 413 {
        return Some(Failure::Body);
    }
    if status == 408 {
        return Some(Failure::Transport);
    }
    if status == 429 {
        return Some(Failure::Throttled);
    }
    if code == Some(-32005) {
        return Some(
            if message.contains("rate")
                || message.contains("quota")
                || message.contains("credit")
                || message.contains("requests per")
            {
                Failure::Throttled
            } else if method == "eth_getLogs"
                && (message.contains("range")
                    || message.contains("too many results")
                    || message.contains("response size")
                    || message.contains("result limit"))
            {
                Failure::Range
            } else {
                Failure::Unclassified
            },
        );
    }
    if status == 429 {
        return Some(Failure::Throttled);
    }
    if status == 408 {
        return Some(Failure::Transport);
    }
    if (500..600).contains(&status) || code == Some(-32603) {
        return Some(Failure::Server);
    }
    if error.is_some() {
        return Some(Failure::Unclassified);
    }
    if !(200..300).contains(&status) {
        return Some(Failure::Request);
    }
    if reply.body.get("result").is_none() {
        return Some(Failure::Malformed);
    }
    None
}
/// Decode with the same Alloy response types before a member attempt succeeds.
fn validate_typed(method: &str, reply: &Value) -> Result<(), Failure> {
    let value = reply.get("result").ok_or(Failure::Malformed)?.clone();
    let valid = match method {
        "eth_getTransactionReceipt" => {
            serde_json::from_value::<Option<alloy::network::AnyTransactionReceipt>>(value).is_ok()
        }
        "eth_getTransactionByHash" => {
            serde_json::from_value::<Option<alloy::rpc::types::Transaction>>(value).is_ok()
        }
        "eth_getBlockByNumber" | "eth_getBlockByHash" => {
            serde_json::from_value::<Option<alloy::rpc::types::Block>>(value).is_ok()
        }
        "eth_getLogs" => serde_json::from_value::<Vec<alloy::rpc::types::Log>>(value).is_ok(),
        "eth_blockNumber" | "eth_getTransactionCount" => {
            serde_json::from_value::<alloy::primitives::U64>(value).is_ok()
        }
        "eth_call" | "eth_getCode" => {
            serde_json::from_value::<alloy::primitives::Bytes>(value).is_ok()
        }
        "eth_chainId" => serde_json::from_value::<alloy::primitives::U64>(value).is_ok(),
        _ => !value.is_object() || !value.as_object().is_some_and(|o| o.is_empty()),
    };
    if valid {
        Ok(())
    } else {
        Err(Failure::Malformed)
    }
}

/// Accepted canonical header evidence.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HeadAnchor {
    /// Block number.
    pub number: u64,
    /// Canonical block hash.
    pub hash: String,
    /// Parent hash.
    pub parent_hash: String,
}
impl HeadAnchor {
    /// Extracts a complete block header from JSON-RPC.
    pub fn parse(value: &Value) -> Result<Self, Failure> {
        let number = value
            .get("number")
            .and_then(Value::as_str)
            .and_then(|s| u64::from_str_radix(s.strip_prefix("0x")?, 16).ok())
            .ok_or(Failure::Malformed)?;
        let hash = value
            .get("hash")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<alloy::primitives::B256>().ok())
            .ok_or(Failure::Malformed)?
            .to_string();
        let parent_hash = value
            .get("parentHash")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<alloy::primitives::B256>().ok())
            .ok_or(Failure::Malformed)?
            .to_string();
        Ok(Self {
            number,
            hash,
            parent_hash,
        })
    }
}
/// Durable state boundary implemented by topup's PostgreSQL repository.
#[async_trait]
pub trait WatermarkStore: Send + Sync {
    /// Refuses every operation while the chain is durably frozen.
    async fn blocked(&self, chain: u64) -> Result<(), Failure>;
    /// Freezes derived progress and credit after a finalized fork.
    async fn freeze(&self, chain: u64) -> Result<(), Failure>;
    /// Queues replay after a nonfinal anchor's canonical hash changes.
    async fn reorg(&self, _chain: u64, _group: &str, _from: u64, _to: u64) -> Result<(), Failure> {
        Ok(())
    }
    /// Loads the persisted high watermark.
    async fn load(&self, chain: u64, group: &str, tag: &str)
    -> Result<Option<HeadAnchor>, Failure>;
    /// Persists an accepted head before any caller can publish it.
    async fn accept(
        &self,
        chain: u64,
        group: &str,
        tag: &str,
        member: &str,
        head: &HeadAnchor,
    ) -> Result<(), Failure>;
}
/// Resolved member; formatting never reveals its endpoint.
#[derive(Clone)]
pub struct Member {
    /// Stable log-safe identifier.
    pub id: String,
    /// Reviewed company id.
    pub company: String,
    /// Resolved redacted URL, held only in memory.
    pub endpoint: Redacted,
    /// Shared account quota id.
    pub account: String,
    /// Shared credential quota id (synthetic for a keyless member).
    pub key: String,
    /// Failover order.
    pub priority: u32,
    /// Relative distribution weight.
    pub weight: u32,
}
impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("id", &self.id)
            .field("company", &self.company)
            .finish_non_exhaustive()
    }
}
#[derive(Default)]
struct Health {
    eligible: bool,
    failures: u32,
    recoveries: u32,
    until: Option<Instant>,
    quarantined: bool,
    score: i64,
    unsupported: BTreeSet<String>,
    probe_after: Option<Instant>,
}
/// Shared group state. Cloned clients share heads, eligibility and budgets.
pub struct RpcGroup {
    /// Stable group id.
    pub id: String,
    /// Chain id.
    pub chain: u64,
    /// Policy.
    pub policy: GroupPolicy,
    /// Configured members.
    pub members: Vec<Member>,
    probe_deadline: Option<Instant>,
    budgets: Arc<Budgets>,
    http: reqwest::Client,
    health: Mutex<Vec<Health>>,
    heads: tokio::sync::Mutex<BTreeMap<String, HeadAnchor>>,
    probe_blocks: tokio::sync::Mutex<BTreeMap<u64, String>>,
    store: RwLock<Option<Arc<dyn WatermarkStore>>>,
}
impl fmt::Debug for RpcGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcGroup")
            .field("id", &self.id)
            .field("members", &self.members)
            .finish_non_exhaustive()
    }
}
impl RpcGroup {
    /// Constructs initially unverified candidates; startup or recovery probes admit them.
    pub fn new(
        id: String,
        chain: u64,
        policy: GroupPolicy,
        members: Vec<Member>,
        budgets: Arc<Budgets>,
    ) -> Result<Arc<Self>, Failure> {
        policy.validate().map_err(|_| Failure::Request)?;
        let health = members.iter().map(|_| Health::default()).collect();
        let group = Arc::new(Self {
            id,
            chain,
            policy,
            members,
            budgets,
            probe_deadline: None,
            http: transport::client()?,
            health: Mutex::new(health),
            heads: tokio::sync::Mutex::new(BTreeMap::new()),
            probe_blocks: tokio::sync::Mutex::new(BTreeMap::new()),
            store: RwLock::new(None),
        });
        metrics::register(&group);
        Ok(group)
    }
    /// Isolated, uncached preflight view sharing the real credentials and account/key budgets.
    pub fn probe_copy(&self) -> Result<Arc<Self>, Failure> {
        self.isolated_copy(
            Instant::now()
                .checked_add(Duration::from_millis(self.policy.probe.deadline))
                .unwrap_or_else(Instant::now),
        )
    }
    /// Isolated read-only operation sharing budgets and probe retry rules, bounded by its
    /// caller's absolute deadline rather than the complete member-probe configuration.
    pub fn isolated_copy(&self, deadline: Instant) -> Result<Arc<Self>, Failure> {
        Ok(Arc::new(Self {
            id: self.id.clone(),
            chain: self.chain,
            policy: self.policy.clone(),
            members: self.members.clone(),
            probe_deadline: Some(deadline),
            budgets: self.budgets.clone(),
            http: self.http.clone(),
            health: Mutex::new(self.members.iter().map(|_| Health::default()).collect()),
            heads: tokio::sync::Mutex::new(BTreeMap::new()),
            probe_blocks: tokio::sync::Mutex::new(BTreeMap::new()),
            store: RwLock::new(None),
        }))
    }
    /// Absolute deadline shared by all RPCs in an isolated member probe.
    pub fn probe_deadline(&self) -> Option<Instant> {
        self.probe_deadline
    }
    /// Redirect/auth quarantine is never readmitted by recovery probing.
    pub fn quarantined(&self, index: usize) -> bool {
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .is_some_and(|h| h.quarantined)
    }
    /// Checks probe evidence against persisted floors without publishing a probe head.
    pub async fn validate_probe(&self, index: usize, probe: &RpcGroup) -> Result<(), Failure> {
        let store = self
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(store) = store else {
            return Ok(());
        };
        let heads = probe.heads.lock().await.clone();
        for tag in ["latest", "safe", "finalized"] {
            let current = heads.get(tag).ok_or(Failure::Malformed)?;
            for floor in [tag, "cursor"] {
                if let Some(previous) = store.load(self.chain, &self.id, floor).await? {
                    if current.number < previous.number {
                        return Err(Failure::Stale);
                    }
                    if current.number == previous.number && current.hash != previous.hash {
                        if tag == "finalized" || floor == "cursor" {
                            store.freeze(self.chain).await?;
                        }
                        return Err(Failure::Fork);
                    }
                    if tag == "finalized" {
                        let value=probe.send(index,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{:x}",previous.number),false]}),Instant::now().checked_add(Duration::from_millis(self.policy.total_deadline_ms)).unwrap_or_else(Instant::now)).await;
                        let value = match value {
                            Err(Failure::Fork) => {
                                // Snapshot consistency can reject before canonical comparison.
                                // Conflicting finalized evidence still closes the durable gate.
                                store.freeze(self.chain).await?;
                                return Err(Failure::Fork);
                            }
                            result => result?,
                        };
                        let canonical =
                            HeadAnchor::parse(value.get("result").ok_or(Failure::Malformed)?)?;
                        if canonical.number != previous.number || canonical.hash != previous.hash {
                            store.freeze(self.chain).await?;
                            return Err(Failure::Fork);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    /// Installs durable state before runtime requests start.
    pub fn set_store(&self, store: Arc<dyn WatermarkStore>) {
        *self.store.write().unwrap_or_else(PoisonError::into_inner) = Some(store);
    }
    /// Sets the result of full startup/capability probes.
    pub fn verified(&self, index: usize, ok: bool) {
        if let Some(h) = self
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(index)
        {
            h.eligible = ok;
            if ok {
                h.quarantined = false;
            }
            h.failures = 0;
        }
    }
    /// Shared durable safety store, for stopped-service cursor anchoring.
    pub fn watermark_store(&self) -> Option<Arc<dyn WatermarkStore>> {
        self.store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    /// Number of independently validated serving candidates.
    pub fn eligible(&self) -> usize {
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|h| h.eligible && !h.quarantined && h.until.is_none())
            .count()
    }
    /// Whether a different member can serve independent log review right now.
    pub fn independent_review_available(&self, answering: &str) -> bool {
        let health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
        self.members.iter().enumerate().any(|(i, m)| {
            m.id != answering
                && !self.budgets.paused(&m.account, &m.key)
                && health.get(i).is_some_and(|h| {
                    h.eligible
                        && !h.quarantined
                        && h.until.is_none()
                        && !h.unsupported.contains("eth_getLogs")
                })
        })
    }
    /// Chooses one member for a complete operation; no empty-pool fallback.
    pub fn select(&self, tried: &BTreeSet<usize>, exclude: Option<&str>) -> Result<usize, Failure> {
        self.select_for(tried, exclude, None)
    }
    /// Selects eligible members with this method's validated capability.
    pub fn select_for(
        &self,
        tried: &BTreeSet<usize>,
        exclude: Option<&str>,
        method: Option<&str>,
    ) -> Result<usize, Failure> {
        let mut health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
        let eligible = self
            .members
            .iter()
            .enumerate()
            .filter(|(i, m)| {
                !tried.contains(i)
                    && !self.budgets.paused(&m.account, &m.key)
                    && exclude != Some(m.id.as_str())
                    && health.get(*i).is_some_and(|h| {
                        h.eligible
                            && !h.quarantined
                            && h.until.is_none()
                            && method.is_none_or(|m| !h.unsupported.contains(m))
                    })
            })
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        if self.policy.selection == Selection::Failover {
            return eligible
                .into_iter()
                .min_by_key(|i| self.members.get(*i).map(|m| m.priority).unwrap_or(u32::MAX))
                .ok_or(Failure::Unavailable);
        }
        let mut total = 0_i64;
        let mut best = None;
        for i in eligible {
            if let (Some(m), Some(h)) = (self.members.get(i), health.get_mut(i)) {
                total = total.saturating_add(i64::from(m.weight));
                h.score = h.score.saturating_add(i64::from(m.weight));
                if best.is_none_or(|(_, score)| h.score > score) {
                    best = Some((i, h.score));
                }
            }
        }
        let (i, _) = best.ok_or(Failure::Unavailable)?;
        if let Some(h) = health.get_mut(i) {
            h.score = h.score.saturating_sub(total);
        }
        Ok(i)
    }
    /// Feeds every failed attempt back into selection.
    pub fn failed(&self, index: usize, error: Failure) {
        metrics::event(self, index, error.code(), 1);
        if let Some(h) = self
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(index)
        {
            if matches!(error, Failure::Redirect | Failure::Identity) {
                h.quarantined = true;
                h.eligible = false;
            } else if error == Failure::Capability {
                h.recoveries = 0;
                h.probe_after =
                    Instant::now().checked_add(Duration::from_millis(self.policy.cooldown_ms));
            } else if matches!(
                error,
                Failure::Transport | Failure::Server | Failure::Malformed | Failure::Stale
            ) {
                h.failures = h.failures.saturating_add(1);
                if error == Failure::Stale || h.failures >= self.policy.failures {
                    h.eligible = false;
                    h.recoveries = 0;
                    h.until = Some(
                        Instant::now()
                            .checked_add(Duration::from_millis(self.policy.cooldown_ms))
                            .unwrap_or_else(Instant::now),
                    );
                }
            }
        }
    }
    /// Only a complete successful operation clears consecutive failures.
    pub fn succeeded(&self, index: usize) {
        if let Some(h) = self
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(index)
        {
            h.failures = 0;
        }
    }
    /// Scales the window deadline to its bounded verification work and shared quotas.
    pub fn verification_time(&self, index: usize, logs: u64) -> Result<Duration, Failure> {
        self.send_time(index, logs.saturating_mul(4).saturating_add(8))
    }
    /// Plans time for bounded filter splits and canonical factory block verification.
    pub(crate) fn send_time(&self, index: usize, sends: u64) -> Result<Duration, Failure> {
        let member = self.members.get(index).ok_or(Failure::Unavailable)?;
        let admission = self
            .budgets
            .planned_time(&member.account, &member.key, sends)
            .map_err(|_| Failure::Request)?;
        Ok(admission.saturating_add(Duration::from_millis(
            sends.saturating_mul(self.policy.attempt_timeout_ms),
        )))
    }
    /// Pinned-member send for preflight and window operations; no selection, redirects or cache.
    pub async fn send(
        &self,
        index: usize,
        request: &Value,
        deadline: Instant,
    ) -> Result<Value, Failure> {
        let deadline = self
            .probe_deadline
            .map_or(deadline, |end| end.min(deadline));
        if self.probe_deadline.is_none() {
            return self.send_once(index, request, deadline).await;
        }
        let operation = Operation {
            value: request.clone(),
            deadline,
            tried: Arc::new(Mutex::new(BTreeSet::new())),
        };
        let service = tower::service_fn(|operation: Operation| async move {
            let method = operation
                .value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            // Only known method/tag names and contract addresses; never URLs, params or bodies.
            let tag = operation
                .value
                .get("params")
                .and_then(|p| p.get(0))
                .and_then(Value::as_str)
                .filter(|tag| matches!(*tag, "latest" | "safe" | "finalized" | "0x0"));
            tracing::debug!(group=%self.id, member=%self.members.get(index).map(|m|m.id.as_str()).unwrap_or("unknown"), probe=method, ?tag, "RPC probe attempt");
            let result = async {
                let value = self
                    .send_once(index, &operation.value, operation.deadline)
                    .await?;
                validate_typed(method, &value)?;
                if method == "eth_getBlockByNumber" {
                    let head = HeadAnchor::parse(value.get("result").ok_or(Failure::Malformed)?)?;
                    // Preserve every hash observed in this single probe, including its tagged
                    // snapshot. A later numeric read must not hide conflicting evidence.
                    let mut blocks = self.probe_blocks.lock().await;
                    if blocks
                        .get(&head.number)
                        .is_some_and(|hash| hash != &head.hash)
                    {
                        return Err(Failure::Fork);
                    }
                    blocks.insert(head.number, head.hash);
                }
                Ok(value)
            }
            .await;
            if let Err(error) = &result {
                tracing::warn!(group=%self.id, member=%self.members.get(index).map(|m|m.id.as_str()).unwrap_or("unknown"), probe=method, ?tag, class=error.code(), "RPC probe attempt failed");
            }
            result
        });
        Retry::new(
            RetryPolicy {
                remaining: self.policy.probe.attempts.saturating_sub(1),
                delay: Duration::from_millis(self.policy.retry_delay_ms),
                probe: true,
            },
            service,
        )
        .oneshot(operation)
        .await
    }
    async fn send_once(
        &self,
        index: usize,
        request: &Value,
        deadline: Instant,
    ) -> Result<Value, Failure> {
        let store = self
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(store) = store {
            store.blocked(self.chain).await?;
        }
        let member = self.members.get(index).ok_or(Failure::Unavailable)?;
        let waiting = Instant::now();
        let admission = self
            .budgets
            .admit(&member.account, &member.key, deadline)
            .await;
        metrics::event(
            self,
            index,
            "budget_wait",
            u64::try_from(waiting.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        admission.map_err(|_| Failure::Deadline)?;
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or(Failure::Request)?;
        record(
            &CallLabels {
                provider: member.id.clone(),
                chain_id: Some(self.chain),
            },
            method,
        );
        let reply = timeout(
            Duration::from_millis(self.policy.attempt_timeout_ms)
                .min(deadline.saturating_duration_since(Instant::now())),
            transport::send(&self.http, member.endpoint.expose(), request),
        )
        .await
        .map_err(|_| Failure::Transport)??;
        let default = classify(method, &reply);
        let rule = if matches!(
            default,
            Some(
                Failure::Unclassified | Failure::Throttled | Failure::Server | Failure::Capability
            )
        ) {
            self.policy
                .rpc_error_rules
                .iter()
                .find(|rule| rule.matches(&member.company, method, &reply))
        } else {
            None
        };
        if let Some(error) = rule.map(|r| r.class.failure()).or(default) {
            if matches!(error, Failure::Identity | Failure::Redirect)
                && let Some(h) = self
                    .health
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_mut(index)
            {
                h.quarantined = true;
                h.eligible = false;
            }
            if error == Failure::Capability
                && let Some(h) = self
                    .health
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_mut(index)
            {
                h.unsupported.insert(method.to_owned());
            }
            if error == Failure::Throttled {
                self.budgets.pause(
                    if rule.is_some_and(|r| r.budget_scope == rules::BudgetScope::Key) {
                        &member.key
                    } else {
                        &member.account
                    },
                    if self.probe_deadline.is_some() {
                        let delay = reply.retry_after.unwrap_or(Duration::from_secs(1));
                        // An unrepresentable delay must not overflow into immediate admission.
                        // A bounded pause beyond this probe prevents another send instead.
                        if Instant::now().checked_add(delay).is_some() {
                            delay
                        } else {
                            deadline.saturating_duration_since(Instant::now())
                        }
                    } else {
                        reply
                            .retry_after
                            .unwrap_or(Duration::from_secs(1))
                            .min(Duration::from_secs(60))
                    },
                );
            }
            return Err(error);
        }
        if reply.body.get("id") != request.get("id")
            || reply.body.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        {
            return Err(Failure::Malformed);
        }
        Ok(reply.body)
    }
    /// Splits only recipient/token/topic arrays on HTTP 413, keeping the numeric range fixed.
    /// Results are returned only after both subrequests succeed on this same member.
    pub fn send_logs<'a>(
        &'a self,
        index: usize,
        request: &'a Value,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send + 'a>> {
        Box::pin(async move {
            match self.send(index, request, deadline).await {
                Err(error @ (Failure::Body | Failure::Range)) => {
                    let filter = request
                        .get("params")
                        .and_then(|p| p.get(0))
                        .ok_or(Failure::Request)?;
                    if error == Failure::Range {
                        let number = |field: &str| {
                            filter
                                .get(field)
                                .and_then(Value::as_str)
                                .and_then(|s| u64::from_str_radix(s.strip_prefix("0x")?, 16).ok())
                                .ok_or(Failure::Range)
                        };
                        let from = number("fromBlock")?;
                        let to = number("toBlock")?;
                        if from >= to {
                            return Err(Failure::Range);
                        }
                        let mid = from
                            .checked_add(
                                to.saturating_sub(from)
                                    .checked_div(2)
                                    .ok_or(Failure::Range)?,
                            )
                            .ok_or(Failure::Range)?;
                        let mut left = request.clone();
                        let mut right = request.clone();
                        *left
                            .pointer_mut("/params/0/toBlock")
                            .ok_or(Failure::Range)? = json!(format!("0x{mid:x}"));
                        *right
                            .pointer_mut("/params/0/fromBlock")
                            .ok_or(Failure::Range)? =
                            json!(format!("0x{:x}", mid.checked_add(1).ok_or(Failure::Range)?));
                        let left = self.send_logs(index, &left, deadline).await?;
                        let right = self.send_logs(index, &right, deadline).await?;
                        let mut logs = left
                            .get("result")
                            .and_then(Value::as_array)
                            .ok_or(Failure::Malformed)?
                            .clone();
                        logs.extend(
                            right
                                .get("result")
                                .and_then(Value::as_array)
                                .ok_or(Failure::Malformed)?
                                .iter()
                                .cloned(),
                        );
                        return Ok(json!({"jsonrpc":"2.0","id":request.get("id"),"result":logs}));
                    }
                    let mut path = None;
                    if filter
                        .get("address")
                        .and_then(Value::as_array)
                        .is_some_and(|a| a.len() > 1)
                    {
                        path = Some("/params/0/address".to_owned());
                    }
                    if path.is_none()
                        && let Some(topics) = filter.get("topics").and_then(Value::as_array)
                    {
                        for (i, topic) in topics.iter().enumerate() {
                            if topic.as_array().is_some_and(|a| a.len() > 1) {
                                path = Some(format!("/params/0/topics/{i}"));
                                break;
                            }
                        }
                    }
                    let path = path.ok_or(Failure::Body)?;
                    let values = request
                        .pointer(&path)
                        .and_then(Value::as_array)
                        .ok_or(Failure::Body)?;
                    let (left, right) =
                        values.split_at(values.len().checked_div(2).ok_or(Failure::Body)?);
                    let mut a = request.clone();
                    let mut b = request.clone();
                    *a.pointer_mut(&path).ok_or(Failure::Body)? = json!(left);
                    *b.pointer_mut(&path).ok_or(Failure::Body)? = json!(right);
                    let a = self.send_logs(index, &a, deadline).await?;
                    let b = self.send_logs(index, &b, deadline).await?;
                    let mut logs = a
                        .get("result")
                        .and_then(Value::as_array)
                        .ok_or(Failure::Malformed)?
                        .clone();
                    logs.extend(
                        b.get("result")
                            .and_then(Value::as_array)
                            .ok_or(Failure::Malformed)?
                            .iter()
                            .cloned(),
                    );
                    Ok(json!({"jsonrpc":"2.0","id":request.get("id"),"result":logs}))
                }
                result => result,
            }
        })
    }
    /// Validates a real head against durable and shared in-memory watermarks.
    pub async fn head(
        &self,
        index: usize,
        tag: &str,
        deadline: Instant,
    ) -> Result<HeadAnchor, Failure> {
        self.head_reply(index, tag, deadline)
            .await
            .map(|(head, _)| head)
    }
    async fn head_reply(
        &self,
        index: usize,
        tag: &str,
        deadline: Instant,
    ) -> Result<(HeadAnchor, Value), Failure> {
        let value=self.send(index,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[tag,false]}),deadline).await?;
        validate_typed("eth_getBlockByNumber", &value)?;
        let head = HeadAnchor::parse(value.get("result").ok_or(Failure::Malformed)?)?;
        tracing::debug!(group=%self.id, member=%self.members.get(index).map(|m|m.id.as_str()).unwrap_or("unknown"), probe=tag, height=head.number, "RPC head decoded");
        let mut heads = self.heads.lock().await;
        let store = self
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(store) = &store
            && let Some(previous) = store.load(self.chain, &self.id, tag).await?
        {
            heads.insert(tag.to_owned(), previous);
        }
        if let Some(store) = &store
            && let Some(cursor) = store.load(self.chain, &self.id, "cursor").await?
        {
            if head.number < cursor.number {
                return Err(Failure::Stale);
            }
            if tag == "finalized" {
                let value=self.send(index,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{:x}",cursor.number),false]}),deadline).await?;
                let canonical = HeadAnchor::parse(value.get("result").ok_or(Failure::Malformed)?)?;
                if canonical.number != cursor.number || canonical.hash != cursor.hash {
                    store.freeze(self.chain).await?;
                    return Err(Failure::Fork);
                }
            }
        }
        if let Some(previous) = heads.get(tag) {
            if head.number < previous.number {
                tracing::warn!(group=%self.id, member=%self.members.get(index).map(|m|m.id.as_str()).unwrap_or("unknown"), probe=tag, previous_height=previous.number, height=head.number, class="stale", "RPC head regressed");
                return Err(Failure::Stale);
            }
            {
                let canonical = if head.number == previous.number {
                    head.clone()
                } else {
                    let v=self.send(index,&json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{:x}",previous.number),false]}),deadline).await?;
                    HeadAnchor::parse(v.get("result").ok_or(Failure::Malformed)?)?
                };
                if canonical.number != previous.number || canonical.hash != previous.hash {
                    if tag == "finalized" {
                        if let Some(store) = &store {
                            store.freeze(self.chain).await?;
                        }
                        return Err(Failure::Fork);
                    }
                    if let Some(store) = &store {
                        // The fork point is unknown: replay all nonfinal progress above finality.
                        let finalized = store
                            .load(self.chain, &self.id, "finalized")
                            .await?
                            .map_or(0, |h| h.number);
                        store
                            .reorg(
                                self.chain,
                                &self.id,
                                finalized.saturating_add(1),
                                head.number,
                            )
                            .await?;
                    }
                }
            }
        }
        let member = self.members.get(index).ok_or(Failure::Unavailable)?;
        if let Some(store) = store {
            store
                .accept(self.chain, &self.id, tag, &member.id, &head)
                .await?;
        }
        heads.insert(tag.to_owned(), head.clone());
        Ok((head, value))
    }
    /// Saves an A/B-agreed cursor hash anchor before issuing addresses or scanning past it.
    pub async fn persist_cursor(&self, index: usize, head: &HeadAnchor) -> Result<(), Failure> {
        let member = self.members.get(index).ok_or(Failure::Unavailable)?;
        let store = self
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(Failure::Persistence)?;
        store
            .accept(self.chain, &self.id, "cursor", &member.id, head)
            .await
    }
    /// Makes a finalized hash conflict durable across every group and derived writer.
    pub async fn freeze(&self) -> Result<(), Failure> {
        let store = self
            .store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(store) = store {
            store.freeze(self.chain).await?;
        }
        Ok(())
    }
    /// Explicit successful recovery probes; never auto-readmit on cooldown expiry.
    pub fn probe_result(&self, index: usize, ok: bool) {
        if let Some(h) = self
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(index)
        {
            if h.quarantined {
                return;
            }
            if ok {
                h.recoveries = h.recoveries.saturating_add(1);
                if h.recoveries >= self.policy.recovery_successes {
                    h.eligible = true;
                    h.until = None;
                    h.failures = 0;
                    h.unsupported.clear();
                    h.probe_after = None;
                }
            } else {
                h.recoveries = 0;
                // Delay full probes without removing unrelated serving capabilities.
                h.probe_after = Some(
                    Instant::now()
                        .checked_add(Duration::from_millis(self.policy.cooldown_ms))
                        .unwrap_or_else(Instant::now),
                );
                if !h.eligible {
                    h.until = h.probe_after;
                }
            }
        }
    }
    /// Members due for a full capability recovery probe.
    pub fn probe_due(&self, index: usize) -> bool {
        self.health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(index)
            .is_some_and(|h| {
                (!h.eligible || !h.unsupported.is_empty())
                    && !h.quarantined
                    && h.until.is_none_or(|t| t <= Instant::now())
                    && h.probe_after.is_none_or(|t| t <= Instant::now())
            })
    }
    /// Executes a bounded request through Tower retry.
    pub async fn request(self: &Arc<Self>, value: Value) -> Result<Value, Failure> {
        let request = Operation {
            value,
            deadline: Instant::now()
                .checked_add(Duration::from_millis(self.policy.total_deadline_ms))
                .unwrap_or_else(Instant::now),
            tried: Arc::new(Mutex::new(BTreeSet::new())),
        };
        let retry = Retry::new(
            RetryPolicy {
                remaining: self.policy.max_attempts.saturating_sub(1),
                delay: Duration::from_millis(self.policy.retry_delay_ms),
                probe: false,
            },
            Attempt(self.clone()),
        );
        let service = tower::timeout::Timeout::new(
            retry,
            Duration::from_millis(self.policy.total_deadline_ms),
        );
        service.oneshot(request).await.map_err(|e| {
            e.downcast_ref::<Failure>()
                .copied()
                .unwrap_or(Failure::Deadline)
        })
    }
}
#[derive(Clone)]
struct Operation {
    value: Value,
    deadline: Instant,
    tried: Arc<Mutex<BTreeSet<usize>>>,
}
#[derive(Clone)]
struct RetryPolicy {
    probe: bool,
    remaining: u32,
    delay: Duration,
}
impl Policy<Operation, Value, Failure> for RetryPolicy {
    type Future = Pin<Box<dyn Future<Output = ()> + Send>>;
    fn retry(
        &mut self,
        request: &mut Operation,
        result: &mut Result<Value, Failure>,
    ) -> Option<Self::Future> {
        if self.remaining == 0
            || Instant::now()
                .checked_add(self.delay)
                .is_none_or(|t| t >= request.deadline)
            || !result.as_ref().err().is_some_and(|e| {
                if self.probe {
                    matches!(e, Failure::Transport | Failure::Server | Failure::Throttled)
                } else {
                    e.retryable()
                }
            })
        {
            return None;
        }
        self.remaining = self.remaining.saturating_sub(1);
        let delay = self.delay;
        if self.probe {
            self.delay = self.delay.saturating_mul(2);
        }
        Some(Box::pin(sleep(delay)))
    }
    fn clone_request(&mut self, request: &Operation) -> Option<Operation> {
        Some(request.clone())
    }
}
#[derive(Clone)]
struct Attempt(Arc<RpcGroup>);
impl Service<Operation> for Attempt {
    type Response = Value;
    type Error = Failure;
    type Future = Pin<Box<dyn Future<Output = Result<Value, Failure>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Failure>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: Operation) -> Self::Future {
        let group = self.0.clone();
        Box::pin(async move {
            let method = request
                .value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("");
            let index = {
                let mut tried = request.tried.lock().unwrap_or_else(PoisonError::into_inner);
                let index = group.select_for(&tried, None, Some(method)).or_else(|_| {
                    tried.clear();
                    group.select_for(&tried, None, Some(method))
                })?;
                tried.insert(index);
                index
            };
            let tag = if method == "eth_blockNumber" {
                Some("latest")
            } else if method == "eth_getBlockByNumber" {
                request
                    .value
                    .get("params")
                    .and_then(|v| v.get(0))
                    .and_then(Value::as_str)
                    .filter(|s| matches!(*s, "latest" | "safe" | "finalized"))
            } else {
                None
            };
            let result = if let Some(tag) = tag {
                group.head_reply(index,tag,request.deadline).await.map(|(h,mut value)| {
                    if method=="eth_blockNumber" {json!({"jsonrpc":"2.0","id":request.value.get("id"),"result":format!("0x{:x}",h.number)})}
                    else {if let Some(id)=value.get_mut("id") {*id=request.value.get("id").cloned().unwrap_or(Value::Null);}value}
                })
            } else {
                async {
                    if method != "eth_chainId" {
                        let head = group.head(index, "latest", request.deadline).await?;
                        let needed = request
                            .value
                            .get("params")
                            .and_then(|p| p.get(1))
                            .and_then(Value::as_str)
                            .and_then(|s| s.strip_prefix("0x"))
                            .and_then(|s| u64::from_str_radix(s, 16).ok());
                        if needed.is_some_and(|n| n > head.number) {
                            return Err(Failure::Stale);
                        }
                    }
                    if method == "eth_getLogs" {
                        group
                            .send_logs(index, &request.value, request.deadline)
                            .await
                    } else {
                        group.send(index, &request.value, request.deadline).await
                    }
                }
                .await
            };
            let result = result.and_then(|value| {
                validate_typed(method, &value)?;
                Ok(value)
            });
            if let Err(e) = &result {
                group.failed(index, *e);
            } else {
                group.succeeded(index);
            }
            result
        })
    }
}
/// Alloy transport for a group, or an explicitly pinned probe/window member.
#[derive(Clone)]
pub struct GroupTransport {
    /// Shared group.
    pub group: Arc<RpcGroup>,
    /// Fixed member for a window/preflight; normal requests use group selection.
    pub pinned: Option<usize>,
    /// Absolute window/evidence deadline, shared by all nested sends and log splits.
    pub deadline: Option<Instant>,
}
impl Service<RequestPacket> for GroupTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, packet: RequestPacket) -> Self::Future {
        let group = self.group.clone();
        let pinned = self.pinned;
        let deadline = self.deadline;
        Box::pin(async move {
            let value = serde_json::to_value(packet)
                .map_err(|_| TransportErrorKind::custom(Failure::Request))?;
            let result = if let Some(index) = pinned {
                let deadline = deadline
                    .or_else(|| group.probe_deadline())
                    .unwrap_or_else(|| {
                        Instant::now()
                            .checked_add(Duration::from_millis(group.policy.total_deadline_ms))
                            .unwrap_or_else(Instant::now)
                    });
                if value.get("method").and_then(Value::as_str) == Some("eth_getLogs") {
                    group.send_logs(index, &value, deadline).await
                } else {
                    group.send(index, &value, deadline).await
                }
            } else {
                group.request(value).await
            };
            let value = result.map_err(TransportErrorKind::custom)?;
            serde_json::from_value(value)
                .map_err(|_| TransportErrorKind::custom(Failure::Malformed))
        })
    }
}

#[cfg(test)]
mod tests;
