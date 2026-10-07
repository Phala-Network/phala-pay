//! Typed HTTP request and response models.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// The operator's view of a deposit's internals (`GET /v1/admin/deposits/{id}`), returned as the
/// `admin` field of the deposit; never present in a merchant response or an event.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositAdmin {
    /// The merchant account the deposit belongs to, `acct_…`.
    pub account: String,
    /// The processing state: `detected`, `confirmed`, `credited`, `swept`, `rejected`, or
    /// `reversed` (`status` is its merchant view).
    pub state: String,
    /// Selected route.
    pub route: Option<String>,
    /// Selected route version.
    pub route_version: Option<u64>,
    /// The payment settings revision the deposit is bound to (`psrev_…`): current in the snapshot
    /// of the statement that recorded it. `null` while `settings_hold` is set.
    pub settings_revision: Option<String>,
    /// The restore whose hold the deposit was recorded under; it waits for the merchant's
    /// reconfirmation, which binds it. `null` when `settings_revision` is set.
    pub settings_hold: Option<Uuid>,
    /// Position of the transfer log in its transaction's receipt; with the chain and transaction,
    /// the deposit's identity.
    pub receipt_log_index: u64,
    /// Including block time.
    pub block_time: DateTime<Utc>,
    /// When both providers showed the transfer at or below `finalized`.
    pub final_at: Option<DateTime<Utc>>,
    /// Eight-decimal scaled price as a decimal string.
    pub price_scaled: Option<String>,
    /// Last processing update time.
    pub updated_at: DateTime<Utc>,
    /// Transitions in ascending creation order.
    pub transitions: Vec<DepositTransition>,
    /// Events about the deposit in ascending creation order, with their delivery.
    pub events: Vec<DepositEventDelivery>,
}

/// One immutable state transition of a deposit.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositTransition {
    /// Timeline row identifier.
    pub id: Uuid,
    /// State before the transition attempt.
    pub from_state: String,
    /// State after the transition attempt.
    pub to_state: String,
    /// Retry attempt recorded for this transition.
    pub attempt: i32,
    /// Durable transition evidence.
    pub evidence: serde_json::Value,
    /// Transition creation time.
    pub created_at: DateTime<Utc>,
}

/// An event about a deposit and its delivery state.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositEventDelivery {
    /// Event id, `evt_…`.
    pub id: String,
    /// Event type, such as `deposit.credited`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event creation time.
    pub created_at: DateTime<Utc>,
    /// When the last of the account's webhook endpoints accepted the event, or `null` while one
    /// has not or the account has none.
    pub delivered_at: Option<DateTime<Utc>>,
}

/// Pause or resume request.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct PauseRequest {
    /// Pause scopes to add or remove.
    pub scopes: Vec<String>,
}

/// Current scopes after a pause mutation.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct PauseResponse {
    /// Current pause scopes.
    pub paused_scopes: Vec<String>,
}

/// `POST /v1/quotes` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateQuoteRequest {
    /// Your identifier of the customer to credit, 1 to 200 characters (Stripe Checkout's
    /// `client_reference_id`); the customer is created on first use.
    pub client_reference_id: String,
    /// The credit to quote, a positive integer in the currency's minor unit (US cents).
    pub amount: u64,
    /// Lowercase ISO currency code; only `usd`.
    pub currency: String,
    /// EVM chain of the payment, one of `GET /v1/config` `assets[].chain_id`.
    pub chain_id: u64,
    /// Asset code of the payment on that chain, such as `pha`.
    pub asset: String,
    /// Stripe's `metadata`: up to 50 string key/value pairs for your own use, keys of up to 40
    /// characters without square brackets, values of up to 500 characters.
    /// A key set to `""` is omitted. The deposit that pays the quote starts with a copy of it.
    /// Phala Pay never reads it. Do not store sensitive information in it, such as personal or
    /// payment details.
    #[serde(default, deserialize_with = "super::metadata::present")]
    #[schema(value_type = MetadataParam, required = false)]
    pub metadata: Option<serde_json::Value>,
}

/// `POST /v1/quotes/{id}`, `POST /v1/deposits/{id}`, `POST /v1/refunds/{id}`, and
/// `POST /v1/deposit_addresses/{id}` body: the
/// object's updatable parameters, of which `metadata` is the one.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMetadataRequest {
    /// Stripe's `metadata`: up to 50 string key/value pairs for your own use, keys of up to 40
    /// characters without square brackets, values of up to 500 characters.
    /// Merged into the object's: a key set to a value is set, a key set to `""` is unset, other
    /// keys are kept, and `metadata: ""` unsets every key.
    /// Phala Pay never reads it. Do not store sensitive information in it, such as personal or
    /// payment details.
    #[serde(default, deserialize_with = "super::metadata::present")]
    #[schema(value_type = MetadataParam, required = false)]
    pub metadata: Option<serde_json::Value>,
}

/// A `metadata` parameter: an object of string values, where `""` unsets the key, or `""` to
/// unset every key.
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum MetadataParam {
    /// Keys to set, or with `""` to unset.
    Pairs(std::collections::BTreeMap<String, String>),
    /// `""`: unset every key.
    Clear(MetadataClear),
}

/// `""`: unset every key of the object's metadata.
#[derive(Serialize)]
pub struct MetadataClear;

impl utoipa::PartialSchema for MetadataClear {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .enum_values(Some([""]))
            .description(Some("`\"\"`: unset every key of the object's metadata."))
            .into()
    }
}

impl ToSchema for MetadataClear {}

/// A quote: a locked price, an exact token amount, and a single-use address to pay it to.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Quote {
    /// `qt_` id. The quote's address salt is `keccak256(abi.encode(account,
    /// client_reference_id, "quote", id))` with the types `(string, string, string, string)`,
    /// where `account` is your `acct_` id.
    pub id: String,
    /// Always `quote`.
    pub object: String,
    /// Whether the quote was created with a live key.
    pub livemode: bool,
    /// Your identifier of the customer the quote credits.
    pub client_reference_id: String,
    /// Credit in the currency's minor unit.
    pub amount: u64,
    /// `usd`.
    pub currency: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Asset code.
    pub asset: String,
    /// The exact token amount to pay, in base units, as a decimal string.
    pub amount_atomic: String,
    /// The locked price in USD per token, a decimal string with 8 decimal places.
    pub exchange_rate: String,
    /// Single-use forwarder address to pay.
    pub address: String,
    /// The treasury the address pays: your treasury of the chain when the quote was created. The
    /// address is the factory's `CREATE2` over it and the salt.
    pub treasury: String,
    /// EIP-681 URI carrying the token, chain, address, and amount.
    pub payment_uri: String,
    /// `open`, `complete` (a matching payment consumed it), `expired`, or `canceled`. A quote stays
    /// `open` after `expires_at` until the finalized chain passes it, so a payment mined in time
    /// is never reported as expired; hide the address once `expires_at` has passed.
    pub status: String,
    /// End of the payment window, Unix seconds.
    pub expires_at: i64,
    /// Deferred cancellation request time, Unix seconds; null before a request.
    pub cancel_requested_at: Option<i64>,
    /// Creation time, Unix seconds.
    pub created: i64,
    /// The payment the checkout page should show, once one is seen on chain; display only.
    pub payment: Option<Payment>,
    /// The deposit that completed the quote: its `dep_` id, or the object with `expand[]=deposit`.
    pub deposit: Option<ExpandableDeposit>,
    /// Lets the payer's browser read the quote's public view, `ClientQuote`, from
    /// `GET /v1/quotes/{id}?client_secret=…` without an API key. Returned only by
    /// `POST /v1/quotes`; a repeat with the same `Idempotency-Key` within 24 hours replays the
    /// first response, the same secret included. Give it only to the paying customer's page, and
    /// do not log it.
    pub client_secret: Option<String>,
    /// The terms the quote was issued with, from your payment settings then; the quote keeps
    /// them whatever the settings say later.
    pub terms: QuoteTerms,
    /// Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)); `{}` when none.
    pub metadata: std::collections::BTreeMap<String, String>,
}

/// The terms a quote was issued with.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct QuoteTerms {
    /// The payment window, in seconds.
    pub quote_ttl_seconds: u64,
    /// The spread below spot of the locked price, in basis points.
    pub quote_spread_bps: u16,
    /// A payment within this many basis points of `amount_atomic`, either way, completes the
    /// quote at its `amount`.
    pub quote_tolerance_bps: u16,
    /// Token decimals `amount_atomic` was rounded up to.
    pub quote_amount_decimals: u8,
    /// Minimum credit in cents of a payment valued at spot.
    pub min_amount: u64,
    /// Minimum creditable deposit in base units, as a decimal string.
    pub min_deposit_atomic: String,
    /// Maximum creditable deposit in base units, as a decimal string.
    pub max_deposit_atomic: String,
    /// Refund dust floor in base units, as a decimal string.
    pub min_refund_atomic: String,
    /// The confirmation the quote required when issued: a depth, `safe`, or `finalized`. A payment
    /// not credited yet waits for the stricter of it and the chain's current floor.
    pub confirmations: String,
}

/// A page of quotes, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct QuoteList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/quotes`.
    pub url: String,
    /// Whether more quotes follow in the direction of this page.
    pub has_more: bool,
    /// The quotes.
    pub data: Vec<Quote>,
}

/// A page of refunds, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RefundList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/refunds`.
    pub url: String,
    /// Whether more refunds follow in the direction of this page.
    pub has_more: bool,
    /// The refunds.
    pub data: Vec<Refund>,
}

/// The public view of a quote, read with its `client_secret` and without a signature, for the
/// payer's checkout page. It has no account or internal fields.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ClientQuote {
    /// `qt_` id.
    pub id: String,
    /// Always `quote`.
    pub object: String,
    /// Whether the quote is in live mode; a test-mode page should say so.
    pub livemode: bool,
    /// `open`, `complete`, `expired`, or `canceled`, as on `Quote`; hide the address once
    /// `expires_at` has passed.
    pub status: String,
    /// Credit in the currency's minor unit.
    pub amount: u64,
    /// `usd`.
    pub currency: String,
    /// Asset code.
    pub asset: String,
    /// The token's decimals, to display `amount_atomic`.
    pub decimals: u8,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The exact token amount to pay, in base units, as a decimal string.
    pub amount_atomic: String,
    /// Single-use forwarder address to pay.
    pub address: String,
    /// EIP-681 URI carrying the token, chain, address, and amount.
    pub payment_uri: String,
    /// End of the payment window, Unix seconds.
    pub expires_at: i64,
    /// Deferred cancellation request time, Unix seconds.
    pub cancel_requested_at: Option<i64>,
    /// Progress of the payment shown on the page; display only, never a reason to deliver
    /// anything: `none`; `seen` (in a block, below the route's confirmation, and may still
    /// disappear); `confirming` (at the route's confirmation, being valued and screened);
    /// `credited`; `rejected` (not credited; the payer should contact the merchant's support); or
    /// `reversed` (credited, then its transaction left the chain before finality: the payment did
    /// not happen, and the credit is taken back).
    pub payment_status: String,
    /// While `seen`: blocks on top of and including the payment's block; otherwise `null`.
    #[schema(required = true)]
    pub confirmations: Option<u64>,
    /// While `credited`: the credit of the payment in the currency's minor unit, the credited
    /// deposit's `amount`. It differs from `amount` for a payment valued at spot (another amount,
    /// or paid late); otherwise `null`.
    #[schema(required = true)]
    pub amount_credited: Option<u64>,
    /// Typical time from payment to credit, in seconds, at the confirmation the quote's payments
    /// are credited at, as `GET /v1/config` reports it.
    pub typical_credit_seconds: u64,
}

