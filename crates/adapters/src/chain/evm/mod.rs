//! The EVM JSON-RPC client shared by every consumer of one (chain, provider), and the
//! finalized-log reader built on it.

pub mod endpoint;
pub mod metrics;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::{self, Formatter};
use std::future::{Future, IntoFuture};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::chain::flush::{
    ContractAddressGetter, FactoryEvent, addressOfCall, balanceOfCall,
    decode_contract_address_getter, encode_address_of, encode_balance_of,
    encode_contract_address_getter, factory_event_signatures,
};
use crate::redaction::{Redacted, RedactedTransportError};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{AnyNetwork, AnyTransactionReceipt, ReceiptResponse as _};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::{CallItem, MULTICALL3_ADDRESS, MulticallError, Provider, RootProvider};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::types::{
    Filter, Log, Topic, TransactionInput, TransactionReceipt, TransactionRequest,
};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::TransportError;
use chrono::{DateTime, Utc};
use metrics::{CallLabels, CountingLayer};
use tokio::time::timeout;
use topup_core::money::AtomicAmount;
use topup_core::route::{ChainHeads, Confirmations};

/// Maximum inclusive block count in one `eth_getLogs` request.
pub const MAX_BLOCKS_PER_REQUEST: u64 = 2_000;
/// Maximum recipient count in one `eth_getLogs` request.
pub const MAX_ADDRESSES_PER_REQUEST: usize = 1_000;
/// Block timestamps kept per client; the head scan revisits the same recent blocks every poll.
const BLOCK_TIME_CACHE_CAPACITY: usize = 1_024;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 amount);
    struct SnapshotCall { address target; bool allowFailure; bytes callData; }
    struct SnapshotResult { bool success; bytes returnData; }
    function aggregate3(SnapshotCall[] calls) external payable returns (SnapshotResult[] returnData);
    function getCurrentBlockTimestamp() external view returns (uint256);
    function getBlockHash(uint256 blockNumber) external view returns (bytes32);
    function isValidSignature(bytes32 hash, bytes signature) external view returns (bytes4);
}

/// The value EIP-1271's `isValidSignature` returns for a valid signature, its own selector.
pub const EIP1271_MAGIC_VALUE: [u8; 4] = [0x16, 0x26, 0xba, 0x7e];

/// One ERC-20 transfer to a tracked address.
///
/// Its identity is `(tx_hash, receipt_log_index)`, which survives the transaction's re-inclusion in
/// another block; the block fields and the block-wide `log_index` are evidence that may change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferLog {
    /// Transaction hash containing the event.
    pub tx_hash: B256,
    /// Position of the log among the logs of the transaction's receipt.
    pub receipt_log_index: u64,
    /// Log index within the block.
    pub log_index: u64,
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: B256,
    /// Timestamp of the block.
    pub block_time: DateTime<Utc>,
    /// Sender of the transaction (not necessarily the token sender).
    pub tx_from: Address,
    /// Nonce of the transaction, which proves it dropped once another transaction consumed it.
    pub tx_nonce: u64,
    /// Token contract that emitted the event.
    pub token: Address,
    /// Transfer sender.
    pub from: Address,
    /// Transfer recipient.
    pub to: Address,
    /// Atomic token amount.
    pub amount: AtomicAmount,
}

/// A transaction's receipt as one provider reports it, with the transfer at one receipt position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptLookup {
    /// The provider has no receipt: the transaction is not in its canonical chain.
    Missing,
    /// The transaction is included in `block_hash`.
    Included {
        /// Including block number.
        block_number: u64,
        /// Including block hash.
        block_hash: B256,
        /// Receipt status independently decoded on this endpoint.
        status: bool,
        /// Including header time independently fetched by hash.
        block_time: DateTime<Utc>,
        /// Transaction sender independently fetched (OP: receipt from).
        tx_from: Address,
        /// Transaction nonce independently fetched (OP: receipt depositNonce).
        tx_nonce: u64,
        /// The ERC-20 `Transfer` at the requested receipt position, if that log is one.
        transfer: Option<Box<TransferLog>>,
    },
}

impl ReceiptLookup {
    /// The transfer at the requested position, when the transaction is included.
    #[must_use]
    pub fn transfer(&self) -> Option<&TransferLog> {
        match self {
            Self::Missing => None,
            Self::Included { transfer, .. } => transfer.as_deref(),
        }
    }
}

/// Evidence judged together from one member, never assembled across a group's members.
pub struct FinalityEvidence {
    /// Member-validated finalized height.
    pub finalized: u64,
    /// Receipt at the requested position.
    pub receipt: ReceiptLookup,
}

/// The provider's current finalized block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizedHead {
    /// Finalized block number.
    pub number: u64,
    /// Timestamp of the finalized block.
    pub time: DateTime<Utc>,
}

/// One `ForwarderFactory` event about a tracked forwarder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactoryLog {
    /// Transaction hash containing the event.
    pub tx_hash: B256,
    /// Log index within the block.
    pub log_index: u64,
    /// Block number.
    pub block_number: u64,
    /// Block hash.
    pub block_hash: B256,
    /// The decoded event.
    pub event: FactoryEvent,
}

/// Independently decoded receipt, transaction and header for factory evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactoryReceipt {
    /// Receipt status, including a successful receipt with no factory event.
    pub status: bool,
    /// Canonical inclusion height.
    pub block_number: u64,
    /// Canonical inclusion hash.
    pub block_hash: B256,
    /// Inclusion timestamp from this endpoint's header.
    pub block_time: DateTime<Utc>,
    /// Transaction sender and nonce from this endpoint.
    pub origin: (Address, u64),
    /// Factory events from this receipt in receipt order.
    pub logs: Vec<FactoryLog>,
}

/// Failure while reading or validating EVM chain data.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ChainError {
    /// The configured provider URL is invalid.
    #[error("invalid RPC URL")]
    InvalidUrl,
    /// The provider returned an RPC failure during the named operation.
    #[error("EVM RPC request failed during {0}")]
    Rpc(&'static str),
    /// The provider transport failed without exposing its configured URL.
    #[error("{0}")]
    Transport(RedactedTransportError),
    /// The provider answered with a value that could not be encoded, decoded, or used.
    #[error("{0}")]
    InvalidResponse(String),
    /// A required finalized block or log field was absent.
    #[error("EVM response omitted `{0}`")]
    MissingField(&'static str),
    /// A block timestamp did not fit the supported UTC representation.
    #[error("block timestamp `{0}` is outside UTC range")]
    InvalidTimestamp(u64),
    /// A log matching the transfer signature could not be decoded.
    #[error("invalid Transfer log: {0}")]
    InvalidTransfer(String),
    /// The caller supplied an invalid inclusive block range.
    #[error("invalid block range: from {from_block} exceeds to {to_block}")]
    InvalidRange {
        /// Inclusive range start.
        from_block: u64,
        /// Inclusive range end.
        to_block: u64,
    },
    /// The provider answered a finalized head below one it answered before: a load-balanced
    /// gateway serving a node that has not caught up. The stale head is refused and the read is
    /// retried; the next answer at or above the highest is used.
    #[error("provider finalized head regressed from {previous} to {current}")]
    FinalizedHeadRegressed {
        /// Highest finalized head previously observed.
        previous: u64,
        /// Lower finalized head returned by the provider.
        current: u64,
    },
    /// The finalized-head guard lock was poisoned.
    #[error("provider health state unavailable")]
    HealthStateUnavailable,
    /// The chain moved between two reads that must describe the same block, such as a log and its
    /// transaction's receipt; the read is retried.
    #[error("chain reorganized during {0}")]
    Reorganized(&'static str),
}

impl ChainError {
    /// Returns whether the provider refused the request for now, so it may be retried after a
    /// backoff; see [`RedactedTransportError::is_rate_limited`].
    #[must_use]
    pub const fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Transport(error) if error.is_rate_limited())
    }
}

/// Chain reads required by the scanner, the confirm step, and the finality watch.
pub trait ChainReader: Send + Sync {
    /// Returns the provider's current finalized block number and time.
    fn finalized_head(&self) -> impl Future<Output = Result<FinalizedHead, ChainError>> + Send;

    /// Finalized header including its hash, reused by checkpoint agreement.
    fn finalized_header(
        &self,
    ) -> impl Future<Output = Result<(FinalizedHead, B256), ChainError>> + Send {
        async {
            let head = self.finalized_head().await?;
            let (hash, time) = self.header(head.number).await?;
            if time != head.time {
                return Err(ChainError::Reorganized("finalized header"));
            }
            Ok((head, hash))
        }
    }

    /// Latest header for the read-only discovery range.
    fn latest_header(&self) -> impl Future<Output = Result<FinalizedHead, ChainError>> + Send {
        async { Err(ChainError::MissingField("latest header")) }
    }

    /// A complete numbered header; callers compare hash and time before advancing coverage.
    fn header(
        &self,
        _number: u64,
    ) -> impl Future<Output = Result<(B256, DateTime<Utc>), ChainError>> + Send {
        async { Err(ChainError::MissingField("numbered header")) }
    }
    /// Four-topic address-less coverage query. Each request must complete on this endpoint.
    fn coverage_logs(
        &self,
        factory: Address,
        addresses: &[Address],
        from: u64,
        to: u64,
    ) -> impl Future<Output = Result<(Vec<TransferLog>, Vec<FactoryLog>), ChainError>> + Send {
        async move {
            Ok((
                self.transfer_logs_to(addresses, from, to).await?,
                self.factory_logs(factory, addresses, from, to).await?,
            ))
        }
    }
    /// Independently decode finalized factory events from a transaction's receipt.
    fn factory_receipt(
        &self,
        _tx: B256,
        _factory: Address,
    ) -> impl Future<Output = Result<Option<FactoryReceipt>, ChainError>> + Send {
        async { Err(ChainError::MissingField("factory receipt")) }
    }

    /// Returns the one head `confirmations` is evaluated on: `latest` for a depth, `safe` for
    /// `safe`, `finalized` for `finalized`. An unread `finalized` is 0, a lower bound, so a check
    /// on a depth or `safe` costs one head read.
    fn confirmation_heads(
        &self,
        confirmations: Confirmations,
    ) -> impl Future<Output = Result<ChainHeads, ChainError>> + Send;

