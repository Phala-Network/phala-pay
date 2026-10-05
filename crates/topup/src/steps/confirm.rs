//! Detected-to-confirmed deposit step: both providers show the same log at the transfer's receipt
//! position, in the same block, at the route's required confirmation; then the deposit is valued.
//!
//! One check reads, on each provider, the one head the confirmation needs and the transaction's
//! receipt; the block time and the nonce come from the recorded deposit, which its block hash
//! and transaction hash fix.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FinalizedReader, KnownTransfer, ReceiptLookup, TransferLog,
};
use topup_adapters::pricing::PriceSource;
use topup_core::deposit::{RejectReason, RetryError, StepOutcome, WaitReason};
use topup_core::identity::event_id;
use topup_core::money::{AtomicAmount, MinorAmount, PRICE_SCALE, ScaledPrice, credit};
use topup_core::route::{ChainHeads, Confirmations, RouteFile, UNIT_DECIMALS};
use topup_core::valuation::{
    LockTerms, RouteValuation, UnixSeconds, ValuationError, ValuationSource, lock_applies,
    value_deposit,
};
use uuid::Uuid;

use crate::db::{
    CanonicalEvidence, Deposit, EventObject, LockConsumption, OutboxEvent, StoredValuation,
    TransitionEffects,
};
use crate::locks::pricing::{
    PricingRuntime, PricingRuntimes, ValidatedQuote, valuation_error_code,
};
use crate::payment_config::{Binding, Resolution, Terms, required_confirmations};
use crate::pump::{Step, StepResult};
use crate::restore_mode::{DeliveredTransfer, ImportedCredit};
use crate::routes::RouteSet;

#[async_trait]
trait ConfirmationReader: Send + Sync {
    async fn evidence(
        &self,
        tx: B256,
        position: u64,
        known: KnownTransfer,
        needed: u64,
        confirmations: Confirmations,
    ) -> Result<(ChainHeads, ReceiptLookup), ChainError>;
}

#[async_trait]
impl<R> ConfirmationReader for R
where
    R: ChainReader + Send + Sync,
{
    async fn evidence(
        &self,
        tx: B256,
        position: u64,
        known: KnownTransfer,
        needed: u64,
        confirmations: Confirmations,
    ) -> Result<(ChainHeads, ReceiptLookup), ChainError> {
        ChainReader::confirmation_evidence(self, tx, position, known, needed, confirmations).await
    }
}

struct ChainPair {
    confirmations: Confirmations,
    primary: Arc<dyn ConfirmationReader>,
    secondary: Arc<dyn ConfirmationReader>,
}

struct RouteRuntime {
    route: RouteFile,
    pricing: Arc<PricingRuntime>,
}

/// Invalid detected-step runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ConfirmConfigError(String);

/// Two-provider finality, quote validation, and credit computation for detected deposits.
pub struct ConfirmStep {
    context_lookup: Arc<dyn ContextLookup>,
    routes: BTreeMap<(String, u64), RouteRuntime>,
    asset_routes: BTreeMap<(u64, Address), (String, u64)>,
    chains: BTreeMap<u64, ChainPair>,
}

impl ConfirmStep {
    /// Builds production adapters for every loaded route version and each chain's providers.
    pub fn from_routes(
        pool: PgPool,
        routes: &RouteSet,
        pricing: PricingRuntimes,
    ) -> Result<Self, ConfirmConfigError> {
        let mut runtimes = BTreeMap::new();
        for route in routes.routes() {
            let pricing = pricing
                .get(&(route.route.clone(), route.version))
                .cloned()
                .ok_or_else(|| {
                    ConfirmConfigError(format!(
                        "missing pricing runtime for route `{}` version {}",
                        route.route, route.version
                    ))
                })?;
            runtimes.insert(
                (route.route.clone(), route.version),
                RouteRuntime {
                    route: route.clone(),
                    pricing,
                },
            );
        }
        let asset_routes = routes
            .current()
            .map(|route| {
                (
                    (route.chain.chain_id, route.asset.contract),
                    (route.route.clone(), route.version),
                )
            })
            .collect();
        let mut chains = BTreeMap::new();
        for chain_id in routes.chain_ids() {
            let reader = |index| -> Result<Arc<dyn ConfirmationReader>, ConfirmConfigError> {
                let client = routes
                    .provider(chain_id, index)
                    .map_err(|error| ConfirmConfigError(error.to_string()))?;
                Ok(Arc::new(FinalizedReader::new(Arc::clone(client))))
            };
            let confirmations = routes
                .chain(chain_id)
                .ok_or_else(|| ConfirmConfigError(format!("chain {chain_id} has no route")))?
                .confirmations;
            chains.insert(
                chain_id,
                ChainPair {
                    confirmations,
                    primary: reader(0)?,
                    secondary: reader(1)?,
                },
            );
        }
        Ok(Self {
            context_lookup: Arc::new(PostgresContextLookup(pool)),
            routes: runtimes,
            asset_routes,
            chains,
        })
    }

    /// Builds a one-route step with injected chain and price adapters for tests.
    #[allow(clippy::too_many_arguments)]
    pub fn single<R1, R2>(
        pool: PgPool,
        route: RouteFile,
        primary_chain: R1,
        secondary_chain: R2,
        primary_price: Arc<dyn PriceSource>,
        check_price: Option<Arc<dyn PriceSource>>,
        fx_price: Option<Arc<dyn PriceSource>>,
    ) -> Self
    where
        R1: ChainReader + Send + Sync + 'static,
        R2: ChainReader + Send + Sync + 'static,
    {
        let chain_id = route.chain.chain_id;
        let confirmations = route.chain.confirmations;
        let asset_contract = route.asset.contract;
        let key = (route.route.clone(), route.version);
        Self {
            context_lookup: Arc::new(PostgresContextLookup(pool)),
            routes: BTreeMap::from([(
                key.clone(),
                RouteRuntime {
                    route,
                    pricing: Arc::new(PricingRuntime::injected(
                        primary_price,
                        check_price,
                        fx_price,
                    )),
                },
            )]),
            asset_routes: BTreeMap::from([((chain_id, asset_contract), key)]),
            chains: BTreeMap::from([(
                chain_id,
                ChainPair {
                    confirmations,
                    primary: Arc::new(primary_chain),
                    secondary: Arc::new(secondary_chain),
                },
            )]),
        }
    }

