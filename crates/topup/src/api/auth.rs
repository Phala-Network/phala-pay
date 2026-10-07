//! Request authentication: merchants present an API key as a Bearer token (design D7); the
//! operator's admin API keeps its RFC 9421 HTTP Message Signatures.

use super::AppState;
use super::error::ApiError;
use super::repository;
use axum::body::{Body, to_bytes};
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use ed25519_dalek::VerifyingKey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use topup_adapters::http_signature::{self, PublicOrigin, SignedMessage};
use zeroize::Zeroizing;

use crate::api_keys::{self, ApiKey, Rejection};
use crate::audit::{Actor, RequestRef};
use crate::db::Account;
use crate::tenancy::Scope;

const MAX_SIGNED_BODY_BYTES: usize = 1_048_576;
const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// How long a well-formed API key waits for a database authentication slot.
const SLOT_WAIT: Duration = Duration::from_millis(250);

/// Both Bearer entry points share slots for authentication's database work only.
#[derive(Clone)]
pub(super) struct MerchantAuthState {
    app: AppState,
    slots: Arc<Semaphore>,
}

impl MerchantAuthState {
    pub(super) fn new(app: AppState) -> Self {
        let slots = usize::try_from(app.pool.options().get_max_connections() / 2)
            .unwrap_or(1)
            .max(1);
        Self {
            app,
            slots: Arc::new(Semaphore::new(slots)),
        }
    }
}

/// A configured RFC 9421 ed25519 verification key of the admin API.
#[derive(Clone, Debug)]
pub struct VerificationKey {
    /// Key identifier required in `Signature-Input`.
    pub kid: String,
    key: VerifyingKey,
}

impl VerificationKey {
    pub(crate) fn same_public_key(&self, other: &Self) -> bool {
        self.key == other.key
    }

    /// Parses a standard-base64 raw ed25519 public key.
    pub fn from_base64(kid: String, encoded: &str) -> Result<Self, &'static str> {
        let decoded = STANDARD
            .decode(encoded)
            .map_err(|_| "public key must be standard base64")?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| "public key must contain 32 bytes")?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| "public key is not a valid ed25519 key")?;
        Ok(Self { kid, key })
    }
}

/// The authenticated merchant of a request: its account, the [`Scope`] every query it makes
/// takes, and the API key it presented.
#[derive(Clone, Debug)]
pub(crate) struct Merchant {
    /// The account the key belongs to.
    pub(crate) account: Account,
    /// Built from the key alone: its account and mode.
    pub(crate) scope: Scope,
    /// The key the request authenticated with.
    pub(crate) key: ApiKey,
    /// The request, recorded on the events it causes.
    pub(crate) request: Option<RequestRef>,
}

impl Merchant {
    /// The audit actor of the merchant's requests: the key, acting through this request.
    pub(crate) fn actor(&self) -> Actor {
        self.key.actor().with_request(self.request.clone())
    }
}

/// Authenticates a merchant request by its `Authorization: Bearer ppay_sk_…` or `ppay_rk_…` key
/// (design D7) and attaches its [`Merchant`], whose scope is the key's account and mode. A key
/// that fails the checksum is refused without a database read; HTTP Basic and any other scheme
/// are refused. A live key of an account the operator has not enabled for live mode is `403
/// testmode_charges_only`. The account and mode's rate limit applies to every authenticated
/// request.
///
/// While the service is frozen after a restore (`crate::restore_mode`) no key authenticates,
/// reads and writes alike: `503 service_restoring`. The restored database may hold a key the
/// merchant revoked after the restore point as valid; keys work again once the operator has
/// revoked such keys again and unfrozen the service. This is the service's one freeze gate for
/// merchant requests: it runs before authorization and the idempotency layer, so a refusal is
/// neither replayed nor saved, and after the key's form and checksum are checked in memory, so a
/// request without a well-formed key is refused without a database read. Database authentication
/// holds at most half the pool's slots; waiting more than 250 ms is `503 database_busy` with
/// `Retry-After: 1`. The slot is released before any handler runs.
pub(super) async fn authenticate_merchant(
    State(auth): State<MerchantAuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = match bearer_key(request.headers()) {
        Ok(presented) => presented,
        Err(error) => return unauthorized(error),
    };
    if api_keys::check_format(&presented).is_none() {
        return unauthorized(ApiError::api_key_invalid());
    }
    let state = &auth.app;
    let authenticated = {
        let _slot = match tokio::time::timeout(SLOT_WAIT, auth.slots.acquire()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) | Err(_) => return ApiError::pre_auth_database_busy().into_response(),
        };
        match crate::restore_mode::is_frozen(&state.pool).await {
            Ok(false) => {}
            Ok(true) => return super::restoring(),
            Err(error) => return ApiError::from(error).into_response(),
        }
        match api_keys::authenticate(&state.pool, &presented).await {
            Ok(Ok(authenticated)) => authenticated,
            Ok(Err(Rejection::Expired)) => return unauthorized(ApiError::api_key_expired()),
            Ok(Err(Rejection::Invalid | Rejection::Revoked)) => {
                return unauthorized(ApiError::api_key_invalid());
            }
            Err(error) => return ApiError::from(error).into_response(),
        }
    };
    let scope = authenticated.key.scope();
    // The operator can turn live mode off after issuing live keys (design D12).
    if scope.livemode() && !authenticated.charges_enabled {
        return ApiError::testmode_charges_only().into_response();
    }
    if !state.rate_limits.allow(scope) {
        return ApiError::too_many_requests().into_response();
    }
    let request_ref = request.extensions().get::<RequestRef>().cloned();
    request.extensions_mut().insert(Merchant {
        account: authenticated.account,
        scope,
        key: authenticated.key,
        request: request_ref,
    });
    next.run(request).await
}

