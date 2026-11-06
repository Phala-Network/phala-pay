//! Admin-only, nonce-bound evidence for the migration export verification key.

use axum::Json;
use topup_adapters::{attestation::DstackAttestor, signer::dstack::DstackSigner};

use super::error::{ApiError, ErrorResponse};
use super::extract::ApiQuery;
use super::models::AttestationQuery;
use crate::migration::KeyEvidence;

#[utoipa::path(
    get,
    path = "/v1/admin/migration/export-key",
    params(AttestationQuery),
    responses(
        (status = 200, description = "Nonce-bound export key evidence", body = KeyEvidence),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 503, description = "Attestation unavailable", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Returns the old app's migration/export/v1 Ed25519 verification key. Verify the TDX quote
/// and app identity before pinning it. The quote binds SHA256("phala-pay/migration/export-key/v1\\0"
/// || len(nonce):u8 || nonce || raw_key). The nonce must be 1 to 32 hexadecimal bytes.
pub(crate) async fn export_key(
    ApiQuery(query): ApiQuery<AttestationQuery>,
) -> Result<Json<KeyEvidence>, ApiError> {
    if query.nonce.is_empty() || query.nonce.len() > 64 {
        return Err(ApiError::bad_request(
            "nonce must be 1 to 32 bytes of hexadecimal",
        ));
    }
    let nonce = hex::decode(query.nonce)
        .map_err(|_| ApiError::bad_request("nonce must be valid hexadecimal"))?;
    crate::migration::export_key(&DstackSigner::new(), &DstackAttestor::new(), &nonce)
        .await
        .map(Json)
        .map_err(|_| ApiError::service_unavailable("migration key attestation is unavailable"))
}