    /// The route of the transfer the providers agree on: the deposit's own for its recorded
    /// token, otherwise the current route of the canonical token, if any.
    fn selected_route<'s>(
        &'s self,
        deposit: &'s Deposit,
        canonical: &TransferLog,
    ) -> Option<(&'s String, u64)> {
        if canonical.token == deposit.asset_contract {
            deposit.route.as_ref().zip(deposit.route_version)
        } else {
            self.asset_routes
                .get(&(deposit.chain_id, canonical.token))
                .map(|(name, version)| (name, *version))
        }
    }

    async fn execute(&self, deposit: &Deposit) -> StepResult {
        let context = match self
            .context_lookup
            .load(deposit.address_id, deposit.id)
            .await
        {
            Ok(context) => context,
            Err(error) => {
                tracing::error!(deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id), %error, "confirm context load failed");
                return retry(
                    RetryError::Transient,
                    json!({"stage": "context", "error": "database"}),
                    TransitionEffects::default(),
                );
            }
        };
        let Some(chains) = self.chains.get(&deposit.chain_id) else {
            return retry(
                RetryError::InvariantViolation,
                json!({"stage": "chain", "error": "missing_chain"}),
                TransitionEffects::default(),
            );
        };

        // A deposit recorded while its account's payment settings were held waits for the
        // merchant's reconfirmation, unless the merchant was told its outcome before a restore
        // (docs/design/payment-settings.md §11).
        if matches!(context.binding, Binding::Pending { .. })
            && context.delivered.is_none()
            && context.delivered_rejection.is_none()
        {
            return StepResult::new(
                StepOutcome::Wait {
                    reason: WaitReason::SettingsUnconfirmed,
                },
                json!({"stage": "payment_settings", "result": "awaiting_reconfirmation"}),
            );
        }
        // The stricter of the chain's current floor, the confirmation the deposit's binding
        // requires, and, for a payment of a quote's asset to its address, the quote's (design
        // §7): which of the two governs is known only once the deposit is confirmed.
        let required = |route: Option<&str>| {
            let quoted = context
                .lock
                .as_ref()
                .filter(|lock| !lock.restored && route == Some(lock.route.as_str()))
                .map(|lock| lock.terms.confirmations);
            required_confirmations(
                chains.confirmations,
                context
                    .binding
                    .confirmations(deposit.chain_id)
                    .into_iter()
                    .chain(quoted),
            )
        };
        let mut confirmations = required(deposit.route.as_deref());
        let (mut canonical, mut is_final) =
            match confirmed_evidence(chains, confirmations, deposit, context.address).await {
                FinalityResult::Ready { log, is_final } => (log, is_final),
                FinalityResult::Wait(evidence) => {
                    return confirmation_wait(confirmations, evidence);
                }
                FinalityResult::Retry(error, evidence) => {
                    return retry(error, evidence, TransitionEffects::default());
                }
            };
        let mut selected_route = self.selected_route(deposit, &canonical);
        // The receipt corrected the token: the requirement is that of the corrected facts, so a
        // payment now of the quote's asset is held to the quote's stricter confirmation.
        let corrected = required(selected_route.map(|(name, _)| name.as_str()));
        if corrected.stricter(confirmations) != confirmations {
            confirmations = corrected;
            (canonical, is_final) =
                match confirmed_evidence(chains, confirmations, deposit, context.address).await {
                    FinalityResult::Ready { log, is_final } => (log, is_final),
                    FinalityResult::Wait(evidence) => {
                        return confirmation_wait(confirmations, evidence);
                    }
                    FinalityResult::Retry(error, evidence) => {
                        return retry(error, evidence, TransitionEffects::default());
                    }
                };
            selected_route = self.selected_route(deposit, &canonical);
        }
        let canonical_effect = canonical_effect(deposit, &canonical, selected_route);
        let mut effects = TransitionEffects {
            canonical_evidence: canonical_effect,
            mark_final: is_final,
            ..TransitionEffects::default()
        };
        // After a restore, the credit the merchant was told for this deposit stands: a settled
        // amount is immutable. A delivered transfer that is not the chain's holds the deposit
        // until the operator discards the delivered credit (deploy/runbooks/restore.md).
        if let Some(delivered) = &context.delivered {
            if let Some(field) = delivered.contradiction(deposit, &canonical) {
                return retry(
                    RetryError::InvariantViolation,
                    json!({
                        "stage": "restore",
                        "error": "delivered_event_contradicts_chain",
                        "event": crate::ids::format(crate::ids::EVENT, delivered.event_id),
                        "field": field,
                        "providers": provider_evidence(&canonical),
                    }),
                    effects,
                );
            }
            return carried_forward(deposit, &canonical, &context, delivered, effects);
        }
        // A rejection the merchant was told before a restore stands: no later policy check
        // rewrites it (docs/design/payment-settings.md §11).
        if let Some(rejection) = &context.delivered_rejection {
            if let Some(field) = rejection.transfer.contradiction(deposit, &canonical) {
                return retry(
                    RetryError::InvariantViolation,
                    json!({
                        "stage": "restore",
                        "error": "delivered_event_contradicts_chain",
                        "event": crate::ids::format(
                            crate::ids::EVENT,
                            event_id("deposit.rejected", deposit.id),
                        ),
                        "field": field,
                        "providers": provider_evidence(&canonical),
                    }),
                    effects,
                );
            }
            return rejected_result(
                deposit,
                rejection.reason,
                json!({
                    "stage": "restore",
                    "result": "delivered_rejection",
                    "reason": rejection.reason.code(),
                    "providers": provider_evidence(&canonical),
                }),
                effects,
            );
        }

        let Some((route_name, route_version)) = selected_route else {
            return rejected_result(
                deposit,
                RejectReason::UnsupportedAsset,
                json!({
                    "stage": "route",
                    "result": "unsupported_asset",
                    "providers": provider_evidence(&canonical),
                }),
                effects,
            );
        };
        let Some(runtime) = self.routes.get(&(route_name.clone(), route_version)) else {
            return retry(
                RetryError::InvariantViolation,
                json!({"stage": "route", "error": "unknown_route_version"}),
                effects,
            );
        };

        // A valid payment of the address's quote is governed by the terms the quote was issued
        // with; every other payment by the payment settings the deposit is bound to (design §8).
        // A quote re-issued after a restore carries the merchant's record of its terms, never
        // applied: a payment to it follows its binding, valued at spot.
        let quote_lock = context
            .lock
            .as_ref()
            .filter(|lock| !lock.restored && lock.route == runtime.route.route)
            .and_then(|lock| {
                Some((
                    LockTerms {
                        asset: canonical.token,
                        amount: lock.amount,
                        price: lock.price,
                        credit_minor: lock.credit_minor,
                        expires_at: lock.expires_at,
                        block_time: unix_seconds(canonical.block_time)?,
                    },
                    lock.terms,
                ))
            })
            .filter(|(lock, terms)| {
                lock_applies(
                    canonical.amount,
                    &RouteValuation {
                        asset: &runtime.route.asset,
                        min_credit_minor: terms.min_amount,
                        lock_tolerance_bps: terms.quote_tolerance_bps,
                    },
                    lock,
                )
            });
        let (terms, lock, basis) = match quote_lock {
            Some((lock, terms)) => (terms, Some(lock), json!({"quote_terms": true})),
            None => {
                let revision = match &context.binding {
                    Binding::Revision { id, .. } => *id,
                    Binding::Pending { .. } => {
                        return StepResult::new(
                            StepOutcome::Wait {
                                reason: WaitReason::SettingsUnconfirmed,
                            },
                            json!({"stage": "payment_settings", "result": "awaiting_reconfirmation"}),
                        );
                    }
                };
                let basis = json!({"revision": crate::ids::format("psrev_", revision)});
                match context.binding.resolve(&runtime.route) {
                    Some(Resolution::Accepted(terms)) => (terms, None, basis),
                    resolution => {
                        return rejected_result(
                            deposit,
                            RejectReason::AssetNotAccepted,
                            json!({
                                "stage": "payment_settings",
                                "result": "asset_not_accepted",
                                "basis": basis,
                                "disabled": match resolution {
                                    Some(Resolution::Disabled(reason)) => Some(reason),
                                    _ => None,
                                },
                                "providers": provider_evidence(&canonical),
                            }),
                            effects,
                        );
                    }
                }
            }
        };

        let valuation_at = Utc::now();
        let quote = match runtime.pricing.fetch_fresh(&runtime.route).await {
            Ok(quote) => quote,
            Err(evidence) => {
                return retry(RetryError::PriceUnavailable, evidence.into(), effects);
            }
        };
        let valuation = value_deposit(
            canonical.amount,
            quote.price,
            &RouteValuation {
                asset: &runtime.route.asset,
                min_credit_minor: terms.min_amount,
                lock_tolerance_bps: terms.quote_tolerance_bps,
            },
            lock.as_ref(),
        );
        let valuation = match valuation {
            Ok(valuation) => valuation,
            Err(ValuationError::BelowMinimum) => {
                let computed = credit(
                    canonical.amount,
                    quote.price,
                    runtime.route.asset.decimals,
                    UNIT_DECIMALS,
                );
                let Ok(credit_minor) = computed else {
                    return reject_out_of_range(deposit, effects, &quote);
                };
                effects.valuation = Some(stored_valuation(
                    valuation_at,
                    quote.price,
                    ValuationSource::Spot,
                    credit_minor,
                    quote.evidence.clone(),
                ));
                return rejected_result(
                    deposit,
                    RejectReason::BelowMinimum,
                    json!({
                        "stage": "valuation",
                        "result": "below_minimum",
                        "providers": provider_evidence(&canonical),
                        "quote": quote.evidence,
                    }),
                    effects,
                );
            }
            Err(ValuationError::ArithmeticOutOfRange | ValuationError::Credit(_)) => {
                return reject_out_of_range(deposit, effects, &quote);
            }
            Err(error) => {
                return retry(
                    RetryError::PriceUnavailable,
                    json!({
                        "stage": "valuation",
                        "error": valuation_error_code(&error),
                        "quote": quote.evidence,
                    }),
                    effects,
                );
            }
        };
        if valuation.source == ValuationSource::Lock {
            effects.lock_consumption = Some(LockConsumption {
                address_id: deposit.address_id,
                idempotent: false,
            });
        }
        effects.valuation = Some(stored_valuation(
            valuation_at,
            valuation.price,
            valuation.source,
            valuation.credit_minor,
            quote.evidence.clone(),
        ));
        StepResult {
            outcome: StepOutcome::Advance,
            evidence: json!({
                "stage": "confirmed",
                "providers": provider_evidence(&canonical),
                "corrected": effects.canonical_evidence.is_some(),
                "terms": basis,
                "quote": quote.evidence,
                "valuation": {
                    "price_scaled": valuation.price.value().to_string(),
                    "price_source": valuation_source_code(valuation.source),
                    "credit_minor": valuation.credit_minor.value().to_string(),
                    "valuation_at": valuation_at,
                },
            }),
            events: Vec::new(),
            effects,
        }
    }
}