    /// Returns the factory's `ForwarderCreated`, `Flushed`, and `FlushFailed` events about any
    /// supplied forwarder in the inclusive block range. Anyone can call the factory, so the caller
    /// decides which of them concern its own `(forwarder, treasury)` pairs.
    fn factory_logs(
        &self,
        factory: Address,
        forwarders: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> impl Future<Output = Result<Vec<FactoryLog>, ChainError>> + Send;

    /// Returns ERC-20 transfers to any supplied recipient in the inclusive block range.
    fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> impl Future<Output = Result<Vec<TransferLog>, ChainError>> + Send;

    /// Returns transfers of `tokens` to any address in `recipients` in the inclusive block range.
    ///
    /// A provider reader requests every transfer of the tokens, one request per block window
    /// whatever the recipient count, and keeps those to `recipients` locally.
    fn token_transfers(
        &self,
        tokens: &[Address],
        recipients: &BTreeSet<Address>,
        from_block: u64,
        to_block: u64,
    ) -> impl Future<Output = Result<Vec<TransferLog>, ChainError>> + Send {
        async move {
            let addresses = recipients.iter().copied().collect::<Vec<_>>();
            let logs = self
                .transfer_logs_to(&addresses, from_block, to_block)
                .await?;
            Ok(logs
                .into_iter()
                .filter(|log| tokens.contains(&log.token))
                .collect())
        }
    }

    /// Reads the transaction's receipt and the ERC-20 transfer at `receipt_log_index` in it.
    fn receipt_transfer(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> impl Future<Output = Result<ReceiptLookup, ChainError>> + Send;

    /// Receipt, transaction, header and confirmation head independently decoded on this endpoint.
    fn confirmation_evidence(
        &self,
        tx: B256,
        position: u64,
        confirmations: Confirmations,
    ) -> impl Future<Output = Result<(ChainHeads, ReceiptLookup), ChainError>> + Send {
        async move {
            let heads = self.confirmation_heads(confirmations).await?;
            let receipt = self.receipt_transfer(tx, position).await?;
            Ok((heads, receipt))
        }
    }

    /// Receipt evidence at an already agreed checkpoint; absence is never a nonce verdict.
    fn finality_evidence(
        &self,
        tx: B256,
        position: u64,
        checkpoint: u64,
    ) -> impl Future<Output = Result<FinalityEvidence, ChainError>> + Send {
        async move {
            Ok(FinalityEvidence {
                finalized: checkpoint,
                receipt: self.receipt_transfer(tx, position).await?,
            })
        }
    }

    /// Returns `account`'s nonce at block `block`: the number of its transactions included up to
    /// and including that block.
    fn nonce_at(
        &self,
        account: Address,
        block: u64,
    ) -> impl Future<Output = Result<u64, ChainError>> + Send;
}

/// The highest finalized head a reader has returned, so it never returns a lower one.
///
/// A load-balanced gateway can answer from nodes that disagree on `finalized` for minutes (Base
/// Sepolia's Tenderly gateway alternated between two heads 156 blocks apart), so a lower answer is
/// a stale node, not a finality violation: it is refused each time, and never marks the provider
/// unusable.
#[derive(Debug, Default)]
struct FinalizedGuard {
    last_finalized: Option<u64>,
}

impl FinalizedGuard {
    fn observe(&mut self, current: u64) -> Result<(), ChainError> {
        if let Some(previous) = self.last_finalized
            && current < previous
        {
            return Err(ChainError::FinalizedHeadRegressed { previous, current });
        }
        self.last_finalized = Some(current);
        Ok(())
    }
}

/// Bounded FIFO cache of values that never change for their key, such as a block's time by its
/// hash, so a reorged block at the same height never lends its data to a log from another block.
/// It evicts the oldest insertion, not the least recently used entry: a key's value never changes,
/// and the head scan reads the newest blocks, which are the newest insertions, so recency
/// tracking would buy nothing.
#[derive(Debug)]
struct FifoCache<K, V> {
    values: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for FifoCache<K, V> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> FifoCache<K, V> {
    fn get(&self, key: &K) -> Option<V> {
        self.values.get(key).cloned()
    }

    fn insert(&mut self, key: K, value: V) {
        if self.values.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
            if self.order.len() > BLOCK_TIME_CACHE_CAPACITY
                && let Some(oldest) = self.order.pop_front()
            {
                self.values.remove(&oldest);
            }
        }
    }
}

type BlockTimes = FifoCache<B256, DateTime<Utc>>;

/// Timeout for one bounded RPC request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Canonical Multicall3 deployment, through which every balance and `addressOf` read is
/// aggregated; `topup run` refuses a chain where it is missing or differs.
pub const MULTICALL3: Address = MULTICALL3_ADDRESS;
/// Calls aggregated into one Multicall3 `eth_call`, bounding its calldata (about 200 bytes per
/// call) and gas (a few thousand per view call) far below provider `eth_call` limits.
pub const MULTICALL_CHUNK: usize = 200;

/// Alloy HTTP client for one RPC provider of one chain, shared by every consumer.
///
/// Errors carry the provider label, never the URL. Every request is bounded by the request
/// timeout, those of the [`FinalizedReader`]s built on it included, and a request that outlasts
/// it fails as a transport error, which every caller retries.
#[derive(Clone)]
pub struct EvmClient {
    provider: RootProvider,
    /// The same transport read with alloy's catch-all network, for receipts: its receipt envelope
    /// takes any EIP-2718 type, such as an OP-stack deposit's (`0x7e`), which the Ethereum
    /// network's refuses.
    receipts: RootProvider<AnyNetwork>,
    endpoint: Redacted,
    request_timeout: Duration,
    labels: CallLabels,
    http: reqwest::Client,
    state: Arc<endpoint::EndpointState>,
    max_log_blocks: u32,
}

impl fmt::Debug for EvmClient {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EvmClient")
            .field("endpoint", &self.endpoint)
            .field("request_timeout", &self.request_timeout)
            .field("labels", &self.labels)
            .finish_non_exhaustive()
    }
}

/// Ethereum and catch-all providers on one HTTP client whose every request is counted under
/// `labels` ([`metrics`]).
fn counted_providers(
    endpoint: &Redacted,
    labels: &CallLabels,
    http: &reqwest::Client,
    state: &Arc<endpoint::EndpointState>,
) -> (RootProvider, RootProvider<AnyNetwork>) {
    let client = ClientBuilder::default()
        .layer(
            alloy::transports::layers::RetryBackoffLayer::new_with_policy(
                2,
                250,
                500,
                endpoint::EndpointRetry(Arc::clone(state)),
            ),
        )
        .layer(CountingLayer::new(labels.clone(), Arc::clone(state)))
        .http_with_client(http.clone(), endpoint.expose().clone());
    (RootProvider::new(client.clone()), RootProvider::new(client))
}

impl EvmClient {
    /// Creates a client with the production request timeout.
    pub fn new(rpc_url: &str) -> Result<Self, ChainError> {
        Self::with_timeout(rpc_url, REQUEST_TIMEOUT)
    }

    /// Creates a client with an explicit request timeout, for tests.
    pub fn with_timeout(rpc_url: &str, request_timeout: Duration) -> Result<Self, ChainError> {
        let endpoint = Redacted::parse(rpc_url).map_err(|_| ChainError::InvalidUrl)?;
        if !matches!(endpoint.expose().scheme(), "http" | "https") {
            return Err(ChainError::InvalidUrl);
        }
        let labels = CallLabels::default();
        let state = Arc::new(endpoint::EndpointState::default());
        let observed = Arc::clone(&state);
        let host = endpoint
            .expose()
            .host_str()
            .ok_or(ChainError::InvalidUrl)?
            .to_owned();
        // Reqwest's documented classifier observes HTTP status before Alloy normalizes error
        // bodies. It disables reqwest's own replays: Alloy is the sole retry layer.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(request_timeout)
            .retry(reqwest::retry::for_host(host).classify_fn(move |reply| {
                if let Some(status) = reply.status() {
                    observed.http_status(status.as_u16());
                    metrics::observe_http_status(status.as_u16());
                }
                reply.success()
            }))
            .build()
            .map_err(|_| ChainError::InvalidUrl)?;
        let (provider, receipts) = counted_providers(&endpoint, &labels, &http, &state);
        Ok(Self {
            provider,
            receipts,
            endpoint,
            request_timeout,
            labels,
            http,
            state,
            max_log_blocks: 3_000,
        })
    }

    /// Labels provider errors and call counters with the configured provider id instead of the
    /// URL.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        let provider = provider.into();
        self.labels.provider.clone_from(&provider);
        self.endpoint = self.endpoint.with_provider(provider);
        (self.provider, self.receipts) =
            counted_providers(&self.endpoint, &self.labels, &self.http, &self.state);
        self
    }

