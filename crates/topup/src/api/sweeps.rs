//! What the account's forwarders hold and how it left them (design D4, §13): the balance
//! (`GET /v1/balance`, Stripe's Balance), the sweeps that moved it (`GET /v1/sweeps`, finalized
//! `Flushed` events, Stripe's Payouts), and the forwarders themselves (`GET /v1/forwarders`), the
//! export that keeps funds recomputable and sweepable without Phala Pay. The `flush` call is
//! built offline by the SDKs from the forwarders.

use std::str::FromStr;

use alloy_primitives::Address as EvmAddress;
use axum::Json;
use axum::extract::{Extension, RawQuery, State};
use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use sqlx::{FromRow, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::ids;
use crate::refunds::DestinationScreening;
use crate::routes::RouteSet;
use crate::tenancy::Scope;

use super::AppState;
use super::auth::Merchant;
use super::error::{ApiError, ErrorResponse};
use super::extract::query_pairs;
use super::models::{Balance, BalanceAmount, Forwarder, ForwarderList, Sweep, SweepList};

type ApiResult<T> = Result<T, ApiError>;

const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;
// Bound screening to stay within each provider's RPC rate budget.
const SWEEPABLE_SCREENING_CONCURRENCY: usize = 4;

/// Starts a query with `held`: per forwarder of the scope and token, every deposit not reversed
/// minus the finalized sweeps (`unswept`), the same over final deposits (`final_unswept`), and
/// whether it holds a deposit rejected as sanctioned.
fn with_held(scope: Scope) -> QueryBuilder<Postgres> {
    let mut builder = QueryBuilder::new(
        "WITH deposited AS ( \
             SELECT deposit.address_id, deposit.asset_contract AS token, \
                    SUM(deposit.amount_atomic) AS total, \
                    COALESCE(SUM(deposit.amount_atomic) \
                        FILTER (WHERE deposit.final_at IS NOT NULL), 0) AS final_total, \
                    bool_or((deposit.state = 'rejected' AND deposit.reason = 'sanctioned') \
                            OR deposit.sanctions_hit_at IS NOT NULL) AS sanctioned \
             FROM deposits AS deposit \
             WHERE deposit.state <> 'reversed' AND deposit.account_id = ",
    );
    builder
        .push_bind(scope.account_id())
        .push(" AND deposit.livemode = ")
        .push_bind(scope.livemode())
        .push(
            " GROUP BY deposit.address_id, deposit.asset_contract \
             ), swept AS ( \
                 SELECT flushed.address_id, flushed.token, SUM(flushed.amount_atomic) AS total \
                 FROM flushed JOIN addresses AS address ON address.id = flushed.address_id \
                 WHERE address.account_id = ",
        )
        .push_bind(scope.account_id())
        .push(" AND address.livemode = ")
        .push_bind(scope.livemode())
        .push(
            " GROUP BY flushed.address_id, flushed.token \
             ), held AS ( \
                 SELECT deposited.address_id, deposited.token, deposited.sanctioned, \
                        GREATEST(deposited.total - COALESCE(swept.total, 0), 0) AS unswept, \
                        GREATEST(deposited.final_total - COALESCE(swept.total, 0), 0) \
                            AS final_unswept \
                 FROM deposited LEFT JOIN swept \
                     ON swept.address_id = deposited.address_id \
                     AND swept.token = deposited.token \
             ) ",
        );
    builder
}

#[utoipa::path(
    get,
    path = "/v1/balance",
    responses(
        (status = 200, description = "OK", body = Balance),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "balance"
)]
/// What the account's forwarders hold in the key's mode, per chain and token: deposits not
/// reversed minus finalized sweeps, and the part of it from final deposits, which is safe to
/// sweep. A forwarder's funds can only ever reach its treasury.
pub(crate) async fn get_balance(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
) -> ApiResult<Json<Balance>> {
    let scope = merchant.scope;
    let mut builder = with_held(scope);
    builder.push(
        "SELECT address.chain_id, held.token, SUM(held.unswept)::text, \
                SUM(held.final_unswept)::text \
         FROM held JOIN addresses AS address ON address.id = held.address_id \
         GROUP BY address.chain_id, held.token \
         HAVING SUM(held.unswept) > 0 \
         ORDER BY address.chain_id, held.token",
    );
    let rows: Vec<(i64, String, String, String)> =
        builder.build_query_as().fetch_all(&state.pool).await?;
    let unswept = rows
        .into_iter()
        .map(|(chain_id, token, amount, final_amount)| {
            let chain_id = u64::try_from(chain_id).map_err(|_| ApiError::internal())?;
            Ok(BalanceAmount {
                chain_id,
                asset: asset(&state.routes, scope, chain_id, &token),
                token,
                amount_atomic: amount,
                final_amount_atomic: final_amount,
            })
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(Balance {
        object: "balance".to_owned(),
        livemode: scope.livemode(),
        unswept,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/sweeps",
    params(
        ("chain_id" = Option<u64>, Query, description = "Only this chain's sweeps"),
        ("forwarder" = Option<String>, Query, description = "Only this forwarder's sweeps, `fwd_…`"),
        ("token" = Option<String>, Query, description = "Only sweeps of this token contract"),
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`sw_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`sw_` id: the page before it")
    ),
    responses(
        (status = 200, description = "OK", body = SweepList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "sweeps"
)]
/// The sweeps of the key's mode, newest first: every finalized `Flushed` event of a forwarder of
/// the account, whoever sent the `flush`. A deposit is `swept` once a sweep after it moved its
/// forwarder's balance.
pub(crate) async fn list_sweeps(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<SweepList>> {
    let scope = merchant.scope;
    let mut builder = QueryBuilder::<Postgres>::new(
        "SELECT flushed.id, address.id AS forwarder, address.livemode, flushed.chain_id, \
                address.address, flushed.token, flushed.treasury, \
                flushed.amount_atomic::text AS amount_atomic, flushed.tx_hash, \
                flushed.log_index, flushed.block_number, flushed.created_at \
         FROM flushed JOIN addresses AS address ON address.id = flushed.address_id \
         WHERE address.account_id = ",
    );
    builder
        .push_bind(scope.account_id())
        .push(" AND address.livemode = ")
        .push_bind(scope.livemode());
    let mut page = Page::default();
    for (name, value) in query_pairs(query.as_deref()) {
        match name.as_str() {
            "chain_id" => {
                builder
                    .push(" AND flushed.chain_id = ")
                    .push_bind(chain_id(&value)?);
            }
            "forwarder" => {
                let id = ids::parse(ids::FORWARDER, &value)
                    .ok_or_else(|| ApiError::invalid_param("forwarder", "not a fwd_ id"))?;
                builder.push(" AND address.id = ").push_bind(id);
            }
            "token" => {
                builder
                    .push(" AND flushed.token = ")
                    .push_bind(token("token", &value)?);
            }
            _ => page.parse(&name, &value, ids::SWEEP)?,
        }
    }
    let cursor = page.cursor()?;
    let before = cursor.is_some_and(|(_, _, before)| before);
    if let Some((id, param, before)) = cursor {
        let created_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT flushed.created_at FROM flushed \
             JOIN addresses AS address ON address.id = flushed.address_id \
             WHERE flushed.id = $1 AND address.account_id = $2 AND address.livemode = $3",
        )
        .bind(id)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::invalid_param(param, "no such sweep"))?;
        builder
            .push(if before {
                " AND (flushed.created_at, flushed.id) > ("
            } else {
                " AND (flushed.created_at, flushed.id) < ("
            })
            .push_bind(created_at)
            .push(", ")
            .push_bind(id)
            .push(")");
    }
    builder
        .push(if before {
            " ORDER BY flushed.created_at ASC, flushed.id ASC LIMIT "
        } else {
            " ORDER BY flushed.created_at DESC, flushed.id DESC LIMIT "
        })
        .push_bind(page.limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<SweepRow>()
        .fetch_all(&state.pool)
        .await?;
    let has_more = page.truncate(&mut rows, before)?;
    let data = rows
        .into_iter()
        .map(|row| {
            let chain_id = u64::try_from(row.chain_id).map_err(|_| ApiError::internal())?;
            Ok(Sweep {
                id: ids::format(ids::SWEEP, row.id),
                object: "sweep".to_owned(),
                livemode: row.livemode,
                chain_id,
                forwarder: ids::format(ids::FORWARDER, row.forwarder),
                address: row.address,
                asset: asset(&state.routes, scope, chain_id, &row.token),
                token: row.token,
                treasury: row.treasury,
                amount_atomic: row.amount_atomic,
                tx_hash: row.tx_hash,
                log_index: u64::try_from(row.log_index).map_err(|_| ApiError::internal())?,
                block_number: u64::try_from(row.block_number).map_err(|_| ApiError::internal())?,
                created: row.created_at.timestamp(),
            })
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(SweepList {
        object: "list".to_owned(),
        url: "/v1/sweeps".to_owned(),
        has_more,
        data,
    }))
}

#[derive(FromRow)]
struct SweepRow {
    id: Uuid,
    forwarder: Uuid,
    livemode: bool,
    chain_id: i64,
    address: String,
    token: String,
    treasury: String,
    amount_atomic: String,
    tx_hash: String,
    log_index: i64,
    block_number: i64,
    created_at: DateTime<Utc>,
}

#[utoipa::path(
    get,
    path = "/v1/forwarders",
    params(
        ("chain_id" = Option<u64>, Query, description = "Only this chain's forwarders"),
        ("quote" = Option<String>, Query, description = "Only this quote's forwarder, `qt_…`"),
        ("deposit_address" = Option<String>, Query, description = "Only this deposit address's networks, `da_…`"),
        (
            "sweepable" = Option<String>, Query,
            description = "A token contract: only forwarders with a final unswept balance of it \
                           that may be swept, never one holding a deposit rejected as sanctioned \
                           or paying a treasury a sanctions list names"
        ),
        ("limit" = Option<i64>, Query, description = "1 to 100, default 10"),
        ("starting_after" = Option<String>, Query, description = "`fwd_` id: the page after it"),
        ("ending_before" = Option<String>, Query, description = "`fwd_` id: the page before it")
    ),
    responses(
        (status = 200, description = "OK", body = ForwarderList),
        (status = 400, description = "Bad Request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 503, description = "`sweepable`: treasury screening is unavailable; retry", body = ErrorResponse)
    ),
    security(("api_key" = [])),
    tag = "forwarders"
)]
/// Every forwarder issued in the key's mode, for quotes and for deposit address networks
/// (current and superseded), with the `(factory, salt, treasury)` its address derives from: the
/// export that keeps funds recomputable and sweepable without Phala Pay (design §13). With
/// `sweepable`, the forwarders to pass to the SDK's `flush_transaction` or `safe_batch`, one call
/// per chain and treasury. Pages follow `id` order.
pub(crate) async fn list_forwarders(
    State(state): State<AppState>,
    Extension(merchant): Extension<Merchant>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<ForwarderList>> {
    let scope = merchant.scope;
    let mut sweepable = None;
    let mut requested_chain = None;
    let mut conditions = Vec::new();
    let mut page = Page::default();
    for (name, value) in query_pairs(query.as_deref()) {
        match name.as_str() {
            "chain_id" => {
                let chain_id = chain_id(&value)?;
                requested_chain = Some(chain_id);
                conditions.push(Condition::Chain(chain_id));
            }
            "quote" => conditions.push(Condition::Quote(
                ids::parse(ids::QUOTE, &value)
                    .ok_or_else(|| ApiError::invalid_param("quote", "not a qt_ id"))?,
            )),
            "deposit_address" => conditions.push(Condition::DepositAddress(
                ids::parse(ids::DEPOSIT_ADDRESS, &value)
                    .ok_or_else(|| ApiError::invalid_param("deposit_address", "not a da_ id"))?,
            )),
            "sweepable" => sweepable = Some(token("sweepable", &value)?),
            _ => page.parse(&name, &value, ids::FORWARDER)?,
        }
    }
    let cursor = page.cursor()?;
    let before = cursor.is_some_and(|(_, _, before)| before);
    if let Some((id, param, _)) = cursor {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM addresses \
             WHERE id = $1 AND account_id = $2 AND livemode = $3)",
        )
        .bind(id)
        .bind(scope.account_id())
        .bind(scope.livemode())
        .fetch_one(&state.pool)
        .await?;
        if !known {
            return Err(ApiError::invalid_param(param, "no such forwarder"));
        }
    }
    let mut builder = if sweepable.is_some() {
        with_held(scope)
    } else {
        QueryBuilder::new("")
    };
    builder.push(
        "SELECT address.id, address.livemode, address.chain_id, address.address, address.salt, \
                address.treasury, address.quote_id, address.deposit_address_id, \
                address.superseded_at \
         FROM addresses AS address ",
    );
    if let Some(token) = &sweepable {
        let treasuries = sweepable_treasuries(&state, scope, token, requested_chain).await?;
        builder
            .push(
                "JOIN held ON held.address_id = address.id AND held.final_unswept > 0 \
                 AND NOT held.sanctioned AND held.token = ",
            )
            .push_bind(token.clone())
            .push(" AND address.treasury = ANY(")
            .push_bind(treasuries)
            .push(") ");
    }
    builder
        .push("WHERE address.account_id = ")
        .push_bind(scope.account_id())
        .push(" AND address.livemode = ")
        .push_bind(scope.livemode());
    for condition in conditions {
        match condition {
            Condition::Chain(chain_id) => {
                builder.push(" AND address.chain_id = ").push_bind(chain_id);
            }
            Condition::Quote(id) => {
                builder.push(" AND address.quote_id = ").push_bind(id);
            }
            Condition::DepositAddress(id) => {
                builder
                    .push(" AND address.deposit_address_id = ")
                    .push_bind(id);
            }
        }
    }
    if let Some((id, _, before)) = cursor {
        builder
            .push(if before {
                " AND address.id < "
            } else {
                " AND address.id > "
            })
            .push_bind(id);
    }
    builder
        .push(if before {
            " ORDER BY address.id DESC LIMIT "
        } else {
            " ORDER BY address.id ASC LIMIT "
        })
        .push_bind(page.limit.saturating_add(1));
    let mut rows = builder
        .build_query_as::<ForwarderRow>()
        .fetch_all(&state.pool)
        .await?;
    let has_more = page.truncate(&mut rows, before)?;
    let data = rows
        .into_iter()
        .map(|row| {
            let chain_id = u64::try_from(row.chain_id).map_err(|_| ApiError::internal())?;
            let contracts = state.routes.chain(chain_id).ok_or_else(|| {
                tracing::error!(chain_id, "an issued forwarder's chain is not loaded");
                ApiError::internal()
            })?;
            Ok(Forwarder {
                id: ids::format(ids::FORWARDER, row.id),
                object: "forwarder".to_owned(),
                livemode: row.livemode,
                chain_id,
                address: row.address,
                factory: format!("{:#x}", contracts.contracts.forwarder_factory),
                salt: row.salt,
                treasury: row.treasury,
                quote: row.quote_id.map(|id| ids::format(ids::QUOTE, id)),
                deposit_address: row
                    .deposit_address_id
                    .map(crate::deposit_addresses::public_id),
                superseded_at: row.superseded_at.map(|at| at.timestamp()),
            })
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(ForwarderList {
        object: "list".to_owned(),
        url: "/v1/forwarders".to_owned(),
        has_more,
        data,
    }))
}

enum Condition {
    Chain(i64),
    Quote(Uuid),
    DepositAddress(Uuid),
}

#[derive(FromRow)]
struct ForwarderRow {
    id: Uuid,
    livemode: bool,
    chain_id: i64,
    address: String,
    salt: String,
    treasury: String,
    quote_id: Option<Uuid>,
    deposit_address_id: Option<Uuid>,
    superseded_at: Option<DateTime<Utc>>,
}

/// The treasuries with a sweepable balance of `token` that no sanctions list names, screened
/// with the oracle of their chain's route. Unavailable screening is `503`: no flush is built to a
/// treasury that could not be screened.
async fn sweepable_treasuries(
    state: &AppState,
    scope: Scope,
    token: &str,
    requested_chain: Option<i64>,
) -> ApiResult<Vec<String>> {
    let mut builder = with_held(scope);
    builder
        .push(
            "SELECT DISTINCT address.chain_id, address.treasury \
             FROM held JOIN addresses AS address ON address.id = held.address_id \
             WHERE held.final_unswept > 0 AND NOT held.sanctioned AND held.token = ",
        )
        .push_bind(token.to_owned());
    if let Some(chain_id) = requested_chain {
        builder.push(" AND address.chain_id = ").push_bind(chain_id);
    }
    let candidates: Vec<(i64, String)> = builder.build_query_as().fetch_all(&state.pool).await?;
    let screening = &*state.screening;
    let mut checks = Vec::new();
    for (chain_id, treasury) in candidates {
        let chain_id = u64::try_from(chain_id).map_err(|_| ApiError::internal())?;
        let Some(route) = state
            .routes
            .current_in(scope.livemode())
            .find(|route| route.chain.chain_id == chain_id)
        else {
            continue;
        };
        let address = EvmAddress::from_str(&treasury).map_err(|_| ApiError::internal())?;
        checks.push(async move { (treasury, screening.screen_cached(route, address).await) });
    }
    let results = stream::iter(checks)
        .buffered(SWEEPABLE_SCREENING_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut clear = Vec::new();
    for (treasury, verdict) in results {
        match verdict {
            DestinationScreening::Clear => clear.push(treasury),
            DestinationScreening::Sanctioned => {}
            DestinationScreening::Unavailable => {
                return Err(ApiError::service_unavailable(
                    "sanctions screening of the treasuries is unavailable; retry",
                ));
            }
        }
    }
    Ok(clear)
}

/// Stripe's `limit`, `starting_after`, and `ending_before`.
struct Page {
    limit: i64,
    starting_after: Option<Uuid>,
    ending_before: Option<Uuid>,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            starting_after: None,
            ending_before: None,
        }
    }
}

impl Page {
    fn parse(&mut self, name: &str, value: &str, prefix: &str) -> ApiResult<()> {
        match name {
            "limit" => {
                self.limit = value
                    .parse::<i64>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| ApiError::invalid_param("limit", "limit must be 1 to 100"))?;
            }
            "starting_after" | "ending_before" => {
                let id = ids::parse(prefix, value)
                    .ok_or_else(|| ApiError::invalid_param(name, format!("not a {prefix} id")))?;
                if name == "starting_after" {
                    self.starting_after = Some(id);
                } else {
                    self.ending_before = Some(id);
                }
            }
            other => {
                return Err(
                    ApiError::unknown_param(format!("unknown parameter {other}")).with_param(other),
                );
            }
        }
        Ok(())
    }

    /// The cursor, its parameter, and whether the page is before it.
    fn cursor(&self) -> ApiResult<Option<(Uuid, &'static str, bool)>> {
        match (self.starting_after, self.ending_before) {
            (Some(_), Some(_)) => Err(ApiError::bad_request(
                "starting_after and ending_before are mutually exclusive",
            )),
            (Some(id), None) => Ok(Some((id, "starting_after", false))),
            (None, Some(id)) => Ok(Some((id, "ending_before", true))),
            (None, None) => Ok(None),
        }
    }

    /// Cuts the extra row fetched to learn `has_more`, and puts a page before the cursor back in
    /// order.
    fn truncate<T>(&self, rows: &mut Vec<T>, before: bool) -> ApiResult<bool> {
        let has_more = i64::try_from(rows.len()).map_err(|_| ApiError::internal())? > self.limit;
        rows.truncate(usize::try_from(self.limit).map_err(|_| ApiError::internal())?);
        if before {
            rows.reverse();
        }
        Ok(has_more)
    }
}

fn chain_id(value: &str) -> ApiResult<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|id| *id >= 0)
        .ok_or_else(|| ApiError::invalid_param("chain_id", "chain_id must be a chain id"))
}

fn token(param: &str, value: &str) -> ApiResult<String> {
    EvmAddress::from_str(value)
        .map(|token| format!("{token:#x}"))
        .map_err(|_| ApiError::invalid_param(param, "must be a 20-byte token contract"))
}

/// The asset code of a routed token of the scope's mode.
fn asset(routes: &RouteSet, scope: Scope, chain_id: u64, token: &str) -> Option<String> {
    let token = EvmAddress::from_str(token).ok()?;
    routes
        .current_in(scope.livemode())
        .find(|route| route.chain.chain_id == chain_id && route.asset.contract == token)
        .map(|route| route.asset.symbol.clone())
}