/// Refuses a merchant request whose key does not hold the permission its route requires
/// (`ROUTE_PERMISSIONS`) with `403 permission_denied`. It runs after authentication and before
/// the idempotency layer, so a refused request neither replays nor saves a result. A request by
/// `client_secret`, without a merchant, passes to the public view its route serves.
pub async fn authorize(request: Request, next: Next) -> Response {
    let Some(merchant) = request.extensions().get::<Merchant>() else {
        return next.run(request).await;
    };
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    match path.and_then(|path| super::required_permission(request.method(), path)) {
        Some(permission) if merchant.key.holds(permission) => next.run(request).await,
        Some(_) => ApiError::permission_denied().into_response(),
        None => {
            tracing::error!(?path, method = %request.method(), "route declares no permission");
            ApiError::permission_denied().into_response()
        }
    }
}

/// Passes a request without `Authorization` that carries a `client_secret` query parameter to
/// the handler without a merchant, which then serves the quote's public view; any other request
/// must be an authenticated merchant request.
pub(super) async fn authenticate_merchant_or_client_secret(
    state: State<MerchantAuthState>,
    request: Request,
    next: Next,
) -> Response {
    let anonymous = !request.headers().contains_key(header::AUTHORIZATION);
    let has_client_secret = request.uri().query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes()).any(|(name, _)| name == "client_secret")
    });
    if anonymous && has_client_secret {
        next.run(request).await
    } else {
        authenticate_merchant(state, request, next).await
    }
}

/// The key of `Authorization: Bearer <key>`. The scheme is case-insensitive (RFC 9110 §11.1).
fn bearer_key(headers: &HeaderMap) -> Result<Zeroizing<String>, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(ApiError::api_key_missing)?
        .to_str()
        .map_err(|_| ApiError::api_key_invalid())?;
    let (scheme, key) = value
        .trim()
        .split_once(' ')
        .ok_or_else(ApiError::api_key_invalid)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(ApiError::api_key_invalid());
    }
    Ok(Zeroizing::new(key.trim().to_owned()))
}

/// A `401` with the challenge RFC 6750 §3 asks for.
fn unauthorized(error: ApiError) -> Response {
    let mut response = error.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"Phala Pay\""),
    );
    response
}

/// Shares RFC 9421 origin, body, time and replay checks across operator and maintenance keys.
/// Maintenance authority is an exact method/path allowlist, enforced before admission/handlers.
pub async fn authenticate_admin(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let keys = std::iter::once(&state.admin_key).chain(&state.maintenance_keys);
    let verified = match verify_request(&mut request, &state.public_origin, keys).await {
        Ok(verified) => verified,
        Err(()) => return ApiError::unauthorized().into_response(),
    };
    if let Err(error) = repository::record_signature(&state.pool, &verified).await {
        return error.into_response();
    }
    let actor = Actor::admin(verified.kid.clone())
        .with_request(request.extensions().get::<RequestRef>().cloned());
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    if verified.kid != state.admin_key.kid
        && !(request.method() == Method::POST
            && matches!(
                path,
                Some("/v1/admin/instance/pause" | "/v1/admin/instance/resume")
            ))
    {
        if let Err(error) = crate::audit::insert(
            &state.pool,
            &crate::audit::Entry {
                account_id: None,
                actor: &actor,
                action: "permission_denied",
                // Matched templates omit query strings and user-controlled identifiers.
                subject: path.unwrap_or("admin"),
                reason: "maintenance key is restricted to POST instance pause/resume",
            },
        )
        .await
        {
            return ApiError::from(error).into_response();
        }
        return ApiError::permission_denied().into_response();
    }
    request.extensions_mut().insert(actor);
    next.run(request).await
}

/// The authenticated administrative signer (operator or maintenance key), with the request
/// recorded on the events it causes.
pub(crate) struct AdminActor(pub(crate) Actor);

impl axum::extract::FromRequestParts<AppState> for AdminActor {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Actor>()
            .cloned()
            .map(Self)
            .ok_or_else(ApiError::unauthorized)
    }
}

