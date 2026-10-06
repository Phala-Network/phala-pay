//! Deposit addresses (`/v1/deposit_addresses`): a customer's persistent, rotatable address for
//! every supported token on every supported chain, credited at spot
//! (docs/design/multi-tenant.md "Deposit addresses").

use std::collections::BTreeMap;

use alloy_primitives::Address as EvmAddress;
use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use axum::response::{IntoResponse as _, Response};
use chrono::{DateTime, TimeDelta, Utc};
use sha2::{Digest as _, Sha256};
use sqlx::{Acquire, PgConnection};
use topup_core::route::RouteFile;
use uuid::Uuid;

use crate::client_secret::ClientSecretKey;
use crate::db::{Account, Customer};
use crate::deposit_addresses::{self, ChainContracts, DepositAddressError, ListFilter, Status};
use crate::ids;
use crate::payment_config;
use crate::routes::RouteSet;
use crate::tenancy::Scope;

use super::AppState;
use super::auth::Merchant;
use super::client_limit;
use super::error::{ApiError, ErrorResponse};
use super::extract::{ApiJson, ApiPath, query_pairs};
use super::handlers::validate_client_reference_id;
use super::idempotency::Idempotent;
use super::metadata::{self, MetadataUpdate, Object};
use super::models::{
    ClientDepositAddress, ClientDepositAddressNetwork, ClientDepositAddressPayment,
    CreateDepositAddressRequest, DepositAddress, DepositAddressAsset, DepositAddressList,
    DepositAddressNetwork, DepositAddressView, Payment, UpdateMetadataRequest,
};
use super::repository;

type ApiResult<T> = Result<T, ApiError>;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;

#[utoipa::path(
    post,
    path = "/v1/deposit_addresses",
    params(
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = CreateDepositAddressRequest,
    responses(
        (status = 200, description = "OK", body = DepositAddress),
        (
            status = 400,
            description = "Bad Request, `asset_not_accepted` (your payment settings accept \
                           nothing in this mode), `payment_settings_unconfirmed`, \
                           `deposit_address_cap_exceeded`, `paused`, `chain_frozen` (a new \
                           address and every chain is frozen or paused), or `treasury_not_set` \
                           (no treasury on any chain that takes one)",
            body = ErrorResponse
        ),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "deposit_addresses"
)]
/// Returns the customer's active deposit address, one address for every token your payment
/// settings accept on every network of the key's mode where you have a treasury (an account that
/// accepts nothing gets `asset_not_accepted`), issuing it if the customer has
/// none: the same request always returns the same address until it is rotated. It also adds the
/// address's network on a chain supported, or given a treasury, since it was issued, and replaces
/// a chain's network whose treasury changed.
pub(crate) async fn create_deposit_address(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiJson(request): ApiJson<CreateDepositAddressRequest>,
) -> ApiResult<Response> {
    validate_client_reference_id(&request.client_reference_id)?;
    let metadata = request
        .metadata
        .as_ref()
        .map(MetadataUpdate::parse)
        .transpose()?;
    let customer =
        repository::ensure_customer(&state.pool, merchant.scope, &request.client_reference_id)
            .await?;
    let issuable = issuable_chains(&state, &merchant.account, &customer).await?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    let (address, _) = deposit_addresses::create(
        &mut *transaction,
        &merchant.account,
        &customer,
        &issuable.chains,
        metadata.as_ref(),
    )
    .await
    .map_err(|error| issuable.map_error(error))?;
    let response = respond_with_client_secret(
        &mut transaction,
        &state.routes,
        state.client_reads.key(),
        &merchant.account,
        &address,
    )
    .await?;
    idempotent.commit(transaction, response).await
}