    /// Counts this client's calls under `chain_id`.
    #[must_use]
    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.labels.chain_id = Some(chain_id);
        (self.provider, self.receipts) =
            counted_providers(&self.endpoint, &self.labels, &self.http, &self.state);
        self
    }

    /// Configured chain identity, also checked against independently read transactions.
    pub fn chain_id(&self) -> Option<u64> {
        self.labels.chain_id
    }
    /// Records independently decoded disagreement using bounded labels, never endpoint URLs.
    pub fn disagreement(&self, method: &'static str) {
        let method = if metrics::METHODS.contains(&method) {
            method
        } else {
            "other"
        };
        metrics::record_error(&self.labels, &[method], "disagreement");
        tracing::warn!(tags.alert="TopupRpcDisagreement",provider=%self.labels.provider,chain_id=?self.labels.chain_id,method,"decoded endpoint evidence disagreed; waiting");
    }

    /// Whether a production endpoint passed its independent contract identity check.
    pub fn contract_ready(&self) -> bool {
        self.state.contract_ready()
    }
    /// Change contract readiness only after a dual check; transport successes cannot change it.
    pub fn contract_checked(&self, passed: bool) {
        self.state.contract_checked(passed);
    }
    /// Measured provider-specific log window limit.
    pub fn with_max_log_blocks(mut self, blocks: u32) -> Self {
        self.max_log_blocks = blocks;
        self
    }

    /// Marks the verification endpoint for the UTC daily-credit halt.
    pub fn as_verify(self) -> Self {
        self.state.set_verify();
        self
    }

    /// Endpoint readiness, independent of the API and other chains.
    pub fn ready(&self) -> bool {
        self.state.ready()
    }

    /// Mark this endpoint unavailable after a decoded capability or identity failure.
    pub fn mark_not_ready(&self) {
        self.state.failed();
    }
    /// Read the typed endpoint chain identity.
    pub async fn network_id(&self) -> Result<u64, ChainError> {
        self.bounded("chain identity", self.provider.get_chain_id())
            .await
    }
    /// Measured inclusive log request limit.
    pub fn max_log_blocks(&self) -> u32 {
        self.max_log_blocks
    }
    /// Typed finalized-block transaction, receipt and blockHash getLogs capability check.
    pub async fn check_receipt_logs(&self, number: u64) -> Result<(), ChainError> {
        let block = self
            .bounded(
                "self-test finalized block",
                self.provider
                    .get_block_by_number(BlockNumberOrTag::Number(number)),
            )
            .await?
            .ok_or(ChainError::MissingField("self-test block"))?;
        for hash in block.transactions.hashes().take(20) {
            let reader = FinalizedReader::new(Arc::new(self.clone()));
            let Some(receipt) = reader.receipt(hash).await? else {
                return Err(ChainError::MissingField("self-test receipt"));
            };
            if receipt.block_number != Some(number) || receipt.block_hash != Some(block.header.hash)
            {
                return Err(ChainError::Reorganized("self-test receipt"));
            }
            if let Some(log) = receipt.logs().first() {
                reader.receipt_lookup(hash, 0).await?;
                let logs = self
                    .logs(&Filter::new().at_block_hash(block.header.hash))
                    .await?;
                if !logs.contains(log) {
                    return Err(ChainError::MissingField("self-test receipt log"));
                }
                return Ok(());
            }
        }
        Err(ChainError::MissingField(
            "finalized block with a logged transaction",
        ))
    }

    /// Current-state pins come from this endpoint's own head, two blocks behind latest.
    pub async fn current_pin(&self) -> Result<(u64, B256, u64), ChainError> {
        let latest = self.latest_head().await?;
        self.price_block(BlockNumberOrTag::Number(latest.saturating_sub(2)))
            .await
    }

    /// Reads one addressed or recipient-topic filter, with no endpoint fallback.
    pub async fn logs(&self, filter: &Filter) -> Result<Vec<Log>, ChainError> {
        self.bounded("eth_getLogs", self.provider.get_logs(filter))
            .await
    }

    /// Executes a heterogeneous Multicall3 aggregate at a canonical hash pin.
    pub async fn multicall(
        &self,
        calls: Vec<(Address, Bytes)>,
        hash: B256,
    ) -> Result<Bytes, ChainError> {
        let calls = calls
            .into_iter()
            .map(|(target, call_data)| SnapshotCall {
                target,
                allowFailure: false,
                callData: call_data,
            })
            .collect();
        self.call(
            "snapshot multicall",
            MULTICALL3,
            aggregate3Call { calls }.abi_encode().into(),
            Some(BlockId::hash_canonical(hash)),
        )
        .await
    }

    /// Returns the redacted endpoint, for scheme checks and log labels.
    #[must_use]
    pub const fn endpoint(&self) -> &Redacted {
        &self.endpoint
    }

    /// Returns the bound applied to each request of the bounded methods.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    fn transport(&self, operation: &'static str, error: &TransportError) -> ChainError {
        if let TransportError::Transport(kind) = error
            && kind
                .as_custom()
                .and_then(|error| error.downcast_ref::<reqwest::Error>())
                .is_some_and(reqwest::Error::is_timeout)
        {
            return ChainError::Transport(self.endpoint.timeout_error(operation));
        }
        ChainError::Transport(self.endpoint.rpc_error(operation, error))
    }

    /// Applies the request timeout, leaving the node's own answer to the caller.
    async fn within<T>(
        &self,
        operation: &'static str,
        request: impl IntoFuture<Output = Result<T, TransportError>>,
    ) -> Result<Result<T, TransportError>, ChainError> {
        timeout(self.request_timeout, request)
            .await
            .map_err(|_| ChainError::Transport(self.endpoint.timeout_error(operation)))
    }

    async fn bounded<T>(
        &self,
        operation: &'static str,
        request: impl IntoFuture<Output = Result<T, TransportError>>,
    ) -> Result<T, ChainError> {
        let result = self.within(operation, request).await;
        match result {
            Ok(Ok(value)) => {
                self.state.succeeded();
                Ok(value)
            }
            Ok(Err(error)) => {
                self.state.failed();
                Err(self.transport(operation, &error))
            }
            Err(error) => {
                self.state.failed();
                Err(error)
            }
        }
    }

    /// Aggregates same-typed view calls through Multicall3 `aggregate3` at `block`, one `eth_call`
    /// per [`MULTICALL_CHUNK`] calls, each with `allowFailure = false`, so one failing call fails
    /// the read instead of yielding a partial answer.
    ///
    /// These reads never use JSON-RPC batches or one request per item: public providers throttle
    /// batches far below their single-request limits (Tenderly's public Sepolia gateway refuses
    /// any batch of more than five `eth_call`s with `429 rate limit exceeded`, which failed
    /// reconciliation once a chain had six addresses), and one request per
    /// address grows with every address ever issued.
    async fn aggregate<D: SolCall + 'static>(
        &self,
        operation: &'static str,
        calls: Vec<CallItem<D>>,
        block: BlockId,
    ) -> Result<Vec<D::Return>, ChainError> {
        let mut results = Vec::with_capacity(calls.len());
        let mut calls = calls.into_iter().peekable();
        while calls.peek().is_some() {
            let multicall = self
                .provider
                .multicall()
                .dynamic::<D>()
                .extend_calls(calls.by_ref().take(MULTICALL_CHUNK))
                .block(block);
            let returns = timeout(self.request_timeout, multicall.aggregate3())
                .await
                .map_err(|_| ChainError::Transport(self.endpoint.timeout_error(operation)))?
                .map_err(|error| match error {
                    MulticallError::TransportError(error) => self.transport(operation, &error),
                    other => ChainError::InvalidResponse(format!("{operation}: {other}")),
                })?;
            for returned in returns {
                results.push(returned.map_err(|failure| {
                    ChainError::InvalidResponse(format!(
                        "{operation}: call {} returned undecodable data",
                        failure.idx
                    ))
                })?);
            }
        }
        Ok(results)
    }

    /// Reads code at one explicit canonical hash, shared by startup capability checks.
    pub async fn code_at_id(&self, address: Address, block: BlockId) -> Result<Bytes, ChainError> {
        self.bounded(
            "canonical code",
            self.provider.get_code_at(address).block_id(block),
        )
        .await
    }

    /// Reads ERC-20 balances at one block through Multicall3.
    pub async fn token_balances(
        &self,
        token: Address,
        addresses: &[Address],
        block: impl Into<BlockId>,
    ) -> Result<Vec<U256>, ChainError> {
        let calls = addresses
            .iter()
            .map(|address| CallItem::<balanceOfCall>::new(token, encode_balance_of(*address)))
            .collect();
        self.aggregate("balanceOf multicall", calls, block.into())
            .await
    }

    /// Reads the deterministic forwarder addresses of `treasury` at the latest block through
    /// Multicall3.
    pub async fn factory_addresses(
        &self,
        factory: Address,
        treasury: Address,
        salts: &[B256],
    ) -> Result<Vec<Address>, ChainError> {
        let calls = salts
            .iter()
            .map(|salt| CallItem::<addressOfCall>::new(factory, encode_address_of(treasury, *salt)))
            .collect();
        self.aggregate("addressOf multicall", calls, BlockId::latest())
            .await
    }

    /// Reads the runtime code deployed at `address`.
    pub async fn code_at(&self, address: Address) -> Result<Bytes, ChainError> {
        self.bounded("eth_getCode", self.provider.get_code_at(address))
            .await
    }

    /// Reads the runtime code at `address` as of block `block`.
    pub async fn code_at_block(&self, address: Address, block: u64) -> Result<Bytes, ChainError> {
        self.bounded(
            "eth_getCode",
            self.provider
                .get_code_at(address)
                .block_id(BlockId::number(block)),
        )
        .await
    }

    /// Asks the contract at `account`, as of block `block`, whether `signature` is its signature
    /// of `hash` (EIP-1271): `true` only when `isValidSignature(hash, signature)` returns the
    /// magic value `0x1626ba7e`. A revert or any other return value is `false`; a transport
    /// failure is an error.
    pub async fn is_valid_signature(
        &self,
        account: Address,
        hash: B256,
        signature: Bytes,
        block: impl Into<BlockId>,
    ) -> Result<bool, ChainError> {
        let operation = "isValidSignature call";
        let input = isValidSignatureCall { hash, signature }.abi_encode();
        let tx = TransactionRequest::default()
            .to(account)
            .input(TransactionInput::new(input.into()));
        let output = match self
            .within(operation, self.provider.call(tx).block(block.into()))
            .await?
        {
            Ok(output) => output,
            // The node executed the call and it reverted: the contract refuses the signature.
            Err(TransportError::ErrorResp(ref payload)) if payload.code == 3 => return Ok(false),
            Err(error) => return Err(self.transport(operation, &error)),
        };
        let value = isValidSignatureCall::abi_decode_returns_validate(&output)
            .map_err(|_| ChainError::InvalidResponse("EIP-1271 returned malformed data".into()))?;
        Ok(value.0 == EIP1271_MAGIC_VALUE)
    }

    /// Runs one `eth_call`, at `block` when given and otherwise at the node's default block.
    pub async fn call(
        &self,
        operation: &'static str,
        to: Address,
        input: Bytes,
        block: Option<BlockId>,
    ) -> Result<Bytes, ChainError> {
        let tx = TransactionRequest::default()
            .to(to)
            .input(TransactionInput::new(input));
        match block {
            Some(block) => {
                self.bounded(operation, self.provider.call(tx).block(block))
                    .await
            }
            None => self.bounded(operation, self.provider.call(tx)).await,
        }
    }

    /// Calls one immutable address getter of a forwarder contract.
    pub async fn contract_address(
        &self,
        contract: Address,
        getter: ContractAddressGetter,
    ) -> Result<Address, ChainError> {
        let output = self
            .call(
                "address getter call",
                contract,
                encode_contract_address_getter(getter),
                None,
            )
            .await?;
        decode_contract_address_getter(getter, &output).map_err(|error| {
            ChainError::InvalidResponse(format!("decode {getter:?} result: {error}"))
        })
    }

    /// Returns the finalized block number, or `None` when the node has none.
    pub async fn finalized_block(&self) -> Result<Option<u64>, ChainError> {
        self.bounded(
            "finalized block",
            self.provider
                .get_block_number_by_id(BlockId::Number(BlockNumberOrTag::Finalized)),
        )
        .await
    }

    /// Reads a transaction receipt by hash.
    pub async fn receipt(&self, hash: B256) -> Result<Option<TransactionReceipt>, ChainError> {
        self.bounded(
            "transaction receipt",
            self.provider.get_transaction_receipt(hash),
        )
        .await
    }

    /// Reads a transaction's sender and nonce by hash, pending or included; `None` when the
    /// provider does not know the transaction.
    pub async fn transaction_origin(
        &self,
        hash: B256,
    ) -> Result<Option<(Address, u64)>, ChainError> {
        use alloy::consensus::Transaction as _;
        use alloy::network::TransactionResponse as _;

        let transaction = self
            .bounded("transaction", self.provider.get_transaction_by_hash(hash))
            .await?;
        Ok(transaction.map(|transaction| (transaction.from(), transaction.nonce())))
    }

    /// Returns `account`'s nonce at `block`: the number of its transactions up to that block.
    pub async fn nonce_at(&self, account: Address, block: u64) -> Result<u64, ChainError> {
        self.bounded(
            "nonce",
            self.provider
                .get_transaction_count(account)
                .block_id(BlockId::number(block)),
        )
        .await
    }

    /// Reads a complete numbered/tagged header through the bounded group transport.
    pub async fn price_block(
        &self,
        block: BlockNumberOrTag,
    ) -> Result<(u64, B256, u64), ChainError> {
        let block = self
            .bounded(
                "price block header",
                self.provider.get_block_by_number(block),
            )
            .await?
            .ok_or(ChainError::MissingField("price block"))?;
        Ok((
            block.header.inner.number,
            block.header.hash,
            block.header.inner.timestamp,
        ))
    }

    /// Returns the provider's current `latest` block number, which the head loop polls.
    pub async fn latest_head(&self) -> Result<u64, ChainError> {
        self.bounded("latest head fetch", self.provider.get_block_number())
            .await
    }
}