/// `GET /v1/quotes/{id}` returns a `Quote` to a signed request and a `ClientQuote` to a request by
/// `client_secret`.
#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum QuoteView {
    /// The product's view.
    Quote(Box<Quote>),
    /// The payer's view.
    Client(Box<ClientQuote>),
}

/// A payment observed at a quote's address or a deposit address. Display only: while `status` is
/// `seen` it may still disappear in a reorg, and nothing has been credited.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Payment {
    /// `seen` (in a block, not recorded as a deposit yet) or `recorded` (recorded as a deposit at
    /// the route's confirmation; follow it as `deposit`). New values may be added.
    pub status: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Asset code; `null` for a token without a route.
    pub asset: Option<String>,
    /// Canonical transaction hash.
    pub tx_hash: String,
    /// Token amount in base units, as a decimal string.
    pub amount_atomic: String,
    /// Blocks on top of and including the transfer's block at the last head scan; `seen` only.
    pub confirmations: Option<u64>,
    /// Estimated finality time, Unix seconds: block time plus 15 minutes; `seen` only.
    pub estimated_final_at: Option<i64>,
    /// On a quote: whether the payment is the quote's asset, in time, and within tolerance, so
    /// it is credited at the quoted price (otherwise at spot). `null` on a deposit address, whose
    /// payments are all credited at spot.
    pub matches_quote: Option<bool>,
    /// `dep_` id the deposit has, or will have once recorded.
    pub deposit: String,
}

/// A quote id, or the quote with `expand[]`.
#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(untagged)]
#[schema(no_recursion)]
pub enum ExpandableQuote {
    /// `qt_` id.
    Id(String),
    /// The expanded quote.
    Object(Box<Quote>),
}

/// A deposit id, or the deposit with `expand[]`.
#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(untagged)]
#[schema(no_recursion)]
pub enum ExpandableDeposit {
    /// `dep_` id.
    Id(String),
    /// The expanded deposit.
    Object(Box<Deposit>),
}

/// A transfer to a quote's address or a deposit address at the route's confirmation: valued,
/// screened, and credited, or rejected; `reversed` if its transaction left the chain before
/// finality.
///
/// Every `deposit.*` event carries the whole deposit, with cumulative amounts, so the customer's
/// balance can be recomputed from the latest snapshot whatever order events arrive in: the
/// deposit contributes `amount - amount_refunded - amount_reversed` cents while its `status` is
/// `credited` or `reversed`, and nothing while it is `pending` or `rejected`. `status` only moves
/// forward (`pending`, then `credited` or `rejected`, then possibly `reversed`) and
/// `amount_refunded` only grows, so of two snapshots the later one has the later status or, for
/// the same status, the larger `amount_refunded`.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Deposit {
    /// `dep_` and the hex of the deposit's deterministic UUID,
    /// `uuid_v5(DEPOSIT_NAMESPACE, "{chain_id}:{tx_hash}:{receipt_log_index}")`, where
    /// `receipt_log_index` is the transfer's position among its transaction's receipt logs. A
    /// deposit recorded at a position after the deposit there was reversed (`replaces`) has
    /// another id, `uuid_v5(DEPOSIT_NAMESPACE, "{chain_id}:{tx_hash}:{receipt_log_index}:{revision}")`.
    pub id: String,
    /// Always `deposit`.
    pub object: String,
    /// Whether the deposit is on a live-mode route.
    pub livemode: bool,
    /// Your identifier of the customer the deposit is credited to.
    pub client_reference_id: String,
    /// The quote whose address received the transfer; `null` for a deposit address.
    pub quote: Option<ExpandableQuote>,
    /// The deposit address that received the transfer, `da_…`, on `chain_id` at `address`; `null`
    /// for a quote's address. Payments to a deposit address, active or retired, are credited at
    /// spot.
    pub deposit_address: Option<String>,
    /// `pending` (recorded at the route's confirmation and being valued and screened, or held
    /// while the account's or customer's `settlement` is paused), `credited`, `rejected` (see
    /// `rejection_reason`), or `reversed` (its transaction is not in the final chain: its credit is
    /// taken back, `amount_reversed`). New values may be added.
    pub status: String,
    /// Whether the deposit's block is final on both providers: a final deposit can no longer be
    /// reversed, and only a final deposit can be refunded. A deposit is credited at the route's
    /// confirmation, before it is final (`GET /v1/config` `typical_finality_seconds`).
    #[serde(rename = "final")]
    pub is_final: bool,
    /// When the finality watch found the deposit's block final on both providers, Unix seconds;
    /// `null` until `final`.
    pub final_at: Option<i64>,
    /// Whether a finalized `Flushed` event after the deposit moved its forwarder's balance of
    /// its token to the treasury (`GET /v1/sweeps`), whoever sent the flush.
    pub swept: bool,
    /// Why the deposit was rejected: `unsupported_asset`, `asset_not_accepted` (a routed asset
    /// the payment settings the deposit is bound to do not accept), `below_minimum`,
    /// `out_of_bounds`, `out_of_range`, or `sanctioned`.
    pub rejection_reason: Option<String>,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Asset code; `null` for a token without a route.
    pub asset: Option<String>,
    /// Token contract address.
    pub asset_contract: String,
    /// Token amount in base units, as a decimal string.
    pub amount_atomic: String,
    /// Credit in the currency's minor unit (cents), once valued.
    pub amount: Option<u64>,
    /// `usd`.
    pub currency: String,
    /// USD per token, a decimal string with 8 places, once valued.
    pub exchange_rate: Option<String>,
    /// `quote` (the quoted price) or `spot`, once valued.
    pub price_source: Option<String>,
    /// Valuation time, Unix seconds.
    pub valued_at: Option<i64>,
    /// Receiving forwarder address.
    pub address: String,
    /// Sender of the transfer.
    pub from_address: String,
    /// Transaction hash.
    pub tx_hash: String,
    /// Position of the transfer among its transaction's receipt logs: with `chain_id`, `tx_hash`,
    /// and `revision`, what `id` is derived from.
    pub receipt_log_index: u64,
    /// How many deposits at the same receipt position were reversed before this one: `0`, or
    /// the revision of the deposit it `replaces` plus one.
    pub revision: u64,
    /// Block-wide log index of the transfer; it changes if the transaction is re-included.
    pub log_index: u64,
    /// Number of the block the transfer is in; it changes if the transaction is re-included.
    pub block_number: u64,
    /// Hash of the block the transfer is in; it changes if the transaction is re-included.
    pub block_hash: String,
    /// Time of the block the transfer is in, Unix seconds.
    pub block_time: i64,
    /// Refunded token amount in base units, as a decimal string: the sum of succeeded refunds.
    pub amount_refunded_atomic: String,
    /// Whether the deposit is fully refunded.
    pub refunded: bool,
    /// Cents of `amount` its succeeded refunds take back: `amount` times `amount_refunded_atomic`
    /// over `amount_atomic`, rounded down, so it never exceeds the refunded share, and all of
    /// `amount` once fully refunded. Computed from the cumulative refunded amount, it only grows.
    /// `0` without `amount`.
    pub amount_refunded: u64,
    /// Cents of `amount` the reversal takes back: `amount` once `status` is `reversed` (a
    /// deposit is refunded only once final, and a final deposit is never reversed, so a reversed
    /// deposit has no refunds); `0` otherwise.
    pub amount_reversed: u64,
    /// The reversed deposit whose place this one took, `dep_…`: its transaction was re-included
    /// before finality against other state (a router or swap output), and the transfer at the same
    /// position in the final chain is this deposit. `null` otherwise, or when that deposit is in
    /// another account or mode.
    pub replaces: Option<String>,
    /// The deposit that took this reversed deposit's place, `dep_…` (its `replaces` is this one);
    /// `null` otherwise, or when that deposit is in another account or mode.
    pub replaced_by: Option<String>,
    /// Detection time, Unix seconds.
    pub created: i64,
    /// Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)): a copy of the
    /// quote's or the deposit address's when the deposit is recorded, independent of it
    /// afterwards; `{}` when none.
    pub metadata: std::collections::BTreeMap<String, String>,
    /// The operator's view of the deposit's internals; only in `GET /v1/admin/deposits/{id}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    pub admin: Option<Box<DepositAdmin>>,
}

/// `POST /v1/deposit_addresses` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateDepositAddressRequest {
    /// Your identifier of the customer, 1 to 200 characters; the customer is created on first use.
    pub client_reference_id: String,
    /// Stripe's `metadata`: up to 50 string key/value pairs for your own use, keys of up to 40
    /// characters without square brackets, values of up to 500 characters.
    /// Merged into the returned address's, as `POST /v1/deposit_addresses/{id}` does: a key set to
    /// `""` is unset. Every deposit to the address starts with a copy of it, and a rotation carries
    /// it to the next version. Phala Pay never reads it. Do not store sensitive information in it,
    /// such as personal or payment details.
    #[serde(default, deserialize_with = "super::metadata::present")]
    #[schema(value_type = MetadataParam, required = false)]
    pub metadata: Option<serde_json::Value>,
}