#[utoipa::path(
    get,
    path = "/v1/deposit_addresses",
    params(
        ("client_reference_id" = Option<String>, Query, description = "Only this customer's addresses"),
        ("status" = Option<String>, Query, description = "`active` or `retired`"),
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`da_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`da_` id: the page before it")
    ),
    responses(
        (status = 200, description = "OK", body = DepositAddressList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "deposit_addresses"
)]
/// The account's deposit addresses in the key's mode, newest first, with Stripe's cursor
/// pagination.
pub(crate) async fn list_deposit_addresses(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<DepositAddressList>> {
    let filter = parse_filter(&query_pairs(query.as_deref()))?;
    let (addresses, has_more) = deposit_addresses::list(&state.pool, merchant.scope, &filter)
        .await
        .map_err(|error| match error {
            DepositAddressError::NotFound => ApiError::invalid_param(
                if filter.before {
                    "ending_before"
                } else {
                    "starting_after"
                },
                "no such deposit address",
            ),
            error => map_error(error),
        })?;
    let mut connection = state.pool.acquire().await?;
    let mut data = Vec::with_capacity(addresses.len());
    for address in &addresses {
        data.push(
            deposit_address_response(&mut connection, &state.routes, merchant.scope, address)
                .await?,
        );
    }
    Ok(Json(DepositAddressList {
        object: "list".to_owned(),
        url: "/v1/deposit_addresses".to_owned(),
        has_more,
        data,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/deposit_addresses/{id}",
    params(
        ("id" = String, Path, description = "Deposit address id, `da_…`"),
        (
            "client_secret" = Option<String>, Query,
            description = "A `client_secret` of the address, to read its public view without an \
                           API key. Send the request without `Authorization`; the response then \
                           allows any origin."
        )
    ),
    responses(
        (
            status = 200,
            description = "OK: a `DepositAddress` to a request with an API key, a \
                           `ClientDepositAddress` to a request by `client_secret`",
            body = DepositAddressView
        ),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (
            status = 404,
            description = "Not Found, also for a `client_secret` that is not this address's",
            body = ErrorResponse
        ),
        (status = 429, description = "Too Many Requests: reads by `client_secret`", body = ErrorResponse)
    ),
    security(("api_key" = []), ()),
    tag = "deposit_addresses"
)]
/// One deposit address. The customer's page can read the address's public view, with the
/// payments seen and credited in the last 24 hours, by a `client_secret` instead of an API key,
/// as a quote's page does.
pub(crate) async fn get_deposit_address(
    State(state): State<AppState>,
    merchant: Option<Extension<Merchant>>,
    ApiPath(id): ApiPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let Some(Extension(merchant)) = merchant else {
        let pairs = query_pairs(query.as_deref());
        let client_secret = pairs
            .iter()
            .find(|(name, _)| name == "client_secret")
            .map(|(_, value)| value.as_str());
        let mut response = match client_deposit_address(&state, &id, client_secret).await {
            Ok(address) => Json(DepositAddressView::Client(Box::new(address))).into_response(),
            Err(error) => error.into_response(),
        };
        super::allow_cross_origin(&mut response);
        return response;
    };
    let address = async {
        let id = ids::parse(ids::DEPOSIT_ADDRESS, &id).ok_or_else(ApiError::not_found)?;
        let address = deposit_addresses::get(&state.pool, merchant.scope, id)
            .await
            .map_err(map_error)?
            .ok_or_else(ApiError::not_found)?;
        deposit_address_response(
            &mut *state.pool.acquire().await?,
            &state.routes,
            merchant.scope,
            &address,
        )
        .await
    };
    match address.await {
        Ok(address) => Json(DepositAddressView::DepositAddress(Box::new(address))).into_response(),
        Err(error) => error.into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/deposit_addresses/{id}",
    params(
        ("id" = String, Path, description = "Deposit address id, `da_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    request_body = UpdateMetadataRequest,
    responses(
        (status = 200, description = "OK", body = DepositAddress),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "deposit_addresses"
)]
/// Updates a deposit address's `metadata`, active or retired; parameters not sent are left
/// unchanged. Deposits already recorded keep their own copy.
pub(crate) async fn update_deposit_address(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<UpdateMetadataRequest>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::DEPOSIT_ADDRESS, &id).ok_or_else(ApiError::not_found)?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    if !metadata::update(
        &mut *transaction,
        &state.routes,
        Object::DepositAddress,
        merchant.scope,
        id,
        request.metadata.as_ref(),
        &merchant.actor(),
    )
    .await?
    {
        return Err(ApiError::not_found());
    }
    let address = deposit_addresses::get_in(&mut transaction, merchant.scope, id)
        .await
        .map_err(map_error)?
        .ok_or_else(ApiError::not_found)?;
    let address =
        deposit_address_response(&mut transaction, &state.routes, merchant.scope, &address).await?;
    idempotent.commit(transaction, Json(address)).await
}