/// Chain-log reader for one consumer of a shared [`EvmClient`].
///
/// Each consumer keeps its own finalized-head regression guard and caches, so one consumer's
/// observations never change what another consumer reads. Every transfer it returns carries its
/// receipt position and its transaction's sender and nonce, read once per transaction. Each
/// request is bounded by the client's request timeout, so a provider that never answers fails the
/// read with a transport error instead of stalling its caller.
pub struct FinalizedReader {
    client: Arc<EvmClient>,
    finalized_guard: Mutex<FinalizedGuard>,
    block_times: Mutex<BlockTimes>,
    /// What a receipt says about its transaction, by `(tx_hash, block_hash)`.
    receipt_facts: Mutex<FifoCache<(B256, B256), ReceiptFacts>>,
    /// A transaction's sender and nonce, which its hash commits to.
    origins: Mutex<FifoCache<B256, (Address, u64)>>,
}

/// EIP-2718 type of an OP-stack deposit transaction (specs.optimism.io, "Deposits").
const DEPOSIT_TX_TYPE: u8 = 0x7e;

/// What the reader keeps of one receipt.
#[derive(Clone, Debug)]
struct ReceiptFacts {
    /// Block-wide log indexes of the receipt's logs, in receipt order.
    log_indexes: Vec<u64>,
    /// The transaction's sender and nonce when the receipt alone reports them: an OP-stack
    /// deposit's ([`deposit_origin`]).
    deposit_origin: Option<(Address, u64)>,
}

/// The sender and nonce of an OP-stack deposit transaction, from its receipt; `None` for any
/// other transaction type.
///
/// A deposit (type `0x7e`, specs.optimism.io, "Deposits") is derived from an L1 event and carries
/// no signature and no nonce. Its `from` is the L1 caller of `OptimismPortal.depositTransaction`,
/// aliased by adding `0x1111000000000000000000000000000000001111` when that caller is a contract,
/// as for every L1-to-L2 message and bridge mint; the node reports it as the receipt's `from`.
/// "Despite the lack of signature validation, we still increment the nonce of the from account",
/// and the receipt's `depositNonce` is "the nonce value of the from sender as registered before
/// the EVM processing", present on every deposit receipt since Canyon. So `(from, depositNonce)`
/// is the nonce the deposit consumed, as for a signed transaction: once the transaction is out of
/// the chain and `from`'s nonce is past it, it cannot return, since a deposit re-derived after an
/// L1 reorganization has a new source hash, so a new transaction hash, and is scanned as a new
/// transfer. A deposit receipt without `depositNonce` is refused rather than given a nonce.
fn deposit_origin(receipt: &AnyTransactionReceipt) -> Result<Option<(Address, u64)>, ChainError> {
    if receipt.inner.inner.r#type != DEPOSIT_TX_TYPE {
        return Ok(None);
    }
    let nonce = receipt
        .other
        .get_deserialized::<alloy::primitives::U64>("depositNonce")
        .ok_or(ChainError::MissingField("receipt.depositNonce"))?
        .map_err(|error| ChainError::InvalidResponse(format!("receipt.depositNonce: {error}")))?;
    Ok(Some((receipt.from(), nonce.to())))
}

