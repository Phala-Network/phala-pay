//! Stable API error responses, Stripe's error object (<https://docs.stripe.com/api/errors>): the
//! HTTP status says what kind of failure it is (`400` the request cannot succeed in the objects'
//! current state, `401` authentication, `403` permission, `404` a missing object, `409` only an
//! `Idempotency-Key` still in use, `429` too many requests, with `Retry-After`, `5xx` the
//! service), and `code` which one.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// Where each error code is documented: the `Errors` section of the API reference, one heading
/// per code.
pub const DOCS_URL: &str = "https://phala-network.github.io/phala-pay/#section/Errors/";

/// Every error code the merchant API returns, with its HTTP status and what to do about it: the
/// `Errors` section of the API reference, which each error's `doc_url` points into.
pub const ERROR_CODES: &[(&str, u16, &str)] = &[
    (
        "parameter_invalid",
        400,
        "A parameter is malformed or out of range; `param` names it. Fix the request.",
    ),
    (
        "parameter_missing",
        400,
        "A required parameter is absent; `param` names it.",
    ),
    (
        "parameter_unknown",
        400,
        "The operation does not take a parameter the request sent; `param` names it.",
    ),
    (
        "amount_too_small",
        400,
        "The amount is below the minimum of the route, or nothing is left to refund.",
    ),
    (
        "amount_too_large",
        400,
        "The amount is above the route's maximum deposit, or above what is left to refund.",
    ),
    (
        "exposure_cap_exceeded",
        400,
        "The quote would take the open quotes of your account in this mode past a cap: their number (`max_open_quotes` of `GET /v1/config`), their credit (`max_open_amount_per_account`), or one customer's credit (`max_open_amount_per_customer`). Wait for quotes to be paid, expire, or be canceled, or ask the operator to raise the cap.",
    ),
    (
        "paused",
        400,
        "A pause of the account, the customer, or the route blocks the operation (`quotes`, `settlement`, or `refunds`). Your own `quotes` pause lifts with `POST /v1/account/resume`; the operator lifts its own.",
    ),
    (
        "chain_frozen",
        400,
        "Reconciliation froze the chain pending the operator's review; no quote is issued on it until the operator lifts the block.",
    ),
    (
        "asset_not_accepted",
        400,
        "Your payment settings do not accept the asset on the chain in this mode, or accept nothing yet (`GET /v1/payment_settings`): quote an asset `GET /v1/config` lists, or accept it with `POST /v1/payment_settings`.",
    ),
    (
        "payment_settings_unconfirmed",
        400,
        "After a restore of the service, your payment settings wait for your reconfirmation: send your complete configuration with `POST /v1/payment_settings`.",
    ),
    (
        "treasury_not_set",
        400,
        "The account has no treasury on the chain: `POST /v1/treasuries/challenge`, then `POST /v1/treasuries`.",
    ),
    (
        "treasury_proof_invalid",
        400,
        "The treasury proof does not prove the address: the message is not the challenge, or the signature does not verify.",
    ),
    (
        "treasury_challenge_expired",
        400,
        "The treasury challenge expired; request a new one.",
    ),
    (
        "treasury_challenge_used",
        400,
        "The treasury challenge was already used; request a new one.",
    ),
    (
        "treasury_not_deployed",
        400,
        "The signature does not recover to the address and no contract is deployed there at the chain's finalized block; deploy the Safe first (ERC-6492 signatures are refused).",
    ),
    (
        "treasury_sanctioned",
        400,
        "A sanctions list names the treasury address.",
    ),
    (
        "treasury_change_pending",
        400,
        "A treasury change is already pending on the chain; cancel it first.",
    ),
    (
        "treasury_unchanged",
        400,
        "The address is already the chain's treasury.",
    ),
    (
        "treasury_unexpected_state",
        400,
        "Only a `pending` treasury can be canceled.",
    ),
    (
        "quote_payment_received",
        400,
        "The quote's address already received a payment, so the quote cannot be canceled.",
    ),
    (
        "quote_window_closed",
        400,
        "The quote's payment window has closed, so it cannot be canceled; it expires on its own.",
    ),
    (
        "quote_unexpected_state",
        400,
        "The quote is `complete` or `expired`; only an `open` quote can be canceled.",
    ),
    (
        "deposit_unexpected_state",
        400,
        "Admin: only a deposit the pump processes, `detected` or `confirmed`, can be nudged; a credited deposit waits for its sweep, which only a finalized `Flushed` event records.",
    ),
    (
        "deposit_not_refundable",
        400,
        "The deposit cannot be refunded: it is not credited or rejected, or it was reversed.",
    ),
    (
        "deposit_not_final",
        400,
        "The deposit is not final yet and could still be reversed; request the refund once `final` is `true` (about 15 minutes after its block on Ethereum).",
    ),
    (
        "destination_sanctioned",
        400,
        "A sanctions list names the refund's destination address.",
    ),
    (
        "transfer_already_used",
        400,
        "The transfer log already pays another refund.",
    ),
    (
        "refund_unexpected_state",
        400,
        "The refund's status does not allow the action: it is not `pending`, it is already marked paid with another transaction, or, being marked paid, it cannot be canceled.",
    ),
    (
        "api_key_inactive",
        400,
        "The API key is revoked or already rolled.",
    ),
    (
        "last_api_key",
        400,
        "The account's last active key of the mode cannot be revoked; create or roll a key first.",
    ),
    (
        "deposit_address_cap_exceeded",
        400,
        "The account has its maximum of active deposit addresses in the mode.",
    ),
    (
        "deposit_address_retired",
        400,
        "The deposit address is retired; rotate the customer's active address instead.",
    ),
    (
        "webhook_endpoint_cap_exceeded",
        400,
        "The account has 16 webhook endpoints in the mode; delete one first.",
    ),
    (
        "webhook_endpoint_disabled",
        400,
        "The webhook endpoint is disabled; enable it before resending events to it.",
    ),
    (
        "idempotency_key_reused",
        400,
        "The `Idempotency-Key` was used with a different request (type `idempotency_error`). Use a new key for a new request.",
    ),
    (
        "signature_invalid",
        401,
        "Admin API only: the RFC 9421 request signature did not verify.",
    ),
    (
        "signature_replayed",
        401,
        "Admin API only: the request signature was already used; sign the request again.",
    ),
    (
        "api_key_missing",
        401,
        "No `Authorization: Bearer` header with an API key (`ppay_rk_…` or `ppay_sk_…`).",
    ),
    (
        "api_key_invalid",
        401,
        "The API key is malformed, unknown, or revoked.",
    ),
    (
        "api_key_expired",
        401,
        "The API key was rolled and its overlap has ended; use the key it was rolled to.",
    ),
    (
        "permission_denied",
        403,
        "The key may not make this request: a restricted key lacks the permission, or the request manages keys, treasuries, webhook endpoints, webhook keys, or account settings, which needs a secret key.",
    ),
    (
        "testmode_charges_only",
        403,
        "The account is not enabled for live mode; use a test key until the operator enables it.",
    ),
    (
        "resource_missing",
        404,
        "No such object in the key's account and mode.",
    ),
    (
        "idempotency_key_in_use",
        409,
        "A request with this `Idempotency-Key` is still running (type `idempotency_error`); retry shortly with the same key. The only `409`.",
    ),
    (
        "rate_limit",
        429,
        "Too many requests of the account and mode (100 per second live, 25 test), or reads of one quote's or deposit address's public view. Retry after `Retry-After` seconds, with exponential backoff.",
    ),
    (
        "customer_rate_limit",
        429,
        "The customer made too many quotes in the last minute, or rotated its deposit address too often in the last hour. Retry after `Retry-After` seconds.",
    ),
    (
        "internal_error",
        500,
        "The service failed. Retry with the same `Idempotency-Key`: a request that started executing replays this response, so it never runs twice; a new key runs it again.",
    ),
    (
        "unavailable",
        503,
        "A dependency (pricing, sanctions screening, attestation) is temporarily unavailable. Retry with backoff; the same `Idempotency-Key` runs the request again.",
    ),
    (
        "service_maintenance",
        503,
        "A planned upgrade temporarily pauses new mutations. Reads continue while the process is up. Retry after Retry-After with the same Idempotency-Key; the request has not executed. The pause expires automatically if deployment fails.",
    ),
    (
        "service_restoring",
        503,
        "The service was restored from backup and is frozen until the operator has reconciled it with you: every request with an API key is refused, reads too, and nothing is credited or delivered meanwhile. Retry after `Retry-After` seconds; the operator contacts you for your records since the restore point.",
    ),
    (
        "restore_not_frozen",
        400,
        "Admin API only: a restore reconciliation action needs the service frozen after a restore.",
    ),
    (
        "restore_rescan_incomplete",
        400,
        "Admin API only: a chain is not rescanned since the restore (`GET /v1/admin/restore`); the freeze cannot be lifted yet.",
    ),
];