/// A customer's persistent deposit address, like a bank-transfer virtual account: one address for
/// every supported token on every supported network. Any amount of a supported token sent to it
/// is credited to the customer at the market (spot) price when it arrives. Rotation retires it and
/// issues a new one on every network; a retired address is still credited.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositAddress {
    /// `da_` id.
    pub id: String,
    /// Always `deposit_address`.
    pub object: String,
    /// Whether the address is in live mode.
    pub livemode: bool,
    /// Your identifier of the customer.
    pub client_reference_id: String,
    /// The address shared by every network, when all of `networks` have the same one; `null`
    /// when a network's treasury differs, and so its address (see `networks`), or when there is
    /// no network.
    pub address: Option<String>,
    /// The address's version among the customer's addresses, from 1. The salt is
    /// `keccak256(abi.encode(account, livemode, client_reference_id, "deposit_address", version))`,
    /// with the types `(string, bool, string, string, uint256)` and `account` your `acct_` id; it
    /// names no chain or asset. On each network the address is the factory's `CREATE2` over that
    /// network's treasury and the salt.
    pub version: u64,
    /// CREATE2 salt, 32 bytes of hex; the same on every network.
    pub salt: String,
    /// `active`, or `retired` by a rotation; payments to either are credited.
    pub status: String,
    /// Creation time, Unix seconds.
    pub created: i64,
    /// Retirement time, Unix seconds; `null` while active.
    pub retired_at: Option<i64>,
    /// Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)); `{}` when none.
    /// Each deposit to the address starts with a copy.
    pub metadata: std::collections::BTreeMap<String, String>,
    /// The address on each supported network of the mode it was issued on, by `chain_id`, with
    /// the tokens it takes there.
    pub networks: Vec<DepositAddressNetwork>,
    /// Payments to the address in the last 24 hours, newest first, at most 10, as a quote's
    /// `payment`: `seen` in a block within about a block time of arriving, then `recorded` as a
    /// deposit. Display only; credit from `deposit.credited`.
    pub payments: Vec<Payment>,
    /// Lets the customer's page read the address's public view, `ClientDepositAddress`, from
    /// `GET /v1/deposit_addresses/{id}?client_secret=…` without an API key, to show a payment as
    /// soon as it is seen. Returned only by `POST /v1/deposit_addresses` and `…/rotate`, each
    /// time a new one; only its hash is stored, and the newest 10 of an address stay valid. Give
    /// it only to the customer's page, and do not log it.
    pub client_secret: Option<String>,
}

/// The public view of a deposit address, read with its `client_secret` and without an API key,
/// for the customer's page. It has no account, customer, treasury, or metadata fields.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ClientDepositAddress {
    /// `da_` id.
    pub id: String,
    /// Always `deposit_address`.
    pub object: String,
    /// Whether the address is in live mode; a test-mode page should say so.
    pub livemode: bool,
    /// `active`, or `retired` by a rotation (still credited; show the new address instead).
    pub status: String,
    /// The address shared by every network, or `null` when a network's differs.
    pub address: Option<String>,
    /// The address on each supported network, with the tokens it takes there.
    pub networks: Vec<ClientDepositAddressNetwork>,
    /// Payments to the address in the last 24 hours, newest first, at most 10: display only,
    /// never a reason to deliver anything.
    pub payments: Vec<ClientDepositAddressPayment>,
}

/// A deposit address on one network, as its public view shows it.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ClientDepositAddressNetwork {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The forwarder address to pay on this chain.
    pub address: String,
    /// The tokens accepted on this chain.
    pub assets: Vec<DepositAddressAsset>,
    /// Typical time from payment to credit, in seconds, at the confirmation this chain's payments
    /// are credited at, as `GET /v1/config` reports it for each of the chain's tokens, which share
    /// the chain's one floor.
    pub typical_credit_seconds: u64,
}

/// A payment to a deposit address, as its public view shows it.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ClientDepositAddressPayment {
    /// `seen` (in a block, below the route's confirmation, and may still disappear);
    /// `confirming` (at the confirmation, being valued and screened); `credited`; `rejected`
    /// (not credited; the payer should contact the merchant's support); or `reversed` (its
    /// transaction left the chain before finality: the payment did not happen).
    pub status: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Asset code; `null` for a token without a route.
    pub asset: Option<String>,
    /// The token's decimals, to display `amount_atomic`; `null` with `asset`.
    pub decimals: Option<u8>,
    /// Token amount in base units, as a decimal string.
    pub amount_atomic: String,
    /// Transaction hash.
    pub tx_hash: String,
    /// While `seen`: blocks on top of and including the payment's block; otherwise `null`.
    #[schema(required = true)]
    pub confirmations: Option<u64>,
    /// When the payment was first seen or recorded, Unix seconds.
    pub created: i64,
}

/// `GET /v1/deposit_addresses/{id}` returns a `DepositAddress` to a request with an API key and a
/// `ClientDepositAddress` to a request by `client_secret`.
#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum DepositAddressView {
    /// The merchant's view.
    DepositAddress(Box<DepositAddress>),
    /// The customer's view.
    Client(Box<ClientDepositAddress>),
}

/// A deposit address on one network (EVM chain).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositAddressNetwork {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The forwarder address to pay on this chain.
    pub address: String,
    /// The treasury the forwarder pays. The address is the same on every network whose treasury
    /// is the same address.
    pub treasury: String,
    /// The tokens your payment settings accept on this chain; any other token sent to the
    /// address is not credited.
    pub assets: Vec<DepositAddressAsset>,
}

/// A token a deposit address takes on one network.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositAddressAsset {
    /// Asset code, such as `pha`.
    pub asset: String,
    /// ERC-20 contract address.
    pub contract: String,
    /// ERC-20 decimal count.
    pub decimals: u8,
    /// EIP-681 ERC-20 transfer URI carrying the token, chain, and address, and no amount: the
    /// payer chooses it.
    pub payment_uri: String,
}

/// A page of deposit addresses, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositAddressList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/deposit_addresses`.
    pub url: String,
    /// Whether more addresses follow in the direction of this page.
    pub has_more: bool,
    /// The deposit addresses.
    pub data: Vec<DepositAddress>,
}

/// A page of a list, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DepositList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/deposits`.
    pub url: String,
    /// Whether more deposits follow in the direction of this page.
    pub has_more: bool,
    /// The deposits.
    pub data: Vec<Deposit>,
}

/// `POST /v1/refunds` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRefundRequest {
    /// `dep_` id of the deposit to refund.
    pub deposit: String,
    /// Address the customer controls; never default it to the sender, which may be an exchange.
    pub destination_address: String,
    /// Amount in base units, as a decimal string; the unrefunded remainder when absent.
    pub amount_atomic: Option<String>,
    /// Stripe's `metadata`: up to 50 string key/value pairs for your own use, keys of up to 40
    /// characters without square brackets, values of up to 500 characters.
    /// A key set to `""` is omitted.
    /// Phala Pay never reads it. Do not store sensitive information in it, such as personal or
    /// payment details.
    #[serde(default, deserialize_with = "super::metadata::present")]
    #[schema(value_type = MetadataParam, required = false)]
    pub metadata: Option<serde_json::Value>,
}

/// `POST /v1/refunds/{id}/mark_paid` body: the merchant's refund transaction.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkRefundPaidRequest {
    /// Hash of the transaction that pays the refund from the treasury of the deposit's address.
    pub transaction_hash: String,
    /// Position of the `Transfer` log that pays the refund among the logs of the transaction's
    /// receipt (0 for the first), not the block-wide `logIndex`, which changes if the transaction
    /// is re-included in another block; any matching log when absent.
    pub receipt_log_index: Option<u64>,
}

/// A refund of (part of) a deposit to the customer, which the merchant pays from the treasury of
/// the deposit's address and attaches with `mark_paid` (design D5).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Refund {
    /// `re_` id.
    pub id: String,
    /// Always `refund`.
    pub object: String,
    /// Whether the refund was requested with a live key.
    pub livemode: bool,
    /// The refunded deposit: its `dep_` id, or the object with `expand[]=deposit`.
    pub deposit: ExpandableDeposit,
    /// Token amount in base units, as a decimal string.
    pub amount_atomic: String,
    /// Destination address.
    pub destination_address: String,
    /// The treasury the refund must be paid from: the one the deposit's address pays, which may
    /// differ from the account's current treasury.
    pub treasury: String,
    /// `pending` (awaiting payment, or its transaction's finality), `succeeded` (the transfer is
    /// final), `failed` (the attached transaction does not pay the refund; see
    /// `failure_reason`), or `canceled` (only before a transaction is attached). A refund marked
    /// paid stays `pending`, reserving its amount of the deposit, until it is `succeeded` or
    /// `failed`.
    pub status: String,
    /// Why the refund failed: `transaction_failed`, `transfer_not_found`, `sender_mismatch`,
    /// `destination_mismatch`, `amount_mismatch`, `transfer_already_used`,
    /// `transaction_dropped` (in no block while, at `finalized` on both providers, its sender's
    /// nonce was used by another transaction), or `transaction_not_found` (no provider returned
    /// it within 24 hours of `mark_paid`). New values may be added.
    pub failure_reason: Option<String>,
    /// The attached refund transaction, once marked paid.
    pub transaction_hash: Option<String>,
    /// Position of the paying `Transfer` log among the logs of the transaction's receipt: as named
    /// when marked paid, or found at verification.
    pub receipt_log_index: Option<u64>,
    /// Request time, Unix seconds.
    pub created: i64,
    /// Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)); `{}` when none.
    pub metadata: std::collections::BTreeMap<String, String>,
}

/// What a product's UI reads instead of hardcoding: assets, limits, and quote terms.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Config {
    /// Always `config`.
    pub object: String,
    /// The mode of the key that reads it: `assets` lists that mode's routes.
    pub livemode: bool,
    /// Credit currency, `usd`.
    pub currency: String,
    /// Cap on the number of your open quotes in this mode.
    pub max_open_quotes: u64,
    /// Cap on the credit of your open quotes in this mode, in cents. Test-mode quotes never
    /// count against live mode's cap.
    pub max_open_amount_per_account: u64,
    /// Cap on the credit of one customer's open quotes, in cents; no single quote can exceed it.
    pub max_open_amount_per_customer: u64,
    /// One customer's quote creations in a rolling minute, from your payment settings.
    pub quote_creations_per_customer_per_minute: u64,
    /// One entry per asset your payment settings accept on a chain where you have a treasury,
    /// with its terms: your effective payment config. Empty until you configure
    /// `POST /v1/payment_settings`.
    pub assets: Vec<ConfigAsset>,
}

/// A payable asset and its terms.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ConfigAsset {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Asset code.
    pub asset: String,
    /// Token contract address.
    pub contract: String,
    /// Token decimals.
    pub decimals: u8,
    /// `spot` or `stablecoin`.
    pub pricing: String,
    /// Minimum credit in cents, for quotes and deposits; smaller deposits are not credited.
    pub min_amount: u64,
    /// Minimum creditable deposit in base units, as a decimal string.
    pub min_deposit_atomic: String,
    /// Maximum creditable deposit in base units, as a decimal string.
    pub max_deposit_atomic: String,
    /// Minimum refundable amount in base units, as a decimal string.
    pub min_refund_atomic: String,
    /// Payment window of a quote, in seconds.
    pub quote_ttl_seconds: u64,
    /// A quote's price is spot / (1 + spread_bps / 10 000); spot-valued payments carry no spread.
    pub quote_spread_bps: u16,
    /// A payment within this many basis points of the quoted amount, either way, completes the
    /// quote at its amount.
    pub quote_tolerance_bps: u16,
    /// Token decimals a quote's amount is rounded up to (the operator's).
    pub quote_amount_decimals: u8,
    /// The confirmation a payment's block must reach before it is credited: a depth (`"2"`: the
    /// block and one more), `"safe"`, or `"finalized"`: the stricter of the chain's floor and the
    /// `confirmations` of your payment settings. A credit before finality can still be reversed
    /// (`deposit.reversed`).
    pub confirmations: String,
    /// Typical time from payment to the `deposit.credited` event, in seconds, at `confirmations`.
    pub typical_credit_seconds: u64,
    /// Typical time from payment to finality, in seconds; refunds wait for it.
    pub typical_finality_seconds: u64,
}