#[utoipa::path(
    post,
    path = "/v1/deposit_addresses/{id}/rotate",
    params(
        ("id" = String, Path, description = "Deposit address id, `da_…`"),
        (
            "Idempotency-Key" = Option<String>, Header,
            description = "Up to 255 characters; for 24 hours a repeat of the same request \
                           returns the first response, and of another request is \
                           `400 idempotency_error`."
        )
    ),
    responses(
        (status = 200, description = "OK: the new active address", body = DepositAddress),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Not Found", body = ErrorResponse),
        (
            status = 400,
            description = "`deposit_address_retired`: already rotated; `paused`, \
                           `chain_frozen` (every chain is frozen or paused), \
                           `asset_not_accepted`, `payment_settings_unconfirmed`, or \
                           `treasury_not_set`",
            body = ErrorResponse
        ),
        (
            status = 429,
            description = "`rate_limit`, or `customer_rate_limit`: the customer's rotations per \
                           hour; retry after `Retry-After` seconds",
            body = ErrorResponse
        )
    ),
    security(("api_key" = [])),
    tag = "deposit_addresses"
)]
/// Retires an active deposit address and returns the customer's new one, a new address on every
/// network. Payments to the retired address are still credited at spot; stop showing it.
pub(crate) async fn rotate_deposit_address(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    idempotent: Idempotent,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Response> {
    let id = ids::parse(ids::DEPOSIT_ADDRESS, &id).ok_or_else(ApiError::not_found)?;
    let current = deposit_addresses::get(&state.pool, merchant.scope, id)
        .await
        .map_err(map_error)?
        .ok_or_else(ApiError::not_found)?;
    let customer =
        repository::find_customer(&state.pool, merchant.scope, &current.client_reference_id)
            .await?
            .ok_or_else(ApiError::internal)?;
    let issuable = issuable_chains(&state, &merchant.account, &customer).await?;
    let mut transaction = idempotent.begin(&state.pool).await?;
    let address = deposit_addresses::rotate(
        &mut *transaction,
        &merchant.account,
        merchant.scope,
        &merchant.actor(),
        id,
        &issuable.chains,
    )
    .await
    .map_err(|error| issuable.map_error(error))?;
    let response = respond_with_client_secret(
        &mut transaction,
        &state.routes,
        state.client_reads.key(),
        &merchant.account,
        &address,
    )
    .await?;
    idempotent.commit(transaction, response).await
}

/// The chains a customer's deposit address gets new networks on, and why a chain was left out.
struct Issuable {
    chains: Vec<ChainContracts>,
    frozen: bool,
}

impl Issuable {
    /// Maps a failure, naming why no chain accepted a new address when none did.
    fn map_error(&self, error: DepositAddressError) -> ApiError {
        match error {
            DepositAddressError::NoChain if self.frozen => ApiError::chain_frozen(),
            DepositAddressError::NoChain => ApiError::paused("new addresses are paused"),
            error => map_error(error),
        }
    }
}

/// New addresses and networks are issued only while `quotes` is not paused for the account or the
/// customer (design §12: no new addresses while paused), and only on the chains of the customer's
/// mode that are not frozen and have a current route the account's payment settings accept that
/// is not paused for `quotes` (docs/design/payment-settings.md §6). A network pays the account's
/// current treasury of its chain; a chain without one gets no network.
async fn issuable_chains(
    state: &AppState,
    account: &Account,
    customer: &Customer,
) -> ApiResult<Issuable> {
    let paused = |scopes: &[String]| scopes.iter().any(|scope| scope == "quotes");
    if paused(&account.paused_scopes) || paused(&customer.paused_scopes) {
        return Err(ApiError::paused("new addresses are paused"));
    }
    let mut connection = state.pool.acquire().await?;
    let scope = Scope::new(account.id, customer.livemode);
    let effective = payment_config::load_effective(&mut connection, &state.routes, scope).await?;
    drop(connection);
    if effective.settings.status == payment_config::Status::Held {
        return Err(ApiError::payment_settings_unconfirmed());
    }
    if effective.assets.is_empty() {
        return Err(ApiError::asset_not_accepted(None));
    }
    let mut by_chain = BTreeMap::<u64, Vec<&RouteFile>>::new();
    for asset in &effective.assets {
        by_chain
            .entry(asset.route.chain.chain_id)
            .or_default()
            .push(asset.route);
    }
    let mut issuable = Issuable {
        chains: Vec::new(),
        frozen: false,
    };
    for (chain_id, routes) in by_chain {
        if crate::reconciler::chain_is_blocked(&state.pool, chain_id).await? {
            issuable.frozen = true;
            continue;
        }
        let mut open = None;
        for route in routes {
            let scopes = repository::route_paused_scopes(&state.pool, &route.route).await?;
            if !paused(&scopes) {
                open = Some(route);
                break;
            }
        }
        if let Some(route) = open {
            issuable.chains.push(ChainContracts::of(route));
        }
    }
    Ok(issuable)
}

/// The API representation of a deposit address: its networks on the chains of `routes`, each with
/// those routes' tokens. The merchant's and the payer's views pass the routes the account's
/// effective config accepts.
pub(crate) fn deposit_address_object<'r>(
    routes: impl IntoIterator<Item = &'r RouteFile>,
    address: &deposit_addresses::DepositAddress,
) -> ApiResult<DepositAddress> {
    let mut assets = BTreeMap::<u64, Vec<&RouteFile>>::new();
    for route in routes {
        assets.entry(route.chain.chain_id).or_default().push(route);
    }
    let networks: Vec<DepositAddressNetwork> = address
        .networks
        .iter()
        .filter_map(|network| {
            let routes = assets.get(&network.chain_id)?;
            Some(DepositAddressNetwork {
                chain_id: network.chain_id,
                address: format!("{:#x}", network.address),
                treasury: format!("{:#x}", network.treasury),
                assets: routes
                    .iter()
                    .map(|route| DepositAddressAsset {
                        asset: route.asset.symbol.clone(),
                        contract: format!("{:#x}", route.asset.contract),
                        decimals: route.asset.decimals,
                        payment_uri: format!(
                            "ethereum:{:#x}@{}/transfer?address={:#x}",
                            route.asset.contract, network.chain_id, network.address
                        ),
                    })
                    .collect(),
            })
        })
        .collect();
    let shared = networks
        .first()
        .map(|first| &first.address)
        .filter(|first| networks.iter().all(|network| &network.address == *first));
    Ok(DepositAddress {
        id: deposit_addresses::public_id(address.id),
        object: "deposit_address".to_owned(),
        livemode: address.livemode,
        client_reference_id: address.client_reference_id.clone(),
        address: shared.cloned(),
        version: address.version,
        salt: format!("{:#x}", address.salt),
        status: address.status.code().to_owned(),
        created: address.created_at.timestamp(),
        retired_at: address.retired_at.map(|at| at.timestamp()),
        metadata: address.metadata.clone(),
        networks,
        payments: Vec::new(),
        client_secret: None,
    })
}