/// A verified request signature ready for single-use persistence.
pub(crate) struct VerifiedSignature {
    pub(crate) kid: String,
    pub(crate) signature_hash: [u8; 32],
    pub(crate) created: DateTime<Utc>,
}

async fn verify_request<'a>(
    request: &mut Request,
    public_origin: &PublicOrigin,
    keys: impl IntoIterator<Item = &'a VerificationKey>,
) -> Result<VerifiedSignature, ()> {
    let body = std::mem::replace(request.body_mut(), Body::empty());
    let bytes = tokio::time::timeout(
        super::deadlines::BODY_READ_TIMEOUT,
        to_bytes(body, MAX_SIGNED_BODY_BYTES),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    *request.body_mut() = Body::from(bytes.clone());

    // `Host` and `X-Forwarded-*` describe the gateway hop, so the configured origin is used.
    let target_uri = public_origin.target_uri(
        request
            .uri()
            .path_and_query()
            .map_or("/", |value| value.as_str()),
    );
    let headers = request.headers();
    let idempotency_key = headers
        .get(IDEMPOTENCY_HEADER)
        .map(|value| value.to_str().map_err(|_| ()))
        .transpose()?;
    let message = SignedMessage {
        method: request.method().as_str(),
        target_uri: &target_uri,
        content_digest: header_value(headers, "content-digest")?,
        idempotency_key,
        signature_input: header_value(headers, "signature-input")?,
        signature: header_value(headers, "signature")?,
        body: &bytes,
    };
    let now = Utc::now().timestamp();
    for key in keys {
        if let Ok(verified) = http_signature::verify(&message, &key.kid, &key.key, now) {
            return Ok(VerifiedSignature {
                kid: verified.keyid,
                signature_hash: verified.signature_hash,
                created: DateTime::from_timestamp(verified.created, 0).ok_or(())?,
            });
        }
    }
    Err(())
}

fn header_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<&'a str, ()> {
    headers
        .get(name)
        .ok_or(())?
        .to_str()
        .map(str::trim)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use ed25519_dalek::SigningKey;

    use super::*;

    fn signed_message<'a>(
        vector: &'a serde_json::Value,
        target_uri: &'a str,
        body: &'a [u8],
    ) -> SignedMessage<'a> {
        let header = |name: &str| vector["headers"][name].as_str();
        SignedMessage {
            method: vector["method"].as_str().unwrap_or_default(),
            target_uri,
            content_digest: header("content-digest").unwrap_or_default(),
            idempotency_key: header("idempotency-key"),
            signature_input: header("signature-input").unwrap_or_default(),
            signature: header("signature").unwrap_or_default(),
            body,
        }
    }

    /// Requests signed by the Python SDK (`sdk/python/tests/vectors.py`) verify with the shared
    /// verifier, and this module rebuilds the same `@target-uri` from the configured origin and
    /// the request target.
    #[test]
    fn python_sdk_signatures_verify() -> Result<(), Box<dyn Error>> {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/rfc9421-python-signer.json"
        ))?;
        let key = VerificationKey::from_base64(
            fixture["keyid"].as_str().ok_or("keyid")?.to_owned(),
            fixture["public_key"].as_str().ok_or("public_key")?,
        )?;
        let other_key = SigningKey::from_bytes(&[9; 32]).verifying_key();
        let created = fixture["created"].as_i64().ok_or("created")?;
        let vectors = fixture["vectors"].as_array().ok_or("vectors")?;
        let origin = PublicOrigin::parse("http://127.0.0.1:18080")?;
        assert_eq!(vectors.len(), 5);
        assert!(
            vectors
                .iter()
                .any(|vector| vector["headers"]["signature-input"]
                    .as_str()
                    .is_some_and(|input| input.contains(";nonce=\""))),
            "the fixture must cover the nonce parameter"
        );

        for vector in vectors {
            let name = vector["name"].as_str().ok_or("name")?;
            let target_uri = origin.target_uri(vector["target"].as_str().ok_or("target")?);
            assert_eq!(
                Some(target_uri.as_str()),
                vector["target_uri"].as_str(),
                "{name}"
            );
            let body = vector["body"].as_str().ok_or("body")?.as_bytes();
            let message = |body| signed_message(vector, &target_uri, body);

            let verified =
                http_signature::verify(&message(body), &key.kid, &key.key, created + 300)
                    .map_err(|_| format!("{name}: Python signature must verify"))?;
            assert_eq!(verified.keyid, "sdk-vector/v1", "{name}");
            assert_eq!(verified.created, created, "{name}");

            assert!(
                http_signature::verify(&message(body), &key.kid, &key.key, created + 301).is_err(),
                "{name}: stale signature must fail"
            );
            assert!(
                http_signature::verify(&message(b"altered"), &key.kid, &key.key, created).is_err(),
                "{name}: altered body must fail"
            );
            assert!(
                http_signature::verify(&message(body), &key.kid, &other_key, created).is_err(),
                "{name}: wrong key must fail"
            );
        }
        Ok(())
    }
}