/// The merchant's contact recorded at onboarding (design D8): the operator's channel for the key
/// hand-over, recovery, incidents, and restores, and the only personal data kept.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    /// The contact's name, 1 to 200 characters.
    pub name: String,
    /// The security contact's email address.
    pub email: String,
}

/// The record of the operator's offline due diligence (design D8): a reference to it, when, and
/// by whom.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DueDiligence {
    /// Reference to the review in Phala's records, 1 to 200 characters.
    pub reference: String,
    /// Date of the review, `YYYY-MM-DD`.
    #[schema(value_type = String, format = Date)]
    pub reviewed_at: chrono::NaiveDate,
    /// Who reviewed, 1 to 200 characters.
    pub reviewed_by: String,
}

/// `POST /v1/admin/accounts` body. Accounts are created only by the operator (design D8).
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateAccountRequest {
    /// Display name, 1 to 200 characters.
    pub name: String,
    /// The merchant's contact.
    pub contact: Contact,
    /// The due diligence the decision rests on.
    pub due_diligence: DueDiligence,
    /// Whether the account may use live mode (design D12). Default `false`.
    #[serde(default)]
    pub charges_enabled: bool,
    /// Why the account is created, 1 to 1024 bytes.
    pub reason: String,
}

/// `POST /v1/admin/accounts/{account}` body; absent fields stay as they are.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateAccountRequest {
    /// Enables or disables live mode. Enabling it for an account without a live key returns the
    /// account's first live key.
    #[serde(default)]
    pub charges_enabled: Option<bool>,
    /// Marks the account restricted for review.
    #[serde(default)]
    pub restricted: Option<bool>,
    /// Replaces the merchant's contact.
    #[serde(default)]
    pub contact: Option<Contact>,
    /// Sets the cap, in cents and per mode, on the credit of the account's deposits credited
    /// before they are final: a deposit whose credit would take that total past it is credited
    /// once final instead. Default 100 000 ($1 000); `0` credits every deposit at finality.
    #[serde(default)]
    pub max_unfinalized_credit: Option<u64>,
    /// Changes the account's caps in one mode (design §12); absent caps stay.
    #[serde(default)]
    pub limits: Option<UpdateLimitsRequest>,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// The caps of one mode an operator changes; absent fields stay.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateLimitsRequest {
    /// The mode whose caps change: test mode's caps never limit live mode, nor the reverse.
    pub livemode: bool,
    /// Open quotes the account may hold in the mode, at least 1.
    #[serde(default)]
    pub max_open_quotes: Option<u64>,
    /// Cap on the credit of the account's open quotes in the mode, in cents.
    #[serde(default)]
    pub max_open_amount_per_account: Option<u64>,
    /// Cap on the credit of one customer's open quotes, in cents.
    #[serde(default)]
    pub max_open_amount_per_customer: Option<u64>,
    /// Active deposit addresses the account may hold in the mode, at least 1.
    #[serde(default)]
    pub max_active_deposit_addresses: Option<u64>,
}

/// An account's effective caps in each mode: the operator's values over the defaults.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AccountLimits {
    /// Live mode's caps.
    pub live: crate::limits::Limits,
    /// Test mode's caps.
    pub test: crate::limits::Limits,
}

/// An account's payment settings in each mode, as the operator sees them.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AccountPaymentSettings {
    /// Live mode's.
    pub live: PaymentSettingsObject,
    /// Test mode's.
    pub test: PaymentSettingsObject,
}

/// An account as the operator sees it.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AccountResponse {
    /// Account id, `acct_…`.
    pub id: String,
    /// Always `account`.
    pub object: String,
    /// Display name.
    pub name: String,
    /// The merchant's contact.
    pub contact: Contact,
    /// The due diligence record.
    pub due_diligence: DueDiligence,
    /// Whether the account may use live mode.
    pub charges_enabled: bool,
    /// Whether the account is restricted for review.
    pub restricted: bool,
    /// Active account-level pause scopes.
    pub paused_scopes: Vec<String>,
    /// Cap, in cents and per mode, on the credit of the account's deposits credited before they
    /// are final; a deposit past it is credited once final.
    pub max_unfinalized_credit: u64,
    /// The account's caps per mode.
    pub limits: AccountLimits,
    /// The account's payment settings per mode, with their effective config in `available`.
    pub payment_settings: AccountPaymentSettings,
    /// Creation time, Unix seconds.
    pub created: i64,
    /// The secret keys this request issued, each with its `secret` shown only here: at creation
    /// a test key and, with `charges_enabled`, a live key; on an update that enables live mode,
    /// the first live key. Send them to the contact; the merchant rolls them on receipt.
    pub api_keys: Vec<ApiKeyObject>,
}

/// `POST /v1/admin/accounts/{account}/api_keys` body: a recovery key (design D7).
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueApiKeyRequest {
    /// The key's mode; `true` needs `charges_enabled`.
    pub livemode: bool,
    /// Revokes every key of the mode first, for a leak the merchant cannot win by rolling.
    #[serde(default)]
    pub revoke_existing: bool,
    /// The key's label, at most 200 characters.
    #[serde(default)]
    pub name: String,
    /// Why, 1 to 1024 bytes: how the request was verified with the recorded contact.
    pub reason: String,
}

/// The account of the request's API key (`GET /v1/account`), in the key's mode.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AccountObject {
    /// Account id, `acct_…`.
    pub id: String,
    /// Always `account`.
    pub object: String,
    /// The mode of the key that reads it.
    pub livemode: bool,
    /// Display name.
    pub name: String,
    /// Whether the operator enabled live mode.
    pub charges_enabled: bool,
    /// Active account-level pause scopes, the operator's and your own (`POST /v1/account/pause`):
    /// while `quotes` is listed, no quote, deposit address, or network is issued. Your resume
    /// lifts only your own pause.
    pub paused_scopes: Vec<String>,
    /// The keys that sign this mode's webhooks: the current one first, then any previous one
    /// still signing during a rotation. Their public keys come from `GET /v1/attestation`.
    pub webhook_keys: Vec<WebhookKeyVersion>,
    /// Creation time, Unix seconds.
    pub created: i64,
}

/// Your payment settings in the key's mode (`GET /v1/payment_settings`): what you accept and on
/// what terms, chosen from the operator's catalog within its bounds
/// (docs/design/payment-settings.md).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct PaymentSettingsObject {
    /// Always `payment_settings`.
    pub object: String,
    /// The mode of the key that reads it.
    pub livemode: bool,
    /// `unconfigured` (accepts nothing: never configured), `configured`, or `held` (after a
    /// restore of the service, until you reconfirm with `POST /v1/payment_settings`; nothing is
    /// accepted meanwhile, and payments recorded wait).
    pub status: String,
    /// The current revision, `psrev_…`: unique and never reused, with no order.
    pub revision: String,
    /// When the current revision was written, Unix seconds.
    pub updated: i64,
    /// One customer's quote creations in a rolling minute; `null` for the default.
    #[schema(required = true)]
    pub quote_creations_per_customer_per_minute: Option<u64>,
    /// The chains you accept, each with its accepted assets. A chain or asset not listed is not
    /// accepted.
    pub chains: Vec<PaymentSettingsChain>,
    /// The operator's catalog of the mode: every chain and asset you may accept, with its
    /// defaults and bounds, and whether you accept it.
    pub available: Vec<AvailableChain>,
}

/// An accepted chain of your payment settings.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PaymentSettingsChain {
    /// A chain of the key's mode.
    pub chain_id: u64,
    /// The confirmation you require on the chain: a depth (`"12"`: the block and eleven more),
    /// `"safe"`, or `"finalized"`, never weaker than the chain's floor and of the chain's kind (a
    /// depth or `finalized` on Ethereum; a depth, `safe`, or `finalized` on an OP-stack chain).
    /// `null` for the floor.
    #[serde(default)]
    #[schema(required = false)]
    pub confirmations: Option<String>,
    /// The accepted assets of the chain, at least one.
    pub assets: Vec<PaymentSettingsAsset>,
}

/// An accepted asset and your terms on it; a term `null` or not sent takes the operator's default.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PaymentSettingsAsset {
    /// An asset code routed on the chain, such as `usdc`.
    pub asset: String,
    /// A quote's payment window, in seconds.
    #[serde(default)]
    #[schema(required = false)]
    pub quote_ttl_seconds: Option<u64>,
    /// A quote's spread below spot, in basis points.
    #[serde(default)]
    #[schema(required = false)]
    pub quote_spread_bps: Option<u16>,
    /// A quote's two-sided payment tolerance, in basis points.
    #[serde(default)]
    #[schema(required = false)]
    pub quote_tolerance_bps: Option<u16>,
    /// The minimum credit in cents of a quote or a deposit valued at spot.
    #[serde(default)]
    #[schema(required = false)]
    pub min_amount: Option<u64>,
    /// The minimum creditable deposit in base units, a decimal string.
    #[serde(default)]
    #[schema(required = false)]
    pub min_deposit_atomic: Option<String>,
    /// The maximum creditable deposit in base units, a decimal string.
    #[serde(default)]
    #[schema(required = false)]
    pub max_deposit_atomic: Option<String>,
    /// The refund dust floor in base units, a decimal string.
    #[serde(default)]
    #[schema(required = false)]
    pub min_refund_atomic: Option<String>,
}

/// A chain of the operator's catalog in the key's mode.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AvailableChain {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// `active` (accepted, with a treasury), `treasury_not_set` (accepted, without a treasury:
    /// nothing is offered on it until you set one), or `not_configured`.
    pub status: String,
    /// The chain's confirmation floor, also its default.
    pub confirmations: AvailableConfirmations,
    /// The chain's assets.
    pub assets: Vec<AvailableAsset>,
}

/// A chain's confirmation floor.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AvailableConfirmations {
    /// The weakest confirmation you may require, a depth, `safe`, or `finalized`.
    pub floor: String,
    /// The confirmation of a chain whose `confirmations` is `null`: the floor.
    pub default: String,
}