/// The merchant's view of a deposit address of `scope`, with its recent payments.
async fn deposit_address_response(
    connection: &mut PgConnection,
    routes: &RouteSet,
    scope: Scope,
    address: &deposit_addresses::DepositAddress,
) -> ApiResult<DepositAddress> {
    let effective = payment_config::load_effective(&mut *connection, routes, scope).await?;
    let mut object =
        deposit_address_object(effective.assets.iter().map(|asset| asset.route), address)?;
    object.payments = recent_payments(connection, routes, address.id, Utc::now())
        .await?
        .into_iter()
        .map(|recent| recent.payment)
        .collect();
    Ok(object)
}

/// Responds to a create or a rotation with a newly issued client secret.
async fn respond_with_client_secret(
    connection: &mut PgConnection,
    routes: &RouteSet,
    key: &ClientSecretKey,
    account: &Account,
    address: &deposit_addresses::DepositAddress,
) -> ApiResult<Json<DepositAddress>> {
    let scope = Scope::new(account.id, address.livemode);
    let mut object = deposit_address_response(connection, routes, scope, address).await?;
    object.client_secret = Some(issue_client_secret(connection, key, account, address.id).await?);
    Ok(Json(object))
}

/// Client secrets of one address that stay valid: the newest, for several open pages.
const CLIENT_SECRETS_KEPT: i64 = 10;
/// How far back the public view lists payments.
const CLIENT_PAYMENTS_WINDOW: TimeDelta = TimeDelta::hours(24);
/// How many payments the public view lists.
const CLIENT_PAYMENTS_SHOWN: usize = 10;