/// Marks an error response to a request no handler executed, which an `Idempotency-Key` does not
/// save: a request that failed authentication, authorization, or validation (`parameter_*`), was
/// rate limited, or met a temporary unavailability, so a retry with the same key runs it (Stripe: "Results are only saved if an
/// API endpoint started executing", <https://docs.stripe.com/api/idempotent_requests>).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NotExecuted;

/// Machine-readable error envelope returned by every API failure, Stripe's error object
/// (<https://docs.stripe.com/api/errors>).
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    /// Error details.
    pub error: ErrorDetail,
}

/// Stable error fields safe to expose to callers.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Error category: `invalid_request_error` for any 4xx except an idempotency conflict,
    /// `idempotency_error` for an `Idempotency-Key` reused with another request or still in use,
    /// `api_error` for 5xx.
    #[serde(rename = "type")]
    pub error_type: ErrorType,
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Human-readable summary without internal details; it may change.
    pub message: String,
    /// The request parameter the error is about, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// The documentation of `code` in the API reference. Every error of this service carries it;
    /// it is optional in the schema, as in Stripe's, so a client never fails on an error without
    /// it.
    #[schema(required = false)]
    pub doc_url: String,
}

/// Error category of [`ErrorDetail`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub enum ErrorType {
    /// The request cannot succeed as sent.
    #[serde(rename = "invalid_request_error")]
    InvalidRequest,
    /// An `Idempotency-Key` was reused with different parameters.
    #[serde(rename = "idempotency_error")]
    Idempotency,
    /// The service failed; retry with backoff.
    #[serde(rename = "api_error")]
    Api,
}