/// An asset of the operator's catalog and its bounds.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AvailableAsset {
    /// Asset code.
    pub asset: String,
    /// Token contract address.
    pub contract: String,
    /// Token decimals.
    pub decimals: u8,
    /// `spot` or `stablecoin`.
    pub pricing: String,
    /// Token decimals a quote's amount is rounded up to (the operator's).
    pub quote_amount_decimals: u8,
    /// Whether your settings list it.
    pub accepted: bool,
    /// Whether its terms, each clamped to its bounds, can be used together; an accepted asset that
    /// is not enabled is not offered or credited until you or the operator change it.
    pub enabled: bool,
    /// Bounds of `quote_ttl_seconds`.
    pub quote_ttl_seconds: BoundsU64,
    /// Bounds of `quote_spread_bps`.
    pub quote_spread_bps: BoundsU64,
    /// Bounds of `quote_tolerance_bps`.
    pub quote_tolerance_bps: BoundsU64,
    /// Bounds of `min_amount`.
    pub min_amount: BoundsU64,
    /// Bounds of `min_deposit_atomic`.
    pub min_deposit_atomic: BoundsAtomic,
    /// Bounds of `max_deposit_atomic`.
    pub max_deposit_atomic: BoundsAtomic,
    /// Bounds of `min_refund_atomic`.
    pub min_refund_atomic: BoundsAtomic,
}

/// The operator's default of an integer term and its inclusive bounds.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct BoundsU64 {
    /// The value when you set none.
    pub default: u64,
    /// The lowest value you may set.
    pub min: u64,
    /// The highest value you may set.
    pub max: u64,
}

/// The operator's default of a base-unit amount and its inclusive bounds, as decimal strings.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct BoundsAtomic {
    /// The value when you set none.
    pub default: String,
    /// The lowest value you may set.
    pub min: String,
    /// The highest value you may set.
    pub max: String,
}

/// `POST /v1/payment_settings` body. A parameter not sent is unchanged; `chains`, when sent,
/// replaces the whole list. Writes are last-write-wins.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePaymentSettingsRequest {
    /// One customer's quote creations in a rolling minute, from 1 to the operator's maximum;
    /// `null` restores the default.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<u64>, nullable, required = false)]
    pub quote_creations_per_customer_per_minute: Option<Option<u64>>,
    /// The chains to accept, replacing the list: each chain and asset of the key's mode once. An
    /// element's term not sent resets to the operator's default. `[]` accepts nothing.
    #[serde(default)]
    #[schema(required = false)]
    pub chains: Option<Vec<PaymentSettingsChain>>,
}

/// Deserializes a present parameter as `Some`, including `null`, so that `null` is told apart
/// from a parameter not sent.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `POST /v1/account/pause` and `POST /v1/account/resume` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountSelfPauseRequest {
    /// `["quotes"]`, the one scope a merchant pauses itself: no quote, deposit address, or
    /// network is issued while it is paused. Existing addresses keep being credited.
    pub scopes: Vec<String>,
}

/// Administrative pause or resume of a whole account, in both modes.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountPauseRequest {
    /// Pause scopes to add or remove: `quotes`, `settlement`, `refunds`.
    pub scopes: Vec<String>,
    /// Why, for the audit log.
    pub reason: String,
}

/// Administrative pause or resume of crediting to one treasury of an account.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminTreasuryPauseRequest {
    /// Why, for the audit log and the `treasury.updated` event's audit row.
    pub reason: String,
}

/// Administrative pause or resume of one customer of an account.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct CustomerPauseRequest {
    /// Pause scopes to add or remove.
    pub scopes: Vec<String>,
    /// The mode of the customer: customers of test and live mode are separate.
    pub livemode: bool,
}

/// An API key (design D7). `secret` is present only in the response that created it.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ApiKeyObject {
    /// Key id, `key_…`.
    pub id: String,
    /// Always `api_key`.
    pub object: String,
    /// The key's mode.
    pub livemode: bool,
    /// `secret`, holding every permission, or `restricted`, holding only `permissions`.
    #[serde(rename = "type")]
    pub key_type: String,
    /// The key's label.
    pub name: String,
    /// A restricted key's permissions, such as `quotes.write`; `null` for a secret key.
    pub permissions: Option<Vec<String>>,
    /// The whole key, `ppay_sk_…` or `ppay_rk_…`, shown once. Store it in a secret manager.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// The key's prefix and last four characters, such as `ppay_sk_test_…a1B2`.
    pub redacted: String,
    /// `active`; `expiring` for a rolled key that still works until `expires_at`; `expired`;
    /// `revoked`.
    pub status: String,
    /// Creation time, Unix seconds.
    pub created: i64,
    /// When a rolled key stops working, Unix seconds.
    pub expires_at: Option<i64>,
    /// Last use, Unix seconds, to the minute.
    pub last_used: Option<i64>,
}

/// `GET /v1/api_keys` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ApiKeyList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/api_keys`.
    pub url: String,
    /// Always `false`: every key of the mode is listed.
    pub has_more: bool,
    /// The mode's keys, newest first.
    pub data: Vec<ApiKeyObject>,
}

/// `POST /v1/api_keys` body.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateApiKeyRequest {
    /// The key's label, at most 200 characters.
    #[serde(default)]
    pub name: String,
    /// `secret`, the default, or `restricted` (Stripe's restricted keys): a key that holds only
    /// `permissions`.
    #[serde(default, rename = "type")]
    pub key_type: Option<String>,
    /// A restricted key's permissions, required with `type: restricted`: codes such as
    /// `quotes.write` or `deposits.read`, where a `write` includes its `read`. Grantable:
    /// `account.read`, `api_keys.read`, `quotes.*`, `deposit_addresses.*`, `deposits.*`,
    /// `refunds.*`, `events.read`, `endpoints.read`, `treasury.read`, `sweeps.read`,
    /// `forwarders.read`. Keys, treasuries, webhook endpoints, webhook keys, and account settings
    /// are managed only with a secret key.
    #[serde(default)]
    pub permissions: Option<Vec<String>>,
}

/// `POST /v1/api_keys/{id}/roll` body.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RollApiKeyRequest {
    /// Seconds the old key keeps working, up to 604800 (7 days); 0, the default, revokes it at
    /// once. A key rolling itself needs at least 3600.
    #[serde(default)]
    pub expires_in: u32,
}

/// `POST /v1/treasuries/challenge` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTreasuryChallengeRequest {
    /// The chain of the treasury: a chain of the key's mode (`GET /v1/config`).
    pub chain_id: u64,
    /// The treasury address to prove: an EOA, or a contract deployed on the chain such as a Safe.
    pub address: String,
}

/// An EIP-4361 (Sign-In with Ethereum) message proving a treasury, usable once: valid for 10
/// minutes for an EOA, 24 hours for an address that holds code (a Safe).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TreasuryChallenge {
    /// Always `treasury_challenge`.
    pub object: String,
    /// The key's mode.
    pub livemode: bool,
    /// The treasury's chain.
    pub chain_id: u64,
    /// The treasury address, as sent.
    pub address: String,
    /// The message's single-use nonce.
    pub nonce: String,
    /// The EIP-4361 message to sign, exactly as given: `domain` and `URI` are the API's origin,
    /// the statement names your account and mode, and `Chain ID` is `chain_id`. An EOA signs it
    /// with `personal_sign` (EIP-191); a Safe's owners sign it as a Safe message (EIP-1271).
    pub message: String,
    /// When the message stops being accepted, Unix seconds.
    pub expires_at: i64,
}

/// `POST /v1/treasuries` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTreasuryRequest {
    /// The treasury's chain, the challenge's.
    pub chain_id: u64,
    /// The challenge's `message`, unchanged.
    pub message: String,
    /// Hex signature of the message: an EOA's 65-byte `personal_sign` signature, or what a
    /// deployed contract's `isValidSignature` accepts (for a Safe, the owners' signatures of the
    /// Safe message, or `0x` after `SignMessageLib` approved it). ERC-6492 signatures are refused.
    pub signature: String,
}

/// An account's treasury of one chain and mode (design D10): the only address the forwarders
/// issued over it can pay.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Treasury {
    /// Treasury id, `trs_…`.
    pub id: String,
    /// Always `treasury`.
    pub object: String,
    /// The mode.
    pub livemode: bool,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The treasury address.
    pub address: String,
    /// `eoa` (an EIP-191 signature recovered to the address) or `contract` (a deployed
    /// contract's EIP-1271 approval).
    pub kind: String,
    /// `pending` (a live change waiting for `effective_at`; cancel it with
    /// `POST /v1/treasuries/{id}/cancel`), `active` (the chain's current treasury: new quotes and
    /// deposit address networks pay it), `replaced` (a former treasury; addresses issued over it
    /// still pay it), or `canceled`.
    pub status: String,
    /// When the treasury applies or applied, Unix seconds: at once for a chain's first treasury
    /// and in test mode, 48 hours after the proof for a later live change.
    pub effective_at: i64,
    /// When it was proven, Unix seconds.
    pub created: i64,
    /// When a later treasury replaced it, Unix seconds.
    pub replaced_at: Option<i64>,
    /// When it was canceled, Unix seconds.
    pub canceled_at: Option<i64>,
    /// Why it was canceled: `requested` (you canceled it) or `sanctioned` (a sanctions list named
    /// the address when the change was due to apply, so it never applied).
    pub cancellation_reason: Option<String>,
    /// Whether crediting of deposits to forwarders over this address is paused: they stay
    /// `pending` and no `deposit.credited` is sent until it resumes.
    pub crediting_paused: bool,
    /// Who paused crediting: `merchant` (`POST /v1/treasuries/{id}/pause`) and, or, `operator`.
    /// Each lifts only its own pause.
    pub crediting_paused_by: Vec<String>,
}

/// `GET /v1/treasuries` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TreasuryList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/treasuries`.
    pub url: String,
    /// Whether more treasuries match than `limit`.
    pub has_more: bool,
    /// The mode's treasuries, newest first.
    pub data: Vec<Treasury>,
}

/// Administrative deposit nudge result.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct NudgeResponse {
    /// Nudged deposit id, `dep_…`.
    pub deposit_id: String,
    /// Newly due processing time.
    pub next_attempt_at: DateTime<Utc>,
}

/// Administrative action body; `reason` is recorded in the action's audit row.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct AdminReasonRequest {
    /// Why the action is taken, 1 to 1024 bytes: the incident or sign-off it rests on.
    pub reason: String,
}

/// A lifted reconciliation block.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ReconciliationBlockLiftResponse {
    /// Lifted block, `chain:{chain_id}` or `address:{address_id}`.
    pub block_key: String,
    /// When the block was lifted; a repeated lift returns the original time.
    pub lifted_at: DateTime<Utc>,
}