/// Values the deposit at the credit a delivered event told the merchant, and consumes the quote it
/// paid when that credit was the quote's.
fn carried_forward(
    deposit: &Deposit,
    canonical: &TransferLog,
    context: &ConfirmationContext,
    delivered: &ImportedCredit,
    mut effects: TransitionEffects,
) -> StepResult {
    let credit = &delivered.credit;
    let event = crate::ids::format(crate::ids::EVENT, delivered.event_id);
    if credit.source == ValuationSource::Lock && context.lock.is_some() {
        effects.lock_consumption = Some(LockConsumption {
            address_id: deposit.address_id,
            idempotent: false,
        });
    }
    effects.valuation = Some(stored_valuation(
        credit.valuation_at,
        credit.price,
        credit.source,
        credit.credit_minor,
        json!({"source": "delivered_event", "event": event}),
    ));
    StepResult {
        outcome: StepOutcome::Advance,
        evidence: json!({
            "stage": "confirmed",
            "providers": provider_evidence(canonical),
            "corrected": effects.canonical_evidence.is_some(),
            "valuation": {
                "price_scaled": credit.price.value().to_string(),
                "price_source": valuation_source_code(credit.source),
                "credit_minor": credit.credit_minor.value().to_string(),
                "valuation_at": credit.valuation_at,
                "delivered_event": event,
            },
        }),
        events: Vec::new(),
        effects,
    }
}

#[async_trait]
impl Step for ConfirmStep {
    async fn run(&self, deposit: &Deposit) -> StepResult {
        self.execute(deposit).await
    }
}

#[derive(Clone)]
struct ConfirmationContext {
    address: Address,
    lock: Option<StoredLock>,
    /// The payment settings the deposit is bound to.
    binding: Binding,
    /// The credit a delivered event imported after a restore told the merchant.
    delivered: Option<ImportedCredit>,
    /// The rejection a delivered `deposit.rejected` imported after a restore told the merchant.
    delivered_rejection: Option<DeliveredRejection>,
}

/// A delivered `deposit.rejected` of the deposit, imported after a restore.
#[derive(Clone)]
struct DeliveredRejection {
    reason: RejectReason,
    /// The transfer the delivery names, checked as a delivered credit's is.
    transfer: DeliveredTransfer,
}

#[derive(Clone)]
struct StoredLock {
    /// Re-issued after a restore from the merchant's record: its terms are never applied.
    restored: bool,
    route: String,
    amount: AtomicAmount,
    price: ScaledPrice,
    credit_minor: MinorAmount,
    expires_at: UnixSeconds,
    /// The terms the quote was issued with.
    terms: Terms,
}

#[async_trait]
trait ContextLookup: Send + Sync {
    async fn load(
        &self,
        address_id: Uuid,
        deposit_id: Uuid,
    ) -> Result<ConfirmationContext, sqlx::Error>;
}

struct PostgresContextLookup(PgPool);

#[async_trait]
impl ContextLookup for PostgresContextLookup {
    async fn load(
        &self,
        address_id: Uuid,
        deposit_id: Uuid,
    ) -> Result<ConfirmationContext, sqlx::Error> {
        load_context(&self.0, address_id, deposit_id).await
    }
}

async fn load_context(
    pool: &PgPool,
    address_id: Uuid,
    deposit_id: Uuid,
) -> Result<ConfirmationContext, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT address.address, quote.terms,
               quote.restore_id IS NOT NULL AS restored, quote.route, quote.amount_atomic::text AS amount_atomic,
               quote.price_scaled::text AS price_scaled,
               quote.credit_minor::text AS credit_minor,
               quote.expires_at, quote.consumed_by, quote.status AS lock_status
        FROM addresses AS address
        LEFT JOIN quotes AS quote ON quote.id = address.quote_id
        WHERE address.id = $1
        "#,
    )
    .bind(address_id)
    .fetch_one(pool)
    .await?;
    let address_text: String = row.try_get("address")?;
    let address = Address::from_str(&address_text)
        .map_err(|error| sqlx::Error::Decode(format!("invalid address: {error}").into()))?;
    let consumed_by: Option<Uuid> = row.try_get("consumed_by")?;
    let lock_status: Option<String> = row.try_get("lock_status")?;
    let lock = if consumed_by.is_none()
        && matches!(lock_status.as_deref(), Some("open" | "expired"))
    {
        let route: Option<String> = row.try_get("route")?;
        let amount: Option<String> = row.try_get("amount_atomic")?;
        let price: Option<String> = row.try_get("price_scaled")?;
        let credit_minor: Option<String> = row.try_get("credit_minor")?;
        let expires_at: Option<DateTime<Utc>> = row.try_get("expires_at")?;
        Some(StoredLock {
            restored: row.try_get("restored")?,
            route: route.ok_or_else(|| sqlx::Error::Decode("lock route is missing".into()))?,
            amount: AtomicAmount::new(
                U256::from_str(
                    amount
                        .as_deref()
                        .ok_or_else(|| sqlx::Error::Decode("lock amount is missing".into()))?,
                )
                .map_err(|error| sqlx::Error::Decode(error.to_string().into()))?,
            ),
            price: ScaledPrice::new(
                price
                    .as_deref()
                    .ok_or_else(|| sqlx::Error::Decode("lock price is missing".into()))?
                    .parse::<u64>()
                    .map_err(|error| sqlx::Error::Decode(error.to_string().into()))?,
                PRICE_SCALE,
            )
            .map_err(|error| sqlx::Error::Decode(error.to_string().into()))?,
            credit_minor: MinorAmount::new(
                credit_minor
                    .as_deref()
                    .ok_or_else(|| sqlx::Error::Decode("lock credit is missing".into()))?
                    .parse::<u64>()
                    .map_err(|error| sqlx::Error::Decode(error.to_string().into()))?,
            ),
            expires_at: unix_seconds(
                expires_at.ok_or_else(|| sqlx::Error::Decode("lock expiry is missing".into()))?,
            )
            .ok_or_else(|| sqlx::Error::Decode("lock expiry is invalid".into()))?,
            terms: row
                .try_get::<Option<sqlx::types::Json<Terms>>, _>("terms")?
                .ok_or_else(|| sqlx::Error::Decode("lock terms are missing".into()))?
                .0,
        })
    } else {
        None
    };
    let mut connection = pool.acquire().await?;
    Ok(ConfirmationContext {
        address,
        lock,
        binding: crate::payment_config::deposit_binding(&mut connection, deposit_id).await?,
        delivered: crate::restore_mode::imported_credit(pool, deposit_id).await?,
        delivered_rejection: delivered_rejection(&mut connection, deposit_id).await?,
    })
}

/// The rejection of an imported delivered `deposit.rejected` of `deposit_id`, if any.
async fn delivered_rejection(
    connection: &mut sqlx::PgConnection,
    deposit_id: Uuid,
) -> Result<Option<DeliveredRejection>, sqlx::Error> {
    let row: Option<(Uuid, bool, Value)> = sqlx::query_as(
        "SELECT event.account_id, event.livemode, event.data -> 'object' \
         FROM events AS event \
         JOIN restore_delivered_events AS delivered ON delivered.event_id = event.id \
         WHERE event.id = $1",
    )
    .bind(event_id("deposit.rejected", deposit_id))
    .fetch_optional(connection)
    .await?;
    let Some((account_id, livemode, object)) = row else {
        return Ok(None);
    };
    let invalid = || sqlx::Error::Decode("a delivered deposit.rejected is invalid".into());
    let text = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(invalid)
    };
    let address = |field: &str| Address::from_str(text(field)?).map_err(|_| invalid());
    Ok(Some(DeliveredRejection {
        reason: crate::db::parse_reason(object.get("rejection_reason").and_then(Value::as_str))?
            .ok_or_else(invalid)?,
        transfer: DeliveredTransfer {
            account_id,
            livemode,
            chain_id: object
                .get("chain_id")
                .and_then(Value::as_u64)
                .ok_or_else(invalid)?,
            tx_hash: B256::from_str(text("tx_hash")?).map_err(|_| invalid())?,
            address: address("address")?,
            asset_contract: address("asset_contract")?,
            from_address: address("from_address")?,
            amount_atomic: AtomicAmount::new(
                U256::from_str(text("amount_atomic")?).map_err(|_| invalid())?,
            ),
        },
    }))
}

/// A deposit short of `confirmations`.
fn confirmation_wait(confirmations: Confirmations, evidence: Value) -> StepResult {
    let reason = if confirmations == Confirmations::Finalized {
        WaitReason::Finality
    } else {
        WaitReason::Confirmations
    };
    StepResult::new(StepOutcome::Wait { reason }, evidence)
}

enum FinalityResult {
    Ready { log: TransferLog, is_final: bool },
    Wait(Value),
    Retry(RetryError, Value),
}

fn receipt_block(lookup: &ReceiptLookup) -> Option<u64> {
    match lookup {
        ReceiptLookup::Missing => None,
        ReceiptLookup::Included { block_number, .. } => Some(*block_number),
    }
}

