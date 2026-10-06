//! The account's payment settings (`GET` and `POST /v1/payment_settings`, design
//! docs/design/payment-settings.md): a singleton per account and mode, as Stripe's Tax Settings,
//! choosing from the operator's catalog within its bounds.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use alloy_primitives::U256;
use axum::Json;
use axum::extract::{Extension, State};
use axum::response::Response;
use sqlx::{Acquire, PgConnection, Postgres};
use topup_core::money::{AtomicAmount, Bps};
use topup_core::route::{Bounded, Confirmations, PricingMode, RouteFile};

use crate::audit::Actor;
use crate::payment_config::{
    self, AssetChoice, ChainChoice, Document, MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE,
    Resolution, Status,
};
use crate::routes::RouteSet;
use crate::tenancy::Scope;

use super::AppState;
use super::auth::Merchant;
use super::error::{ApiError, ErrorResponse};
use super::extract::ApiJson;
use super::idempotency::Idempotent;
use super::models::{
    AvailableAsset, AvailableChain, AvailableConfirmations, BoundsAtomic, BoundsU64,
    PaymentSettingsAsset, PaymentSettingsChain, PaymentSettingsObject,
    UpdatePaymentSettingsRequest,
};

/// Public id prefix of a payment settings revision.
const REVISION: &str = "psrev_";

#[utoipa::path(
    get,
    path = "/v1/payment_settings",
    responses(
        (status = 200, description = "OK", body = PaymentSettingsObject),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "payment_settings"
)]
/// Your payment settings in the key's mode: the chains and assets you accept and your terms on
/// each, with the operator's catalog of the mode in `available`. A new account accepts nothing
/// until you configure it.
pub(crate) async fn get_payment_settings(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
) -> Result<Json<PaymentSettingsObject>, ApiError> {
    payment_settings_object(
        &mut *state.pool.acquire().await?,
        &state.routes,
        merchant.scope,
    )
    .await
    .map(Json)
}

#[utoipa::path(
    post,
    path = "/v1/payment_settings",
    params(
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = UpdatePaymentSettingsRequest,
    responses(
        (status = 200, description = "OK", body = PaymentSettingsObject),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "payment_settings"
)]
/// Updates your payment settings in the key's mode. A parameter not sent is unchanged; `chains`,
/// when sent, replaces the whole list, and a term an element does not send takes the operator's
/// default. Writes are last-write-wins. The settings govern quotes and deposit addresses issued
/// from now on, and every payment recorded after the change; a quote keeps the terms it was
/// issued with. After a restore of the service the settings are `held` until a `POST` with your
/// complete configuration, even unchanged, reconfirms them: `chains` is then required, and a
/// parameter not sent takes its default. Announced as `payment_settings.updated`.
pub(crate) async fn update_payment_settings(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiJson(request): ApiJson<UpdatePaymentSettingsRequest>,
) -> Result<Response, ApiError> {
    let chains = request
        .chains
        .as_deref()
        .map(|chains| validate_chains(&state.routes, merchant.scope, chains))
        .transpose()?;
    let rate = request
        .quote_creations_per_customer_per_minute
        .map(|rate| {
            rate.map(|rate| {
                if (1..=MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE).contains(&rate) {
                    Ok(rate)
                } else {
                    Err(ApiError::invalid_param(
                        "quote_creations_per_customer_per_minute",
                        format!(
                            "must be between 1 and {MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE}"
                        ),
                    ))
                }
            })
            .transpose()
        })
        .transpose()?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    update(
        &mut *transaction,
        &state.routes,
        merchant.scope,
        rate,
        chains,
        &merchant.actor(),
    )
    .await?;
    let object = payment_settings_object(&mut transaction, &state.routes, merchant.scope).await?;
    idempotent.commit(transaction, Json(object)).await
}

