//! Attestation boundary between the API and the dstack attestor.

use std::future::Future;
use std::pin::Pin;

use topup_adapters::attestation::{AttestedWebhookKey, DstackAttestor};

/// Boxed attestation operation suitable for an application-state trait object.
pub type AttestationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AttestationEvidence, AttestationError>> + Send + 'a>>;

/// Failure to collect current attestation evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttestationError {
    /// The configured attestation provider is temporarily unavailable.
    Unavailable,
}

/// What an attestation binds: a nonce, and an account's webhook keys in one mode.
#[derive(Clone, Copy, Debug)]
pub struct AttestationRequest<'a> {
    /// Decoded caller nonce, at most 32 bytes.
    pub nonce: &'a [u8],
    /// The caller's account, `acct_…`.
    pub account: &'a str,
    /// The caller's mode.
    pub livemode: bool,
    /// Key versions that sign the account's deliveries, current first.
    pub versions: &'a [u32],
}

/// Evidence binding a request (`topup_adapters::attestation::report_data`).
#[derive(Clone, Debug)]
pub struct AttestationEvidence {
    /// The requested keys, in the requested order.
    pub webhook_keys: Vec<AttestedWebhookKey>,
    /// The report data the quote carries.
    pub report_data: [u8; 32],
    /// Versioned dstack attestation bytes.
    pub quote: Vec<u8>,
}

/// Boxed derivation of an account's public webhook keys.
pub type WebhookKeysFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<AttestedWebhookKey>, AttestationError>> + Send + 'a>>;

/// Boxed collection of public guest Info metadata.
pub type GuestInfoFuture<'a> =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, AttestationError>> + Send + 'a>>;

/// Runtime source of nonce-bound webhook-key evidence.
pub trait Attestor: Send + Sync {
    /// Returns public guest metadata; providers without a guest fail closed.
    fn guest_info(&self) -> GuestInfoFuture<'_> {
        Box::pin(async { Err(AttestationError::Unavailable) })
    }

    /// Derives the requested keys and collects evidence binding them, the account, the mode, and
    /// the nonce.
    fn attest<'a>(&'a self, request: AttestationRequest<'a>) -> AttestationFuture<'a>;

    /// Derives the public keys of `account` in the given mode at `versions`, in order, without
    /// evidence: the keys an attestation binds, which verify the account's deliveries.
    fn webhook_keys<'a>(
        &'a self,
        account: &'a str,
        livemode: bool,
        versions: &'a [u32],
    ) -> WebhookKeysFuture<'a>;
}

impl Attestor for DstackAttestor {
    fn guest_info(&self) -> GuestInfoFuture<'_> {
        Box::pin(async move {
            DstackAttestor::guest_info(self)
                .await
                .map_err(|_| AttestationError::Unavailable)
        })
    }

    fn attest<'a>(&'a self, request: AttestationRequest<'a>) -> AttestationFuture<'a> {
        Box::pin(async move {
            let evidence = DstackAttestor::attest(
                self,
                request.nonce,
                request.account,
                request.livemode,
                request.versions,
            )
            .await
            .map_err(|_| AttestationError::Unavailable)?;
            Ok(AttestationEvidence {
                webhook_keys: evidence.webhook_keys,
                report_data: evidence.report_data,
                quote: evidence.quote,
            })
        })
    }

    fn webhook_keys<'a>(
        &'a self,
        account: &'a str,
        livemode: bool,
        versions: &'a [u32],
    ) -> WebhookKeysFuture<'a> {
        Box::pin(async move {
            DstackAttestor::webhook_keys(self, account, livemode, versions)
                .await
                .map_err(|_| AttestationError::Unavailable)
        })
    }
}

/// Public metadata from the local dstack guest's v0 Info, cached in process for 60 seconds.
/// Includes app/instance/device ids, public app certificate, `tcb_info` (SDK-decoded object),
/// `compose_hash`, `os_image_hash`, `mr_aggregated`, `key_provider_info`, and `vm_config`.
/// No environment values or private keys. This transport is untrusted: verify `vm_config`
/// and the measurements against the quote using the official dstack verifier.
#[utoipa::path(
    get,
    path = "/v1/attestation/info",
    tag = "attestation",
    responses((status = 200, description = "Public dstack guest Info (v0 SDK representation)", body = Object)),
    security(())
)]
pub(super) async fn get_guest_info(
    axum::extract::State(state): axum::extract::State<super::AppState>,
) -> Result<impl axum::response::IntoResponse, super::error::ApiError> {
    let info = state.attestor.guest_info().await.map_err(|_| {
        super::error::ApiError::service_unavailable("guest Info is unavailable; retry")
    })?;
    Ok((
        [(axum::http::header::CACHE_CONTROL, "public, max-age=60")],
        axum::Json(info),
    ))
}