/// Reads the transfer at the deposit's receipt position on both providers and returns it once
/// both show the same log in the same block and the block has reached the route's confirmation
/// on both. The block is final too when both providers' `finalized` covers it.
async fn confirmed_evidence(
    chains: &ChainPair,
    confirmations: Confirmations,
    deposit: &Deposit,
    address: Address,
) -> FinalityResult {
    // Every deposit the scanner records has its transaction's nonce; only a reversed deposit
    // restored from a delivered event lacks it, and it is never confirmed.
    let Some(tx_nonce) = deposit.tx_nonce else {
        return FinalityResult::Retry(
            RetryError::InvariantViolation,
            json!({"stage": "finality", "error": "tx_nonce_missing"}),
        );
    };
    let known = KnownTransfer {
        block_hash: deposit.block_hash,
        block_time: deposit.block_time,
        tx_nonce,
    };
    let (primary, secondary) = tokio::join!(
        chains.primary.evidence(
            deposit.tx_hash,
            deposit.receipt_log_index,
            known,
            deposit.block_number,
            confirmations
        ),
        chains.secondary.evidence(
            deposit.tx_hash,
            deposit.receipt_log_index,
            known,
            deposit.block_number,
            confirmations
        ),
    );
    let (primary_heads, primary_receipt) = match primary {
        Ok((h, r)) => (Ok(h), Ok(r)),
        Err(e) => (Err(e.clone()), Err(e)),
    };
    let (secondary_heads, secondary_receipt) = match secondary {
        Ok((h, r)) => (Ok(h), Ok(r)),
        Err(e) => (Err(e.clone()), Err(e)),
    };
    let (primary_heads, secondary_heads, primary_receipt, secondary_receipt) = match (
        primary_heads,
        secondary_heads,
        primary_receipt,
        secondary_receipt,
    ) {
        (Ok(primary_heads), Ok(secondary_heads), Ok(primary), Ok(secondary)) => {
            (primary_heads, secondary_heads, primary, secondary)
        }
        (primary_heads, secondary_heads, primary_receipt, secondary_receipt) => {
            return FinalityResult::Retry(
                RetryError::RpcDisagreement,
                json!({
                    "stage": "finality",
                    "error": "rpc_failure",
                    "provider_a_head": chain_result_code(&primary_heads),
                    "provider_b_head": chain_result_code(&secondary_heads),
                    "provider_a_receipt": chain_result_code(&primary_receipt),
                    "provider_b_receipt": chain_result_code(&secondary_receipt),
                }),
            );
        }
    };
    let required_block = receipt_block(&primary_receipt)
        .into_iter()
        .chain(receipt_block(&secondary_receipt))
        .max()
        .unwrap_or(deposit.block_number);
    if !confirmations.reached(required_block, primary_heads)
        || !confirmations.reached(required_block, secondary_heads)
    {
        return FinalityResult::Wait(json!({
            "stage": "finality",
            "result": "not_confirmed",
            "confirmations": confirmations.policy_value(),
            "required_block": required_block,
            "provider_a_horizon": confirmations.horizon(primary_heads),
            "provider_b_horizon": confirmations.horizon(secondary_heads),
            "provider_a_finalized": primary_heads.finalized,
            "provider_b_finalized": secondary_heads.finalized,
        }));
    }
    let is_final =
        required_block <= primary_heads.finalized && required_block <= secondary_heads.finalized;
    match (primary_receipt.transfer(), secondary_receipt.transfer()) {
        (Some(primary), Some(secondary)) if primary == secondary => {
            if primary.to != address {
                return FinalityResult::Retry(
                    RetryError::RpcDisagreement,
                    json!({
                        "stage": "finality",
                        "error": "recipient_mismatch",
                        "expected_to": format!("{address:#x}"),
                        "provider_a": provider_evidence(primary),
                        "provider_b": provider_evidence(secondary),
                    }),
                );
            }
            FinalityResult::Ready {
                log: primary.clone(),
                is_final,
            }
        }
        // Past finality on both providers, a missing transfer is a finality or provider fault
        // until the finality watch proves the transaction dropped and reverses the deposit.
        (None, None) if is_final => FinalityResult::Retry(
            RetryError::InvariantViolation,
            json!({
                "stage": "finality",
                "error": "log_absent_at_finality",
                "provider_a_finalized": primary_heads.finalized,
                "provider_b_finalized": secondary_heads.finalized,
            }),
        ),
        // Before finality the transaction may be re-included; the finality watch follows it.
        (None, None) => FinalityResult::Wait(json!({
            "stage": "finality",
            "result": "transfer_absent",
            "provider_a_receipt": receipt_block(&primary_receipt),
            "provider_b_receipt": receipt_block(&secondary_receipt),
        })),
        (primary, secondary) => FinalityResult::Retry(
            RetryError::RpcDisagreement,
            json!({
                "stage": "finality",
                "error": "rpc_disagreement",
                "provider_a": primary.map(provider_evidence),
                "provider_b": secondary.map(provider_evidence),
            }),
        ),
    }
}

fn chain_result_code<T>(result: &Result<T, ChainError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(ChainError::FinalizedHeadRegressed { .. }) => "finalized_regression",
        Err(_) => "rpc_error",
    }
}

fn canonical_effect(
    deposit: &Deposit,
    canonical: &TransferLog,
    selected_route: Option<(&String, u64)>,
) -> Option<CanonicalEvidence> {
    let selected_name = selected_route.map(|(name, _)| name.as_str());
    let selected_version = selected_route.map(|(_, version)| version);
    let changed = deposit.block_number != canonical.block_number
        || deposit.log_index != canonical.log_index
        || deposit.block_hash != canonical.block_hash
        || deposit.block_time != canonical.block_time
        || deposit.asset_contract != canonical.token
        || deposit.from_address != canonical.from
        || deposit.amount_atomic != canonical.amount
        || deposit.route.as_deref() != selected_name
        || deposit.route_version != selected_version;
    changed.then_some(CanonicalEvidence {
        log_index: canonical.log_index,
        block_number: canonical.block_number,
        block_hash: canonical.block_hash,
        block_time: canonical.block_time,
        asset_contract: canonical.token,
        from_address: canonical.from,
        amount_atomic: canonical.amount,
        route: selected_name.map(str::to_owned),
        route_version: selected_version,
    })
}

fn stored_valuation(
    valuation_at: DateTime<Utc>,
    price: ScaledPrice,
    source: ValuationSource,
    credit_minor: MinorAmount,
    quote: Value,
) -> StoredValuation {
    StoredValuation {
        valuation_at,
        price_scaled: price.value(),
        price_source: valuation_source_code(source).to_owned(),
        credit_minor,
        quote,
    }
}

const fn valuation_source_code(source: ValuationSource) -> &'static str {
    match source {
        ValuationSource::Spot => "spot",
        ValuationSource::Lock => "lock",
    }
}

fn reject_out_of_range(
    deposit: &Deposit,
    effects: TransitionEffects,
    quote: &ValidatedQuote,
) -> StepResult {
    rejected_result(
        deposit,
        RejectReason::OutOfRange,
        json!({
            "stage": "valuation",
            "result": "out_of_range",
            "quote": quote.evidence,
        }),
        effects,
    )
}

fn rejected_result(
    deposit: &Deposit,
    reason: RejectReason,
    evidence: Value,
    effects: TransitionEffects,
) -> StepResult {
    StepResult {
        outcome: StepOutcome::Reject(reason),
        evidence,
        events: vec![rejected_event(deposit)],
        effects,
    }
}

fn rejected_event(deposit: &Deposit) -> OutboxEvent {
    OutboxEvent {
        id: event_id("deposit.rejected", deposit.id),
        event_type: "deposit.rejected".to_owned(),
        account_id: deposit.account_id,
        livemode: deposit.livemode,
        object: EventObject::Deposit(deposit.id),
        next_attempt_at: Utc::now(),
        actor: crate::db::SYSTEM_ACTOR.to_owned(),
        request: None,
        signing_key_version: None,
    }
}

fn retry(error: RetryError, evidence: Value, effects: TransitionEffects) -> StepResult {
    StepResult {
        outcome: StepOutcome::Retry { error },
        evidence,
        events: Vec::new(),
        effects,
    }
}

fn unix_seconds(time: DateTime<Utc>) -> Option<UnixSeconds> {
    u64::try_from(time.timestamp()).ok().map(UnixSeconds::new)
}