impl fmt::Debug for FinalizedReader {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FinalizedReader")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

/// How a transfer-log request selects recipients.
#[derive(Clone, Copy)]
enum Recipients<'a> {
    /// In the request's recipient topic.
    Topic(&'a [Address]),
    /// Kept locally from every transfer of the requested tokens.
    Local(&'a BTreeSet<Address>),
}

/// A decoded `Transfer` log before its receipt position and transaction origin are known.
struct DecodedTransfer {
    tx_hash: B256,
    log_index: u64,
    block_number: u64,
    block_hash: B256,
    token: Address,
    from: Address,
    to: Address,
    amount: AtomicAmount,
}

impl DecodedTransfer {
    fn complete(
        self,
        receipt_log_index: u64,
        block_time: DateTime<Utc>,
        (tx_from, tx_nonce): (Address, u64),
    ) -> TransferLog {
        TransferLog {
            tx_hash: self.tx_hash,
            receipt_log_index,
            log_index: self.log_index,
            block_number: self.block_number,
            block_hash: self.block_hash,
            block_time,
            tx_from,
            tx_nonce,
            token: self.token,
            from: self.from,
            to: self.to,
            amount: self.amount,
        }
    }
}

impl FinalizedReader {
    /// Creates a reader with a fresh regression guard and caches.
    #[must_use]
    pub fn new(client: Arc<EvmClient>) -> Self {
        Self {
            client,
            finalized_guard: Mutex::new(FinalizedGuard::default()),
            block_times: Mutex::new(BlockTimes::default()),
            receipt_facts: Mutex::new(FifoCache::default()),
            origins: Mutex::new(FifoCache::default()),
        }
    }

    /// Returns the shared client.
    #[must_use]
    pub const fn client(&self) -> &Arc<EvmClient> {
        &self.client
    }

    /// Returns the provider's current `latest` block number (`eth_blockNumber`).
    pub async fn latest_head(&self) -> Result<u64, ChainError> {
        self.client.latest_head().await
    }

    /// Returns the provider's current `safe` block number.
    pub async fn safe_head(&self) -> Result<u64, ChainError> {
        self.tagged_block_number(BlockNumberOrTag::Safe, "safe head fetch")
            .await
    }

    async fn block_time(&self, block_hash: B256) -> Result<DateTime<Utc>, ChainError> {
        let cached = self
            .block_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&block_hash);
        if let Some(time) = cached {
            return Ok(time);
        }
        let block = self
            .client
            .bounded(
                "block timestamp fetch",
                self.client.provider.get_block_by_hash(block_hash),
            )
            .await?
            .ok_or(ChainError::MissingField("block"))?;
        let time = utc_timestamp(block.header.inner.timestamp)?;
        self.block_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(block_hash, time);
        Ok(time)
    }

    /// Reads a receipt of any transaction type, an OP-stack deposit's included.
    async fn receipt(&self, tx_hash: B256) -> Result<Option<AnyTransactionReceipt>, ChainError> {
        self.client
            .bounded(
                "transaction receipt fetch",
                self.client.receipts.get_transaction_receipt(tx_hash),
            )
            .await
    }

    /// The sender and nonce of a signed transaction, read from the transaction; a deposit's come
    /// from its receipt ([`deposit_origin`]). A transaction the provider no longer knows was
    /// reorganized away between the reads.
    async fn origin(&self, tx_hash: B256) -> Result<(Address, u64), ChainError> {
        use alloy::consensus::Transaction as _;
        use alloy::network::TransactionResponse as _;

        let cached = self
            .origins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tx_hash);
        if let Some(origin) = cached {
            return Ok(origin);
        }
        let transaction = self
            .client
            .bounded(
                "transaction fetch",
                self.client.provider.get_transaction_by_hash(tx_hash),
            )
            .await?
            .ok_or(ChainError::Reorganized("transaction fetch"))?;
        let origin = (transaction.from(), transaction.nonce());
        self.origins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tx_hash, origin);
        Ok(origin)
    }

    /// The position of the log with block-wide index `log_index` in its transaction's receipt,
    /// which must be in `block_hash`, and the transaction's origin when the receipt reports it.
    async fn receipt_position(
        &self,
        tx_hash: B256,
        block_hash: B256,
        log_index: u64,
    ) -> Result<(u64, Option<(Address, u64)>), ChainError> {
        let cached = self
            .receipt_facts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(tx_hash, block_hash));
        let facts = match cached {
            Some(facts) => facts,
            None => {
                let receipt = self
                    .receipt(tx_hash)
                    .await?
                    .ok_or(ChainError::Reorganized("receipt fetch"))?;
                if receipt.block_hash != Some(block_hash) {
                    return Err(ChainError::Reorganized("receipt fetch"));
                }
                let log_indexes = receipt
                    .logs()
                    .iter()
                    .map(|log| {
                        log.log_index
                            .ok_or(ChainError::MissingField("log.log_index"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let facts = ReceiptFacts {
                    log_indexes,
                    deposit_origin: deposit_origin(&receipt)?,
                };
                self.receipt_facts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert((tx_hash, block_hash), facts.clone());
                facts
            }
        };
        let position = facts
            .log_indexes
            .iter()
            .position(|index| *index == log_index)
            .ok_or(ChainError::MissingField("receipt log"))?;
        let position = u64::try_from(position)
            .map_err(|_| ChainError::MissingField("receipt log position"))?;
        Ok((position, facts.deposit_origin))
    }

    async fn complete(&self, decoded: DecodedTransfer) -> Result<TransferLog, ChainError> {
        let block_time = self.block_time(decoded.block_hash).await?;
        let (position, deposit_origin) = self
            .receipt_position(decoded.tx_hash, decoded.block_hash, decoded.log_index)
            .await?;
        let origin = match deposit_origin {
            Some(origin) => origin,
            None => self.origin(decoded.tx_hash).await?,
        };
        Ok(decoded.complete(position, block_time, origin))
    }

    async fn transfer_logs_request(
        &self,
        tokens: &[Address],
        recipients: Recipients<'_>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        let logs = self
            .raw_transfer_logs(tokens, recipients, from_block, to_block)
            .await?;
        self.complete_logs(logs, recipients).await
    }
    async fn raw_transfer_logs(
        &self,
        tokens: &[Address],
        recipients: Recipients<'_>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>, ChainError> {
        let mut filter = Filter::new()
            .from_block(from_block)
            .to_block(to_block)
            .event_signature(Transfer::SIGNATURE_HASH);
        if let Recipients::Topic(addresses) = recipients {
            filter = filter.topic2(
                addresses
                    .iter()
                    .copied()
                    .fold(Topic::default(), Topic::extend),
            );
        }
        if !tokens.is_empty() {
            filter = filter.address(tokens.to_vec());
        }
        self.client
            .bounded("transfer log fetch", self.client.provider.get_logs(&filter))
            .await
    }
    async fn complete_logs(
        &self,
        logs: Vec<Log>,
        recipients: Recipients<'_>,
    ) -> Result<Vec<TransferLog>, ChainError> {
        let mut transfers = Vec::new();
        for log in logs {
            let Some(decoded) = decode_transfer_log(&log)? else {
                continue;
            };
            if let Recipients::Local(kept) = recipients
                && !kept.contains(&decoded.to)
            {
                continue;
            }
            transfers.push(self.complete(decoded).await?);
        }
        Ok(transfers)
    }

    async fn transfer_logs(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        if from_block > to_block {
            return Err(ChainError::InvalidRange {
                from_block,
                to_block,
            });
        }
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let mut transfers = Vec::new();
        for (window_from, window_to) in
            log_windows(from_block, to_block, self.client.max_log_blocks)?
        {
            for batch in addresses.chunks(MAX_ADDRESSES_PER_REQUEST) {
                transfers.extend(
                    self.transfer_logs_request(
                        &[],
                        Recipients::Topic(batch),
                        window_from,
                        window_to,
                    )
                    .await?,
                );
            }
        }
        Ok(transfers)
    }

    /// Every factory event in the range, one request whatever the forwarder count, kept when it
    /// is about one of `forwarders`.
    async fn factory_logs_request(
        &self,
        factory: Address,
        forwarders: &BTreeSet<Address>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        let filter = Filter::new()
            .address(factory)
            .from_block(from_block)
            .to_block(to_block)
            .event_signature(factory_event_signatures().to_vec());
        let logs = self
            .client
            .bounded("factory log fetch", self.client.provider.get_logs(&filter))
            .await?;
        let mut kept = Vec::new();
        for log in &logs {
            let decoded = decode_factory_log(log)?;
            if forwarders.contains(&decoded.event.forwarder()) {
                kept.push(decoded);
            }
        }
        Ok(kept)
    }

    async fn tagged_block_number(
        &self,
        tag: BlockNumberOrTag,
        operation: &'static str,
    ) -> Result<u64, ChainError> {
        Ok(self
            .client
            .bounded(operation, self.client.provider.get_block_by_number(tag))
            .await?
            .ok_or(ChainError::MissingField("tagged block"))?
            .header
            .inner
            .number)
    }
}

fn utc_timestamp(timestamp: u64) -> Result<DateTime<Utc>, ChainError> {
    i64::try_from(timestamp)
        .ok()
        .and_then(|value| DateTime::from_timestamp(value, 0))
        .ok_or(ChainError::InvalidTimestamp(timestamp))
}

fn decode_transfer_log(log: &Log) -> Result<Option<DecodedTransfer>, ChainError> {
    let topic_count = log.topics().len();
    let data_length = log.data().data.len();
    if topic_count != 3 || data_length != 32 {
        tracing::warn!(
            transaction_hash = ?log.transaction_hash,
            log_index = ?log.log_index,
            topic_count,
            data_length,
            "skipping Transfer log with a non-ERC-20 layout"
        );
        return Ok(None);
    }
    let decoded = match log.log_decode_validate::<Transfer>() {
        Ok(decoded) => decoded,
        Err(error) => {
            tracing::warn!(
                transaction_hash = ?log.transaction_hash,
                log_index = ?log.log_index,
                %error,
                "skipping invalid ERC-20 Transfer log"
            );
            return Ok(None);
        }
    };
    Ok(Some(DecodedTransfer {
        tx_hash: decoded
            .transaction_hash
            .ok_or(ChainError::MissingField("log.transaction_hash"))?,
        log_index: decoded
            .log_index
            .ok_or(ChainError::MissingField("log.log_index"))?,
        block_number: decoded
            .block_number
            .ok_or(ChainError::MissingField("log.block_number"))?,
        block_hash: decoded
            .block_hash
            .ok_or(ChainError::MissingField("log.block_hash"))?,
        token: decoded.address(),
        from: decoded.inner.data.from,
        to: decoded.inner.data.to,
        amount: AtomicAmount::new(decoded.inner.data.amount),
    }))
}

fn decode_factory_log(log: &Log) -> Result<FactoryLog, ChainError> {
    let event = FactoryEvent::decode(log.data())
        .map_err(|error| ChainError::InvalidResponse(format!("factory event: {error}")))?;
    Ok(FactoryLog {
        tx_hash: log
            .transaction_hash
            .ok_or(ChainError::MissingField("log.transaction_hash"))?,
        log_index: log
            .log_index
            .ok_or(ChainError::MissingField("log.log_index"))?,
        block_number: log
            .block_number
            .ok_or(ChainError::MissingField("log.block_number"))?,
        block_hash: log
            .block_hash
            .ok_or(ChainError::MissingField("log.block_hash"))?,
        event,
    })
}

/// Whether `log` is an ERC-20 `Transfer` event: the signature topic and the ERC-20 layout.
fn is_transfer(log: &Log) -> bool {
    log.topics().first() == Some(&Transfer::SIGNATURE_HASH)
}

impl ChainReader for FinalizedReader {
    async fn header(&self, number: u64) -> Result<(B256, DateTime<Utc>), ChainError> {
        let (height, hash, timestamp) = self
            .client
            .price_block(BlockNumberOrTag::Number(number))
            .await?;
        if height != number {
            return Err(ChainError::Reorganized("numbered header"));
        }
        Ok((hash, utc_timestamp(timestamp)?))
    }
    async fn coverage_logs(
        &self,
        factory: Address,
        addresses: &[Address],
        from: u64,
        to: u64,
    ) -> Result<(Vec<TransferLog>, Vec<FactoryLog>), ChainError> {
        let mut transfers = Vec::new();
        let mut events = Vec::new();
        let mut signatures = factory_event_signatures().to_vec();
        signatures.push(Transfer::SIGNATURE_HASH);
        for (start, end) in log_windows(from, to, self.client.max_log_blocks)? {
            for chunk in addresses.chunks(MAX_ADDRESSES_PER_REQUEST) {
                let filter = Filter::new()
                    .from_block(start)
                    .to_block(end)
                    .event_signature(signatures.clone())
                    .topic2(chunk.iter().copied().fold(Topic::default(), Topic::extend));
                for log in self.client.logs(&filter).await? {
                    if is_transfer(&log) {
                        if let Some(decoded) = decode_transfer_log(&log)? {
                            if chunk.contains(&decoded.to) {
                                transfers.push(self.complete(decoded).await?);
                            }
                        }
                    } else if log.address() == factory {
                        let event = decode_factory_log(&log)?;
                        if chunk.contains(&event.event.forwarder()) {
                            events.push(event);
                        }
                    }
                }
            }
        }
        Ok((transfers, events))
    }
    async fn factory_receipt(
        &self,
        tx: B256,
        factory: Address,
    ) -> Result<Option<FactoryReceipt>, ChainError> {
        let Some(receipt) = self.receipt(tx).await? else {
            return Ok(None);
        };
        let number = receipt
            .block_number
            .ok_or(ChainError::MissingField("receipt.block_number"))?;
        let hash = receipt
            .block_hash
            .ok_or(ChainError::MissingField("receipt.block_hash"))?;
        if receipt.transaction_hash != tx {
            return Err(ChainError::Reorganized("factory receipt"));
        }
        let (canonical, block_time) = self.header(number).await?;
        if hash != canonical {
            return Err(ChainError::Reorganized("factory header"));
        }
        // Read the transaction independently even for factory events with no token transfer.
        let origin = match deposit_origin(&receipt)? {
            Some(origin) => origin,
            None => self.origin(tx).await?,
        };
        let mut logs = Vec::new();
        if receipt.status() {
            for log in receipt.logs() {
                if log.address() == factory
                    && log
                        .topics()
                        .first()
                        .is_some_and(|t| factory_event_signatures().contains(t))
                {
                    if log.transaction_hash != Some(tx)
                        || log.block_hash != Some(hash)
                        || log.block_number != Some(number)
                    {
                        return Err(ChainError::Reorganized("factory log identity"));
                    }
                    logs.push(decode_factory_log(log)?);
                }
            }
        }
        Ok(Some(FactoryReceipt {
            status: receipt.status(),
            block_number: number,
            block_hash: hash,
            block_time,
            origin,
            logs,
        }))
    }
    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(self.finalized_header().await?.0)
    }

    async fn latest_header(&self) -> Result<FinalizedHead, ChainError> {
        let (number, _, timestamp) = self.client.price_block(BlockNumberOrTag::Latest).await?;
        Ok(FinalizedHead {
            number,
            time: utc_timestamp(timestamp)?,
        })
    }

    async fn finalized_header(&self) -> Result<(FinalizedHead, B256), ChainError> {
        let block = self
            .client
            .bounded(
                "finalized head fetch",
                self.client
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Finalized),
            )
            .await?
            .ok_or(ChainError::MissingField("finalized block"))?;
        let current = block.header.inner.number;
        let time = utc_timestamp(block.header.inner.timestamp)?;
        self.finalized_guard
            .lock()
            .map_err(|_| ChainError::HealthStateUnavailable)?
            .observe(current)?;
        Ok((
            FinalizedHead {
                number: current,
                time,
            },
            block.header.hash,
        ))
    }

    async fn confirmation_heads(
        &self,
        confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        let mut heads = ChainHeads::default();
        if confirmations.needs_latest() {
            heads.latest = Some(self.client.latest_head().await?);
        } else if confirmations.needs_safe() {
            heads.safe = Some(self.safe_head().await?);
        } else {
            heads.finalized = ChainReader::finalized_head(self).await?.number;
        }
        Ok(heads)
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        self.transfer_logs(addresses, from_block, to_block).await
    }

    async fn token_transfers(
        &self,
        tokens: &[Address],
        recipients: &BTreeSet<Address>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        if from_block > to_block {
            return Err(ChainError::InvalidRange {
                from_block,
                to_block,
            });
        }
        let mut transfers = Vec::new();
        if tokens.is_empty() || recipients.is_empty() {
            return Ok(transfers);
        }
        for (window_from, window_to) in
            log_windows(from_block, to_block, self.client.max_log_blocks)?
        {
            transfers.extend(
                self.transfer_logs_request(
                    tokens,
                    Recipients::Local(recipients),
                    window_from,
                    window_to,
                )
                .await?,
            );
        }
        Ok(transfers)
    }

    async fn factory_logs(
        &self,
        factory: Address,
        forwarders: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        if from_block > to_block {
            return Err(ChainError::InvalidRange {
                from_block,
                to_block,
            });
        }
        let mut logs = Vec::new();
        if forwarders.is_empty() {
            return Ok(logs);
        }
        let forwarders = forwarders.iter().copied().collect::<BTreeSet<_>>();
        for (window_from, window_to) in
            log_windows(from_block, to_block, self.client.max_log_blocks)?
        {
            logs.extend(
                self.factory_logs_request(factory, &forwarders, window_from, window_to)
                    .await?,
            );
        }
        Ok(logs)
    }

    async fn receipt_transfer(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        self.receipt_lookup(tx_hash, receipt_log_index).await
    }

    async fn nonce_at(&self, account: Address, block: u64) -> Result<u64, ChainError> {
        self.client
            .bounded(
                "nonce fetch",
                self.client
                    .provider
                    .get_transaction_count(account)
                    .block_id(BlockId::number(block)),
            )
            .await
    }
}

