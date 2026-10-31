//! The merchant's API keys (`/v1/api_keys`, design D7): list, create, roll, and revoke the keys
//! of the requesting key's account and mode with a secret key. A test key never reaches live keys.
//! A secret key may create restricted keys (design PR 12), which hold only the permissions they
//! are granted.

use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};

use crate::api_keys::{self, ApiKey, ApiKeyError, IssuedKey, KeyKind};
use crate::ids;
use crate::tenancy::Permission;

use super::AppState;
use super::auth::Merchant;
use super::error::{ApiError, ErrorResponse};
use super::extract::{ApiJson, ApiPath, query_pairs};
use super::idempotency::{ContainsSecret, Idempotent};
use super::models::{ApiKeyList, ApiKeyObject, CreateApiKeyRequest, RollApiKeyRequest};
use super::pagination::Page;

type ApiResult<T> = Result<T, ApiError>;

/// The shortest `expires_in` of a key rolling itself: the old key must outlive a lost response,
/// whose new secret a replay never returns, so the merchant can recover by rolling the new key.
const MIN_SELF_ROLL_EXPIRES_IN: u32 = 3_600;

#[utoipa::path(
    get,
    path = "/v1/api_keys",
    params(
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`key_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`key_` id: the page before it")
    ),
    responses(
        (status = 200, description = "OK", body = ApiKeyList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "api_keys"
)]
/// The keys of the requesting key's account and mode, newest first, without their secrets, with
/// Stripe's cursor pagination.
pub(crate) async fn list_api_keys(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<ApiKeyList>> {
    let mut page = Page::default();
    for (name, value) in query_pairs(query.as_deref()) {
        if !page.accept(&name, &value, ids::API_KEY)? {
            return Err(
                ApiError::unknown_param(format!("unknown parameter {name}")).with_param(name)
            );
        }
    }
    let cursor = page.cursor.map(|id| (id, page.before));
    let (keys, has_more) = api_keys::list(&state.pool, merchant.scope, page.limit, cursor)
        .await
        .map_err(map_error)?
        .ok_or_else(|| page.unknown_cursor("API key"))?;
    Ok(Json(ApiKeyList {
        object: "list".to_owned(),
        url: "/v1/api_keys".to_owned(),
        has_more,
        data: keys.iter().map(|key| api_key_object(key, None)).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/api_keys/{id}",
    params(("id" = String, Path, description = "Key id, `key_…`")),
    responses(
        (status = 200, description = "OK", body = ApiKeyObject),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "api_keys"
)]
/// One key of the requesting key's account and mode, without its secret.
pub(crate) async fn get_api_key(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ApiKeyObject>> {
    let id = ids::parse(ids::API_KEY, &id).ok_or_else(ApiError::not_found)?;
    let key = api_keys::get(&state.pool, merchant.scope, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(api_key_object(&key, None)))
}

#[utoipa::path(
    post,
    path = "/v1/api_keys",
    params(
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = CreateApiKeyRequest,
    responses(
        (status = 200, description = "OK: the key with its `secret`, shown once", body = ApiKeyObject),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "api_keys"
)]
/// Creates a key in the requesting key's mode: a secret key, or with `type: restricted` a
/// restricted key (`ppay_rk_…`) holding only `permissions`, Stripe's restricted keys. Run
/// production servers with a restricted key and keep secret keys for administration. The
/// response is the only time its `secret` is shown; a replay of the request (`Idempotency-Key`)
/// returns the key without it.
pub(crate) async fn create_api_key(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiJson(request): ApiJson<CreateApiKeyRequest>,
) -> ApiResult<Response> {
    validate_name(&request.name)?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    let issued = match (request.key_type.as_deref(), request.permissions) {
        (None | Some("secret"), None) => {
            api_keys::create(
                &mut *transaction,
                merchant.scope,
                &request.name,
                &merchant.actor(),
                "",
            )
            .await
        }
        (None | Some("secret"), Some(_)) => {
            return Err(ApiError::invalid_param(
                "permissions",
                "permissions apply only to a key of type restricted",
            ));
        }
        (Some("restricted"), permissions) => {
            let permissions = parse_permissions(permissions.unwrap_or_default())?;
            api_keys::create_restricted(
                &mut *transaction,
                merchant.scope,
                &request.name,
                &permissions,
                &merchant.actor(),
            )
            .await
        }
        (Some(_), _) => {
            return Err(ApiError::invalid_param(
                "type",
                "type must be secret or restricted",
            ));
        }
    }
    .map_err(map_error)?;
    let response = idempotent
        .commit(transaction, issued_response(&issued))
        .await?;
    state.rate_limits.invalidate_api_key(&issued.secret);
    Ok(response)
}

#[utoipa::path(
    post,
    path = "/v1/api_keys/{id}/roll",
    params(
        ("id" = String, Path, description = "Key id, `key_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = RollApiKeyRequest,
    responses(
        (status = 200, description = "OK: the new key with its `secret`, shown once", body = ApiKeyObject),
        (status = 400, description = "Bad Request, `parameter_invalid` for a key rolling itself with `expires_in` under 3600, or `api_key_inactive`: revoked or already rolled", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "api_keys"
)]
/// Rolls a key: returns a new key of the same type, name, and permissions, and the old key keeps
/// working for `expires_in` seconds (at most 7 days), Stripe's roll; `0`, the default, revokes it
/// at once. A secret key may roll itself, keeping itself working for at least an hour
/// (`expires_in` ≥ 3600): the new key's secret is shown only in this response, and a replay omits
/// it, so if the response is lost, roll the new key (its id is in the replay) with the old key
/// while it still works. To stop the old key sooner, revoke it with the new key.
pub(crate) async fn roll_api_key(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<RollApiKeyRequest>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::API_KEY, &id).ok_or_else(ApiError::not_found)?;
    if id == merchant.key.id && request.expires_in < MIN_SELF_ROLL_EXPIRES_IN {
        return Err(ApiError::invalid_param(
            "expires_in",
            "a key rolling itself keeps working for at least 3600 seconds, so a lost response can \
             be recovered with it: roll the new key with this one; revoke this key with the new \
             one to stop it sooner",
        ));
    }
    let mut transaction = idempotent.begin(&state.pool).await?;
    let issued = api_keys::roll(
        &mut *transaction,
        merchant.scope,
        id,
        Duration::seconds(i64::from(request.expires_in)),
        &merchant.actor(),
    )
    .await
    .map_err(map_error)?;
    let response = idempotent
        .commit(transaction, issued_response(&issued))
        .await?;
    state.rate_limits.invalidate_api_key(&issued.secret);
    Ok(response)
}