/// Per-route daily finance report produced by C12.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RouteDailyReport {
    /// Stable route name.
    pub route: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Route asset contract.
    pub asset_contract: String,
    /// Deposits not reversed minus finalized `Flushed` amounts: what the route's forwarders
    /// still hold for their merchants to sweep.
    pub unflushed_balance_atomic: String,
    /// Sum of unconsumed rate-lock token amounts.
    pub open_rate_lock_exposure_atomic: String,
    /// Rejected token amount still held after succeeded refunds.
    pub rejected_holds_atomic: String,
    /// Deposit counts keyed by state.
    pub deposits_by_state: std::collections::BTreeMap<String, u64>,
    /// Credited deposits whose `deposit.credited` webhook an endpoint has not acknowledged yet.
    pub credited_undelivered: u64,
    /// Age in seconds of the oldest of those events; zero when every one was delivered.
    pub credited_undelivered_max_age_seconds: u64,
    /// Refund counts keyed by status.
    pub refunds_by_status: std::collections::BTreeMap<String, u64>,
    /// Maximum age in seconds keyed by current deposit state.
    pub age_in_state_max_seconds: std::collections::BTreeMap<String, u64>,
}

/// Latest reconciliation round of the serving process.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ReconciliationRoundReport {
    /// When the round finished.
    pub at: DateTime<Utc>,
    /// Checks that could not complete; empty after a complete round.
    pub failed_checks: Vec<FailedCheckReport>,
}

/// A persistent reconciliation block (architecture §13).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ReconciliationBlockReport {
    /// Block identifier, `chain:{chain_id}`.
    pub block_key: String,
    /// `chain`: the chain is frozen. No check writes the `address` scope any more; it excluded an
    /// address from the removed operator flusher.
    pub scope: String,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Blocked address for an `address` block.
    pub address_id: Option<Uuid>,
    /// Check that wrote the block, such as `address_derivation`.
    pub check: String,
    /// Why the check blocked.
    pub reason: String,
    /// When the block was written.
    pub created_at: DateTime<Utc>,
}

/// One reconciliation check that could not complete.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct FailedCheckReport {
    /// Check code, such as `custody_balance`.
    pub check: String,
    /// Error that stopped the check, without provider URLs.
    pub error: String,
}

impl From<crate::observability::ReconciliationStatus> for ReconciliationRoundReport {
    fn from(status: crate::observability::ReconciliationStatus) -> Self {
        Self {
            at: status.at,
            failed_checks: status
                .failed_checks
                .into_iter()
                .map(|(check, error)| FailedCheckReport { check, error })
                .collect(),
        }
    }
}

/// Daily finance report produced by C12.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DailyReportResponse {
    /// Report snapshot time.
    pub generated_at: DateTime<Utc>,
    /// Open rate-lock credit across all accounts in destination minor units: the sum the global
    /// exposure cap is enforced against.
    pub exposure_minor: String,
    /// SQL-computed metrics for each configured route.
    pub routes: Vec<RouteDailyReport>,
    /// Latest reconciliation round of the serving process; absent until the first round after a
    /// restart.
    pub reconciliation: Option<ReconciliationRoundReport>,
    /// Active reconciliation blocks in `block_key` order.
    pub reconciliation_blocks: Vec<ReconciliationBlockReport>,
    /// The threshold of `failing_webhook_endpoints`, in hours.
    pub failing_for_hours: u32,
    /// Enabled webhook endpoints of any account whose oldest undelivered event is older than
    /// `failing_for_hours`, oldest first. Deliveries are retried until delivered and never given
    /// up on, so each has failed that long; contact the account (docs/integration.md,
    /// "Delivery health").
    pub failing_webhook_endpoints: Vec<FailingWebhookEndpoint>,
}

/// A webhook endpoint failing for longer than the daily report's threshold.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct FailingWebhookEndpoint {
    /// Endpoint id, `we_…`.
    pub id: String,
    /// The endpoint's account, `acct_…`.
    pub account: String,
    /// The endpoint's mode.
    pub livemode: bool,
    /// Where it listens.
    pub url: String,
    /// Deliveries to it not delivered yet.
    pub pending_deliveries: i64,
    /// Creation time of its oldest undelivered event.
    pub oldest_pending_at: DateTime<Utc>,
    /// Its latest delivery attempt.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// That attempt's HTTP status; `null` when no response arrived.
    pub last_attempt_status: Option<u16>,
}

/// Administrative route pause response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RoutePauseResponse {
    /// Route name.
    pub route: String,
    /// Current route-level pause scopes.
    pub paused_scopes: Vec<String>,
}

/// Attestation query parameters.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AttestationQuery {
    /// Non-empty hexadecimal nonce of at most 32 bytes.
    pub nonce: String,
}

/// `GET /v1/admin/attestation` query parameters.
#[derive(Clone, Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AdminAttestationQuery {
    /// Account id, `acct_…`.
    pub account: String,
    /// The mode whose webhook keys are attested.
    pub livemode: bool,
    /// Non-empty hexadecimal nonce of at most 32 bytes.
    pub nonce: String,
}

/// TDX evidence binding a nonce to the webhook public keys of the caller's account in the
/// caller's mode (design D11).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct AttestationResponse {
    /// Always `attestation`.
    pub object: String,
    /// The caller's account, `acct_…`.
    pub account: String,
    /// The caller's mode; each mode has its own key.
    pub livemode: bool,
    /// The keys that sign the account's deliveries in this mode: the current one first, then any
    /// previous one still signing during a rotation.
    pub webhook_keys: Vec<WebhookKeyObject>,
    /// `sha256(len(nonce) ‖ nonce ‖ len(account) ‖ account ‖ livemode ‖ (version ‖
    /// public_key)*)` as lowercase hexadecimal: lengths are one byte, `account` is UTF-8,
    /// `livemode` is one byte (`1` live, `0` test), and each key of `webhook_keys`, in order, is
    /// its version as 4 big-endian bytes and its 32 raw public-key bytes.
    pub report_data: String,
    /// Versioned dstack TDX quote bytes as lowercase hexadecimal.
    pub tdx_quote: String,
}

/// One version of an account's webhook signing key, with its public key.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct WebhookKeyObject {
    /// Key version, from 1; it grows by one per roll.
    pub version: u32,
    /// The ed25519 public key in Standard Webhooks' form: `whpk_` and the standard base64 of its
    /// 32 raw bytes, which `report_data` binds. Pin it after verifying the attestation: every
    /// delivery carries a `v1a` signature by it.
    pub public_key: String,
    /// When a rolled key stops signing, Unix seconds; `null` for the current key.
    pub expires_at: Option<i64>,
}

/// One version of an account's webhook signing key; its public key comes from
/// `GET /v1/attestation`.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct WebhookKeyVersion {
    /// Key version, from 1.
    pub version: u32,
    /// When a rolled key stops signing, Unix seconds; `null` for the current key.
    pub expires_at: Option<i64>,
}

/// `POST /v1/account/webhook_keys/roll` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RollWebhookKeyRequest {
    /// Seconds the current key keeps signing beside the new one: 172800 (48 hours, the treasury
    /// time-lock) to 604800 (7 days) in live mode, 0 to 604800 in test mode, where 0 stops it at
    /// once. Default 172800.
    #[serde(default = "default_webhook_key_overlap")]
    pub expires_in: u32,
}

impl Default for RollWebhookKeyRequest {
    fn default() -> Self {
        Self {
            expires_in: default_webhook_key_overlap(),
        }
    }
}

/// The default overlap of a webhook key roll: the shortest a live roll accepts, 48 hours.
const fn default_webhook_key_overlap() -> u32 {
    172_800
}

/// A webhook endpoint (design D11): where the account's events of one mode are delivered, signed
/// with the account's webhook key of that mode.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct WebhookEndpointObject {
    /// Endpoint id, `we_…`.
    pub id: String,
    /// Always `webhook_endpoint`.
    pub object: String,
    /// The endpoint's mode.
    pub livemode: bool,
    /// Where events are delivered.
    pub url: String,
    /// The event types delivered, or `["*"]` for all. Account events (`account.*`, `api_key.*`,
    /// `treasury.*`, `webhook_endpoint.*`) are delivered to every enabled endpoint whatever this
    /// lists.
    pub enabled_events: Vec<String>,
    /// `enabled` or `disabled`.
    pub status: String,
    /// `gone` when Phala Pay disabled the endpoint because it answered `410 Gone`; `null`
    /// otherwise. Failing deliveries never disable an endpoint: they are retried until delivered.
    pub disabled_reason: Option<String>,
    /// Your description.
    pub description: Option<String>,
    /// Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)); `{}` when none.
    pub metadata: std::collections::BTreeMap<String, String>,
    /// Creation time, Unix seconds.
    pub created: i64,
    /// `true` in the `webhook_endpoint.deleted` event; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    pub deleted: Option<bool>,
    /// Deliveries to the endpoint not delivered yet. They are retried with backoff (capped at an
    /// hour) until delivered and are never given up on; a count that keeps growing means the
    /// endpoint is failing: fix it, then list what it missed with
    /// `GET /v1/events?delivery_success=false`.
    pub pending_deliveries: i64,
    /// Creation time of the oldest event not delivered to the endpoint yet, Unix seconds; `null`
    /// when none is pending.
    pub oldest_pending_at: Option<i64>,
    /// The latest delivery attempt to the endpoint; `null` before the first.
    pub last_attempt: Option<DeliveryAttempt>,
}

/// A delivery attempt to a webhook endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct DeliveryAttempt {
    /// When it was made, Unix seconds.
    pub at: i64,
    /// The endpoint's HTTP status; `null` when no response arrived (a timeout, a refused
    /// connection, or a URL the egress proxy refused).
    pub status_code: Option<u16>,
}

/// A page of webhook endpoints, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct WebhookEndpointList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/webhook_endpoints`.
    pub url: String,
    /// Whether more endpoints follow in the direction of this page.
    pub has_more: bool,
    /// The endpoints.
    pub data: Vec<WebhookEndpointObject>,
}

/// `POST /v1/webhook_endpoints` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhookEndpointRequest {
    /// Where to deliver events, up to 2048 characters, without credentials or fragment: `https` on
    /// port 443; in test mode also `http` on port 80. Redirects are not followed.
    pub url: String,
    /// The event types to deliver, such as `deposit.credited`, or `["*"]` for all.
    pub enabled_events: Vec<String>,
    /// Your description, up to 5000 characters.
    #[serde(default)]
    pub description: Option<String>,
    /// Stripe's `metadata`: up to 50 string key/value pairs for your own use, keys of up to 40
    /// characters without square brackets, values of up to 500 characters.
    #[serde(default, deserialize_with = "super::metadata::present")]
    #[schema(value_type = MetadataParam, required = false)]
    pub metadata: Option<serde_json::Value>,
}

/// `POST /v1/webhook_endpoints/{id}` body; parameters not sent are left unchanged.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateWebhookEndpointRequest {
    /// A new URL, as on creation.
    #[serde(default)]
    pub url: Option<String>,
    /// New event types, or `["*"]`.
    #[serde(default)]
    pub enabled_events: Option<Vec<String>>,
    /// A new description; `""` unsets it.
    #[serde(default)]
    pub description: Option<String>,
    /// `true` disables the endpoint, `false` enables it. A disabled endpoint receives nothing and
    /// its pending deliveries stop; resend missed events with `POST /v1/events/{id}/resend`.
    #[serde(default)]
    pub disabled: Option<bool>,
    /// Merged into the endpoint's metadata: a key set to `""` is unset, and `metadata: ""` unsets
    /// every key.
    #[serde(default, deserialize_with = "super::metadata::present")]
    #[schema(value_type = MetadataParam, required = false)]
    pub metadata: Option<serde_json::Value>,
}