impl FinalizedReader {
    async fn receipt_lookup(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        let Some(receipt) = self.receipt(tx_hash).await? else {
            return Ok(ReceiptLookup::Missing);
        };
        let block_number = receipt
            .block_number
            .ok_or(ChainError::MissingField("receipt.block_number"))?;
        let block_hash = receipt
            .block_hash
            .ok_or(ChainError::MissingField("receipt.block_hash"))?;
        if receipt.transaction_hash != tx_hash {
            return Err(ChainError::Reorganized("receipt transaction hash"));
        }
        let log = usize::try_from(receipt_log_index)
            .ok()
            .and_then(|position| receipt.logs().get(position));
        let decoded = match log {
            Some(log) if receipt.status() && is_transfer(log) => {
                if log.transaction_hash != Some(tx_hash)
                    || log.block_number != Some(block_number)
                    || log.block_hash != Some(block_hash)
                {
                    return Err(ChainError::MissingField("receipt.log_identity"));
                }
                decode_transfer_log(log)?
            }
            _ => None,
        };
        let block = self
            .client
            .bounded(
                "receipt block header",
                self.client.provider.get_block_by_hash(block_hash),
            )
            .await?
            .ok_or(ChainError::MissingField("receipt block header"))?;
        if block.header.inner.number != block_number || block.header.hash != block_hash {
            return Err(ChainError::Reorganized("receipt block header"));
        }
        let block_time = utc_timestamp(block.header.inner.timestamp)?;
        let origin = match deposit_origin(&receipt)? {
            Some(origin) => origin,
            None => {
                use alloy::consensus::Transaction as _;
                use alloy::network::TransactionResponse as _;
                let transaction = self
                    .client
                    .bounded(
                        "receipt transaction",
                        self.client.provider.get_transaction_by_hash(tx_hash),
                    )
                    .await?
                    .ok_or(ChainError::Reorganized("receipt transaction"))?;
                if transaction.tx_hash() != tx_hash
                    || transaction.from() != receipt.from()
                    || transaction.block_hash != Some(block_hash)
                    || transaction.block_number != Some(block_number)
                    || self
                        .client
                        .labels
                        .chain_id
                        .is_some_and(|chain| transaction.chain_id() != Some(chain))
                {
                    return Err(ChainError::Reorganized("receipt transaction identity"));
                }
                (transaction.from(), transaction.nonce())
            }
        };
        let transfer = decoded
            .map(|decoded| Box::new(decoded.complete(receipt_log_index, block_time, origin)));
        Ok(ReceiptLookup::Included {
            block_number,
            block_hash,
            status: receipt.status(),
            block_time,
            tx_from: origin.0,
            tx_nonce: origin.1,
            transfer,
        })
    }
}

