//! The operator's reconciliation after a restore from backup (`crate::restore_mode`,
//! `deploy/RESTORE.md`): the freeze's status, re-applying the security changes made after the
//! restore point, restoring treasury changes that applied after it, re-issuing deposit addresses
//! and quotes given out after it, importing the events
//! the merchant received after it, and unfreezing. Every write needs an active freeze, except
//! discarding a delivered credit the chain contradicts, and is audited.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr as _;

use alloy_primitives::{Address as EvmAddress, B256, U256};
use axum::Json;
use axum::extract::State;
use chrono::DateTime;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use topup_core::identity::deposit_revision_id;
use topup_core::money::{AtomicAmount, MinorAmount, PRICE_SCALE, ScaledPrice};
use topup_core::valuation::ValuationSource;
use uuid::Uuid;

use crate::api_keys;
use crate::audit::{self, Actor};
use crate::deposit_addresses::{self, ChainContracts, ReissueTarget};
use crate::ids;
use crate::locks::{self, ReissuedTerms};
use crate::outbox::SignedWebhook;
use crate::restore_mode::{self, DeliveredCredit, DeliveredEvent, DeliveredIdentity, Restore};
use crate::tenancy::Scope;
use crate::treasuries::{self, Status as TreasuryStatus};

use super::AppState;
use super::auth::AdminActor;
use super::error::{ApiError, ErrorResponse};
use super::extract::ApiJson;
use super::handlers::{parse_account_id, validate_client_reference_id, validate_reason};
use super::models::{
    self, ApiKeyObject, DeletedWebhookEndpoint, EventImport, RestoreApiKeyRevokeRequest,
    RestoreDeliveredCreditDiscardRequest, RestoreDeliveredCreditDiscardResponse,
    RestoreDepositAddressRequest, RestoreDepositAddressResponse, RestoreEventsImportRequest,
    RestoreEventsImportResponse, RestoreObject, RestoreQuoteRequest, RestoreQuoteResponse,
    RestoreStatus, RestoreTreasuryApplyRequest, RestoreTreasuryApplyResponse,
    RestoreTreasuryVerifyRequest, RestoreTreasuryVerifyResponse, RestoreUnfreezeRequest,
    RestoreWebhookEndpointDeleteRequest, TreasuryVerification,
};

type ApiResult<T> = Result<T, ApiError>;

/// At most this many treasuries or events per request.
const MAX_ITEMS: usize = 100;