/// API failure with an HTTP status and stable public body.
#[derive(Clone, Debug)]
pub struct ApiError {
    status: StatusCode,
    detail: ErrorDetail,
    /// Seconds after which a `429` may be retried, sent as `Retry-After`.
    retry_after: Option<u64>,
}

impl ApiError {
    /// Returns a request-validation failure not tied to one parameter.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "parameter_invalid", message)
    }

    /// Returns a validation failure of the request parameter `param`.
    #[must_use]
    pub fn invalid_param(param: impl Into<String>, message: impl Into<String>) -> Self {
        Self::bad_request(message).with_param(param)
    }

    /// Returns a request missing a required parameter.
    #[must_use]
    pub fn missing_param(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "parameter_missing", message)
    }

    /// Returns a request naming a parameter the operation does not take.
    #[must_use]
    pub fn unknown_param(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "parameter_unknown", message)
    }

    /// Returns a failure for a requested amount below the minimum.
    #[must_use]
    pub fn amount_too_small(param: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "amount_too_small", message).with_param(param)
    }

    /// Returns a failure for a requested amount above the maximum.
    #[must_use]
    pub fn amount_too_large(param: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "amount_too_large", message).with_param(param)
    }

    /// Returns an admin request whose RFC 9421 signature did not verify.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "signature_invalid",
            "request signature verification failed",
        )
    }

    /// Returns a merchant request without `Authorization: Bearer`.
    #[must_use]
    pub fn api_key_missing() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "api_key_missing",
            "send your API key as `Authorization: Bearer ppay_rk_…` or `ppay_sk_…`",
        )
    }

    /// Returns a merchant request with a malformed, unknown, or revoked key.
    #[must_use]
    pub fn api_key_invalid() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "api_key_invalid",
            "the API key is invalid or revoked",
        )
    }

    /// Returns a merchant request with a rolled key past its expiry (Stripe's code).
    #[must_use]
    pub fn api_key_expired() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "api_key_expired",
            "the API key has expired; use the key it was rolled to",
        )
    }

    /// Returns a request the credential is not permitted to make (design D13).
    #[must_use]
    pub fn permission_denied() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "the credential does not have the required permission",
        )
    }

    /// Returns a missing or foreign resource.
    #[must_use]
    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "resource_missing",
            "resource not found",
        )
    }

    /// Returns a request for a path, or a method on it, the API does not serve, answered as
    /// Stripe answers one: `404 resource_missing`.
    #[must_use]
    pub fn unrecognized_request() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "resource_missing",
            "unrecognized request URL or method",
        )
    }

    /// Returns an open quote exposure cap failure.
    #[must_use]
    pub fn exposure_cap(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "exposure_cap_exceeded", message).with_param("amount")
    }

    /// Returns a cancellation refused because the quote's address already received a payment.
    #[must_use]
    pub fn quote_payment_received() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "quote_payment_received",
            "the quote's address already received a payment",
        )
    }

    /// Returns a cancellation refused because the quote's payment window has closed.
    #[must_use]
    pub fn quote_window_closed() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "quote_window_closed",
            "the quote's payment window has closed",
        )
    }

    /// Returns a cancellation refused because the quote is complete or expired.
    #[must_use]
    pub fn quote_unexpected_state(status: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "quote_unexpected_state",
            format!("the quote is {status}"),
        )
    }

    /// Returns a nudge of a deposit in `state`, which the pump never claims.
    #[must_use]
    pub fn deposit_unexpected_state(state: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "deposit_unexpected_state",
            format!("the deposit is {state}; only a detected or confirmed deposit can be nudged"),
        )
    }

    /// Returns a refund refused because the deposit is not refundable (architecture §15).
    #[must_use]
    pub fn deposit_not_refundable() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "deposit_not_refundable",
            "the deposit is not eligible for a refund",
        )
    }

    /// Returns a refund refused because the deposit is not final yet and could still be reversed
    /// (design D1, D5).
    #[must_use]
    pub fn deposit_not_final() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "deposit_not_final",
            "the deposit is not final yet; request the refund once it is (about 15 minutes after its block on Ethereum)",
        )
    }

    /// Returns a refund refused because a sanctions list names its destination (design §8).
    #[must_use]
    pub fn destination_sanctioned() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "destination_sanctioned",
            "the destination address is on a sanctions list",
        )
        .with_param("destination_address")
    }

    /// Returns a refund transaction whose transfer log already pays another refund.
    #[must_use]
    pub fn transfer_already_used() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "transfer_already_used",
            "the transfer log already pays another refund",
        )
        .with_param("receipt_log_index")
    }

    /// Returns a refund action its status does not allow.
    #[must_use]
    pub fn refund_unexpected_state(detail: impl std::fmt::Display) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "refund_unexpected_state",
            format!("the refund is {detail}"),
        )
    }

    /// Returns an `Idempotency-Key` reused with a different request (design §13).
    #[must_use]
    pub fn idempotency_key_reused() -> Self {
        let mut error = Self::new(
            StatusCode::BAD_REQUEST,
            "idempotency_key_reused",
            "the Idempotency-Key was used with a different request",
        );
        error.detail.error_type = ErrorType::Idempotency;
        error
    }

    /// Returns a request whose `Idempotency-Key` is held by a request still running; retry.
    #[must_use]
    pub fn idempotency_key_in_use() -> Self {
        let mut error = Self::new(
            StatusCode::CONFLICT,
            "idempotency_key_in_use",
            "a request with this Idempotency-Key is still running; retry",
        );
        error.detail.error_type = ErrorType::Idempotency;
        error
    }

    /// Returns a request over the account's or the platform's rate limit (design §12), which
    /// frees within a second (<https://docs.stripe.com/rate-limits>).
    #[must_use]
    pub fn too_many_requests() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit",
            "too many requests; retry after Retry-After seconds, with exponential backoff",
        )
        .with_retry_after(1)
    }

    /// Returns a roll of a revoked or already rolled key.
    #[must_use]
    pub fn api_key_inactive() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "api_key_inactive",
            "the API key is revoked or already rolled",
        )
    }

    /// Returns a revoke that would leave the account's mode without a non-expiring key.
    #[must_use]
    pub fn last_api_key() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "last_api_key",
            "the account's last active key cannot be revoked; create or roll a key first",
        )
    }

    /// Returns a live-mode request of an account the operator has not enabled for live mode
    /// (design D12; Stripe's code).
    #[must_use]
    pub fn testmode_charges_only() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "testmode_charges_only",
            "the account is not enabled for live mode; use a test key",
        )
    }

    /// Returns a read of a quote's or deposit address's public view by `client_secret` over a
    /// limit, retryable after `retry_after` seconds.
    #[must_use]
    pub fn client_reads_limited(retry_after: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit",
            "too many reads by client_secret; retry after Retry-After seconds",
        )
        .with_retry_after(retry_after)
    }

    /// Returns a quote creation over the customer's per-minute limit of the route, retryable
    /// after `retry_after` seconds.
    #[must_use]
    pub fn customer_quote_limit(retry_after: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "customer_rate_limit",
            "the customer has created too many quotes in the last minute; retry after \
             Retry-After seconds",
        )
        .with_param("client_reference_id")
        .with_retry_after(retry_after)
    }

    /// Returns a deposit address rotation over the customer's hourly limit, retryable after
    /// `retry_after` seconds.
    #[must_use]
    pub fn customer_rotation_limit(retry_after: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "customer_rate_limit",
            "the customer's deposit address was rotated too often in the last hour; retry after \
             Retry-After seconds",
        )
        .with_retry_after(retry_after)
    }

    /// Returns a new deposit address over the account's cap of active addresses in the mode.
    #[must_use]
    pub fn deposit_address_cap(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "deposit_address_cap_exceeded",
            message,
        )
    }

    /// Returns a new webhook endpoint over the 16 of the account's mode.
    #[must_use]
    pub fn webhook_endpoint_cap() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "webhook_endpoint_cap_exceeded",
            "the account has 16 webhook endpoints in this mode; delete one first",
        )
    }

    /// Returns a resend to a disabled webhook endpoint.
    #[must_use]
    pub fn webhook_endpoint_disabled() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "webhook_endpoint_disabled",
            "the webhook endpoint is disabled; enable it first",
        )
        .with_param("webhook_endpoint")
    }

    /// Returns a rotation of an already retired deposit address.
    #[must_use]
    pub fn deposit_address_retired() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "deposit_address_retired",
            "the deposit address is retired; rotate the customer's active address",
        )
    }

    /// Returns a quote or deposit address requested where the account has no treasury (design
    /// D10).
    #[must_use]
    pub fn treasury_not_set() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_not_set",
            "set a treasury on the chain first: POST /v1/treasuries/challenge, then POST \
             /v1/treasuries",
        )
    }

    /// Returns a quote or an address of an asset the account's payment settings do not accept.
    #[must_use]
    pub fn asset_not_accepted(param: Option<&'static str>) -> Self {
        let error = Self::new(
            StatusCode::BAD_REQUEST,
            "asset_not_accepted",
            "your payment settings do not accept this asset in this mode; accept it with POST \
             /v1/payment_settings",
        );
        match param {
            Some(param) => error.with_param(param),
            None => error,
        }
    }

    /// Returns an issuance refused while the account's payment settings are held after a restore.
    #[must_use]
    pub fn payment_settings_unconfirmed() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "payment_settings_unconfirmed",
            "your payment settings await your reconfirmation after a restore: send your complete \
             configuration with POST /v1/payment_settings",
        )
    }

    /// Returns a treasury proof that does not prove the address (design D10).
    #[must_use]
    pub fn treasury_proof_invalid(param: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "treasury_proof_invalid", message).with_param(param)
    }

    /// Returns a treasury proof whose challenge expired.
    #[must_use]
    pub fn treasury_challenge_expired() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_challenge_expired",
            "the challenge has expired; request a new one",
        )
        .with_param("message")
    }

    /// Returns a treasury proof whose challenge was already used.
    #[must_use]
    pub fn treasury_challenge_used() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_challenge_used",
            "the challenge was already used; request a new one",
        )
        .with_param("message")
    }

    /// Returns a treasury proof by a contract that is not deployed on the chain.
    #[must_use]
    pub fn treasury_not_deployed() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_not_deployed",
            "the signature does not recover to the address, and no contract is deployed at it at \
             the chain's finalized block; deploy the Safe on this chain first (ERC-6492 \
             signatures are not accepted)",
        )
        .with_param("signature")
    }

    /// Returns a treasury a sanctions list names (design §8).
    #[must_use]
    pub fn treasury_sanctioned() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_sanctioned",
            "the treasury address is on a sanctions list",
        )
        .with_param("address")
    }

    /// Returns a treasury change while another one of the chain is pending.
    #[must_use]
    pub fn treasury_change_pending() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_change_pending",
            "a treasury change is already pending on this chain; cancel it first",
        )
    }

    /// Returns a treasury change to the chain's current treasury.
    #[must_use]
    pub fn treasury_unchanged() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_unchanged",
            "the address is already the chain's treasury",
        )
    }

    /// Returns a cancellation of a treasury that is not pending.
    #[must_use]
    pub fn treasury_unexpected_state(status: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "treasury_unexpected_state",
            format!("the treasury is {status}"),
        )
    }

    /// Returns a replayed request signature.
    #[must_use]
    pub fn signature_replayed() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "signature_replayed",
            "request signature has already been used",
        )
    }

    /// Returns an operation blocked by a pause scope; not retried automatically.
    #[must_use]
    pub fn paused(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "paused", message)
    }

    /// Returns an operation blocked because reconciliation froze the chain.
    #[must_use]
    pub fn chain_frozen() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "chain_frozen",
            "the route chain is frozen pending reconciliation review",
        )
    }

    /// Returns a temporary dependency failure; retry.
    #[must_use]
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    /// Returns a request that did not complete within its deadline; retry.
    #[must_use]
    pub fn request_deadline_exceeded() -> Self {
        Self::service_unavailable("the request did not complete within its deadline; retry")
            .with_retry_after(2)
    }

    /// A `503` for a request the database could not run now (a connection not available, or a
    /// transaction rolled back by a conflict), which a retry runs.
    #[must_use]
    pub fn database_busy() -> Self {
        Self::service_unavailable("the database is busy; retry").with_retry_after(1)
    }

    /// A mutation refused before execution during a planned upgrade.
    #[must_use]
    pub fn service_maintenance() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_maintenance",
            "the service is upgrading; retry after Retry-After seconds",
        )
        .with_retry_after(5)
    }

    /// Returns a write refused while the service is frozen after a restore from backup
    /// (`crate::restore_mode`), retried after `retry_after` seconds.
    #[must_use]
    pub fn service_restoring(retry_after: u64) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_restoring",
            "the service was restored from backup and is being reconciled: API requests are \
             paused; retry after Retry-After seconds",
        )
        .with_retry_after(retry_after)
    }

    /// Returns a restore action that needs the service frozen after a restore.
    #[must_use]
    pub fn restore_not_frozen() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "restore_not_frozen",
            "the service is not frozen after a restore",
        )
    }

    /// Returns an unfreeze refused because a chain is not rescanned since the restore.
    #[must_use]
    pub fn restore_rescan_incomplete() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "restore_rescan_incomplete",
            "a chain is not rescanned since the restore; see GET /v1/admin/restore",
        )
    }

    /// Returns an internal failure without exposing its cause.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "request could not be completed",
        )
    }

    /// Names the request parameter the error is about.
    #[must_use]
    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.detail.param = Some(param.into());
        self
    }

    /// Sets `Retry-After`, at least one second.
    #[must_use]
    fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds.max(1));
        self
    }

    /// The error's stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.detail.code
    }

    /// Whether the error answers a request no handler executed, which an idempotency key does
    /// not save ([`NotExecuted`]).
    fn not_executed(&self) -> bool {
        self.detail.code.starts_with("parameter_")
            || self.status == StatusCode::UNAUTHORIZED
            || self.status == StatusCode::FORBIDDEN
            || self.status == StatusCode::TOO_MANY_REQUESTS
            || self.status == StatusCode::SERVICE_UNAVAILABLE
    }

    /// The request parameter the error names.
    #[cfg(test)]
    pub(crate) fn param(&self) -> Option<&str> {
        self.detail.param.as_deref()
    }

    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        let error_type = if status.is_server_error() {
            ErrorType::Api
        } else {
            ErrorType::InvalidRequest
        };
        Self {
            status,
            detail: ErrorDetail {
                error_type,
                code,
                message: message.into(),
                param: None,
                doc_url: format!("{DOCS_URL}{code}"),
            },
            retry_after: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let not_executed = self.not_executed();
        let retry_after = match (self.status, self.retry_after) {
            (StatusCode::TOO_MANY_REQUESTS, seconds) => Some(seconds.unwrap_or(1)),
            (_, seconds) => seconds,
        };
        let mut response =
            (self.status, Json(ErrorResponse { error: self.detail })).into_response();
        if let Some(seconds) = retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if not_executed {
            response.extensions_mut().insert(NotExecuted);
        }
        response
    }
}