#[cfg(test)]
fn block_windows(from_block: u64, to_block: u64) -> Result<Vec<(u64, u64)>, ChainError> {
    log_windows(
        from_block,
        to_block,
        u32::try_from(MAX_BLOCKS_PER_REQUEST)
            .map_err(|_| ChainError::MissingField("window limit"))?,
    )
}
/// Split inclusive coverage ranges at the measured per-endpoint limit.
pub fn log_windows(
    from_block: u64,
    to_block: u64,
    limit: u32,
) -> Result<Vec<(u64, u64)>, ChainError> {
    if from_block > to_block || limit == 0 {
        return Err(ChainError::InvalidRange {
            from_block,
            to_block,
        });
    }
    let mut windows = Vec::new();
    let mut start = from_block;
    loop {
        let end = start
            .saturating_add(u64::from(limit).saturating_sub(1))
            .min(to_block);
        windows.push((start, end));
        if end == to_block {
            break;
        }
        start = end.checked_add(1).ok_or(ChainError::InvalidRange {
            from_block,
            to_block,
        })?;
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256};
    use serde_json::Value;

    use super::*;

    #[test]
    fn block_times_are_keyed_by_hash_and_bounded() {
        let mut times = BlockTimes::default();
        let time = |seconds| DateTime::from_timestamp(seconds, 0).expect("timestamp");
        let hash = |index: usize| B256::from(alloy::primitives::U256::from(index));
        for index in 0..=BLOCK_TIME_CACHE_CAPACITY {
            times.insert(hash(index), time(i64::try_from(index).expect("index")));
        }
        assert_eq!(times.values.len(), BLOCK_TIME_CACHE_CAPACITY);
        assert_eq!(times.get(&hash(0)), None, "oldest entry is evicted");
        assert_eq!(times.get(&hash(1)), Some(time(1)));
        // A different block at the same height has a different hash and never shares a time.
        assert_eq!(times.get(&hash(BLOCK_TIME_CACHE_CAPACITY + 1)), None);
    }

    #[test]
    fn windows_are_inclusive_and_never_exceed_two_thousand_blocks() {
        assert_eq!(block_windows(7, 7).expect("valid range"), vec![(7, 7)]);
        assert_eq!(
            block_windows(10, 4_010).expect("valid range"),
            vec![(10, 2_009), (2_010, 4_009), (4_010, 4_010)]
        );
    }

    /// Answers recorded from Base Sepolia's provider A (the Tenderly gateway) on 2026-09-29.
    fn base_sepolia(name: &str) -> Value {
        let json = match name {
            "block-47297199" => {
                include_str!("../../../tests/fixtures/base-sepolia/block-47297199.json")
            }
            "block-47445875" => {
                include_str!("../../../tests/fixtures/base-sepolia/block-47445875.json")
            }
            "bridge-mint-logs" => {
                include_str!("../../../tests/fixtures/base-sepolia/bridge-mint-logs.json")
            }
            "bridge-mint-receipt" => {
                include_str!("../../../tests/fixtures/base-sepolia/bridge-mint-receipt.json")
            }
            "l1-attributes-receipt" => {
                include_str!("../../../tests/fixtures/base-sepolia/l1-attributes-receipt.json")
            }
            "finalized-47446540" => {
                include_str!("../../../tests/fixtures/base-sepolia/finalized-47446540.json")
            }
            "finalized-47446696" => {
                include_str!("../../../tests/fixtures/base-sepolia/finalized-47446696.json")
            }
            "receipt" => include_str!("../../../tests/fixtures/base-sepolia/receipt.json"),
            "transaction" => include_str!("../../../tests/fixtures/base-sepolia/transaction.json"),
            "transfer-logs" => {
                include_str!("../../../tests/fixtures/base-sepolia/transfer-logs.json")
            }
            other => panic!("no fixture {other}"),
        };
        serde_json::from_str(json).expect("fixture is JSON")
    }

    /// Serves each JSON-RPC method its recorded answer, and the `finalized` block reads the
    /// recorded heads in turn.
    async fn replay_node(
        answers: Vec<(&'static str, Value)>,
        finalized: Vec<Value>,
    ) -> (
        FinalizedReader,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        use axum::routing::post;
        use axum::{Json, Router};

        let answers = Arc::new(answers.into_iter().collect::<HashMap<_, _>>());
        let finalized = Arc::new(Mutex::new(VecDeque::from(finalized)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        let node = Router::new().route(
            "/rpc",
            post(move |Json(request): Json<Value>| async move {
                let method = request["method"].as_str().unwrap_or_default();
                let result =
                    if method == "eth_getBlockByNumber" && request["params"][0] == "finalized" {
                        finalized.lock().expect("finalized answers").pop_front()
                    } else {
                        answers.get(method).cloned()
                    };
                Json(match result {
                    Some(result) => {
                        serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                    }
                    None => serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                        "code": -32601, "message": format!("{method} not recorded")}}),
                })
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, node).await });
        let client = EvmClient::new(&format!("http://{address}/rpc"))
            .expect("production adapter accepts URL")
            .with_provider("base-sepolia-a");
        (FinalizedReader::new(Arc::new(client)), server)
    }

    /// A real L1-to-L2 bridge mint on Base Sepolia: the deposit transaction (type `0x7e`)
    /// `0x63fa…cb88`, from the aliased L1CrossDomainMessenger (`0xC34855F4De64F1840e5686e64278da901e261f20`
    /// plus `0x1111000000000000000000000000000000001111`), mints 500 units to `0x3c0f…ed34`.
    const BRIDGE_MINT: B256 =
        b256!("0x63fac1334834dc776a3fc9a524efa886fb365d170e2f50acb4524f08456ecb88");

    fn bridge_mint_transfer() -> TransferLog {
        TransferLog {
            tx_hash: BRIDGE_MINT,
            receipt_log_index: 0,
            log_index: 1,
            block_number: 47_297_199,
            block_hash: b256!("0x92536ca815a609fb99feaab4d6339ce4fa0ea7174328e013ade3e2241f131125"),
            block_time: DateTime::parse_from_rfc3339("2026-09-25T18:58:06Z")
                .expect("time")
                .into(),
            tx_from: address!("0xd45955f4de64f1840e5686e64278da901e263031"),
            // The receipt's `depositNonce`: the sender's nonce before the deposit ran.
            tx_nonce: 724_602,
            token: address!("0x94b1febed36174422fdb48000c620fc8432b7121"),
            from: Address::ZERO,
            to: address!("0x3c0fe91b38c2f708d360f5724208fa7ecaa6ed34"),
            amount: AtomicAmount::new(U256::from(500)),
        }
    }

    // The node records no `eth_getTransactionByHash` answer: the Ethereum network's transaction
    // types refuse a deposit, and its origin is read from the receipt alone.
    #[tokio::test]
    async fn an_op_stack_deposit_transfer_takes_its_origin_from_the_receipt() {
        let answers = vec![
            ("eth_getLogs", base_sepolia("bridge-mint-logs")),
            (
                "eth_getTransactionReceipt",
                base_sepolia("bridge-mint-receipt"),
            ),
            ("eth_getBlockByHash", base_sepolia("block-47297199")),
        ];
        let (scanner, scanner_node) = replay_node(answers.clone(), Vec::new()).await;
        let (confirm, confirm_node) = replay_node(answers, Vec::new()).await;
        let recipient = address!("0x3c0fe91b38c2f708d360f5724208fa7ecaa6ed34");
        let logs = scanner
            .transfer_logs_to(&[recipient], 47_297_199, 47_297_199)
            .await;
        let lookup = confirm.receipt_transfer(BRIDGE_MINT, 0).await;
        scanner_node.abort();
        confirm_node.abort();

        let transfer = bridge_mint_transfer();
        assert_eq!(logs.expect("transfer logs"), vec![transfer.clone()]);
        assert_eq!(
            lookup.expect("receipt lookup"),
            ReceiptLookup::Included {
                block_number: 47_297_199,
                block_hash: transfer.block_hash,
                status: true,
                block_time: transfer.block_time,
                tx_from: transfer.tx_from,
                tx_nonce: transfer.tx_nonce,
                transfer: Some(Box::new(transfer)),
            }
        );
    }

    // Base Sepolia's L1 attributes deposit, the first transaction of the incident's block: a
    // system deposit without logs, which the confirm step may read at any receipt position.
    #[tokio::test]
    async fn an_op_stack_deposit_receipt_without_a_transfer_is_included() {
        let (reader, node) = replay_node(
            vec![
                (
                    "eth_getTransactionReceipt",
                    base_sepolia("l1-attributes-receipt"),
                ),
                ("eth_getBlockByHash", base_sepolia("block-47445875")),
            ],
            Vec::new(),
        )
        .await;
        let lookup = reader
            .receipt_transfer(
                b256!("0x492bed3f97b8258ada814f12d8d87d4df6f6668b112597f1b14c245e06643421"),
                0,
            )
            .await;
        node.abort();

        assert!(matches!(
            lookup.expect("receipt lookup"),
            ReceiptLookup::Included {
                block_number: 47_445_875,
                transfer: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_deposit_receipt_without_its_nonce_is_refused() {
        let mut receipt = base_sepolia("bridge-mint-receipt");
        receipt
            .as_object_mut()
            .expect("receipt object")
            .remove("depositNonce");
        let (reader, node) = replay_node(
            vec![
                ("eth_getLogs", base_sepolia("bridge-mint-logs")),
                ("eth_getTransactionReceipt", receipt),
                ("eth_getBlockByHash", base_sepolia("block-47297199")),
            ],
            Vec::new(),
        )
        .await;
        let recipient = address!("0x3c0fe91b38c2f708d360f5724208fa7ecaa6ed34");
        let logs = reader
            .transfer_logs_to(&[recipient], 47_297_199, 47_297_199)
            .await;
        node.abort();

        assert_eq!(logs, Err(ChainError::MissingField("receipt.depositNonce")));
    }

    /// Accepts every request and never answers, like a provider whose connection stalls.
    async fn hanging_node(
        request_timeout: Duration,
    ) -> (
        FinalizedReader,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        use axum::Router;
        use axum::routing::post;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        let node = Router::new().route("/rpc", post(std::future::pending::<String>));
        let server = tokio::spawn(async move { axum::serve(listener, node).await });
        let client = EvmClient::with_timeout(&format!("http://{address}/rpc"), request_timeout)
            .expect("production adapter accepts URL")
            .with_provider("provider-a");
        (FinalizedReader::new(Arc::new(client)), server)
    }

    #[tokio::test]
    async fn every_reader_request_times_out_instead_of_hanging() {
        let (reader, node) = hanging_node(Duration::from_millis(200)).await;
        let address = Address::repeat_byte(1);
        let reads = async {
            let tokens = BTreeSet::from([address]);
            vec![
                reader.latest_head().await.map(drop),
                reader.safe_head().await.map(drop),
                reader.finalized_head().await.map(drop),
                reader.transfer_logs_to(&[address], 1, 1).await.map(drop),
                reader
                    .token_transfers(&[address], &tokens, 1, 1)
                    .await
                    .map(drop),
                reader
                    .factory_logs(address, &[address], 1, 1)
                    .await
                    .map(drop),
                reader.receipt_transfer(B256::ZERO, 0).await.map(drop),
                reader.nonce_at(address, 1).await.map(drop),
            ]
        };
        let results = timeout(Duration::from_secs(10), reads)
            .await
            .expect("every read is bounded");
        node.abort();

        for result in results {
            let error = result.expect_err("a provider that never answers fails the read");
            assert!(matches!(error, ChainError::Transport(_)), "{error:?}");
            assert!(error.to_string().contains("timeout"), "{error}");
            assert!(!error.is_rate_limited(), "{error}");
        }
    }

    // On 2026-09-29 the gateway answered `finalized` 47446696, then 47446540 for minutes, while
    // provider B answered 47446696 with the same hash: a lagging node, not a finality violation.
    // Marking the provider unusable for good stopped Base Sepolia's scanner.
    #[tokio::test]
    async fn a_gateway_flapping_between_finalized_heads_is_refused_then_used_again() {
        let (reader, server) = replay_node(
            Vec::new(),
            vec![
                base_sepolia("finalized-47446696"),
                base_sepolia("finalized-47446540"),
                base_sepolia("finalized-47446540"),
                base_sepolia("finalized-47446696"),
            ],
        )
        .await;
        let mut heads = Vec::new();
        for _ in 0..4 {
            heads.push(reader.finalized_head().await.map(|head| head.number));
        }
        server.abort();

        let stale = Err(ChainError::FinalizedHeadRegressed {
            previous: 47_446_696,
            current: 47_446_540,
        });
        assert_eq!(
            heads,
            vec![Ok(47_446_696), stale.clone(), stale, Ok(47_446_696)]
        );
    }

    // The incident's payment, read as the per-block scan and the confirm step read it: a type-2
    // transaction whose OP-stack receipt carries the L1 fee fields, with `blockTimestamp` on the log.
    #[tokio::test]
    async fn base_sepolia_transfer_decodes_from_recorded_answers() {
        let answers = vec![
            ("eth_getLogs", base_sepolia("transfer-logs")),
            ("eth_getTransactionReceipt", base_sepolia("receipt")),
            ("eth_getTransactionByHash", base_sepolia("transaction")),
            ("eth_getBlockByHash", base_sepolia("block-47445875")),
        ];
        let (scanner, scanner_node) = replay_node(answers.clone(), Vec::new()).await;
        let (confirm, confirm_node) = replay_node(answers, Vec::new()).await;
        let recipient = address!("0xfa810b787da3f2ca13fc13082762e78c4104ab10");
        let tx_hash = b256!("0x4b6cf1a33019535930118d535e51966a0405d78874213f5e34e5df2eb223902f");
        let logs = scanner
            .transfer_logs_to(&[recipient], 47_445_875, 47_445_875)
            .await;
        let lookup = confirm.receipt_transfer(tx_hash, 0).await;
        scanner_node.abort();
        confirm_node.abort();

        let block_hash =
            b256!("0xcac908304ca374430510276e42beb9f0f28596b6d925097e9b192913c6d195a2");
        let payer = address!("0x1d49cc344c26be92c0f941064412dd258f026b96");
        let transfer = TransferLog {
            tx_hash,
            receipt_log_index: 0,
            log_index: 35,
            block_number: 47_445_875,
            block_hash,
            block_time: DateTime::parse_from_rfc3339("2026-09-29T05:33:58Z")
                .expect("time")
                .into(),
            tx_from: payer,
            tx_nonce: 4,
            token: address!("0x1a6f260377e42ead1418c7c1afdfd5de371a9284"),
            from: payer,
            to: recipient,
            amount: AtomicAmount::new(U256::from(81_209_600_000_000_000_000_u128)),
        };
        assert_eq!(logs.expect("transfer logs"), vec![transfer.clone()]);
        assert_eq!(
            lookup.expect("receipt lookup"),
            ReceiptLookup::Included {
                block_number: 47_445_875,
                block_hash,
                status: true,
                block_time: transfer.block_time,
                tx_from: transfer.tx_from,
                tx_nonce: transfer.tx_nonce,
                transfer: Some(Box::new(transfer)),
            }
        );
    }

    #[test]
    fn address_chunks_never_exceed_one_thousand() {
        let addresses = vec![Address::ZERO; 2_001];
        let sizes = addresses
            .chunks(MAX_ADDRESSES_PER_REQUEST)
            .map(<[Address]>::len)
            .collect::<Vec<_>>();
        assert_eq!(sizes, vec![1_000, 1_000, 1]);
    }

    #[test]
    fn configuration_never_echoes_an_invalid_url() {
        let secret = "not a url with api-key=secret";
        let error = EvmClient::new(secret).expect_err("invalid URL must fail");
        assert_eq!(error, ChainError::InvalidUrl);
        assert!(!error.to_string().contains(secret));
    }

    const SECRET: &str = "rpc-secret-token";

    /// Serves every JSON-RPC request with `status` and the JSON-RPC `error` object, behind a
    /// URL carrying `SECRET` in its credentials and query.
    async fn mock_node(
        status: u16,
        error: &'static str,
    ) -> (EvmClient, tokio::task::JoinHandle<std::io::Result<()>>) {
        use axum::Router;
        use axum::http::{StatusCode, header};
        use axum::routing::post;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        let status = StatusCode::from_u16(status).expect("valid status");
        let body = format!(r#"{{"jsonrpc":"2.0","id":0,"error":{error}}}"#);
        let node = Router::new().route(
            "/rpc",
            post(
                move || async move { (status, [(header::CONTENT_TYPE, "application/json")], body) },
            ),
        );
        let server = tokio::spawn(async move { axum::serve(listener, node).await });
        let client = EvmClient::new(&format!(
            "http://user:{SECRET}@{address}/rpc?api_key={SECRET}"
        ))
        .expect("production adapter accepts URL")
        .with_provider("provider-a");
        (client, server)
    }

    /// One `eth_call` the mock node answered: the block tag and each aggregated call's
    /// `allowFailure` flag.
    type ObservedCall = (Value, Vec<bool>);

    /// Answers like Tenderly's public Sepolia gateway (staging's provider A): a JSON-RPC batch
    /// carrying more than five `eth_call`s is refused with `429` and one `-32005` object, while
    /// single requests are served. A single `eth_call` must be Multicall3 `aggregate3`; every
    /// aggregated call returns the word `1`, and the node records what it answered.
    async fn batch_capped_node() -> (
        EvmClient,
        Arc<Mutex<Vec<ObservedCall>>>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        use alloy::providers::bindings::IMulticall3::{Result as Call3Result, aggregate3Call};
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::{Json, Router};

        fn answer(request: &Value, observed: &Mutex<Vec<ObservedCall>>) -> Value {
            let transaction = &request["params"][0];
            let input = transaction
                .get("input")
                .or_else(|| transaction.get("data"))
                .and_then(Value::as_str)
                .and_then(|input| input.parse::<Bytes>().ok());
            let aggregate = input.and_then(|input| aggregate3Call::abi_decode(&input).ok());
            let (true, Some(aggregate)) = (
                transaction["to"].as_str().and_then(|to| to.parse().ok()) == Some(MULTICALL3),
                aggregate,
            ) else {
                return serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                    "code": -32000, "message": "only Multicall3 aggregate3 is served"}});
            };
            observed.lock().expect("observed calls").push((
                request["params"][1].clone(),
                aggregate
                    .calls
                    .iter()
                    .map(|call| call.allowFailure)
                    .collect(),
            ));
            let word = Bytes::from(U256::from(1).to_be_bytes::<32>());
            let results = aggregate
                .calls
                .iter()
                .map(|_| Call3Result {
                    success: true,
                    returnData: word.clone(),
                })
                .collect::<Vec<_>>();
            let output = Bytes::from(aggregate3Call::abi_encode_returns(&results));
            serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": output})
        }

        let observed = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener binds");
        let address = listener.local_addr().expect("listener address");
        let node = Router::new().route(
            "/rpc",
            post({
                let observed = Arc::clone(&observed);
                move |Json(body): Json<Value>| async move {
                    let Some(batch) = body.as_array() else {
                        return (StatusCode::OK, Json(answer(&body, &observed)));
                    };
                    let calls = batch
                        .iter()
                        .filter(|request| request["method"] == "eth_call")
                        .count();
                    if calls > 5 {
                        let refusal = serde_json::json!({"jsonrpc": "2.0", "id": 0, "error": {
                            "code": -32005, "message": "rate limit exceeded"}});
                        return (StatusCode::TOO_MANY_REQUESTS, Json(refusal));
                    }
                    let answers = batch
                        .iter()
                        .map(|request| answer(request, &observed))
                        .collect();
                    (StatusCode::OK, Json(answers))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, node).await });
        let client = EvmClient::new(&format!("http://{address}/rpc"))
            .expect("production adapter accepts URL")
            .with_provider("provider-a");
        (client, observed, server)
    }

    #[tokio::test]
    async fn balance_and_address_reads_never_depend_on_provider_batch_limits() {
        let (client, observed, server) = batch_capped_node().await;
        let count = MULTICALL_CHUNK + 1;
        let addresses = (0..count)
            .map(|index| Address::with_last_byte(u8::try_from(index % 256).expect("byte")))
            .collect::<Vec<_>>();
        let salts = vec![B256::repeat_byte(1); 6];

        let tokens = client
            .token_balances(Address::ZERO, &addresses, BlockNumberOrTag::Number(7))
            .await;
        let derived = client
            .factory_addresses(Address::ZERO, Address::ZERO, &salts)
            .await;
        server.abort();

        assert_eq!(tokens.expect("token balances"), vec![U256::from(1); count]);
        assert_eq!(
            derived.expect("derived addresses"),
            vec![Address::with_last_byte(1); 6]
        );
        // One aggregate3 `eth_call` per chunk, at the read's block, where no call may fail.
        let observed = observed.lock().expect("observed calls").clone();
        let shape = observed
            .iter()
            .map(|(block, calls)| (block.clone(), calls.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                (Value::from("0x7"), MULTICALL_CHUNK),
                (Value::from("0x7"), 1),
                (Value::from("latest"), 6),
            ]
        );
        assert!(
            observed
                .iter()
                .all(|(_, calls)| calls.iter().all(|allow| !allow))
        );
    }

    fn assert_redacted(error: &ChainError, client: &EvmClient) {
        for rendered in [error.to_string(), format!("{error:?} {client:?}")] {
            assert!(!rendered.contains(SECRET), "{rendered}");
            assert!(!rendered.contains("127.0.0.1"), "{rendered}");
        }
    }

    async fn finalized_block_error(status: u16, error: &'static str) -> ChainError {
        let (client, server) = mock_node(status, error).await;
        let result = client.finalized_block().await;
        server.abort();
        let error = result.expect_err("node rejects the request");
        assert_redacted(&error, &client);
        error
    }

    #[tokio::test]
    async fn node_rejection_reason_reaches_the_operator_without_the_url() {
        let error =
            finalized_block_error(200, r#"{"code":-32000,"message":"header not found"}"#).await;
        let display = error.to_string();
        assert!(
            display.contains(
                "finalized block failed for provider `provider-a` \
                 (JSON-RPC error -32000: header not found)"
            ),
            "{display}"
        );
    }

    // alloy 2 surfaces a JSON-RPC error body on a non-2xx response as that JSON-RPC error, not as
    // an HTTP error, so the classification must rest on the payload alone.
    #[tokio::test]
    async fn http_errors_with_a_json_rpc_body_keep_their_rate_limit_class() {
        let error =
            finalized_block_error(502, r#"{"code":-32603,"message":"upstream failed"}"#).await;
        assert!(matches!(error, ChainError::Transport(_)), "{error:?}");
        assert!(!error.is_rate_limited(), "{error:?}");

        let error =
            finalized_block_error(429, r#"{"code":-32005,"message":"request rate exceeded"}"#)
                .await;
        assert!(matches!(error, ChainError::Transport(_)), "{error:?}");
        // Alloy returns its bounded exhaustion error after the single retry layer stops.
        // Exact retries and HTTP classification are exercised by endpoint::tests.
    }
}