#[utoipa::path(
    get,
    path = "/v1/admin/restore",
    responses(
        (status = 200, description = "OK", body = RestoreStatus),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Whether the service is frozen after a restore from backup, the latest restore, each chain's
/// rescan since it, and the delivered events imported for it compared with the ledger.
pub(crate) async fn get_restore(State(state): State<AppState>) -> ApiResult<Json<RestoreStatus>> {
    let latest = restore_mode::latest(&state.pool).await?;
    let (rescan, delivered_events) = match &latest {
        Some(restore) => {
            let chains = restore_mode::rescan_progress(&state.pool, restore, &state.routes).await?;
            let (imported, findings) =
                restore_mode::delivered_event_findings(&state.pool, restore).await?;
            (
                chains.into_iter().map(chain_rescan).collect(),
                models::DeliveredEvents {
                    imported,
                    findings: findings
                        .into_iter()
                        .map(|finding| models::DeliveredEventFinding {
                            event: ids::format(ids::EVENT, finding.event_id),
                            event_type: finding.event_type,
                            deposit: ids::format(ids::DEPOSIT, finding.deposit_id),
                            status: finding.status.to_owned(),
                            delivered_amount_atomic: finding.delivered_amount_atomic,
                            delivered_amount: finding.delivered_amount,
                            ledger_amount_atomic: finding.ledger_amount_atomic,
                            ledger_amount: finding.ledger_amount,
                        })
                        .collect(),
                },
            )
        }
        None => (
            Vec::new(),
            models::DeliveredEvents {
                imported: 0,
                findings: Vec::new(),
            },
        ),
    };
    Ok(Json(RestoreStatus {
        object: "restore_status".to_owned(),
        frozen: latest
            .as_ref()
            .is_some_and(|restore| restore.unfrozen_at.is_none()),
        restore: latest.as_ref().map(restore_object),
        rescan,
        delivered_events,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/api_keys/revoke",
    request_body = RestoreApiKeyRevokeRequest,
    responses(
        (status = 200, description = "OK: revoked, or already revoked", body = ApiKeyObject),
        (status = 400, description = "Bad Request: no or both selectors, several keys match (name it by `id`), or `last_api_key` (issue a recovery key with `revoke_existing`); or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no key of the account matches", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Revokes again a key the merchant revoked after the restore point, which the restore made
/// valid again: by its `id`, or by its `prefix` and `last4` as the merchant's records show it.
/// Announced as `api_key.revoked`. Audited.
pub(crate) async fn revoke_api_key(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreApiKeyRevokeRequest>,
) -> ApiResult<Json<ApiKeyObject>> {
    validate_reason(&request.reason)?;
    let restore = frozen(&state).await?;
    let account_id = parse_account_id(&request.account)?;
    let key = match (&request.id, &request.prefix, &request.last4) {
        (Some(id), None, None) => {
            let id = ids::parse(ids::API_KEY, id)
                .ok_or_else(|| ApiError::invalid_param("id", "id must be a key_ id"))?;
            let mut found = None;
            for livemode in [false, true] {
                if let Some(key) =
                    api_keys::get(&state.pool, Scope::new(account_id, livemode), id).await?
                {
                    found = Some(key);
                }
            }
            found.ok_or_else(ApiError::not_found)?
        }
        (None, Some(prefix), Some(last4)) => {
            let livemode = key_mode(prefix)?;
            let scope = Scope::new(account_id, livemode);
            // The keys with this prefix and last four, not revoked first.
            let matching: Vec<(Uuid, bool)> = sqlx::query_as(
                "SELECT id, revoked_at IS NOT NULL FROM api_keys \
                 WHERE account_id = $1 AND livemode = $2 AND prefix = $3 AND last4 = $4 \
                 ORDER BY revoked_at IS NOT NULL, created_at",
            )
            .bind(account_id)
            .bind(livemode)
            .bind(prefix)
            .bind(last4)
            .fetch_all(&state.pool)
            .await?;
            let id = match matching.as_slice() {
                [] => return Err(ApiError::not_found()),
                [(id, _)] | [(id, false), (_, true), ..] | [(id, true), ..] => *id,
                _ => {
                    return Err(ApiError::bad_request(
                        "several keys have this prefix and last4; revoke by id",
                    ));
                }
            };
            api_keys::get(&state.pool, scope, id)
                .await?
                .ok_or_else(ApiError::not_found)?
        }
        _ => {
            return Err(ApiError::bad_request(
                "name the key by id, or by prefix and last4",
            ));
        }
    };
    let revoked = api_keys::revoke(&state.pool, key.scope(), key.id, &actor)
        .await
        .map_err(super::keys::map_error)?;
    record(
        &state,
        &restore,
        Some(account_id),
        &actor,
        "restore.api_key_revoke",
        &format!("api_key:{}", revoked.public_id()),
        &request.reason,
    )
    .await?;
    Ok(Json(super::keys::api_key_object(&revoked, None)))
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/treasuries/verify",
    request_body = RestoreTreasuryVerifyRequest,
    responses(
        (status = 200, description = "OK", body = RestoreTreasuryVerifyResponse),
        (status = 400, description = "Bad Request; or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Compares the treasuries of the merchant's latest `treasury` events with the restored ones and,
/// with `reapply`, cancels again each pending change the merchant canceled after the restore point
/// (while frozen no treasury change applies, so none takes effect first) and pauses or resumes
/// crediting again as the merchant last did. A change that applied after the restore point is
/// `application_lost`: it is restored from its signed `treasury.updated`
/// ([`apply_treasury`]). Audited, each change announced as `treasury.*`.
pub(crate) async fn verify_treasuries(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreTreasuryVerifyRequest>,
) -> ApiResult<Json<RestoreTreasuryVerifyResponse>> {
    validate_reason(&request.reason)?;
    if request.treasuries.is_empty() || request.treasuries.len() > MAX_ITEMS {
        return Err(ApiError::invalid_param(
            "treasuries",
            "treasuries must hold 1 to 100 objects",
        ));
    }
    let restore = frozen(&state).await?;
    let scope = Scope::new(parse_account_id(&request.account)?, request.livemode);
    let mut ids_seen = BTreeSet::new();
    let mut received = Vec::new();
    for treasury in &request.treasuries {
        let id = ids::parse(ids::TREASURY, &treasury.id)
            .ok_or_else(|| ApiError::invalid_param("treasuries", "id must be a trs_ id"))?;
        if !ids_seen.insert(id) {
            return Err(ApiError::invalid_param(
                "treasuries",
                "send one object per treasury, the latest received",
            ));
        }
        let address = EvmAddress::from_str(&treasury.address)
            .map_err(|_| ApiError::invalid_param("treasuries", "address must be an address"))?;
        received.push((id, treasury, address));
    }
    // The changes the merchant received applied, whatever their order in the request.
    let received_active: BTreeSet<(Uuid, u64, EvmAddress)> = received
        .iter()
        .filter(|(_, treasury, _)| treasury.status == TreasuryStatus::Active.code())
        .map(|(id, treasury, address)| (*id, treasury.chain_id, *address))
        .collect();
    let mut data = Vec::new();
    for (id, treasury, address) in received {
        let current = treasuries::get(&state.pool, scope, id)
            .await
            .map_err(super::treasuries::map_error)?;
        let Some(mut current) = current else {
            data.push(TreasuryVerification {
                id: treasury.id.clone(),
                received_status: treasury.status.clone(),
                status: None,
                result: "missing".to_owned(),
                crediting: treasury
                    .crediting_paused_by
                    .as_ref()
                    .map(|_| "missing".to_owned()),
            });
            continue;
        };
        let same_place = current.chain_id == treasury.chain_id && current.address == address;
        let current_status = super::treasuries::treasury_object(&current).status;
        let result = if !same_place {
            "differs"
        } else if current_status == treasury.status {
            "matches"
        } else if treasury.status == TreasuryStatus::Active.code()
            && current.status == TreasuryStatus::Pending
        {
            // Restored from its signed application, not from this unsigned object.
            "application_lost"
        } else if treasury.status == TreasuryStatus::Replaced.code()
            && current.status == TreasuryStatus::Active
            && treasuries::pending_on(&state.pool, scope, current.chain_id)
                .await
                .map_err(super::treasuries::map_error)?
                .is_some_and(|(pending, pending_address)| {
                    received_active.contains(&(pending, current.chain_id, pending_address))
                })
        {
            // The other half of an `application_lost` in this request: restoring that change,
            // the chain's pending one, replaces this one.
            "replacement_lost"
        } else if treasury.status == "canceled" && current.status == TreasuryStatus::Pending {
            if request.reapply {
                current = treasuries::cancel(&state.pool, scope, &actor, id)
                    .await
                    .map_err(super::treasuries::map_error)?;
                record(
                    &state,
                    &restore,
                    Some(scope.account_id()),
                    &actor,
                    "restore.treasury_cancel",
                    &format!("treasury:{}", treasury.id),
                    &request.reason,
                )
                .await?;
                "canceled"
            } else {
                "cancellation_lost"
            }
        } else {
            "differs"
        };
        // The merchant's own crediting pause (design, "launch hardening"): a pause or resume
        // after the restore point is lost with it. The operator's pauses are its own to re-apply.
        let merchant = crate::pause::PauseOwner::Merchant;
        let crediting = match &treasury.crediting_paused_by {
            None => None,
            Some(_) if !same_place => Some("differs"),
            Some(received) => {
                let paused = received.iter().any(|owner| owner == merchant.code());
                let current_paused = current
                    .crediting_paused_by
                    .iter()
                    .any(|owner| owner == merchant.code());
                Some(if paused == current_paused {
                    "matches"
                } else if request.reapply {
                    current = treasuries::set_crediting_paused(
                        &state.pool,
                        scope,
                        id,
                        merchant,
                        paused,
                        &actor,
                        &format!("restore {}: {}", restore.id, request.reason.trim()),
                    )
                    .await
                    .map_err(super::treasuries::map_error)?;
                    if paused { "paused" } else { "resumed" }
                } else if paused {
                    "pause_lost"
                } else {
                    "resume_lost"
                })
            }
        };
        data.push(TreasuryVerification {
            id: treasury.id.clone(),
            received_status: treasury.status.clone(),
            status: Some(super::treasuries::treasury_object(&current).status),
            result: result.to_owned(),
            crediting: crediting.map(str::to_owned),
        });
    }
    record(
        &state,
        &restore,
        Some(scope.account_id()),
        &actor,
        "restore.treasury_verify",
        &format!("account:{}", request.account),
        &request.reason,
    )
    .await?;
    Ok(Json(RestoreTreasuryVerifyResponse {
        object: "list".to_owned(),
        data,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/treasuries/apply",
    request_body = RestoreTreasuryApplyRequest,
    responses(
        (status = 200, description = "OK: applied, or in force already", body = RestoreTreasuryApplyResponse),
        (status = 400, description = "Bad Request: the delivery's signature does not verify with its account's webhook keys, it is not the `treasury.updated` of a pending change applying, the restored treasury is another or canceled, its time-lock had not ended, or a sanctions list names it now; or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no account has the event's `account`, or the restored database lacks the treasury (proven after the restore point: the merchant proves it again after the unfreeze)", body = ErrorResponse),
        (status = 503, description = "Service Unavailable: the webhook keys cannot be derived; retry", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Applies again a treasury change that applied after the restore point and was lost with it,
/// from the merchant's delivery of its `treasury.updated`: only a delivery the service signed
/// (with one of the account's webhook keys in its mode) of a pending treasury becoming `active`
/// is accepted. The change applies as its time-lock applied it, at the event's `created`, so the
/// deposit addresses and quotes issued over it are re-issued over it while frozen, and those
/// issued before it over the treasury it replaced. It is screened again when screening is
/// available; no event is sent again. Audited in the transaction that applies it.
pub(crate) async fn apply_treasury(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreTreasuryApplyRequest>,
) -> ApiResult<Json<RestoreTreasuryApplyResponse>> {
    validate_reason(&request.reason)?;
    let restore = frozen(&state).await?;
    let delivery = &request.delivery;
    let invalid = |message: &str| ApiError::invalid_param("delivery", message.to_owned());
    let event: Value = serde_json::from_str(&delivery.body)
        .map_err(|_| invalid("the delivery's body is not an event object"))?;
    let applied = delivered_application(&event).ok_or_else(|| {
        invalid(
            "the delivery is not the treasury.updated of a pending treasury change becoming \
             active",
        )
    })?;
    if delivery.webhook_id != ids::format(ids::EVENT, applied.event_id) {
        return Err(invalid("the delivery's webhook_id is not its event's id"));
    }
    let scope = Scope::new(applied.account_id, applied.livemode);
    let keys = webhook_public_keys(&state, scope).await?;
    if !SignedWebhook::verifies(
        &keys,
        &delivery.webhook_id,
        &delivery.webhook_timestamp,
        delivery.body.as_bytes(),
        &delivery.webhook_signature,
    ) {
        return Err(invalid(
            "the delivery's signature does not verify with its account's webhook keys",
        ));
    }
    let (treasury, applied_now) = treasuries::restore_application(
        &state.pool,
        &state.routes,
        state.screening.as_ref(),
        scope,
        applied.application,
        &actor,
        &format!("restore {}: {}", restore.id, request.reason.trim()),
    )
    .await
    .map_err(|error| match error {
        treasuries::RestoreApplicationError::Refused(message) => invalid(message),
        treasuries::RestoreApplicationError::Treasury(error) => super::treasuries::map_error(error),
    })?;
    Ok(Json(RestoreTreasuryApplyResponse {
        applied: applied_now,
        treasury: super::treasuries::treasury_object(&treasury),
    }))
}

/// A treasury change applying, as its delivered `treasury.updated` shows it.
struct DeliveredApplication {
    event_id: Uuid,
    account_id: Uuid,
    livemode: bool,
    application: treasuries::Application,
}

/// Reads a `treasury.updated` whose object became `active` from `pending`: the event the time-lock
/// sends when a change applies, at its `created`.
fn delivered_application(event: &Value) -> Option<DeliveredApplication> {
    let text =
        |value: &Value, field: &str| value.get(field).and_then(Value::as_str).map(str::to_owned);
    if text(event, "type")? != "treasury.updated" {
        return None;
    }
    let data = event.get("data")?;
    let object = data.get("object")?;
    let before = data.get("previous_attributes")?;
    if text(object, "status")? != TreasuryStatus::Active.code()
        || text(before, "status")? != TreasuryStatus::Pending.code()
    {
        return None;
    }
    let livemode = event.get("livemode").and_then(Value::as_bool)?;
    if object.get("livemode").and_then(Value::as_bool) != Some(livemode) {
        return None;
    }
    Some(DeliveredApplication {
        event_id: ids::parse(ids::EVENT, &text(event, "id")?)?,
        account_id: ids::parse(ids::ACCOUNT, &text(event, "account")?)?,
        livemode,
        application: treasuries::Application {
            id: ids::parse(ids::TREASURY, &text(object, "id")?)?,
            chain_id: object.get("chain_id").and_then(Value::as_u64)?,
            address: EvmAddress::from_str(&text(object, "address")?).ok()?,
            applied_at: event
                .get("created")
                .and_then(Value::as_i64)
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))?,
        },
    })
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/webhook_endpoints/delete",
    request_body = RestoreWebhookEndpointDeleteRequest,
    responses(
        (status = 200, description = "OK: deleted", body = DeletedWebhookEndpoint),
        (status = 400, description = "Bad Request; or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no endpoint of the account and mode, or deleted already", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Deletes again an endpoint the merchant deleted after the restore point, before deliveries
/// resume, announced as `webhook_endpoint.deleted`. Audited.
pub(crate) async fn delete_webhook_endpoint(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreWebhookEndpointDeleteRequest>,
) -> ApiResult<Json<DeletedWebhookEndpoint>> {
    validate_reason(&request.reason)?;
    let restore = frozen(&state).await?;
    let scope = Scope::new(parse_account_id(&request.account)?, request.livemode);
    let id = ids::parse(ids::WEBHOOK_ENDPOINT, &request.id)
        .ok_or_else(|| ApiError::invalid_param("id", "id must be a we_ id"))?;
    let deleted = crate::webhook_endpoints::delete(&state.pool, scope, id, &actor)
        .await
        .map_err(super::webhook_endpoints::map_error)?;
    record(
        &state,
        &restore,
        Some(scope.account_id()),
        &actor,
        "restore.webhook_endpoint_delete",
        &format!("webhook_endpoint:{}", request.id),
        &request.reason,
    )
    .await?;
    Ok(Json(DeletedWebhookEndpoint {
        id: deleted.public_id(),
        object: "webhook_endpoint".to_owned(),
        deleted: true,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/deposit_addresses",
    request_body = RestoreDepositAddressRequest,
    responses(
        (status = 200, description = "OK: re-issued, or the customer's existing version", body = RestoreDepositAddressResponse),
        (status = 400, description = "Bad Request: the address is not the customer's over a treasury of the account in force since the restore point (restore a lost treasury change first), the `client_secret` is not one the service issued for the `id` to the account, or `treasury_not_set`; or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Re-issues a deposit address the merchant gave a customer after the restore point, from the
/// merchant's record of its address or version: the salt is derived from the account, mode,
/// customer, and version, so it is the same address over the treasury its network paid (one in
/// force since the restore point), backfilled from the restored cursor so the rescan credits
/// payments made to it since. Audited.
pub(crate) async fn reissue_deposit_address(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreDepositAddressRequest>,
) -> ApiResult<Json<RestoreDepositAddressResponse>> {
    validate_reason(&request.reason)?;
    validate_client_reference_id(&request.client_reference_id)?;
    let restore = frozen(&state).await?;
    let account_id = parse_account_id(&request.account)?;
    let account = crate::db::get_account(&state.pool, account_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let address = request
        .address
        .as_deref()
        .map(|value| {
            EvmAddress::from_str(value)
                .map_err(|_| ApiError::invalid_param("address", "address must be an address"))
        })
        .transpose()?;
    let target = ReissueTarget {
        version: request.version,
        address,
    };
    let id = request
        .id
        .as_deref()
        .map(|id| {
            ids::parse(ids::DEPOSIT_ADDRESS, id)
                .ok_or_else(|| ApiError::invalid_param("id", "id must be a da_ id"))
        })
        .transpose()?;
    let client_secret_hash = request
        .client_secret
        .as_deref()
        .map(|secret| {
            request
                .id
                .as_deref()
                .and_then(|id| client_secret_hash(&state, &account, id, secret))
                .ok_or_else(|| {
                    ApiError::invalid_param(
                        "client_secret",
                        "client_secret is not one the service issued for the address's id to \
                         this account",
                    )
                })
        })
        .transpose()?;
    // Reconstruction still requires healthy chain evidence and honors the chain freeze.
    let chains: Vec<ChainContracts> = state
        .routes
        .current_in(request.livemode)
        .filter(|route| state.routes.chain_ready(route.chain.chain_id))
        .map(|route| (route.chain.chain_id, ChainContracts::of(route)))
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_values()
        .collect();
    let (reissued, issued) = deposit_addresses::reissue(
        &state.pool,
        &account,
        request.livemode,
        &request.client_reference_id,
        &chains,
        target,
        id,
        client_secret_hash.as_ref(),
        restore.restore_point,
        &restore.restored_cursors,
        &actor,
        &request.reason,
    )
    .await
    .map_err(super::deposit_addresses::map_error)?;
    Ok(Json(RestoreDepositAddressResponse {
        reissued: issued,
        // Every network the address has: re-issuing never depends on the payment settings, which
        // are held until the merchant reconfirms them.
        deposit_address: super::deposit_addresses::deposit_address_object(
            state.routes.current_in(request.livemode),
            &reissued,
        )?,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/quotes",
    request_body = RestoreQuoteRequest,
    responses(
        (status = 200, description = "OK: re-issued, or the quote exists already for the customer at the address", body = RestoreQuoteResponse),
        (status = 400, description = "Bad Request: the address is not the quote's over the account's treasury of the chain in force at its `created` (restore a lost treasury change first), the `client_secret` is not one the service issued for the quote to the account, no route has the chain and asset, the quote exists with another customer or address, or `treasury_not_set`; or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Re-issues a quote the merchant created after the restore point, from its record of the quote
/// object: the address salt is derived from the account, customer, and `qt_` id, so only the
/// quote's own address over the treasury in force at its `created` is accepted, backfilled from
/// the restored cursor so the rescan finds a
/// payment made to it since. A `client_secret` the service issued for the quote to the account is
/// kept, so the payer's page reads it again. The terms are stored as recorded but never applied:
/// a payment to the quote is credited at spot unless an imported `deposit.credited` for it, which
/// the service signed, carries its credit, and the payment window closes at the restore's
/// detection at the latest. Audited.
pub(crate) async fn reissue_quote(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreQuoteRequest>,
) -> ApiResult<Json<RestoreQuoteResponse>> {
    validate_reason(&request.reason)?;
    validate_client_reference_id(&request.client_reference_id)?;
    let restore = frozen(&state).await?;
    let account_id = parse_account_id(&request.account)?;
    let account = crate::db::get_account(&state.pool, account_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let id = ids::parse(ids::QUOTE, &request.id)
        .ok_or_else(|| ApiError::invalid_param("id", "id must be a qt_ id"))?;
    let address = EvmAddress::from_str(&request.address)
        .map_err(|_| ApiError::invalid_param("address", "address must be an address"))?;
    let amount_atomic = decimal_u256(&request.amount_atomic).ok_or_else(|| {
        ApiError::invalid_param("amount_atomic", "amount_atomic must be a decimal integer")
    })?;
    let price = super::quotes::parse_decimal(&request.exchange_rate)
        .and_then(|price| ScaledPrice::new(price, PRICE_SCALE).ok())
        .ok_or_else(|| {
            ApiError::invalid_param("exchange_rate", "exchange_rate must have 8 decimal places")
        })?;
    let timestamp = |param: &'static str, seconds: i64| {
        DateTime::from_timestamp(seconds, 0)
            .ok_or_else(|| ApiError::invalid_param(param, format!("{param} must be Unix seconds")))
    };
    let client_secret_hash = request
        .client_secret
        .as_deref()
        .map(|secret| {
            client_secret_hash(&state, &account, &request.id, secret).ok_or_else(|| {
                ApiError::invalid_param(
                    "client_secret",
                    "client_secret is not one the service issued for this quote to this account",
                )
            })
        })
        .transpose()?;
    let terms = ReissuedTerms {
        id,
        client_reference_id: request.client_reference_id.clone(),
        address,
        amount_atomic: AtomicAmount::new(amount_atomic),
        price,
        credit_minor: MinorAmount::new(request.amount),
        created_at: timestamp("created", request.created)?,
        expires_at: timestamp("expires_at", request.expires_at)?,
        metadata: super::metadata::on_create(request.metadata.as_ref())?,
        client_secret_hash,
    };
    // A route of the mode with the quote's chain and asset, paused or not and current or not:
    // nothing new is given out, the quote was issued already, and only its contracts are used.
    let route = state
        .routes
        .current_in(request.livemode)
        .chain(
            state
                .routes
                .routes()
                .iter()
                .filter(|route| route.livemode == request.livemode),
        )
        .find(|route| {
            route.chain.chain_id == request.chain_id
                && route.asset.symbol.eq_ignore_ascii_case(&request.asset)
        })
        .ok_or_else(|| ApiError::invalid_param("asset", "no route has the chain and asset"))?;
    if !state.routes.chain_ready(route.chain.chain_id) {
        return Err(ApiError::price_unavailable());
    }
    let (lock, issued) = locks::reissue(
        &state.pool,
        &account,
        request.livemode,
        route,
        &terms,
        &restore,
        &actor,
        &request.reason,
    )
    .await
    .map_err(super::quotes::map_error)?;
    let mut connection = state.pool.acquire().await?;
    Ok(Json(RestoreQuoteResponse {
        reissued: issued,
        quote: super::quotes::quote_object(&mut connection, &state.routes, lock).await?,
    }))
}

/// The SHA-256 of `secret` when it is a client secret the service issued for the object whose
/// public id is `id` to `account` (`crate::client_secret`): its tag proves the service issued
/// that id, its owner tag that it issued it to the account, so no other account's record of the
/// id, nor a secret taken from the payer's page, re-issues it elsewhere.
fn client_secret_hash(
    state: &AppState,
    account: &crate::db::Account,
    id: &str,
    secret: &str,
) -> Option<[u8; 32]> {
    state
        .client_reads
        .key()
        .verify_owner(&account.public_id, id, secret)
        .then(|| Sha256::digest(secret.as_bytes()).into())
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/events",
    request_body = RestoreEventsImportRequest,
    responses(
        (status = 200, description = "OK", body = RestoreEventsImportResponse),
        (status = 400, description = "Bad Request: a delivery's signature does not verify with its account's webhook keys, or its event is malformed, of another type, or its id is not the one its type and deposit derive; or `restore_not_frozen`", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no account has an event's `account`", body = ErrorResponse),
        (status = 503, description = "Service Unavailable: the webhook keys cannot be derived", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Imports the deposit events a merchant received after the restore point, from its webhook
/// receiver's record of each delivery. Only a delivery the service signed is imported: its
/// `webhook-signature` must verify over its id, timestamp, and raw body with one of its account's
/// webhook keys in its mode. Each event is stored as delivered, with no delivery, so when the rescan
/// re-derives the deposit its event is recorded already and nothing is sent again with another
/// body; the credit a `deposit.credited` or `deposit.reversed` carries is kept, so the deposit is
/// valued at it, not re-valued. An event recorded already keeps its stored snapshot; a different
/// delivered body is reported as `mismatch`. Audited.
pub(crate) async fn import_events(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreEventsImportRequest>,
) -> ApiResult<Json<RestoreEventsImportResponse>> {
    validate_reason(&request.reason)?;
    if request.deliveries.is_empty() || request.deliveries.len() > MAX_ITEMS {
        return Err(ApiError::invalid_param(
            "deliveries",
            "deliveries must hold 1 to 100 deliveries",
        ));
    }
    let restore = frozen(&state).await?;
    let events = request
        .deliveries
        .iter()
        .map(|delivery| {
            let body: Value = serde_json::from_str(&delivery.body).map_err(|_| {
                ApiError::invalid_param("deliveries", "a delivery's body is not an event object")
            })?;
            let event = delivered_event(&body)?;
            if delivery.webhook_id != ids::format(ids::EVENT, event.id) {
                return Err(ApiError::invalid_param(
                    "deliveries",
                    "a delivery's webhook_id is not its event's id",
                ));
            }
            Ok((delivery, event))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let mut keys = BTreeMap::new();
    for (delivery, event) in &events {
        let scope = (event.account_id, event.livemode);
        if let Entry::Vacant(entry) = keys.entry(scope) {
            entry.insert(webhook_public_keys(&state, Scope::new(scope.0, scope.1)).await?);
        }
        let verified = keys.get(&scope).is_some_and(|keys| {
            SignedWebhook::verifies(
                keys,
                &delivery.webhook_id,
                &delivery.webhook_timestamp,
                delivery.body.as_bytes(),
                &delivery.webhook_signature,
            )
        });
        if !verified {
            return Err(ApiError::invalid_param(
                "deliveries",
                format!(
                    "the signature of {} does not verify with its account's webhook keys",
                    delivery.webhook_id
                ),
            ));
        }
    }
    // A reversed deposit is restored after the one it replaced, so it links to it at once.
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_by_key(|&index| events[index].1.identity.revision);
    let mut data = vec![None; events.len()];
    for index in order {
        let event = &events[index].1;
        let (outcome, reversed) = restore_mode::import_delivered_event(
            &state.pool,
            &state.routes,
            &restore,
            event,
            &actor,
        )
        .await?;
        data[index] = Some(EventImport {
            id: ids::format(ids::EVENT, event.id),
            result: outcome.code().to_owned(),
            reversed_deposit: reversed.map(|reversed| reversed.code().to_owned()),
        });
    }
    let data = data.into_iter().flatten().collect();
    Ok(Json(RestoreEventsImportResponse {
        object: "list".to_owned(),
        data,
    }))
}

/// How many rolls after the restore point, lost with it, [`webhook_public_keys`] allows for.
const LOST_KEY_ROLLS: u32 = 4;

/// The public webhook keys that may have signed a delivery of `scope`: every version up to the
/// restored current one, and the next [`LOST_KEY_ROLLS`], since rolls after the restore point are
/// lost with it. An account that does not exist is `404`.
async fn webhook_public_keys(
    state: &AppState,
    scope: Scope,
) -> ApiResult<Vec<topup_core::Ed25519PublicKey>> {
    let mut connection = state.pool.acquire().await?;
    let keys = crate::webhook_keys::active(&mut connection, scope)
        .await?
        .ok_or_else(|| ApiError::not_found().with_param("deliveries"))?;
    let current = keys
        .versions
        .first()
        .map(|key| key.version)
        .ok_or_else(ApiError::internal)?;
    let versions: Vec<u32> = (1..=current.saturating_add(LOST_KEY_ROLLS)).collect();
    let derived = state
        .attestor
        .webhook_keys(&keys.account, scope.livemode(), &versions)
        .await
        .map_err(|_| ApiError::service_unavailable("the webhook keys cannot be derived"))?;
    Ok(derived.into_iter().map(|key| key.public_key).collect())
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/delivered_credits/discard",
    request_body = RestoreDeliveredCreditDiscardRequest,
    responses(
        (status = 200, description = "OK: discarded, or discarded already", body = RestoreDeliveredCreditDiscardResponse),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found: no delivered credit was imported for the deposit", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Discards the delivered credit of a deposit whose recorded transfer contradicts it (a finding
/// `contradicted` of `GET /v1/admin/restore`): the deposit, held until now, is valued from the
/// chain as any other, and the operator settles the difference with the merchant. Works frozen or
/// not, since the confirm step can find a contradiction after the unfreeze. Audited.
pub(crate) async fn discard_delivered_credit(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreDeliveredCreditDiscardRequest>,
) -> ApiResult<Json<RestoreDeliveredCreditDiscardResponse>> {
    validate_reason(&request.reason)?;
    let deposit = ids::parse(ids::DEPOSIT, &request.deposit)
        .ok_or_else(|| ApiError::invalid_param("deposit", "deposit must be a dep_ id"))?;
    restore_mode::discard_credit(&state.pool, deposit, &actor, &request.reason)
        .await
        .map_err(|error| match error {
            restore_mode::DiscardError::NotFound => ApiError::not_found().with_param("deposit"),
            restore_mode::DiscardError::Database(error) => ApiError::from(error),
        })?;
    Ok(Json(RestoreDeliveredCreditDiscardResponse {
        deposit: ids::format(ids::DEPOSIT, deposit),
        discarded: true,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/admin/restore/unfreeze",
    request_body = RestoreUnfreezeRequest,
    responses(
        (status = 200, description = "OK: unfrozen", body = RestoreObject),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 400, description = "Bad Request: a checklist item is not `true`, `restore_not_frozen`, or `restore_rescan_incomplete`", body = ErrorResponse)
    ),
    security(("http_message_signature" = [])),
    tag = "admin"
)]
/// Lifts the freeze after a restore once every chain is rescanned and the operator confirms the
/// checklist: crediting, settlement, quote expiry, treasury changes, refund verification, and
/// event delivery resume, and merchants' API keys authenticate again. The reason and checklist are
/// recorded in the restore and in `audit`.
pub(crate) async fn unfreeze(
    State(state): State<AppState>,
    AdminActor(actor): AdminActor,
    ApiJson(request): ApiJson<RestoreUnfreezeRequest>,
) -> ApiResult<Json<RestoreObject>> {
    validate_reason(&request.reason)?;
    for (param, done) in [
        (
            "security_changes_reapplied",
            request.security_changes_reapplied,
        ),
        (
            "deposit_addresses_reissued",
            request.deposit_addresses_reissued,
        ),
        ("quotes_reissued", request.quotes_reissued),
        (
            "delivered_events_imported",
            request.delivered_events_imported,
        ),
    ] {
        if !done {
            return Err(ApiError::invalid_param(
                param,
                format!("{param} must be true: finish that step first (deploy/RESTORE.md)"),
            ));
        }
    }
    let reason = format!(
        "{}; checklist: security_changes_reapplied, deposit_addresses_reissued, \
         quotes_reissued, delivered_events_imported",
        request.reason.trim()
    );
    let restore = restore_mode::unfreeze(&state.pool, &state.routes, &actor, &reason)
        .await
        .map_err(|error| match error {
            restore_mode::UnfreezeError::NotFrozen => ApiError::restore_not_frozen(),
            restore_mode::UnfreezeError::RescanIncomplete(_) => {
                ApiError::restore_rescan_incomplete()
            }
            restore_mode::UnfreezeError::Database(error) => ApiError::from(error),
        })?;
    Ok(Json(restore_object(&restore)))
}

/// The active freeze; every restore write needs one.
async fn frozen(state: &AppState) -> ApiResult<Restore> {
    restore_mode::active(&state.pool)
        .await?
        .ok_or_else(ApiError::restore_not_frozen)
}

/// Appends the audit row of a restore action.
async fn record(
    state: &AppState,
    restore: &Restore,
    account_id: Option<Uuid>,
    actor: &Actor,
    action: &str,
    subject: &str,
    reason: &str,
) -> ApiResult<()> {
    audit::insert(
        &state.pool,
        &audit::Entry {
            account_id,
            actor,
            action,
            subject,
            reason: &format!("restore {}: {}", restore.id, reason.trim()),
        },
    )
    .await?;
    Ok(())
}

/// The mode a key prefix names.
fn key_mode(prefix: &str) -> ApiResult<bool> {
    [
        (api_keys::KeyKind::Secret, false),
        (api_keys::KeyKind::Secret, true),
        (api_keys::KeyKind::Restricted, false),
        (api_keys::KeyKind::Restricted, true),
    ]
    .into_iter()
    .find(|(kind, livemode)| api_keys::prefix(*kind, *livemode) == prefix)
    .map(|(_, livemode)| livemode)
    .ok_or_else(|| {
        ApiError::invalid_param(
            "prefix",
            "prefix must be ppay_sk_test_, ppay_sk_live_, ppay_rk_test_, or ppay_rk_live_",
        )
    })
}

/// Reads and checks one delivered event: a re-derived deposit event whose id is the one its type
/// and deposit derive.
fn delivered_event(event: &Value) -> ApiResult<DeliveredEvent> {
    let invalid = |message: &str| ApiError::invalid_param("events", message.to_owned());
    let text = |field: &str| {
        event
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(&format!("each event needs a string `{field}`")))
    };
    let id =
        ids::parse(ids::EVENT, text("id")?).ok_or_else(|| invalid("an event id is not evt_"))?;
    let event_type = text("type")?;
    if !restore_mode::REDERIVED_EVENT_TYPES.contains(&event_type) {
        return Err(invalid(
            "only deposit.credited, deposit.rejected, and deposit.reversed events are imported",
        ));
    }
    let account_id = ids::parse(ids::ACCOUNT, text("account")?)
        .ok_or_else(|| invalid("an event's account is not acct_"))?;
    let livemode = event
        .get("livemode")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("each event needs a boolean `livemode`"))?;
    let created = event
        .get("created")
        .and_then(Value::as_i64)
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or_else(|| invalid("each event needs `created`, Unix seconds"))?;
    let data = event
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| invalid("each event needs its `data`"))?;
    let object = data
        .get("object")
        .ok_or_else(|| invalid("an event's data has no object"))?;
    let deposit_id = object
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| ids::parse(ids::DEPOSIT, id))
        .ok_or_else(|| invalid("an event's object is not a dep_ deposit"))?;
    if object.get("livemode").and_then(Value::as_bool) != Some(livemode) {
        return Err(invalid("an event's object is in another mode"));
    }
    if topup_core::identity::event_id(event_type, deposit_id) != id {
        return Err(invalid(
            "an event's id is not the one its type and deposit derive",
        ));
    }
    let actor = text("actor")?;
    // A credited deposit was valued; a reversed one was credited, and valued, or rejected, maybe
    // unvalued (an unsupported token).
    let valued = ["amount", "exchange_rate", "price_source", "valued_at"]
        .iter()
        .any(|field| object.get(*field).is_some_and(|value| !value.is_null()));
    let credit = match event_type {
        "deposit.credited" => Some(delivered_credit(object).ok_or_else(|| {
            invalid("a credited event's deposit needs its transfer and valuation")
        })?),
        "deposit.reversed" if valued => Some(delivered_credit(object).ok_or_else(|| {
            invalid("a reversed event's valued deposit needs its transfer and valuation")
        })?),
        _ => None,
    };
    let identity = delivered_identity(object, deposit_id)?;
    Ok(DeliveredEvent {
        id,
        account_id,
        livemode,
        event_type: event_type.to_owned(),
        deposit_id,
        created,
        actor: actor.to_owned(),
        data: data.clone(),
        credit,
        identity,
    })
}

/// The identity and chain evidence of a deposit's snapshot, whose id must be the one its receipt
/// position and revision derive.
fn delivered_identity(object: &Value, deposit_id: Uuid) -> ApiResult<DeliveredIdentity> {
    let invalid = || {
        ApiError::invalid_param(
            "events",
            "an event's deposit needs its transfer, receipt position, revision, and block",
        )
    };
    let text = |field: &str| object.get(field).and_then(Value::as_str);
    let number = |field: &str| object.get(field).and_then(Value::as_u64);
    let address = |field: &str| text(field).and_then(|value| EvmAddress::from_str(value).ok());
    let seconds = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_i64)
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
    };
    let deposit = |field: &str| match object.get(field) {
        None | Some(Value::Null) => Some(None),
        Some(value) => value
            .as_str()
            .and_then(|id| ids::parse(ids::DEPOSIT, id))
            .map(Some),
    };
    let identity = (|| {
        Some(DeliveredIdentity {
            chain_id: number("chain_id")?,
            tx_hash: text("tx_hash").and_then(|value| B256::from_str(value).ok())?,
            receipt_log_index: number("receipt_log_index")?,
            revision: number("revision")?,
            log_index: number("log_index")?,
            block_number: number("block_number")?,
            block_hash: text("block_hash").and_then(|value| B256::from_str(value).ok())?,
            block_time: seconds("block_time")?,
            address: address("address")?,
            asset_contract: address("asset_contract")?,
            from_address: address("from_address")?,
            amount_atomic: AtomicAmount::new(text("amount_atomic").and_then(decimal_u256)?),
            replaces: deposit("replaces")?,
            replaced_by: deposit("replaced_by")?,
            created: seconds("created")?,
            metadata: object
                .get("metadata")
                .filter(|metadata| metadata.is_object())
                .cloned()?,
        })
    })()
    .ok_or_else(invalid)?;
    if deposit_revision_id(
        identity.chain_id,
        identity.tx_hash,
        identity.receipt_log_index,
        identity.revision,
    ) != deposit_id
    {
        return Err(ApiError::invalid_param(
            "events",
            "an event's deposit id is not the one its receipt position and revision derive",
        ));
    }
    Ok(identity)
}

/// The transfer and valuation of a credited deposit's snapshot, as the deposit object renders
/// them.
fn delivered_credit(object: &Value) -> Option<DeliveredCredit> {
    let text = |field: &str| object.get(field).and_then(Value::as_str);
    let address = |field: &str| text(field).and_then(|value| EvmAddress::from_str(value).ok());
    Some(DeliveredCredit {
        chain_id: object.get("chain_id").and_then(Value::as_u64)?,
        tx_hash: text("tx_hash").and_then(|value| B256::from_str(value).ok())?,
        address: address("address")?,
        asset_contract: address("asset_contract")?,
        from_address: address("from_address")?,
        amount_atomic: AtomicAmount::new(text("amount_atomic").and_then(decimal_u256)?),
        price: text("exchange_rate")
            .and_then(super::quotes::parse_decimal)
            .and_then(|price| ScaledPrice::new(price, PRICE_SCALE).ok())?,
        source: match text("price_source")? {
            "quote" => ValuationSource::Lock,
            "spot" => ValuationSource::Spot,
            _ => return None,
        },
        credit_minor: MinorAmount::new(object.get("amount").and_then(Value::as_u64)?),
        valuation_at: object
            .get("valued_at")
            .and_then(Value::as_i64)
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))?,
    })
}

/// A non-negative decimal integer, digits only.
fn decimal_u256(text: &str) -> Option<U256> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| U256::from_str_radix(text, 10).ok())
        .flatten()
}

fn restore_object(restore: &Restore) -> RestoreObject {
    RestoreObject {
        id: restore.id.to_string(),
        object: "restore".to_owned(),
        detected_at: restore.detected_at.timestamp(),
        detected_by: restore.detected_by.clone(),
        timeline_id: restore.timeline_id,
        restore_point: restore.restore_point.map(|at| at.timestamp()),
        restored_cursors: restore
            .restored_cursors
            .iter()
            .map(|(chain_id, block)| (chain_id.to_string(), *block))
            .collect(),
        unfrozen_at: restore.unfrozen_at.map(|at| at.timestamp()),
        unfrozen_by: restore.unfrozen_by.clone(),
        unfreeze_reason: restore.unfreeze_reason.clone(),
    }
}

fn chain_rescan(chain: restore_mode::ChainRescan) -> models::ChainRescan {
    models::ChainRescan {
        chain_id: chain.chain_id,
        restored_block: chain.restored_block,
        scanned_block: chain.scanned_block,
        scanned_block_time: chain.scanned_block_time.map(|at| at.timestamp()),
        pending_backfills: chain.pending_backfills,
        blocked: chain.blocked,
        complete: chain.complete,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use topup_core::identity::event_id;

    use super::*;

    fn credited(deposit: Uuid) -> Value {
        json!({
            "id": ids::format(ids::EVENT, event_id("deposit.credited", deposit)),
            "object": "event",
            "account": ids::format(ids::ACCOUNT, Uuid::from_u128(1)),
            "livemode": false,
            "type": "deposit.credited",
            "created": 1_790_000_000,
            "actor": "system",
            "data": {"object": {
                "id": ids::format(ids::DEPOSIT, deposit),
                "livemode": false,
                "chain_id": 1,
                "tx_hash": format!("{:#x}", B256::repeat_byte(0x5a)),
                "address": format!("{:#x}", EvmAddress::repeat_byte(3)),
                "asset_contract": format!("{:#x}", EvmAddress::repeat_byte(4)),
                "from_address": format!("{:#x}", EvmAddress::repeat_byte(5)),
                "amount_atomic": "1000",
                "amount": 250,
                "exchange_rate": "0.25000000",
                "price_source": "quote",
                "valued_at": 1_790_000_000,
                "receipt_log_index": 0,
                "revision": 0,
                "log_index": 40,
                "block_number": 120,
                "block_hash": format!("{:#x}", B256::repeat_byte(0xb1)),
                "block_time": 1_789_999_990,
                "replaces": null,
                "replaced_by": null,
                "created": 1_789_999_995,
                "metadata": {},
            }},
        })
    }

    /// The deposit at the snapshot's receipt position, revision 0.
    fn deposit() -> Uuid {
        deposit_revision_id(1, B256::repeat_byte(0x5a), 0, 0)
    }

    #[test]
    fn a_delivered_event_is_read_with_its_derived_identity_and_credit() {
        let deposit = deposit();
        let event = delivered_event(&credited(deposit)).unwrap();
        assert_eq!(event.id, event_id("deposit.credited", deposit));
        assert_eq!(event.deposit_id, deposit);
        assert_eq!(event.account_id, Uuid::from_u128(1));
        assert!(!event.livemode);
        assert_eq!(event.data, credited(deposit)["data"]);
        let credit = event.credit.unwrap();
        assert_eq!(credit.tx_hash, B256::repeat_byte(0x5a));
        assert_eq!(credit.address, EvmAddress::repeat_byte(3));
        assert_eq!(credit.amount_atomic, AtomicAmount::new(U256::from(1000)));
        assert_eq!(credit.price.value(), 25_000_000);
        assert_eq!(credit.source, ValuationSource::Lock);
        assert_eq!(credit.credit_minor, MinorAmount::new(250));
        assert_eq!(credit.valuation_at.timestamp(), 1_790_000_000);

        // A rejected event carries no credit; a credited one without its valuation is refused.
        let mut rejected = credited(deposit);
        rejected["type"] = json!("deposit.rejected");
        rejected["id"] = json!(ids::format(
            ids::EVENT,
            event_id("deposit.rejected", deposit)
        ));
        assert!(delivered_event(&rejected).unwrap().credit.is_none());
        let mut unvalued = credited(deposit);
        unvalued["data"]["object"]["exchange_rate"] = Value::Null;
        assert!(delivered_event(&unvalued).is_err());
    }

    fn reversed(deposit: Uuid, valued: bool) -> Value {
        let mut event = credited(deposit);
        event["type"] = json!("deposit.reversed");
        event["id"] = json!(ids::format(
            ids::EVENT,
            event_id("deposit.reversed", deposit)
        ));
        if !valued {
            for field in ["amount", "exchange_rate", "price_source", "valued_at"] {
                event["data"]["object"][field] = Value::Null;
            }
        }
        event
    }

    #[test]
    fn a_reversed_event_carries_a_credit_only_when_its_deposit_was_valued() {
        let deposit = deposit();
        // A rejected deposit of an unsupported token was never valued: its reversal carries no
        // credit but is imported.
        let unvalued = delivered_event(&reversed(deposit, false)).unwrap();
        assert!(unvalued.credit.is_none());
        let valued = delivered_event(&reversed(deposit, true)).unwrap();
        assert_eq!(valued.credit.unwrap().credit_minor, MinorAmount::new(250));
        // A valuation that is there only in part is malformed.
        let mut partial = reversed(deposit, false);
        partial["data"]["object"]["amount"] = json!(250);
        assert!(delivered_event(&partial).is_err());
    }

    #[test]
    fn a_delivered_identity_must_derive_the_deposit_id() {
        let tx_hash = B256::repeat_byte(0x5a);
        let deposit = deposit_revision_id(1, tx_hash, 2, 1);
        let mut event = reversed(deposit, false);
        let object = &mut event["data"]["object"];
        object["receipt_log_index"] = json!(2);
        object["revision"] = json!(1);
        object["replaces"] = json!(ids::format(
            ids::DEPOSIT,
            deposit_revision_id(1, tx_hash, 2, 0)
        ));
        object["metadata"] = json!({"order": "o-1"});
        let identity = delivered_event(&event).unwrap().identity;
        assert_eq!((identity.receipt_log_index, identity.revision), (2, 1));
        assert_eq!(
            identity.replaces,
            Some(deposit_revision_id(1, tx_hash, 2, 0))
        );
        assert_eq!(identity.block_time.timestamp(), 1_789_999_990);

        // Another revision, or a snapshot without its position or block, is refused.
        let mut other_revision = event.clone();
        other_revision["data"]["object"]["revision"] = json!(2);
        let mut refused = vec![other_revision];
        for field in ["receipt_log_index", "revision", "block_hash", "block_time"] {
            let mut missing = event.clone();
            missing["data"]["object"][field] = Value::Null;
            refused.push(missing);
        }
        for refused in refused {
            assert!(delivered_event(&refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn events_that_a_rescan_does_not_derive_are_refused() {
        let deposit = deposit();
        let mut other_type = credited(deposit);
        other_type["type"] = json!("deposit.rejected");
        let mut other_deposit = credited(deposit);
        other_deposit["data"]["object"]["id"] =
            json!(ids::format(ids::DEPOSIT, Uuid::from_u128(8)));
        let mut refund = credited(deposit);
        refund["type"] = json!("deposit.refunded");
        let mut other_mode = credited(deposit);
        other_mode["data"]["object"]["livemode"] = json!(true);
        let mut no_data = credited(deposit);
        no_data["data"] = json!(null);
        for event in [other_type, other_deposit, refund, other_mode, no_data] {
            assert!(delivered_event(&event).is_err(), "{event}");
        }
    }

    #[test]
    fn a_key_prefix_names_its_mode() {
        assert!(!key_mode("ppay_sk_test_").unwrap());
        assert!(key_mode("ppay_rk_live_").unwrap());
        assert!(key_mode("ppay_sk_").is_err());
    }
}