/// Issues a `client_secret` ([`crate::client_secret`]), storing only its SHA-256 and dropping the
/// address's secrets older than the newest [`CLIENT_SECRETS_KEPT`].
async fn issue_client_secret(
    connection: &mut PgConnection,
    key: &ClientSecretKey,
    account: &Account,
    id: Uuid,
) -> ApiResult<String> {
    let secret = key
        .issue(&account.public_id, &deposit_addresses::public_id(id))
        .map_err(|error| {
            tracing::error!(%error, "no client secret issued");
            ApiError::internal()
        })?;
    let mut transaction = connection.begin().await?;
    sqlx::query(
        "INSERT INTO deposit_address_client_secrets (secret_hash, deposit_address_id) \
         VALUES ($1, $2)",
    )
    .bind(Sha256::digest(secret.as_bytes()).as_slice())
    .bind(id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM deposit_address_client_secrets WHERE deposit_address_id = $1 \
         AND secret_hash NOT IN (SELECT secret_hash FROM deposit_address_client_secrets \
             WHERE deposit_address_id = $1 ORDER BY created_at DESC, secret_hash LIMIT $2)",
    )
    .bind(id)
    .bind(CLIENT_SECRETS_KEPT)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(secret)
}

/// The public view of the deposit address `id` whose client secret is `client_secret`.
async fn client_deposit_address(
    state: &AppState,
    id: &str,
    client_secret: Option<&str>,
) -> ApiResult<ClientDepositAddress> {
    let address_id = ids::parse(ids::DEPOSIT_ADDRESS, id).ok_or_else(ApiError::not_found)?;
    let client_secret = client_secret.ok_or_else(ApiError::not_found)?;
    let _slot = state
        .client_reads
        .admit(id, address_id, client_secret)
        .await?;
    let secret = Sha256::digest(client_secret.as_bytes());
    client_limit::bounded(client_deposit_address_view(state, address_id, &secret)).await
}

/// The public view of the deposit address `address_id` if `secret_hash` is the SHA-256 of one of
/// its valid client secrets.
async fn client_deposit_address_view(
    state: &AppState,
    address_id: Uuid,
    secret_hash: &[u8],
) -> ApiResult<ClientDepositAddress> {
    // The secret authenticates for its address's account and mode, as an API key does for its
    // own; the scope comes from the stored row, never from the request.
    let owner: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT address.account_id, address.livemode \
         FROM deposit_address_client_secrets AS secret \
         JOIN deposit_addresses AS address ON address.id = secret.deposit_address_id \
         WHERE secret.secret_hash = $1 AND address.id = $2",
    )
    .bind(secret_hash)
    .bind(address_id)
    .fetch_optional(&state.pool)
    .await?;
    let (account_id, livemode) = owner.ok_or_else(ApiError::not_found)?;
    let scope = Scope::new(account_id, livemode);
    let address = deposit_addresses::get(&state.pool, scope, address_id)
        .await
        .map_err(map_error)?
        .ok_or_else(ApiError::not_found)?;
    let effective =
        payment_config::load_effective(&mut *state.pool.acquire().await?, &state.routes, scope)
            .await?;
    let object =
        deposit_address_object(effective.assets.iter().map(|asset| asset.route), &address)?;
    let payments = client_payments(
        &mut *state.pool.acquire().await?,
        &state.routes,
        address_id,
        Utc::now(),
    )
    .await?;
    let networks = object
        .networks
        .into_iter()
        .map(|network| {
            // The chain's confirmation: the stricter of its floor and the account's requirement,
            // the same for every accepted asset of the chain.
            let confirmations = effective
                .assets
                .iter()
                .filter(|asset| asset.route.chain.chain_id == network.chain_id)
                .map(|asset| {
                    asset
                        .route
                        .chain
                        .confirmations
                        .stricter(asset.terms.confirmations)
                })
                .reduce(|left, right| left.stricter(right))
                .ok_or_else(ApiError::internal)?;
            Ok(ClientDepositAddressNetwork {
                chain_id: network.chain_id,
                address: network.address,
                assets: network.assets,
                typical_credit_seconds: confirmations.typical_credit_seconds(network.chain_id),
            })
        })
        .collect::<ApiResult<_>>()?;
    Ok(ClientDepositAddress {
        id: object.id,
        object: object.object,
        livemode: object.livemode,
        status: object.status,
        address: object.address,
        networks,
        payments,
    })
}