fn provider_evidence(log: &TransferLog) -> Value {
    json!({
        "tx_hash": format!("{:#x}", log.tx_hash),
        "receipt_log_index": log.receipt_log_index,
        "log_index": log.log_index,
        "block_number": log.block_number,
        "block_hash": format!("{:#x}", log.block_hash),
        "block_time": log.block_time,
        "token": format!("{:#x}", log.token),
        "from": format!("{:#x}", log.from),
        "to": format!("{:#x}", log.to),
        "amount_atomic": log.amount.value().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::time::Duration;

    use topup_adapters::chain::evm::FinalizedHead;
    use topup_adapters::pricing::{Observation, PriceError};
    use topup_core::route::PricingMode;
    use topup_core::valuation::SourceId;

    use super::*;
    use topup_core::deposit::DepositState;

    #[derive(Clone)]
    struct MockChain {
        head: Result<u64, ChainError>,
        latest_ahead: u64,
        logs: Result<Vec<TransferLog>, ChainError>,
    }

    impl ChainReader for MockChain {
        fn finalized_head(
            &self,
        ) -> impl std::future::Future<Output = Result<FinalizedHead, ChainError>> + Send {
            ready(self.head.clone().map(|number| FinalizedHead {
                number,
                time: DateTime::UNIX_EPOCH,
            }))
        }

        fn confirmation_heads(
            &self,
            confirmations: Confirmations,
        ) -> impl std::future::Future<Output = Result<ChainHeads, ChainError>> + Send {
            // `latest` is two blocks past `finalized` when the route asks for it.
            ready(self.head.clone().map(|finalized| {
                ChainHeads {
                    latest: confirmations
                        .needs_latest()
                        .then(|| finalized + self.latest_ahead),
                    safe: None,
                    finalized,
                }
            }))
        }

        async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
            panic!("the confirm step never reads nonces")
        }

        async fn transfer_logs_to(
            &self,
            _addresses: &[Address],
            _from_block: u64,
            _to_block: u64,
        ) -> Result<Vec<TransferLog>, ChainError> {
            panic!("confirm finality must locate the log by receipt identity")
        }

        async fn factory_logs(
            &self,
            _factory: Address,
            _forwarders: &[Address],
            _from_block: u64,
            _to_block: u64,
        ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
            panic!("the confirm step never reads factory events")
        }

        fn receipt_transfer(
            &self,
            tx_hash: B256,
            receipt_log_index: u64,
        ) -> impl std::future::Future<Output = Result<ReceiptLookup, ChainError>> + Send {
            ready(self.logs.clone().map(|logs| {
                logs.into_iter()
                    .find(|log| {
                        log.tx_hash == tx_hash && log.receipt_log_index == receipt_log_index
                    })
                    .map_or(ReceiptLookup::Missing, |log| ReceiptLookup::Included {
                        block_number: log.block_number,
                        block_hash: log.block_hash,
                        transfer: Some(Box::new(log)),
                    })
            }))
        }
    }

    struct MockPrice(Result<Observation, PriceError>);

    #[async_trait]
    impl PriceSource for MockPrice {
        async fn observe(&self) -> Result<Observation, PriceError> {
            self.0.clone()
        }
    }

    struct DelayedPrice {
        source: &'static str,
        value: u64,
        delay: Duration,
    }

    #[async_trait]
    impl PriceSource for DelayedPrice {
        async fn observe(&self) -> Result<Observation, PriceError> {
            tokio::time::sleep(self.delay).await;
            Ok(observation(self.source, self.value, now_seconds()))
        }
    }

    struct MockContext(ConfirmationContext);

    #[async_trait]
    impl ContextLookup for MockContext {
        async fn load(
            &self,
            _address_id: Uuid,
            _deposit_id: Uuid,
        ) -> Result<ConfirmationContext, sqlx::Error> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn quote_and_confirm_share_one_runtime() {
        let mut route: RouteFile =
            serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml"))
                .unwrap();
        route.route = "shared-pricing-runtime".into();
        route.chain.rpc_providers = vec!["http://127.0.0.1:1".into(), "http://127.0.0.1:2".into()];
        let key = (route.route.clone(), route.version);
        let routes = RouteSet::new(vec![route.clone()]).unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/shared-pricing")
            .unwrap();
        let pricing = PricingRuntime::build_all(&routes, pool.clone()).unwrap();
        let runtime = Arc::clone(&pricing[&key]);
        let confirm = ConfirmStep::from_routes(pool, &routes, pricing).unwrap();
        assert!(Arc::ptr_eq(&runtime, &confirm.routes[&key].pricing));
    }

    #[tokio::test(start_paused = true)]
    async fn confirm_evidence_records_cache_provenance_on_hit() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let mut confirm = step(
            route(PricingMode::Spot),
            chain(100, vec![log.clone()]),
            chain(100, vec![log]),
            prices(now_seconds()),
            context(None),
        );
        let runtime = confirm.routes.values_mut().next().unwrap();
        runtime.pricing = Arc::new(PricingRuntime::injected(
            Arc::new(DelayedPrice {
                source: "primary",
                value: 10_000_000,
                delay: Duration::from_millis(10),
            }),
            Some(Arc::new(DelayedPrice {
                source: "check",
                value: 10_000_000,
                delay: Duration::from_millis(10),
            })),
            Some(Arc::new(DelayedPrice {
                source: "fx",
                value: 100_000_000,
                delay: Duration::from_millis(10),
            })),
        ));
        // Both confirm callers arrive before the shared fetch completes (D1).
        let (a, b) = tokio::join!(confirm.run(&deposit), confirm.run(&deposit));
        assert_eq!(a.outcome, StepOutcome::Advance);
        assert_eq!(b.outcome, StepOutcome::Advance);
        let observations = &b.effects.valuation.as_ref().unwrap().quote["observations"];
        assert!(
            observations
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o.get("cached").is_some())
        );
        assert_eq!(b.evidence["quote"]["observations"], *observations);
        tokio::time::advance(Duration::from_millis(1)).await;
        let fresh = confirm.run(&deposit).await;
        assert_eq!(fresh.outcome, StepOutcome::Advance);
        assert!(
            fresh.effects.valuation.unwrap().quote["observations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o.get("cached").is_none())
        );
    }
    #[tokio::test]
    async fn provider_disagreement_retries() {
        let deposit = deposit(1_000);
        let first = transfer(&deposit);
        let mut second = first.clone();
        second.block_hash = B256::repeat_byte(9);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![first]),
            chain(100, vec![second]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Retry {
                error: RetryError::RpcDisagreement
            }
        );
        assert_eq!(result.evidence["error"], "rpc_disagreement");
    }

    #[tokio::test]
    async fn log_absent_on_both_final_providers_is_not_a_disagreement() {
        let result = step(
            route(PricingMode::Spot),
            chain(100, Vec::new()),
            chain(100, Vec::new()),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit(1_000))
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Retry {
                error: RetryError::InvariantViolation
            }
        );
        assert_eq!(result.evidence["error"], "log_absent_at_finality");
    }

    #[tokio::test]
    async fn lagging_provider_waits_for_finality() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![log.clone()]),
            chain(9, vec![log]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Finality
            }
        );
    }

    #[tokio::test]
    async fn lagging_provider_without_receipt_waits_for_finality() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![log]),
            chain(9, Vec::new()),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Finality
            }
        );
    }

    fn depth_route() -> RouteFile {
        let mut route = route(PricingMode::Spot);
        route.chain.confirmations = Confirmations::Depth(2);
        route
    }

    fn chain_at(finalized: u64, latest: u64, logs: Vec<TransferLog>) -> MockChain {
        MockChain {
            head: Ok(finalized),
            latest_ahead: latest - finalized,
            logs: Ok(logs),
        }
    }

    #[tokio::test]
    async fn a_depth_route_confirms_before_finality_without_marking_final() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        // Block 10 with the heads at 11: two blocks, the transfer's own included.
        let result = step(
            depth_route(),
            chain_at(4, 11, vec![log.clone()]),
            chain_at(4, 11, vec![log]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        assert!(!result.effects.mark_final);
    }

    #[tokio::test]
    async fn an_account_requirement_stricter_than_the_route_holds_the_credit() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        for (policy, reason) in [
            (Confirmations::Depth(5), WaitReason::Confirmations),
            (Confirmations::Finalized, WaitReason::Finality),
        ] {
            let result = step(
                depth_route(),
                chain_at(4, 11, vec![log.clone()]),
                chain_at(4, 11, vec![log.clone()]),
                prices(now_seconds()),
                ConfirmationContext {
                    binding: accepting(Some(policy)),
                    ..context(None)
                },
            )
            .run(&deposit)
            .await;
            assert_eq!(result.outcome, StepOutcome::Wait { reason }, "{policy:?}");
        }
        // A requirement weaker than the route's floor never lowers it.
        let result = step(
            depth_route(),
            chain_at(4, 10, vec![log.clone()]),
            chain_at(4, 10, vec![log]),
            prices(now_seconds()),
            ConfirmationContext {
                binding: accepting(Some(Confirmations::Depth(1))),
                ..context(None)
            },
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Confirmations
            }
        );
    }

    #[tokio::test]
    async fn a_depth_route_waits_for_a_lagging_provider_at_the_head_poll_interval() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let result = step(
            depth_route(),
            chain_at(4, 11, vec![log.clone()]),
            chain_at(4, 10, vec![log]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Confirmations
            }
        );
        assert_eq!(result.evidence["result"], "not_confirmed");
    }

    #[tokio::test]
    async fn a_transfer_gone_before_finality_waits_for_the_finality_watch() {
        let result = step(
            depth_route(),
            chain_at(4, 20, Vec::new()),
            chain_at(4, 20, Vec::new()),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit(1_000))
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Confirmations
            }
        );
        assert_eq!(result.evidence["result"], "transfer_absent");
    }

    #[tokio::test]
    async fn a_block_final_on_both_providers_marks_the_deposit_final() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let result = step(
            depth_route(),
            chain_at(10, 12, vec![log.clone()]),
            chain_at(10, 12, vec![log]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        assert!(result.effects.mark_final);
    }

    #[tokio::test]
    async fn agreed_canonical_evidence_corrects_provisional_row() {
        let deposit = deposit(1_000);
        let mut canonical = transfer(&deposit);
        canonical.block_hash = B256::repeat_byte(7);
        canonical.from = Address::repeat_byte(8);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![canonical.clone()]),
            chain(100, vec![canonical.clone()]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        let correction = result.effects.canonical_evidence.expect("correction");
        assert_eq!(correction.block_hash, canonical.block_hash);
        assert_eq!(correction.from_address, canonical.from);
        assert_eq!(result.evidence["corrected"], true);
    }

    #[tokio::test]
    async fn wrong_provisional_block_is_corrected_from_receipt_identity() {
        let mut deposit = deposit(1_000);
        deposit.block_number = 4;
        let mut canonical = transfer(&deposit);
        canonical.block_number = 80;
        canonical.block_hash = B256::repeat_byte(8);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![canonical.clone()]),
            chain(100, vec![canonical.clone()]),
            prices(now_seconds()),
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        assert_eq!(
            result
                .effects
                .canonical_evidence
                .expect("block correction")
                .block_number,
            canonical.block_number
        );
    }

    #[tokio::test]
    async fn canonical_token_reselects_route_and_its_valuation_policy() {
        let deposit = deposit(1_000);
        let mut canonical_route = route(PricingMode::Spot);
        canonical_route.route = "canonical-token-route".to_owned();
        canonical_route.version = 7;
        canonical_route.asset.contract = Address::repeat_byte(9);
        canonical_route.asset.decimals = 3;
        canonical_route.merchant.min_amount.default = 5;
        let mut canonical = transfer(&deposit);
        canonical.token = canonical_route.asset.contract;
        let original_route = route(PricingMode::Spot);
        let original_key = (original_route.route.clone(), original_route.version);
        let canonical_key = (canonical_route.route.clone(), canonical_route.version);
        let now = now_seconds();
        let runtime = |route: RouteFile| RouteRuntime {
            route,
            pricing: Arc::new(PricingRuntime::injected(
                Arc::new(MockPrice(Ok(observation("primary", 10_000_000, now)))),
                Some(Arc::new(MockPrice(Ok(observation(
                    "check", 10_000_000, now,
                ))))),
                Some(Arc::new(MockPrice(Ok(observation("fx", 100_000_000, now))))),
            )),
        };
        let context = context(None);
        let step = ConfirmStep {
            context_lookup: Arc::new(MockContext(context)),
            routes: BTreeMap::from([
                (original_key.clone(), runtime(original_route)),
                (canonical_key.clone(), runtime(canonical_route.clone())),
            ]),
            asset_routes: BTreeMap::from([
                ((deposit.chain_id, deposit.asset_contract), original_key),
                ((deposit.chain_id, canonical.token), canonical_key),
            ]),
            chains: BTreeMap::from([(
                deposit.chain_id,
                ChainPair {
                    confirmations: Confirmations::Finalized,
                    primary: Arc::new(chain(100, vec![canonical.clone()])),
                    secondary: Arc::new(chain(100, vec![canonical])),
                },
            )]),
        };
        let result = step.run(&deposit).await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        let correction = result.effects.canonical_evidence.expect("token correction");
        assert_eq!(correction.route.as_deref(), Some("canonical-token-route"));
        assert_eq!(correction.route_version, Some(7));
        assert_eq!(
            result.effects.valuation.expect("valuation").credit_minor,
            MinorAmount::new(10)
        );
        // Confirmation announces nothing; the product learns of the deposit when it is credited.
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn a_payment_corrected_to_the_quotes_asset_waits_for_the_quotes_confirmation() {
        // Recorded as another token, the transfer is the quote's asset on both providers: the
        // quote's stricter confirmation governs a payment of it (design §7).
        let mut other = depth_route();
        other.route = "other-token-route".to_owned();
        other.asset.contract = Address::repeat_byte(9);
        let quoted = depth_route();
        let mut deposit = deposit(1_000);
        deposit.route = Some(other.route.clone());
        deposit.asset_contract = other.asset.contract;
        let mut canonical = transfer(&deposit);
        canonical.token = quoted.asset.contract;
        let now = now_seconds();
        let mut quote = lock(now);
        quote.terms.confirmations = Confirmations::Depth(10);
        let runtime = |route: RouteFile| RouteRuntime {
            route,
            pricing: Arc::new(PricingRuntime::injected(
                Arc::new(MockPrice(Ok(observation("primary", 10_000_000, now)))),
                Some(Arc::new(MockPrice(Ok(observation(
                    "check", 10_000_000, now,
                ))))),
                Some(Arc::new(MockPrice(Ok(observation("fx", 100_000_000, now))))),
            )),
        };
        let other_key = (other.route.clone(), other.version);
        let quoted_key = (quoted.route.clone(), quoted.version);
        let step = ConfirmStep {
            context_lookup: Arc::new(MockContext(context(Some(quote)))),
            routes: BTreeMap::from([
                (other_key.clone(), runtime(other.clone())),
                (quoted_key.clone(), runtime(quoted.clone())),
            ]),
            asset_routes: BTreeMap::from([
                ((deposit.chain_id, other.asset.contract), other_key),
                ((deposit.chain_id, quoted.asset.contract), quoted_key),
            ]),
            chains: BTreeMap::from([(
                deposit.chain_id,
                ChainPair {
                    confirmations: Confirmations::Depth(2),
                    // Block 10 with the heads at 11: two confirmations, short of the quote's ten.
                    primary: Arc::new(chain_at(4, 11, vec![canonical.clone()])),
                    secondary: Arc::new(chain_at(4, 11, vec![canonical])),
                },
            )]),
        };
        let result = step.run(&deposit).await;
        assert_eq!(
            result.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Confirmations
            }
        );
        assert!(result.effects.valuation.is_none());
    }

    #[tokio::test]
    async fn unsupported_canonical_token_rejects_with_event() {
        let deposit = deposit(1_000);
        let mut canonical = transfer(&deposit);
        canonical.token = Address::repeat_byte(9);
        let context = context(None);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![canonical.clone()]),
            chain(100, vec![canonical]),
            prices(now_seconds()),
            context,
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Reject(RejectReason::UnsupportedAsset)
        );
        assert_rejected_event(&result.events[0], &deposit);
    }

    #[tokio::test]
    async fn freshness_reference_is_captured_after_slow_price_fetches() {
        let mut route = route(PricingMode::Spot);
        route.pricing.max_age_s = 1;
        let runtime = RouteRuntime {
            route,
            pricing: Arc::new(PricingRuntime::injected(
                Arc::new(DelayedPrice {
                    source: "primary",
                    value: 10_000_000,
                    delay: Duration::from_secs(2),
                }),
                Some(Arc::new(DelayedPrice {
                    source: "check",
                    value: 10_000_000,
                    delay: Duration::from_secs(2),
                })),
                Some(Arc::new(DelayedPrice {
                    source: "fx",
                    value: 100_000_000,
                    delay: Duration::from_secs(2),
                })),
            )),
        };
        let quote = runtime
            .pricing
            .fetch(&runtime.route)
            .await
            .expect("slow quote remains fresh");
        assert_eq!(quote.price.value(), 10_000_000);
    }

    #[tokio::test]
    async fn stale_divergent_and_depegged_quotes_retry_with_observations() {
        let now = now_seconds();
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let cases = [
            (
                route(PricingMode::Spot),
                PriceSet {
                    primary: observation("primary", 10_000_000, now - 121),
                    check: Some(observation("check", 10_000_000, now)),
                    fx: Some(observation("fx", 100_000_000, now)),
                },
                "stale",
            ),
            (
                route(PricingMode::Spot),
                PriceSet {
                    primary: observation("primary", 10_000_000, now),
                    check: Some(observation("check", 20_000_000, now)),
                    fx: Some(observation("fx", 100_000_000, now)),
                },
                "divergent",
            ),
            (
                route(PricingMode::Stablecoin),
                PriceSet {
                    primary: observation("primary", 90_000_000, now),
                    check: None,
                    fx: None,
                },
                "depeg",
            ),
        ];
        for (route, prices, error) in cases {
            let result = step(
                route,
                chain(100, vec![log.clone()]),
                chain(100, vec![log.clone()]),
                prices,
                context(None),
            )
            .run(&deposit)
            .await;
            assert_eq!(
                result.outcome,
                StepOutcome::Retry {
                    error: RetryError::PriceUnavailable
                }
            );
            assert_eq!(result.evidence["error"], error);
            assert!(result.evidence["quote"].is_object());
        }
    }

    #[tokio::test]
    async fn stablecoin_mode_uses_fixed_dollar_without_check_sources() {
        let now = now_seconds();
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let result = step(
            route(PricingMode::Stablecoin),
            chain(100, vec![log.clone()]),
            chain(100, vec![log]),
            PriceSet {
                primary: observation("primary", 100_500_000, now),
                check: None,
                fx: None,
            },
            context(None),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        let valuation = result.effects.valuation.expect("valuation");
        assert_eq!(valuation.price_scaled, 100_000_000);
        assert_eq!(valuation.price_source, "spot");
    }

    #[tokio::test]
    async fn lock_at_both_tolerance_bounds_uses_frozen_credit() {
        for amount in [990, 1_010] {
            let now = now_seconds();
            let deposit = deposit(amount);
            let log = transfer(&deposit);
            let result = step(
                route(PricingMode::Spot),
                chain(100, vec![log.clone()]),
                chain(100, vec![log]),
                prices(now),
                context(Some(lock(now))),
            )
            .run(&deposit)
            .await;
            assert_eq!(result.outcome, StepOutcome::Advance);
            let valuation = result.effects.valuation.expect("valuation");
            assert_eq!(valuation.price_source, "lock");
            assert_eq!(valuation.price_scaled, 9_000_000);
            assert_eq!(valuation.credit_minor, MinorAmount::new(777));
            assert!(result.effects.lock_consumption.is_some());
        }
    }

    #[tokio::test]
    async fn lock_miss_falls_back_to_validated_spot() {
        let now = now_seconds();
        let deposit = deposit(1_011);
        let log = transfer(&deposit);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![log.clone()]),
            chain(100, vec![log]),
            prices(now),
            context(Some(lock(now))),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        let valuation = result.effects.valuation.expect("valuation");
        assert_eq!(valuation.price_source, "spot");
        assert_eq!(valuation.credit_minor, MinorAmount::new(101));
        assert!(result.effects.lock_consumption.is_none());
    }

    #[tokio::test]
    async fn a_reissued_quotes_lock_is_never_applied() {
        let now = now_seconds();
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let restored = StoredLock {
            restored: true,
            ..lock(now)
        };
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![log.clone()]),
            chain(100, vec![log]),
            prices(now),
            context(Some(restored)),
        )
        .run(&deposit)
        .await;
        assert_eq!(result.outcome, StepOutcome::Advance);
        let valuation = result.effects.valuation.expect("valuation");
        assert_eq!(valuation.price_source, "spot");
        assert_eq!(valuation.credit_minor, MinorAmount::new(100));
        assert!(result.effects.lock_consumption.is_none());
    }

    fn delivered(deposit: &Deposit, source: ValuationSource) -> ImportedCredit {
        ImportedCredit {
            event_id: event_id("deposit.credited", deposit.id),
            account_id: deposit.account_id,
            livemode: deposit.livemode,
            credit: crate::restore_mode::DeliveredCredit {
                chain_id: deposit.chain_id,
                tx_hash: deposit.tx_hash,
                address: recipient(),
                asset_contract: deposit.asset_contract,
                from_address: deposit.from_address,
                amount_atomic: deposit.amount_atomic,
                price: ScaledPrice::new(25_000_000, PRICE_SCALE).expect("price"),
                source,
                credit_minor: MinorAmount::new(250),
                valuation_at: DateTime::from_timestamp(1_790_000_000, 0).expect("time"),
            },
        }
    }

    #[tokio::test]
    async fn a_delivered_credit_is_carried_forward_instead_of_revaluing_the_deposit() {
        let now = now_seconds();
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        for (source, lock) in [
            (ValuationSource::Spot, None),
            (ValuationSource::Lock, Some(lock(now))),
        ] {
            let result = step(
                route(PricingMode::Spot),
                chain(100, vec![log.clone()]),
                chain(100, vec![log.clone()]),
                prices(now),
                ConfirmationContext {
                    delivered: Some(delivered(&deposit, source)),
                    ..context(lock.clone())
                },
            )
            .run(&deposit)
            .await;
            assert_eq!(result.outcome, StepOutcome::Advance);
            // Spot would credit 100 cents at 0.10; the merchant was told 250 at 0.25.
            let valuation = result.effects.valuation.expect("valuation");
            assert_eq!(valuation.credit_minor, MinorAmount::new(250));
            assert_eq!(valuation.price_scaled, 25_000_000);
            assert_eq!(valuation.price_source, valuation_source_code(source));
            assert_eq!(valuation.valuation_at.timestamp(), 1_790_000_000);
            assert_eq!(valuation.quote["source"], "delivered_event");
            // The quote it paid is consumed, as when it was first credited.
            assert_eq!(result.effects.lock_consumption.is_some(), lock.is_some());
        }
    }

    #[tokio::test]
    async fn a_delivered_credit_the_chain_contradicts_holds_the_deposit() {
        let now = now_seconds();
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let mut other_recipient = delivered(&deposit, ValuationSource::Spot);
        other_recipient.credit.address = Address::repeat_byte(9);
        let mut other_amount = delivered(&deposit, ValuationSource::Spot);
        other_amount.credit.amount_atomic = AtomicAmount::new(U256::from(999_u64));
        let mut other_account = delivered(&deposit, ValuationSource::Spot);
        other_account.account_id = Uuid::new_v4();
        for (delivered, field) in [
            (other_recipient, "address"),
            (other_amount, "amount_atomic"),
            (other_account, "account"),
        ] {
            let result = step(
                route(PricingMode::Spot),
                chain(100, vec![log.clone()]),
                chain(100, vec![log.clone()]),
                prices(now),
                ConfirmationContext {
                    delivered: Some(delivered),
                    ..context(None)
                },
            )
            .run(&deposit)
            .await;
            assert_eq!(
                result.outcome,
                StepOutcome::Retry {
                    error: RetryError::InvariantViolation
                }
            );
            assert_eq!(
                result.evidence["error"],
                "delivered_event_contradicts_chain"
            );
            assert_eq!(result.evidence["field"], field);
            assert!(result.effects.valuation.is_none());
        }
    }

    #[tokio::test]
    async fn a_delivered_rejection_requires_the_full_transfer_identity() {
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let transfer = DeliveredTransfer {
            account_id: deposit.account_id,
            livemode: deposit.livemode,
            chain_id: deposit.chain_id,
            tx_hash: deposit.tx_hash,
            address: log.to,
            asset_contract: log.token,
            from_address: log.from,
            amount_atomic: log.amount,
        };
        for field in [
            "account",
            "livemode",
            "chain_id",
            "tx_hash",
            "address",
            "asset_contract",
            "from_address",
            "amount_atomic",
        ] {
            let mut contradicted = transfer.clone();
            match field {
                "account" => contradicted.account_id = Uuid::new_v4(),
                "livemode" => contradicted.livemode = !deposit.livemode,
                "chain_id" => contradicted.chain_id = 999,
                "tx_hash" => contradicted.tx_hash = B256::repeat_byte(9),
                "address" => contradicted.address = Address::repeat_byte(9),
                "asset_contract" => contradicted.asset_contract = Address::repeat_byte(9),
                "from_address" => contradicted.from_address = Address::repeat_byte(9),
                "amount_atomic" => contradicted.amount_atomic = AtomicAmount::new(U256::from(999)),
                _ => unreachable!(),
            }
            let result = step(
                route(PricingMode::Spot),
                chain(100, vec![log.clone()]),
                chain(100, vec![log.clone()]),
                prices(now_seconds()),
                ConfirmationContext {
                    delivered_rejection: Some(DeliveredRejection {
                        reason: RejectReason::BelowMinimum,
                        transfer: contradicted,
                    }),
                    ..context(None)
                },
            )
            .run(&deposit)
            .await;
            assert_eq!(
                result.outcome,
                StepOutcome::Retry {
                    error: RetryError::InvariantViolation,
                },
                "{field}"
            );
            assert_eq!(
                result.evidence["error"],
                "delivered_event_contradicts_chain"
            );
            assert_eq!(result.evidence["field"], field);
            assert!(result.events.is_empty());
            assert!(result.effects.valuation.is_none());
        }
    }

    #[tokio::test]
    async fn an_unrouted_canonical_token_cannot_rewrite_a_delivered_outcome() {
        let deposit = deposit(1_000);
        let mut canonical = transfer(&deposit);
        canonical.token = Address::repeat_byte(9);
        let rejection = DeliveredRejection {
            reason: RejectReason::BelowMinimum,
            transfer: DeliveredTransfer {
                account_id: deposit.account_id,
                livemode: deposit.livemode,
                chain_id: deposit.chain_id,
                tx_hash: deposit.tx_hash,
                address: recipient(),
                asset_contract: deposit.asset_contract,
                from_address: deposit.from_address,
                amount_atomic: deposit.amount_atomic,
            },
        };
        for restored in [
            ConfirmationContext {
                delivered: Some(delivered(&deposit, ValuationSource::Spot)),
                ..context(None)
            },
            ConfirmationContext {
                delivered_rejection: Some(rejection),
                ..context(None)
            },
        ] {
            let result = step(
                route(PricingMode::Spot),
                chain(100, vec![canonical.clone()]),
                chain(100, vec![canonical.clone()]),
                prices(now_seconds()),
                restored,
            )
            .run(&deposit)
            .await;
            assert_eq!(
                result.outcome,
                StepOutcome::Retry {
                    error: RetryError::InvariantViolation,
                }
            );
            assert_eq!(
                result.evidence["error"],
                "delivered_event_contradicts_chain"
            );
            assert_eq!(result.evidence["field"], "asset_contract");
            assert!(result.events.is_empty());
            assert!(result.effects.valuation.is_none());
        }
    }

    #[tokio::test]
    async fn below_minimum_rejects_and_keeps_quote_fields() {
        let now = now_seconds();
        let mut route = route(PricingMode::Spot);
        route.merchant.min_amount = topup_core::route::Bounded::at(200);
        let deposit = deposit(1_000);
        let log = transfer(&deposit);
        let context = context(None);
        let result = step(
            route,
            chain(100, vec![log.clone()]),
            chain(100, vec![log]),
            prices(now),
            context,
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Reject(RejectReason::BelowMinimum)
        );
        let valuation = result.effects.valuation.expect("valuation");
        assert_eq!(valuation.credit_minor, MinorAmount::new(100));
        assert!(valuation.quote.is_object());
        assert_rejected_event(&result.events[0], &deposit);
    }

    #[tokio::test]
    async fn arithmetic_out_of_range_rejects_with_event() {
        let now = now_seconds();
        let mut deposit = deposit(1);
        deposit.amount_atomic = AtomicAmount::new(U256::MAX);
        let log = transfer(&deposit);
        let context = context(None);
        let result = step(
            route(PricingMode::Spot),
            chain(100, vec![log.clone()]),
            chain(100, vec![log]),
            prices(now),
            context,
        )
        .run(&deposit)
        .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Reject(RejectReason::OutOfRange)
        );
        assert_rejected_event(&result.events[0], &deposit);
    }

    struct PriceSet {
        primary: Observation,
        check: Option<Observation>,
        fx: Option<Observation>,
    }

    fn step(
        route: RouteFile,
        primary_chain: MockChain,
        secondary_chain: MockChain,
        prices: PriceSet,
        context: ConfirmationContext,
    ) -> ConfirmStep {
        let chain_id = route.chain.chain_id;
        let route_confirmations = route.chain.confirmations;
        let asset_contract = route.asset.contract;
        let key = (route.route.clone(), route.version);
        ConfirmStep {
            context_lookup: Arc::new(MockContext(context)),
            routes: BTreeMap::from([(
                key.clone(),
                RouteRuntime {
                    route,
                    pricing: Arc::new(PricingRuntime::injected(
                        Arc::new(MockPrice(Ok(prices.primary))),
                        prices.check.map(|observation| {
                            Arc::new(MockPrice(Ok(observation))) as Arc<dyn PriceSource>
                        }),
                        prices.fx.map(|observation| {
                            Arc::new(MockPrice(Ok(observation))) as Arc<dyn PriceSource>
                        }),
                    )),
                },
            )]),
            asset_routes: BTreeMap::from([((chain_id, asset_contract), key)]),
            chains: BTreeMap::from([(
                chain_id,
                ChainPair {
                    confirmations: route_confirmations,
                    primary: Arc::new(primary_chain),
                    secondary: Arc::new(secondary_chain),
                },
            )]),
        }
    }

    fn route(mode: PricingMode) -> RouteFile {
        let mut route: RouteFile =
            serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml"))
                .expect("route fixture");
        route.pricing.mode = mode;
        route.asset.decimals = 2;
        route.merchant.min_amount = topup_core::route::Bounded {
            default: 1,
            min: 1,
            max: u64::MAX,
        };
        if mode == PricingMode::Stablecoin {
            route.pricing.check.clear();
            route.pricing.primary.clear();
            route.pricing.fx.clear();
            route.pricing.sources = vec![topup_core::price::Source::Kraken {
                symbol: "USDCUSD".into(),
                company: "kraken".into(),
            }];
        }
        route
    }

    fn deposit(amount: u64) -> Deposit {
        let now = Utc::now();
        Deposit {
            id: Uuid::new_v4(),
            chain_id: 1,
            tx_hash: B256::repeat_byte(1),
            receipt_log_index: 0,
            log_index: 3,
            block_number: 10,
            block_hash: B256::repeat_byte(2),
            block_time: now,
            address_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            livemode: true,
            customer_id: Uuid::new_v4(),
            route: Some("phala-cloud-ethereum-pha-usd".to_owned()),
            route_version: Some(1),
            asset_contract: asset(),
            from_address: Address::repeat_byte(4),
            amount_atomic: AtomicAmount::new(U256::from(amount)),
            tx_from: Some(Address::repeat_byte(4)),
            tx_nonce: Some(0),
            final_at: None,
            state: DepositState::Detected,
            reason: None,
            attempt: 0,
            next_attempt_at: now,
            lease_token: None,
            lease_until: None,
            valuation_at: None,
            price_scaled: None,
            price_source: None,
            credit_minor: None,
            quote: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn transfer(deposit: &Deposit) -> TransferLog {
        TransferLog {
            tx_hash: deposit.tx_hash,
            receipt_log_index: deposit.receipt_log_index,
            log_index: deposit.log_index,
            block_number: deposit.block_number,
            block_hash: deposit.block_hash,
            block_time: deposit.block_time,
            tx_from: Address::repeat_byte(4),
            tx_nonce: 0,
            token: deposit.asset_contract,
            from: deposit.from_address,
            to: recipient(),
            amount: deposit.amount_atomic,
        }
    }

    fn chain(head: u64, logs: Vec<TransferLog>) -> MockChain {
        MockChain {
            head: Ok(head),
            latest_ahead: 2,
            logs: Ok(logs),
        }
    }

    fn context(lock: Option<StoredLock>) -> ConfirmationContext {
        ConfirmationContext {
            address: recipient(),
            lock,
            binding: accepting(None),
            delivered: None,
            delivered_rejection: None,
        }
    }

    /// A binding accepting the test routes' asset on chain 1, with `confirmations`.
    fn accepting(confirmations: Option<Confirmations>) -> Binding {
        Binding::Revision {
            id: Uuid::nil(),
            document: crate::payment_config::Document {
                quote_creations_per_customer_per_minute: None,
                chains: vec![crate::payment_config::ChainChoice {
                    chain_id: 1,
                    confirmations,
                    assets: vec![crate::payment_config::AssetChoice::on_defaults("pha")],
                }],
            },
        }
    }

    fn lock(now: u64) -> StoredLock {
        StoredLock {
            restored: false,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            amount: AtomicAmount::new(U256::from(1_000_u64)),
            price: ScaledPrice::new(9_000_000, PRICE_SCALE).expect("lock price"),
            credit_minor: MinorAmount::new(777),
            expires_at: UnixSeconds::new(now + 300),
            terms: crate::payment_config::Terms::defaults(&route(PricingMode::Spot)),
        }
    }

    fn prices(now: u64) -> PriceSet {
        PriceSet {
            primary: observation("primary", 10_000_000, now),
            check: Some(observation("check", 10_000_000, now)),
            fx: Some(observation("fx", 100_000_000, now)),
        }
    }

    fn observation(source: &str, value: u64, at: u64) -> Observation {
        Observation {
            source: SourceId::new(source),
            price: ScaledPrice::new(value, PRICE_SCALE).expect("price"),
            observed_at: UnixSeconds::new(at),
        }
    }

    fn now_seconds() -> u64 {
        u64::try_from(Utc::now().timestamp()).expect("current timestamp")
    }

    fn assert_rejected_event(event: &OutboxEvent, deposit: &Deposit) {
        assert_eq!(event.event_type, "deposit.rejected");
        assert_eq!(event.id, event_id("deposit.rejected", deposit.id));
        assert_eq!(event.account_id, deposit.account_id);
        assert_eq!(event.livemode, deposit.livemode);
        assert_eq!(event.object, EventObject::Deposit(deposit.id));
    }

    fn recipient() -> Address {
        Address::repeat_byte(3)
    }

    fn asset() -> Address {
        Address::from_str("0x6c5bA91642F10282b576d91922Ae6448C9d52f4E").expect("fixture asset")
    }
}