/// Applies a validated change: its revision, audit row, and `payment_settings.updated`, when it
/// changes anything or reconfirms held settings.
async fn update<'c>(
    db: impl Acquire<'c, Database = Postgres>,
    routes: &RouteSet,
    scope: Scope,
    rate: Option<Option<u64>>,
    chains: Option<Vec<ChainChoice>>,
    actor: &Actor,
) -> Result<(), ApiError> {
    let mut transaction = db.begin().await?;
    // The writer's side of the barrier, before anything is read: a concurrent write applies
    // after this one commits, on what it wrote.
    let current = payment_config::load_for_update(&mut transaction, scope).await?;
    let object = crate::db::EventObject::PaymentSettings(scope.account_id());
    let before = crate::db::render(&mut transaction, routes, scope, object).await?;
    let document = if current.status == Status::Held {
        // Held after a restore: the merchant's complete configuration lifts the hold, and
        // nothing of the restored settings, which may be stale, is carried into it (design §11).
        let Some(chains) = chains else {
            return Err(ApiError::missing_param(
                "your payment settings are held after a restore of the service: send your \
                 complete configuration, `chains` included, to reconfirm them",
            )
            .with_param("chains"));
        };
        Document {
            quote_creations_per_customer_per_minute: rate.flatten(),
            chains,
        }
    } else {
        Document {
            quote_creations_per_customer_per_minute: rate
                .unwrap_or(current.document.quote_creations_per_customer_per_minute),
            chains: chains.unwrap_or(current.document.chains),
        }
    };
    let written =
        payment_config::write(&mut transaction, scope, &document, &actor.to_string()).await?;
    if written.changed {
        let public_id: String = sqlx::query_scalar("SELECT public_id FROM accounts WHERE id = $1")
            .bind(scope.account_id())
            .fetch_one(&mut *transaction)
            .await?;
        crate::audit::insert(
            &mut *transaction,
            &crate::audit::Entry {
                account_id: Some(scope.account_id()),
                actor,
                action: "payment_settings.update",
                subject: &format!("account:{public_id}"),
                reason: &serde_json::json!({
                    "livemode": scope.livemode(),
                    "reconfirmed": current.status == Status::Held,
                    "bound_pending_deposits": written.bound,
                })
                .to_string(),
            },
        )
        .await?;
        let event =
            crate::db::NewOutboxEvent::new("payment_settings.updated", scope, object, actor);
        crate::db::enqueue_in(&mut transaction, routes, &event, Some(&before)).await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Checks `chains` against the catalog of the key's mode and returns them as stored.
fn validate_chains(
    routes: &RouteSet,
    scope: Scope,
    chains: &[PaymentSettingsChain],
) -> Result<Vec<ChainChoice>, ApiError> {
    let catalog: Vec<&RouteFile> = routes.current_in(scope.livemode()).collect();
    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(chains.len());
    for (index, chain) in chains.iter().enumerate() {
        let param = |field: &str| format!("chains[{index}][{field}]");
        let on_chain: Vec<&RouteFile> = catalog
            .iter()
            .copied()
            .filter(|route| route.chain.chain_id == chain.chain_id)
            .collect();
        let Some(first) = on_chain.first() else {
            return Err(ApiError::invalid_param(
                param("chain_id"),
                "not a chain of the key's mode",
            ));
        };
        if !seen.insert(chain.chain_id) {
            return Err(ApiError::invalid_param(
                param("chain_id"),
                "each chain may be listed once",
            ));
        }
        let floor = first.chain.confirmations;
        let confirmations = chain
            .confirmations
            .as_deref()
            .map(|value| confirmations(chain.chain_id, floor, value))
            .transpose()
            .map_err(|message| ApiError::invalid_param(param("confirmations"), message))?;
        if chain.assets.is_empty() {
            return Err(ApiError::invalid_param(
                param("assets"),
                "list at least one asset; leave the chain out to accept none of it",
            ));
        }
        let mut assets = Vec::with_capacity(chain.assets.len());
        let mut listed = BTreeSet::new();
        for (asset_index, asset) in chain.assets.iter().enumerate() {
            let param = |field: &str| format!("chains[{index}][assets][{asset_index}][{field}]");
            let Some(route) = on_chain
                .iter()
                .find(|route| route.asset.symbol == asset.asset)
            else {
                let routed = on_chain
                    .iter()
                    .map(|route| route.asset.symbol.as_str())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ApiError::invalid_param(
                    param("asset"),
                    format!("not routed on chain {}; routed: {routed}", chain.chain_id),
                ));
            };
            if !listed.insert(asset.asset.clone()) {
                return Err(ApiError::invalid_param(
                    param("asset"),
                    "each asset may be listed once per chain",
                ));
            }
            let choice = asset_choice(route, asset, &param)?;
            let single = Document {
                quote_creations_per_customer_per_minute: None,
                chains: vec![ChainChoice {
                    chain_id: chain.chain_id,
                    confirmations,
                    assets: vec![choice.clone()],
                }],
            };
            if let Resolution::Disabled(reason) = payment_config::resolve(route, &single) {
                return Err(ApiError::invalid_param(
                    format!("chains[{index}][assets][{asset_index}]"),
                    format!("the terms cannot be used together: {reason}"),
                ));
            }
            assets.push(choice);
        }
        validated.push(ChainChoice {
            chain_id: chain.chain_id,
            confirmations,
            assets,
        });
    }
    Ok(validated)
}

/// A requested confirmation: of the chain's kind, and never weaker than its floor.
fn confirmations(
    chain_id: u64,
    floor: Confirmations,
    value: &str,
) -> Result<Confirmations, String> {
    let required = Confirmations::parse_policy(value)
        .ok_or("must be a depth of 1 to 999999, safe, or finalized")?;
    if required.validate_for_chain(chain_id).is_err() {
        return Err(
            "the chain accepts a depth or finalized (Ethereum), or a depth, safe, or finalized \
             (OP-stack)"
                .to_owned(),
        );
    }
    if floor.stricter(required) != required {
        return Err(format!(
            "the chain's floor is {}; a requirement may only be stricter: a deeper depth, then \
             safe (OP-stack), then finalized",
            floor.policy_value()
        ));
    }
    Ok(required)
}

/// A requested asset's terms, each within its bounds.
fn asset_choice(
    route: &RouteFile,
    asset: &PaymentSettingsAsset,
    param: &dyn Fn(&str) -> String,
) -> Result<AssetChoice, ApiError> {
    let bounds = &route.merchant;
    let within = |field: &str, value: u64, bounded: Bounded<u64>| {
        if bounded.contains(value) {
            Ok(value)
        } else {
            Err(ApiError::invalid_param(
                param(field),
                format!("must be between {} and {}", bounded.min, bounded.max),
            ))
        }
    };
    let bps = |field: &str, value: Option<u16>, bounded: Bounded<Bps>| {
        value
            .map(|value| {
                let bounded = Bounded {
                    default: u64::from(bounded.default.value()),
                    min: u64::from(bounded.min.value()),
                    max: u64::from(bounded.max.value()),
                };
                within(field, u64::from(value), bounded).map(|_| {
                    Bps::new(value).map_err(|_| ApiError::invalid_param(param(field), "invalid"))
                })?
            })
            .transpose()
    };
    let atomic = |field: &str, value: Option<&str>, bounded: Bounded<AtomicAmount>| {
        value
            .map(|value| {
                let amount = (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| U256::from_str(value).ok())
                    .flatten()
                    .map(AtomicAmount::new)
                    .ok_or_else(|| {
                        ApiError::invalid_param(
                            param(field),
                            "must be a decimal string of base units",
                        )
                    })?;
                if bounded.contains(amount) {
                    Ok(amount)
                } else {
                    Err(ApiError::invalid_param(
                        param(field),
                        format!(
                            "must be between {} and {}",
                            bounded.min.value(),
                            bounded.max.value()
                        ),
                    ))
                }
            })
            .transpose()
    };
    Ok(AssetChoice {
        asset: asset.asset.clone(),
        quote_ttl_seconds: asset
            .quote_ttl_seconds
            .map(|value| within("quote_ttl_seconds", value, bounds.quote_ttl_seconds))
            .transpose()?,
        quote_spread_bps: bps(
            "quote_spread_bps",
            asset.quote_spread_bps,
            bounds.quote_spread_bps,
        )?,
        quote_tolerance_bps: bps(
            "quote_tolerance_bps",
            asset.quote_tolerance_bps,
            bounds.quote_tolerance_bps,
        )?,
        min_amount: asset
            .min_amount
            .map(|value| within("min_amount", value, bounds.min_amount))
            .transpose()?,
        min_deposit_atomic: atomic(
            "min_deposit_atomic",
            asset.min_deposit_atomic.as_deref(),
            bounds.min_deposit_atomic,
        )?,
        max_deposit_atomic: atomic(
            "max_deposit_atomic",
            asset.max_deposit_atomic.as_deref(),
            bounds.max_deposit_atomic,
        )?,
        min_refund_atomic: atomic(
            "min_refund_atomic",
            asset.min_refund_atomic.as_deref(),
            bounds.min_refund_atomic,
        )?,
    })
}

/// The scope's payment settings as `GET /v1/payment_settings` returns them.
pub(crate) async fn payment_settings_object(
    connection: &mut PgConnection,
    routes: &RouteSet,
    scope: Scope,
) -> Result<PaymentSettingsObject, ApiError> {
    let settings = payment_config::load(connection, scope).await?;
    let treasuries = crate::treasuries::current(connection, scope)
        .await
        .map_err(|_| ApiError::internal())?;
    let document = &settings.document;
    let mut by_chain = BTreeMap::<u64, Vec<&RouteFile>>::new();
    for route in routes.current_in(scope.livemode()) {
        by_chain
            .entry(route.chain.chain_id)
            .or_default()
            .push(route);
    }
    let available = by_chain
        .into_iter()
        .map(|(chain_id, on_chain)| {
            let floor = on_chain
                .first()
                .map(|route| route.chain.confirmations.policy_value())
                .unwrap_or_default();
            let status = match (document.chain(chain_id), treasuries.contains_key(&chain_id)) {
                (None, _) => "not_configured",
                (Some(_), true) => "active",
                (Some(_), false) => "treasury_not_set",
            };
            AvailableChain {
                chain_id,
                status: status.to_owned(),
                confirmations: AvailableConfirmations {
                    default: floor.clone(),
                    floor,
                },
                assets: on_chain
                    .into_iter()
                    .map(|route| available_asset(route, document))
                    .collect(),
            }
        })
        .collect();
    Ok(PaymentSettingsObject {
        object: "payment_settings".to_owned(),
        livemode: scope.livemode(),
        status: settings.status.code().to_owned(),
        revision: crate::ids::format(REVISION, settings.revision),
        updated: settings.updated.timestamp(),
        quote_creations_per_customer_per_minute: document.quote_creations_per_customer_per_minute,
        chains: chains_object(document),
        available,
    })
}

fn chains_object(document: &Document) -> Vec<PaymentSettingsChain> {
    document
        .chains
        .iter()
        .map(|chain| PaymentSettingsChain {
            chain_id: chain.chain_id,
            confirmations: chain.confirmations.map(Confirmations::policy_value),
            assets: chain
                .assets
                .iter()
                .map(|asset| PaymentSettingsAsset {
                    asset: asset.asset.clone(),
                    quote_ttl_seconds: asset.quote_ttl_seconds,
                    quote_spread_bps: asset.quote_spread_bps.map(Bps::value),
                    quote_tolerance_bps: asset.quote_tolerance_bps.map(Bps::value),
                    min_amount: asset.min_amount,
                    min_deposit_atomic: asset.min_deposit_atomic.map(atomic_text),
                    max_deposit_atomic: asset.max_deposit_atomic.map(atomic_text),
                    min_refund_atomic: asset.min_refund_atomic.map(atomic_text),
                })
                .collect(),
        })
        .collect()
}

fn atomic_text(amount: AtomicAmount) -> String {
    amount.value().to_string()
}

fn available_asset(route: &RouteFile, document: &Document) -> AvailableAsset {
    let bounds = &route.merchant;
    let integer = |bounded: Bounded<u64>| BoundsU64 {
        default: bounded.default,
        min: bounded.min,
        max: bounded.max,
    };
    let bps = |bounded: Bounded<Bps>| BoundsU64 {
        default: u64::from(bounded.default.value()),
        min: u64::from(bounded.min.value()),
        max: u64::from(bounded.max.value()),
    };
    let atomic = |bounded: Bounded<AtomicAmount>| BoundsAtomic {
        default: atomic_text(bounded.default),
        min: atomic_text(bounded.min),
        max: atomic_text(bounded.max),
    };
    let resolution = payment_config::resolve(route, document);
    AvailableAsset {
        asset: route.asset.symbol.clone(),
        contract: format!("{:#x}", route.asset.contract),
        decimals: route.asset.decimals,
        pricing: match route.pricing.mode {
            PricingMode::Spot => "spot",
            PricingMode::Stablecoin => "stablecoin",
        }
        .to_owned(),
        quote_amount_decimals: route.asset.quote_amount_decimals,
        accepted: resolution != Resolution::NotAccepted,
        enabled: matches!(resolution, Resolution::Accepted(_)),
        quote_ttl_seconds: integer(bounds.quote_ttl_seconds),
        quote_spread_bps: bps(bounds.quote_spread_bps),
        quote_tolerance_bps: bps(bounds.quote_tolerance_bps),
        min_amount: integer(bounds.min_amount),
        min_deposit_atomic: atomic(bounds.min_deposit_atomic),
        max_deposit_atomic: atomic(bounds.max_deposit_atomic),
        min_refund_atomic: atomic(bounds.min_refund_atomic),
    }
}