/// A recent deposit: id, chain, state, token, amount, transaction, and creation time.
type RecentDepositRow = (Uuid, i64, String, String, String, String, DateTime<Utc>);

/// A payment to a deposit address with the processing state of its deposit, once recorded.
struct RecentPayment {
    payment: Payment,
    decimals: Option<u8>,
    state: Option<String>,
    created: i64,
}

/// Transfers seen and not recorded yet, and deposits recorded within
/// [`CLIENT_PAYMENTS_WINDOW`], newest first, at most [`CLIENT_PAYMENTS_SHOWN`].
async fn recent_payments(
    connection: &mut PgConnection,
    routes: &RouteSet,
    deposit_address_id: Uuid,
    now: DateTime<Utc>,
) -> ApiResult<Vec<RecentPayment>> {
    let since = now
        .checked_sub_signed(CLIENT_PAYMENTS_WINDOW)
        .ok_or_else(ApiError::internal)?;
    let mut payments = Vec::new();
    for transfer in pending_transfers(connection, deposit_address_id).await? {
        let asset = route_asset(routes, transfer.chain_id, transfer.asset_contract);
        payments.push(RecentPayment {
            payment: Payment {
                status: "seen".to_owned(),
                chain_id: transfer.chain_id,
                asset: asset.as_ref().map(|(asset, _)| asset.clone()),
                tx_hash: format!("{:#x}", transfer.tx_hash),
                amount_atomic: transfer.amount_atomic.value().to_string(),
                confirmations: Some(transfer.confirmations()),
                estimated_final_at: Some(
                    super::pending::estimated_final_at(transfer.block_time).timestamp(),
                ),
                matches_quote: None,
                deposit: ids::format(ids::DEPOSIT, transfer.deposit_id),
            },
            decimals: asset.map(|(_, decimals)| decimals),
            state: None,
            created: transfer.first_seen_at.timestamp(),
        });
    }
    let deposits: Vec<RecentDepositRow> = sqlx::query_as(
        "SELECT deposit.id, deposit.chain_id, deposit.state, deposit.asset_contract, \
                    deposit.amount_atomic::text, deposit.tx_hash, deposit.created_at \
             FROM deposits AS deposit \
             JOIN addresses AS address ON address.id = deposit.address_id \
             WHERE address.deposit_address_id = $1 AND deposit.created_at >= $2 \
             ORDER BY deposit.created_at DESC, deposit.id DESC LIMIT $3",
    )
    .bind(deposit_address_id)
    .bind(since)
    .bind(i64::try_from(CLIENT_PAYMENTS_SHOWN).map_err(|_| ApiError::internal())?)
    .fetch_all(&mut *connection)
    .await?;
    for (id, chain_id, state, contract, amount, tx_hash, created) in deposits {
        let chain_id = u64::try_from(chain_id).map_err(|_| ApiError::internal())?;
        let contract: EvmAddress = contract.parse().map_err(|_| ApiError::internal())?;
        let asset = route_asset(routes, chain_id, contract);
        payments.push(RecentPayment {
            payment: Payment {
                status: "recorded".to_owned(),
                chain_id,
                asset: asset.as_ref().map(|(asset, _)| asset.clone()),
                tx_hash,
                amount_atomic: amount,
                confirmations: None,
                estimated_final_at: None,
                matches_quote: None,
                deposit: ids::format(ids::DEPOSIT, id),
            },
            decimals: asset.map(|(_, decimals)| decimals),
            state: Some(state),
            created: created.timestamp(),
        });
    }
    payments.sort_by_key(|payment| std::cmp::Reverse(payment.created));
    payments.truncate(CLIENT_PAYMENTS_SHOWN);
    Ok(payments)
}

/// The payments the customer's page shows, by their progress.
async fn client_payments(
    connection: &mut PgConnection,
    routes: &RouteSet,
    deposit_address_id: Uuid,
    now: DateTime<Utc>,
) -> ApiResult<Vec<ClientDepositAddressPayment>> {
    Ok(recent_payments(connection, routes, deposit_address_id, now)
        .await?
        .into_iter()
        .map(|recent| ClientDepositAddressPayment {
            status: match recent.state.as_deref() {
                None => "seen",
                Some("credited" | "swept") => "credited",
                Some("rejected") => "rejected",
                Some("reversed") => "reversed",
                Some(_) => "confirming",
            }
            .to_owned(),
            chain_id: recent.payment.chain_id,
            asset: recent.payment.asset,
            decimals: recent.decimals,
            amount_atomic: recent.payment.amount_atomic,
            tx_hash: recent.payment.tx_hash,
            confirmations: recent.payment.confirmations,
            created: recent.created,
        })
        .collect())
}