#[utoipa::path(
    delete,
    path = "/v1/api_keys/{id}",
    params(("id" = String, Path, description = "Key id, `key_…`")),
    responses(
        (status = 200, description = "OK: revoked, or already revoked", body = ApiKeyObject),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse),
        (
            status = 400,
            description = "`last_api_key`: the mode's last key that is neither revoked nor \
                           expiring; create or roll a key first",
            body = ErrorResponse
        )
    ),
    security(("api_key" = [])),
    tag = "api_keys"
)]
/// Revokes a key at once. The mode's last key that is neither revoked nor expiring cannot be
/// revoked, so the account always keeps a working key; to replace a leaked last key, roll it and
/// revoke it with the new key.
pub(crate) async fn revoke_api_key(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ApiKeyObject>> {
    let id = ids::parse(ids::API_KEY, &id).ok_or_else(ApiError::not_found)?;
    let key = api_keys::revoke(&state.pool, merchant.scope, id, &merchant.actor())
        .await
        .map_err(map_error)?;
    Ok(Json(api_key_object(&key, None)))
}

/// A newly issued key's response: shown with its secret, which the idempotency layer never
/// stores.
pub(crate) fn issued_response(issued: &IssuedKey) -> Response {
    (
        Extension(ContainsSecret),
        Json(api_key_object(
            &issued.key,
            Some(issued.secret.as_str().to_owned()),
        )),
    )
        .into_response()
}

/// The API representation of `key`.
pub(crate) fn api_key_object(key: &ApiKey, secret: Option<String>) -> ApiKeyObject {
    let now = Utc::now();
    let status = if key.revoked_at.is_some() {
        "revoked"
    } else {
        match key.expires_at {
            None => "active",
            Some(expires_at) if expires_at > now => "expiring",
            Some(_) => "expired",
        }
    };
    ApiKeyObject {
        id: key.public_id(),
        object: "api_key".to_owned(),
        livemode: key.livemode,
        key_type: match key.kind {
            KeyKind::Secret => "secret",
            KeyKind::Restricted => "restricted",
        }
        .to_owned(),
        name: key.name.clone(),
        permissions: key.permissions.as_ref().map(|permissions| {
            permissions
                .iter()
                .map(|permission| permission.code().to_owned())
                .collect()
        }),
        secret,
        redacted: format!("{}…{}", key.prefix, key.last4),
        status: status.to_owned(),
        created: key.created_at.timestamp(),
        expires_at: key.expires_at.map(|time| time.timestamp()),
        last_used: key.last_used_at.map(|time| time.timestamp()),
    }
}

/// A restricted key's requested permission codes; at least one, each a known permission.
fn parse_permissions(codes: Vec<String>) -> ApiResult<Vec<Permission>> {
    if codes.is_empty() {
        return Err(ApiError::invalid_param(
            "permissions",
            "a restricted key needs at least one permission",
        ));
    }
    codes
        .iter()
        .map(|code| {
            Permission::parse(code).ok_or_else(|| {
                ApiError::invalid_param("permissions", format!("unknown permission {code}"))
            })
        })
        .collect()
}

/// A key's label is at most 200 characters.
pub(crate) fn validate_name(name: &str) -> ApiResult<()> {
    if name.chars().count() > 200 {
        return Err(ApiError::invalid_param(
            "name",
            "name must be at most 200 characters",
        ));
    }
    Ok(())
}

/// Maps a key repository failure to its API error.
pub(crate) fn map_error(error: ApiKeyError) -> ApiError {
    match error {
        ApiKeyError::NotFound => ApiError::not_found(),
        ApiKeyError::Inactive => ApiError::api_key_inactive(),
        ApiKeyError::LastActiveKey => ApiError::last_api_key(),
        ApiKeyError::ChargesNotEnabled => ApiError::testmode_charges_only(),
        ApiKeyError::InvalidExpiry => ApiError::invalid_param(
            "expires_in",
            "expires_in must be between 0 and 604800 seconds (7 days)",
        ),
        ApiKeyError::PermissionNotGrantable(code) => ApiError::invalid_param(
            "permissions",
            format!(
                "{code} cannot be granted to a restricted key: keys, treasuries, webhook \
                 endpoints, webhook keys, and account settings need a secret key"
            ),
        ),
        ApiKeyError::EntropyUnavailable => ApiError::internal(),
        ApiKeyError::Database(error) => error.into(),
    }
}