impl From<sqlx::Error> for ApiError {
    /// A `500`, except when no pool connection was acquired or PostgreSQL rolled back a
    /// deadlock or serialization failure (`40P01`, `40001`): the request is a `503` to retry,
    /// which an idempotency key does not save.
    fn from(error: sqlx::Error) -> Self {
        if matches!(error, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed) {
            if matches!(error, sqlx::Error::PoolTimedOut) {
                crate::observability::metrics::pool_acquire_timed_out();
            }
            tracing::warn!(%error, "database connection was not acquired");
            return Self::database_busy();
        }
        let code = error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        if matches!(code.as_deref(), Some("40P01" | "40001")) {
            tracing::warn!(%error, "transaction rolled back by a conflict");
            return Self::database_busy();
        }
        tracing::error!(%error, "database operation failed");
        Self::internal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constructor's code is documented, with its status.
    #[test]
    fn every_code_is_documented_with_its_status() {
        let errors = [
            ApiError::bad_request(""),
            ApiError::missing_param(""),
            ApiError::unknown_param(""),
            ApiError::amount_too_small("amount", ""),
            ApiError::amount_too_large("amount", ""),
            ApiError::api_key_missing(),
            ApiError::api_key_invalid(),
            ApiError::api_key_expired(),
            ApiError::permission_denied(),
            ApiError::not_found(),
            ApiError::exposure_cap(""),
            ApiError::quote_payment_received(),
            ApiError::quote_window_closed(),
            ApiError::quote_unexpected_state("expired"),
            ApiError::deposit_not_refundable(),
            ApiError::deposit_not_final(),
            ApiError::destination_sanctioned(),
            ApiError::transfer_already_used(),
            ApiError::refund_unexpected_state("failed"),
            ApiError::idempotency_key_reused(),
            ApiError::idempotency_key_in_use(),
            ApiError::too_many_requests(),
            ApiError::api_key_inactive(),
            ApiError::last_api_key(),
            ApiError::testmode_charges_only(),
            ApiError::client_reads_limited(1),
            ApiError::customer_quote_limit(1),
            ApiError::customer_rotation_limit(1),
            ApiError::deposit_address_cap(""),
            ApiError::webhook_endpoint_cap(),
            ApiError::webhook_endpoint_disabled(),
            ApiError::deposit_address_retired(),
            ApiError::asset_not_accepted(None),
            ApiError::payment_settings_unconfirmed(),
            ApiError::treasury_not_set(),
            ApiError::treasury_proof_invalid("signature", ""),
            ApiError::treasury_challenge_expired(),
            ApiError::treasury_challenge_used(),
            ApiError::treasury_not_deployed(),
            ApiError::treasury_sanctioned(),
            ApiError::treasury_change_pending(),
            ApiError::treasury_unchanged(),
            ApiError::treasury_unexpected_state("current"),
            ApiError::deposit_unexpected_state("credited"),
            ApiError::paused(""),
            ApiError::chain_frozen(),
            ApiError::service_unavailable(""),
            ApiError::request_deadline_exceeded(),
            ApiError::service_restoring(300),
            ApiError::service_maintenance(),
            ApiError::restore_not_frozen(),
            ApiError::restore_rescan_incomplete(),
            ApiError::internal(),
            ApiError::unauthorized(),
            ApiError::signature_replayed(),
        ];
        for error in errors {
            let documented = ERROR_CODES
                .iter()
                .find(|(code, _, _)| *code == error.code())
                .map(|(_, status, _)| *status);
            assert_eq!(
                documented,
                Some(error.status.as_u16()),
                "{} is documented with its status",
                error.code()
            );
            assert_eq!(error.detail.doc_url, format!("{DOCS_URL}{}", error.code()));
        }
    }

    #[test]
    fn pool_timed_out_is_unsaved_database_busy() {
        for error in [sqlx::Error::PoolTimedOut, sqlx::Error::PoolClosed] {
            let error = ApiError::from(error);
            assert_eq!(error.code(), "unavailable");
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert!(response.extensions().get::<NotExecuted>().is_some());
        }
    }

    /// Only an idempotency key in use is a `409`, and every `429` says when to retry.
    #[test]
    fn statuses_follow_stripe() {
        for (code, status, _) in ERROR_CODES {
            assert_eq!(
                *status == 409,
                *code == "idempotency_key_in_use",
                "{code} is {status}"
            );
        }
        for error in [
            ApiError::too_many_requests(),
            ApiError::customer_quote_limit(42),
            ApiError::client_reads_limited(0),
        ] {
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(response.headers().contains_key(header::RETRY_AFTER));
            assert!(response.extensions().get::<NotExecuted>().is_some());
        }
        let replayable = ApiError::deposit_not_final().into_response();
        assert_eq!(replayable.status(), StatusCode::BAD_REQUEST);
        assert!(replayable.extensions().get::<NotExecuted>().is_none());
    }
}