/// A deleted webhook endpoint.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DeletedWebhookEndpoint {
    /// Endpoint id, `we_…`.
    pub id: String,
    /// Always `webhook_endpoint`.
    pub object: String,
    /// Always `true`.
    pub deleted: bool,
}

/// An event (<https://docs.stripe.com/api/events/object>): what happened to an object of the
/// account in one mode, and who caused it. The same object is the body of every webhook delivery;
/// `GET /v1/events` is also the account's audit log.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct EventObjectResponse {
    /// Event id, `evt_…`, also the `webhook-id` header of its deliveries.
    pub id: String,
    /// Always `event`.
    pub object: String,
    /// The account, `acct_…`.
    pub account: String,
    /// The event's mode.
    pub livemode: bool,
    /// Event type, such as `deposit.credited`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Creation time, Unix seconds.
    pub created: i64,
    /// Who caused it: an API key id (`key_…`), `admin` (the operator), or `system`.
    pub actor: String,
    /// The API request that caused the event; `null` when the service's own workers did (a
    /// payment credited, a quote expired, a time-locked treasury applied).
    pub request: Option<EventRequest>,
    /// The object when the event happened: `object` is its API representation, rendered in the
    /// same transaction as the change and never changed afterwards, so it can differ from a later
    /// `GET` of the object. `*.updated` events add `previous_attributes`, the former values of
    /// the fields that changed (<https://docs.stripe.com/api/events/object>).
    pub data: EventData,
    /// Deliveries to webhook endpoints that are neither delivered nor stopped.
    pub pending_webhooks: i64,
}

/// An event's `data`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct EventData {
    /// The object's API representation when the event was created: a deposit, quote, refund,
    /// API key, treasury, webhook endpoint, or the account, as its `GET` returned it then.
    #[schema(value_type = Object)]
    pub object: serde_json::Value,
    /// On `*.updated` events: the fields that changed, with their values before the change (a
    /// changed `metadata` holds only its changed keys; a field that was added is `null`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>, required = false)]
    pub previous_attributes: Option<serde_json::Value>,
}

/// The API request that caused an event (<https://docs.stripe.com/api/events/object>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct EventRequest {
    /// The request's `Request-Id`, `req_…`.
    pub id: String,
    /// The `Idempotency-Key` the request sent; `null` when it sent none.
    pub idempotency_key: Option<String>,
}

/// A page of events, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct EventList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/events`.
    pub url: String,
    /// Whether more events follow in the direction of this page.
    pub has_more: bool,
    /// The events.
    pub data: Vec<EventObjectResponse>,
}

/// `POST /v1/events/{id}/resend` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResendEventRequest {
    /// The enabled endpoint to deliver the event to again, `we_…`.
    pub webhook_endpoint: String,
}

/// The account's balance held in its forwarders, in the key's mode (Stripe's Balance): what
/// payments put there and no finalized `Flushed` event has moved to a treasury yet.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Balance {
    /// Always `balance`.
    pub object: String,
    /// The mode.
    pub livemode: bool,
    /// One entry per chain and token held.
    pub unswept: Vec<BalanceAmount>,
}

/// A token's unswept amounts on one chain.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct BalanceAmount {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The token contract.
    pub token: String,
    /// Asset code; `null` for a token without a route.
    pub asset: Option<String>,
    /// Every deposit not reversed, minus finalized sweeps, in base units, as a decimal string.
    pub amount_atomic: String,
    /// The part of `amount_atomic` from final deposits, which can no longer be reversed: what is
    /// safe to sweep.
    pub final_amount_atomic: String,
}

/// A sweep: one finalized `Flushed` event of the factory, which moved a forwarder's whole balance
/// of a token to its treasury (Stripe's Payout). Anyone can send the `flush`; the merchant usually
/// does, with the SDK's `flush_transaction` or `safe_batch`.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Sweep {
    /// Sweep id, `sw_…`.
    pub id: String,
    /// Always `sweep`.
    pub object: String,
    /// The mode.
    pub livemode: bool,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The forwarder swept, `fwd_…`.
    pub forwarder: String,
    /// The forwarder's address.
    pub address: String,
    /// The token contract.
    pub token: String,
    /// Asset code; `null` for a token without a route.
    pub asset: Option<String>,
    /// The treasury paid, fixed in the forwarder's address.
    pub treasury: String,
    /// Amount moved, in base units, as a decimal string.
    pub amount_atomic: String,
    /// The flush transaction.
    pub tx_hash: String,
    /// Block-wide index of the `Flushed` log.
    pub log_index: u64,
    /// Block number of the transaction.
    pub block_number: u64,
    /// When the finalized event was indexed, Unix seconds.
    pub created: i64,
}

/// A page of sweeps, newest first (<https://docs.stripe.com/api/pagination>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct SweepList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/sweeps`.
    pub url: String,
    /// Whether more sweeps follow in the direction of this page.
    pub has_more: bool,
    /// The sweeps.
    pub data: Vec<Sweep>,
}

/// A forwarder the account was issued (design §13): everything needed to recompute its address
/// and sweep it without Phala Pay. The address is `factory`'s `CREATE2` clone of the pinned
/// implementation over `treasury` and `salt`.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Forwarder {
    /// Forwarder id, `fwd_…`.
    pub id: String,
    /// Always `forwarder`.
    pub object: String,
    /// The mode.
    pub livemode: bool,
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The forwarder address.
    pub address: String,
    /// The forwarder factory.
    pub factory: String,
    /// The `CREATE2` salt, 32 bytes of hex.
    pub salt: String,
    /// The treasury the forwarder pays, fixed in its address.
    pub treasury: String,
    /// The quote it was issued for, `qt_…`; `null` for a deposit address's network.
    pub quote: Option<String>,
    /// The deposit address it is a network of, `da_…`; `null` for a quote's.
    pub deposit_address: Option<String>,
    /// When a treasury change replaced this deposit address network, Unix seconds; it is still
    /// watched and credited, and pays its own treasury.
    pub superseded_at: Option<i64>,
}

/// A page of forwarders (<https://docs.stripe.com/api/pagination>), in `id` order.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ForwarderList {
    /// Always `list`.
    pub object: String,
    /// The list's path, `/v1/forwarders`.
    pub url: String,
    /// Whether more forwarders follow in the direction of this page.
    pub has_more: bool,
    /// The forwarders.
    pub data: Vec<Forwarder>,
}

/// `GET /v1/admin/restore`: the restore freeze and the reconciliation before unfreezing
/// (`deploy/RESTORE.md`).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreStatus {
    /// Always `restore_status`.
    pub object: String,
    /// Whether the service is frozen after a restore from backup: merchant writes answer
    /// `503 service_restoring`, and nothing credits, settles, expires a quote, applies a treasury
    /// change, verifies a refund, or delivers an event.
    pub frozen: bool,
    /// The most recent restore; `null` when the database was never restored.
    pub restore: Option<RestoreObject>,
    /// The rescan of each configured chain since that restore; the freeze lifts only once every
    /// chain is `complete`.
    pub rescan: Vec<ChainRescan>,
    /// Delivered events imported from merchants' records, compared with the ledger.
    pub delivered_events: DeliveredEvents,
}

/// A restore from backup the service detected.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreObject {
    /// Restore id, a UUID.
    pub id: String,
    /// Always `restore`.
    pub object: String,
    /// When the service found the restore and froze, Unix seconds.
    pub detected_at: i64,
    /// `restore_check` (recorded after a restore on boot of the restore-check variant) or
    /// `timeline` (`topup run` found a PostgreSQL timeline newer than the acknowledged one).
    pub detected_by: String,
    /// The PostgreSQL timeline the restore promoted to.
    pub timeline_id: i64,
    /// Newest heartbeat in the restored database, Unix seconds: changes after it may be lost.
    pub restore_point: Option<i64>,
    /// Each chain's scanned block when the restore was detected, by chain id: where the rescan
    /// starts, and where a re-issued deposit address is backfilled from.
    pub restored_cursors: std::collections::BTreeMap<String, u64>,
    /// When the operator unfroze the service, Unix seconds.
    pub unfrozen_at: Option<i64>,
    /// Who unfroze it.
    pub unfrozen_by: Option<String>,
    /// Why, with the operator's checklist.
    pub unfreeze_reason: Option<String>,
}

/// One chain's rescan since the restore.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ChainRescan {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// The chain's scanned block when the restore was detected.
    pub restored_block: Option<u64>,
    /// The chain's scanned block now.
    pub scanned_block: Option<u64>,
    /// Block time of the finalized head the scanner last committed through, Unix seconds.
    pub scanned_block_time: Option<i64>,
    /// Issued addresses of the chain whose history the scanner has not read yet, such as
    /// re-issued deposit addresses.
    pub pending_backfills: i64,
    /// Whether a reconciliation block freezes the chain: its scanner is paused, so it is left out
    /// of the rescan, and it credits nothing until the block is lifted.
    pub blocked: bool,
    /// Whether the chain is rescanned: finalized past the moment the restore was detected, with
    /// every issued address backfilled.
    pub complete: bool,
}

/// Delivered events imported for the restore.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DeliveredEvents {
    /// How many were imported.
    pub imported: i64,
    /// Each imported event whose deposit the ledger does not hold as delivered yet.
    pub findings: Vec<DeliveredEventFinding>,
}

/// An imported event whose deposit is not re-derived yet, contradicts it, or whose amounts
/// differ.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DeliveredEventFinding {
    /// Event id, `evt_…`.
    pub event: String,
    /// Event type.
    #[serde(rename = "type")]
    pub event_type: String,
    /// The deposit, `dep_…`.
    pub deposit: String,
    /// `rescanned` (a `deposit.reversed` whose receipt position the rescan recorded first, with a
    /// deposit that is not reversed at its revision or below: the reversed deposit could not be
    /// restored; escalate), `pending` (the rescan has not re-derived or valued the deposit
    /// yet), `contradicted` (the recorded transfer is not the one the delivered event names: the deposit is held, not
    /// credited, until the operator discards the delivered credit), or `mismatch` (the ledger's
    /// token amount or credit differs from what the merchant received; the delivered event is
    /// kept and never sent again).
    pub status: String,
    /// Delivered `amount_atomic`.
    pub delivered_amount_atomic: Option<String>,
    /// Delivered `amount`, the credit in minor units.
    pub delivered_amount: Option<String>,
    /// The ledger's `amount_atomic`.
    pub ledger_amount_atomic: Option<String>,
    /// The ledger's credit in minor units.
    pub ledger_amount: Option<String>,
}