/// Transfers to any network of the deposit address, current or superseded, seen in a block and
/// not recorded as deposits yet, oldest first.
async fn pending_transfers(
    connection: &mut PgConnection,
    deposit_address_id: Uuid,
) -> ApiResult<Vec<crate::db::PendingTransfer>> {
    let address_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM addresses WHERE deposit_address_id = $1 ORDER BY chain_id, id",
    )
    .bind(deposit_address_id)
    .fetch_all(&mut *connection)
    .await?;
    let mut transfers = Vec::new();
    for address_id in address_ids {
        for transfer in crate::db::list_address_pending(&mut *connection, address_id).await? {
            let recorded: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM deposits WHERE id = $1)")
                    .bind(transfer.deposit_id)
                    .fetch_one(&mut *connection)
                    .await?;
            if !recorded {
                transfers.push(transfer);
            }
        }
    }
    transfers.sort_by_key(|transfer| (transfer.block_number, transfer.log_index));
    Ok(transfers)
}

/// The asset code and decimals of a routed token of `chain_id`.
fn route_asset(routes: &RouteSet, chain_id: u64, contract: EvmAddress) -> Option<(String, u8)> {
    routes
        .current()
        .find(|route| route.chain.chain_id == chain_id && route.asset.contract == contract)
        .map(|route| (route.asset.symbol.clone(), route.asset.decimals))
}

fn parse_filter(pairs: &[(String, String)]) -> ApiResult<ListFilter> {
    let mut filter = ListFilter {
        limit: DEFAULT_LIMIT,
        ..ListFilter::default()
    };
    let mut starting_after = None;
    let mut ending_before = None;
    for (name, value) in pairs {
        match name.as_str() {
            "client_reference_id" => filter.client_reference_id = Some(value.clone()),
            "status" => {
                filter.status = Some(match value.as_str() {
                    "active" => Status::Active,
                    "retired" => Status::Retired,
                    _ => return Err(ApiError::invalid_param("status", "unknown status")),
                });
            }
            "limit" => {
                filter.limit = value
                    .parse::<i64>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| ApiError::invalid_param("limit", "limit must be 1 to 100"))?;
            }
            "starting_after" | "ending_before" => {
                let id = ids::parse(ids::DEPOSIT_ADDRESS, value)
                    .ok_or_else(|| ApiError::invalid_param(name.clone(), "not a da_ id"))?;
                if name == "starting_after" {
                    starting_after = Some(id);
                } else {
                    ending_before = Some(id);
                }
            }
            other => {
                return Err(
                    ApiError::unknown_param(format!("unknown parameter {other}")).with_param(other),
                );
            }
        }
    }
    match (starting_after, ending_before) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "starting_after and ending_before are mutually exclusive",
            ));
        }
        (Some(id), None) => filter.cursor = Some(id),
        (None, Some(id)) => {
            filter.cursor = Some(id);
            filter.before = true;
        }
        (None, None) => {}
    }
    Ok(filter)
}

pub(super) fn map_error(error: DepositAddressError) -> ApiError {
    match error {
        DepositAddressError::NotFound => ApiError::not_found(),
        DepositAddressError::Retired => ApiError::deposit_address_retired(),
        error @ DepositAddressError::CapReached(_) => {
            ApiError::deposit_address_cap(error.to_string())
        }
        DepositAddressError::RateLimited { retry_after } => {
            ApiError::customer_rotation_limit(retry_after)
        }
        DepositAddressError::NoChain => ApiError::paused("new addresses are paused"),
        DepositAddressError::NoTreasury => ApiError::treasury_not_set(),
        DepositAddressError::InvalidInput(message) => ApiError::bad_request(message),
        DepositAddressError::Metadata(error) => error,
        DepositAddressError::DatabaseInvariant => ApiError::internal(),
        DepositAddressError::Database(error) => ApiError::from(error),
    }
}