/// `POST /v1/admin/restore/api_keys/revoke` body: a key the merchant revoked after the restore
/// point, named by its id or by its prefix and last four characters.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreApiKeyRevokeRequest {
    /// Account id, `acct_…`.
    pub account: String,
    /// The key's id, `key_…`.
    #[serde(default)]
    pub id: Option<String>,
    /// The key's prefix, such as `ppay_sk_live_`, with `last4`.
    #[serde(default)]
    pub prefix: Option<String>,
    /// The key's last four characters, with `prefix`.
    #[serde(default)]
    pub last4: Option<String>,
    /// Why, 1 to 1024 bytes: the merchant's record of the revocation.
    pub reason: String,
}

/// `POST /v1/admin/restore/treasuries/verify` body: the treasuries of the merchant's latest
/// `treasury` events, as received.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreTreasuryVerifyRequest {
    /// Account id, `acct_…`.
    pub account: String,
    /// The mode.
    pub livemode: bool,
    /// Each treasury's latest object the merchant received (an event's `data.object`), one per
    /// treasury; at most 100.
    pub treasuries: Vec<ReceivedTreasury>,
    /// Apply again what the restore undid: cancel each pending treasury the merchant received as
    /// `canceled`, and pause or resume crediting as the merchant's `crediting_paused_by` shows.
    #[serde(default)]
    pub reapply: bool,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// A treasury object as the merchant received it; other fields are ignored.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct ReceivedTreasury {
    /// Treasury id, `trs_…`.
    pub id: String,
    /// The received `status`.
    pub status: String,
    /// The received `chain_id`.
    pub chain_id: u64,
    /// The received `address`.
    pub address: String,
    /// The received `crediting_paused_by`; only its `merchant` entry is compared, the operator
    /// re-applies its own pauses.
    #[serde(default)]
    pub crediting_paused_by: Option<Vec<String>>,
}

/// `POST /v1/admin/restore/treasuries/verify` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreTreasuryVerifyResponse {
    /// Always `list`.
    pub object: String,
    /// One result per received treasury, in request order.
    pub data: Vec<TreasuryVerification>,
}

/// A received treasury compared with the restored one.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TreasuryVerification {
    /// Treasury id, `trs_…`.
    pub id: String,
    /// The status the merchant received.
    pub received_status: String,
    /// The restored treasury's status now; `null` when it is missing.
    pub status: Option<String>,
    /// `matches`; `canceled` (canceled again now); `cancellation_lost` (the merchant canceled it,
    /// the restore undid it: cancel it with `reapply`); `application_lost` (the merchant received
    /// it `active` and the restore left it pending: restore it with its signed `treasury.updated`,
    /// `POST /v1/admin/restore/treasuries/apply`); `replacement_lost` (the merchant received it
    /// `replaced` and the restore left it current, while the chain's pending change is one sent
    /// here as `active`, same id and address: the other half of that `application_lost`, whose
    /// restore replaces it); `missing` (created after the
    /// restore point: the merchant creates it again after the unfreeze); or `differs` (another
    /// status, chain, or address: escalate).
    pub result: String,
    /// The merchant's crediting pause, when `crediting_paused_by` was sent: `matches`; `paused` or
    /// `resumed` (applied again now); `pause_lost` or `resume_lost` (without `reapply`); `missing`;
    /// or `differs` (another chain or address).
    pub crediting: Option<String>,
}

/// `POST /v1/admin/restore/treasuries/apply` body: the delivery of the `treasury.updated` that
/// announced a treasury change applying after the restore point, as the merchant's receiver got
/// it.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreTreasuryApplyRequest {
    /// The delivery exactly as received: a `treasury.updated` whose object is `active` and whose
    /// `previous_attributes.status` is `pending`.
    pub delivery: DeliveredWebhook,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// `POST /v1/admin/restore/treasuries/apply` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreTreasuryApplyResponse {
    /// Whether the change applied now; `false` when the treasury was in force already.
    pub applied: bool,
    /// The treasury.
    pub treasury: Treasury,
}

/// `POST /v1/admin/restore/webhook_endpoints/delete` body: an endpoint the merchant deleted
/// after the restore point.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreWebhookEndpointDeleteRequest {
    /// Account id, `acct_…`.
    pub account: String,
    /// The mode.
    pub livemode: bool,
    /// Endpoint id, `we_…`.
    pub id: String,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// `POST /v1/admin/restore/deposit_addresses` body: a deposit address the merchant holds, given
/// out after the restore point.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreDepositAddressRequest {
    /// Account id, `acct_…`.
    pub account: String,
    /// The mode.
    pub livemode: bool,
    /// The customer's `client_reference_id`.
    pub client_reference_id: String,
    /// The address's `version`; with `address`, both must agree. At most 32 past the customer's
    /// latest version: re-issue one further behind in steps (32, 64, ...).
    #[serde(default)]
    pub version: Option<u64>,
    /// The address the merchant holds (the top-level `address` or a network's); the version is
    /// found by deriving the customer's versions over each treasury of the account in force since
    /// the restore point.
    #[serde(default)]
    pub address: Option<String>,
    /// The `da_` id the merchant holds, kept for the re-issued address.
    #[serde(default)]
    pub id: Option<String>,
    /// A `client_secret` of the address the merchant holds (needs `id`): kept, so the payer's
    /// page reads the address again. Only a secret the service issued for the `id` to the
    /// account is accepted.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// `POST /v1/admin/restore/deposit_addresses` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreDepositAddressResponse {
    /// Whether the address was issued now; `false` when the customer already had this version.
    pub reissued: bool,
    /// The deposit address.
    pub deposit_address: DepositAddress,
}

/// `POST /v1/admin/restore/events` body: the deliveries of the `deposit.credited`,
/// `deposit.rejected`, and `deposit.reversed` events the merchant received after the restore
/// point, as its webhook receiver got them.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreEventsImportRequest {
    /// Up to 100 deliveries exactly as received.
    pub deliveries: Vec<DeliveredWebhook>,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// One webhook delivery as the merchant's receiver got it: its Standard Webhooks headers and its
/// raw body, which the service signed.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveredWebhook {
    /// The `webhook-id` header, the event's `evt_` id.
    pub webhook_id: String,
    /// The `webhook-timestamp` header, Unix seconds.
    pub webhook_timestamp: String,
    /// The `webhook-signature` header: one or more `v1a,<base64>` signatures.
    pub webhook_signature: String,
    /// The request body exactly as received, byte for byte: the event object.
    pub body: String,
}

/// `POST /v1/admin/restore/events` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreEventsImportResponse {
    /// Always `list`.
    pub object: String,
    /// One result per event, in request order.
    pub data: Vec<EventImport>,
}

/// What importing one delivered event did.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct EventImport {
    /// Event id, `evt_…`.
    pub id: String,
    /// `imported` (stored as delivered, with no delivery), `matches` (recorded already with the
    /// same `data`), or `mismatch` (recorded already with other `data`, which is kept).
    pub result: String,
    /// For a `deposit.reversed`, its deposit: `restored` (restored, reversed, from the delivery,
    /// so the rescan records the transfer now at its receipt position as the deposit that
    /// replaced it); `recorded` (the ledger holds it already); `address_unknown` (its address is
    /// not issued in the event's account and mode: re-issue it, then import the event again);
    /// or `rescanned` (the rescan recorded its receipt position first, with a deposit that is not
    /// reversed at its revision or below: not restored; escalate). `null` for other events.
    pub reversed_deposit: Option<String>,
}

/// `POST /v1/admin/restore/delivered_credits/discard` body.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreDeliveredCreditDiscardRequest {
    /// The deposit, `dep_…`, of a `contradicted` finding.
    pub deposit: String,
    /// Why, 1 to 1024 bytes: the incident and how the difference is settled with the merchant.
    pub reason: String,
}

/// `POST /v1/admin/restore/delivered_credits/discard` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreDeliveredCreditDiscardResponse {
    /// The deposit, `dep_…`.
    pub deposit: String,
    /// Always `true`: the deposit is valued from the chain as any other.
    pub discarded: bool,
}

/// `POST /v1/admin/restore/quotes` body: a quote the merchant created after the restore point, as
/// its records hold the quote object.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreQuoteRequest {
    /// Account id, `acct_…`.
    pub account: String,
    /// The mode.
    pub livemode: bool,
    /// The quote's `qt_` id: its address salt is derived from it.
    pub id: String,
    /// The quote's `client_reference_id`.
    pub client_reference_id: String,
    /// The quote's `chain_id`.
    pub chain_id: u64,
    /// The quote's `asset`.
    pub asset: String,
    /// The quote's `amount`, the credit in minor units.
    pub amount: u64,
    /// The quote's `amount_atomic`.
    pub amount_atomic: String,
    /// The quote's `exchange_rate`.
    pub exchange_rate: String,
    /// The quote's `address`.
    pub address: String,
    /// The quote's `created`, Unix seconds.
    pub created: i64,
    /// The quote's `expires_at`, Unix seconds; the re-issued quote's window closes at the
    /// restore's detection at the latest.
    pub expires_at: i64,
    /// The quote's `metadata`.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<serde_json::Value>,
    /// The quote's `client_secret`, when the merchant holds it: kept, so the payer's page reads the
    /// quote again. Only a secret the service issued for the `id` to the account is accepted, and
    /// it proves the service issued the quote to the account. Optional: without it the quote is
    /// re-issued, and found, all the same, but its public view is not readable.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Why, 1 to 1024 bytes.
    pub reason: String,
}

/// `POST /v1/admin/restore/quotes` response.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RestoreQuoteResponse {
    /// Whether the quote was issued now; `false` when it exists already for the customer at the
    /// address.
    pub reissued: bool,
    /// The quote, with the recorded terms. They are the merchant's record, not the service's, so
    /// they are never applied: a payment to it is credited at spot unless an imported
    /// `deposit.credited` for it carries its credit, and `expires_at` is the restore's detection
    /// at the latest.
    pub quote: Quote,
}

/// `POST /v1/admin/restore/unfreeze` body: the operator's reason and checklist.
#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreUnfreezeRequest {
    /// Why, 1 to 1024 bytes: the incident and its sign-off.
    pub reason: String,
    /// Every contact confirmed the key revocations, treasury cancellations, and endpoint
    /// deletions made after the restore point, and each was applied again; must be `true`.
    pub security_changes_reapplied: bool,
    /// Every deposit address given out after the restore point was re-issued; must be `true`.
    pub deposit_addresses_reissued: bool,
    /// Every quote created after the restore point that a merchant reported was re-issued; must be
    /// `true`.
    pub quotes_reissued: bool,
    /// Every event delivered after the restore point was imported; must be `true`.
    pub delivered_events_imported: bool,
}
