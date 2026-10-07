//! Confirmed-to-credited screening step.
//!
//! A deposit that passes screening is credited: its USD value is final and owed to the account,
//! which learns it from the `deposit.credited` webhook written in the same transaction. A deposit
//! that is not final yet waits for finality instead when its credit would take the account's
//! credit that is not final past `accounts.max_unfinalized_credit`.

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy_primitives::Address;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use topup_adapters::risk::oracle::{SanctionsOracle, SanctionsOracleConfigError, SanctionsSource};
use topup_core::deposit::{DepositState, RejectReason, RetryError, StepOutcome};
use topup_core::identity::{credited_event_id, event_id};
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use topup_core::screening::{Bounds, PauseScopes, SanctionsResult, screen};

use crate::db::{Deposit, EventObject, OutboxEvent};
use crate::pause::{self, PauseScopeSources};
use crate::pump::{Step, StepResult};
use crate::routes::{ProviderError, RouteSet};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RouteKey {
    name: String,
    version: u64,
}

/// One immutable route version and its sanctions source. A deposit's amount bounds are those of
/// the terms that govern it (`crate::payment_config`), read when it is screened.
#[derive(Clone)]
pub struct ScreenRoute {
    route: RouteFile,
    sanctions: Arc<dyn SanctionsSource>,
}

impl ScreenRoute {
    /// Creates an injectable route configuration, including a mockable sanctions source.
    #[must_use]
    pub fn new(route: RouteFile, sanctions: Arc<dyn SanctionsSource>) -> Self {
        Self { route, sanctions }
    }

    fn key(&self) -> RouteKey {
        RouteKey {
            name: self.route.route.clone(),
            version: self.route.version,
        }
    }

    async fn evaluate(
        &self,
        deposit: &Deposit,
        pause_scopes: PauseScopeSources,
        bounds: Bounds,
    ) -> StepResult {
        let sanctions = self
            .sanctions
            .sanctions(deposit.from_address, deposit.block_number)
            .await;
        if sanctions.block_number < deposit.block_number {
            return transient_result("sanctions_pin_before_payment", deposit.block_number);
        }
        let outcome = screen(
            deposit.amount_atomic,
            &sanctions,
            &bounds,
            &pause_scopes.effective,
            &PauseScopes::default(),
        );
        let oracle = self.route.screening.sanctions_oracle;
        let evidence = screening_evidence(oracle, sanctions, bounds, &pause_scopes);
        let mut result = StepResult::new(outcome, evidence);
        if let StepOutcome::Reject(_) = outcome {
            result.events.push(rejected_event(deposit));
        }
        result
    }
}

/// Failure while constructing the route-to-screening registry.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ScreenStepConfigError {
    /// One of the route's first two RPC providers is unusable.
    #[error("route `{route}` version {version}: {source}")]
    Provider {
        /// Stable route name.
        route: String,
        /// Immutable route version.
        version: u64,
        /// Non-secret provider failure.
        source: ProviderError,
    },
    /// A route's sanctions-oracle client could not be configured.
    #[error("route `{route}` version {version} has invalid sanctions configuration: {source}")]
    InvalidOracle {
        /// Stable route name.
        route: String,
        /// Immutable route version.
        version: u64,
        /// Non-secret configuration failure.
        source: SanctionsOracleConfigError,
    },
    /// Two supplied route files used the same name and version.
    #[error("duplicate route `{route}` version {version}")]
    DuplicateRoute {
        /// Stable route name.
        route: String,
        /// Immutable route version.
        version: u64,
    },
}

/// Real screening step backed by PostgreSQL pause scopes and route-specific oracle clients.
pub struct ScreenStep {
    pool: PgPool,
    routes: BTreeMap<RouteKey, ScreenRoute>,
}

impl ScreenStep {
    /// Creates a step from already composed route policies and sanctions sources.
    pub fn new(
        pool: PgPool,
        routes: impl IntoIterator<Item = ScreenRoute>,
    ) -> Result<Self, ScreenStepConfigError> {
        let mut by_key = BTreeMap::new();
        for route in routes {
            let key = route.key();
            if by_key.insert(key.clone(), route).is_some() {
                return Err(ScreenStepConfigError::DuplicateRoute {
                    route: key.name,
                    version: key.version,
                });
            }
        }
        Ok(Self {
            pool,
            routes: by_key,
        })
    }

    /// Creates route-specific sanctions checks on each route chain's first two providers.
    pub fn from_routes(pool: PgPool, routes: &RouteSet) -> Result<Self, ScreenStepConfigError> {
        let mut screening_routes = Vec::with_capacity(routes.routes().len());
        for route in routes.routes() {
            let provider = |index| {
                routes
                    .provider(route.chain.chain_id, index)
                    .map(Arc::clone)
                    .map_err(|source| ScreenStepConfigError::Provider {
                        route: route.route.clone(),
                        version: route.version,
                        source,
                    })
            };
            let oracle =
                SanctionsOracle::new(provider(0)?, provider(1)?, route.screening.sanctions_oracle)
                    .map_err(|source| ScreenStepConfigError::InvalidOracle {
                        route: route.route.clone(),
                        version: route.version,
                        source,
                    })?;
            screening_routes.push(ScreenRoute::new(route.clone(), Arc::new(oracle)));
        }
        Self::new(pool, screening_routes)
    }

    /// The amount bounds of the terms that govern `deposit` on `route`; none bound a credit the
    /// merchant was told before a restore (docs/design/payment-settings.md §11).
    async fn bounds(
        &self,
        route: &RouteFile,
        deposit: &Deposit,
        delivered: bool,
    ) -> Result<Option<Bounds>, sqlx::Error> {
        if delivered {
            return Ok(Some(Bounds {
                min_atomic: AtomicAmount::default(),
                max_atomic: AtomicAmount::new(alloy_primitives::U256::MAX),
            }));
        }
        let mut connection = self.pool.acquire().await?;
        Ok(
            crate::payment_config::deposit_terms_on(&mut connection, route, deposit.id)
                .await?
                .map(|terms| terms.bounds()),
        )
    }

    async fn pause_scopes(
        &self,
        deposit: &Deposit,
        route: &str,
    ) -> Result<Option<PauseScopeSources>, sqlx::Error> {
        pause::customer_pause_scopes(&self.pool, deposit.customer_id, route, deposit.address_id)
            .await
    }
}

#[async_trait]
impl Step for ScreenStep {
    async fn run(&self, deposit: &Deposit) -> StepResult {
        if deposit.state != DepositState::Confirmed {
            return invariant_result("screen_step_requires_confirmed", deposit.block_number);
        }
        let (Some(route), Some(version)) = (&deposit.route, deposit.route_version) else {
            return invariant_result("missing_deposit_route", deposit.block_number);
        };
        let key = RouteKey {
            name: route.clone(),
            version,
        };
        let Some(screening_route) = self.routes.get(&key) else {
            return invariant_result("unknown_deposit_route", deposit.block_number);
        };
        let pause_scopes = match self.pause_scopes(deposit, route).await {
            Ok(Some(pauses)) => pauses,
            Ok(None) => return invariant_result("customer_not_found", deposit.block_number),
            Err(_) => return transient_result("pause_scope_load_failed", deposit.block_number),
        };
        // A credit the merchant was told before a restore stands (design §11).
        let delivered = match crate::restore_mode::imported_credit(&self.pool, deposit.id).await {
            Ok(credit) => credit.is_some(),
            Err(_) => {
                return transient_result("delivered_credit_load_failed", deposit.block_number);
            }
        };
        let bounds = match self
            .bounds(&screening_route.route, deposit, delivered)
            .await
        {
            Ok(Some(bounds)) => bounds,
            Ok(None) => return invariant_result("deposit_terms_missing", deposit.block_number),
            Err(_) => return transient_result("deposit_terms_load_failed", deposit.block_number),
        };
        let mut result = screening_route
            .evaluate(deposit, pause_scopes, bounds)
            .await;
        if delivered && result.outcome == StepOutcome::Reject(RejectReason::Sanctioned) {
            result = delivered_credit_sanctioned(deposit, result.evidence);
        }
        if result.outcome == StepOutcome::Advance {
            // A deposit that is not final yet is credited only within the account's cap on
            // credit a reorganization could still reverse; past it, it is credited once final.
            if deposit.final_at.is_none() {
                let credit = deposit.credit_minor.map_or(0, |credit| credit.value());
                match crate::db::unfinalized_credit(
                    &self.pool,
                    deposit.account_id,
                    deposit.livemode,
                    deposit.id,
                )
                .await
                {
                    Ok(exposure) if !exposure.admits(credit) => {
                        return crate::pump::unfinalized_cap_wait(exposure, credit);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return transient_result(
                            "unfinalized_credit_load_failed",
                            deposit.block_number,
                        );
                    }
                }
            }
            match credited_event(deposit) {
                Ok(event) => result.events.push(event),
                Err(error) => return invariant_result(error, deposit.block_number),
            }
        }
        result
    }
}

/// A sanctions hit on a credit the merchant was told before a restore: compliance, not commercial
/// policy, so the credit stands (no `deposit.rejected` rewrites it). The service records the hit,
/// which keeps the forwarder from every sweep (`GET /v1/forwarders?sweepable`), and raises
/// `TopupDeliveredCreditSanctioned` for the operator (docs/design/payment-settings.md §11).
fn delivered_credit_sanctioned(deposit: &Deposit, mut evidence: serde_json::Value) -> StepResult {
    tracing::error!(
        tags.alert = "TopupDeliveredCreditSanctioned",
        deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
        account_id = %deposit.account_id,
        livemode = deposit.livemode,
        chain_id = deposit.chain_id,
        "a sanctions list names the sender of a credit delivered before a restore: the credit \
         stands and its forwarder is not swept (deploy/runbooks/restore.md)"
    );
    evidence["sanctions_hit"] = json!("delivered_credit_stands");
    let mut result = StepResult::new(StepOutcome::Advance, evidence);
    result.effects.sanctions_hit = true;
    result
}

/// The fulfillment event: `deposit.credited`, whose object is the credited deposit, keyed by an
/// id derived from the deposit id.
fn credited_event(deposit: &Deposit) -> Result<OutboxEvent, &'static str> {
    // A credit always has its valuation; the event's deposit object carries it.
    deposit.credit_minor.ok_or("credit_minor_missing")?;
    deposit.valuation_at.ok_or("valuation_at_missing")?;
    Ok(OutboxEvent {
        id: credited_event_id(deposit.id),
        event_type: "deposit.credited".to_owned(),
        account_id: deposit.account_id,
        livemode: deposit.livemode,
        object: EventObject::Deposit(deposit.id),
        next_attempt_at: Utc::now(),
        actor: crate::db::SYSTEM_ACTOR.to_owned(),
        request: None,
        signing_key_version: None,
    })
}

fn screening_evidence(
    oracle: Address,
    sanctions: SanctionsResult,
    bounds: Bounds,
    pause_scopes: &PauseScopeSources,
) -> serde_json::Value {
    json!({
        "oracle": format!("{oracle:#x}"),
        "block_number": sanctions.block_number,
        "provider_a": sanctions.provider_a,
        "provider_b": sanctions.provider_b,
        "bounds": {
            "min_atomic": bounds.min_atomic,
            "max_atomic": bounds.max_atomic,
        },
        "pause_scopes": {
            "customer": pause_scopes.customer,
            "account": pause_scopes.account,
            "route": pause_scopes.route,
            "treasury": pause_scopes.treasury,
        },
    })
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

fn invariant_result(error: &'static str, block_number: u64) -> StepResult {
    StepResult::new(
        StepOutcome::Retry {
            error: RetryError::InvariantViolation,
        },
        json!({
            "block_number": block_number,
            "error": error,
        }),
    )
}

fn transient_result(error: &'static str, block_number: u64) -> StepResult {
    StepResult::new(
        StepOutcome::Retry {
            error: RetryError::Transient,
        },
        json!({
            "block_number": block_number,
            "error": error,
        }),
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};
    use chrono::Utc;
    use topup_core::deposit::{RejectReason, StepOutcome, WaitReason};
    use topup_core::money::AtomicAmount;
    use topup_core::screening::SanctionsAnswer;
    use uuid::Uuid;

    use super::*;

    struct FixedSanctions(SanctionsResult);

    #[async_trait]
    impl SanctionsSource for FixedSanctions {
        async fn sanctions(&self, _address: Address, _block_number: u64) -> SanctionsResult {
            self.0
        }
    }

    fn amount(value: u64) -> AtomicAmount {
        AtomicAmount::new(U256::from(value))
    }

    fn deposit(amount_atomic: AtomicAmount) -> Deposit {
        let now = Utc::now();
        Deposit {
            id: Uuid::new_v4(),
            chain_id: 1,
            tx_hash: B256::ZERO,
            log_index: 0,
            receipt_log_index: 0,
            tx_from: None,
            tx_nonce: None,
            final_at: None,
            block_number: 123,
            block_hash: B256::ZERO,
            block_time: now,
            address_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            livemode: true,
            customer_id: Uuid::new_v4(),
            route: Some("route".to_owned()),
            route_version: Some(1),
            asset_contract: Address::ZERO,
            from_address: Address::repeat_byte(7),
            amount_atomic,
            state: DepositState::Confirmed,
            reason: None,
            attempt: 0,
            next_attempt_at: now,
            lease_token: None,
            lease_until: None,
            valuation_at: Some(now),
            price_scaled: Some(1),
            price_source: Some("spot".to_owned()),
            credit_minor: None,
            quote: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn route(provider_a: SanctionsAnswer, provider_b: SanctionsAnswer) -> ScreenRoute {
        let mut route: topup_core::route::RouteFile =
            serde_saphyr::from_str(include_str!("../../tests/fixtures/phala-cloud-pha.yaml"))
                .expect("route fixture");
        route.route = "route".to_owned();
        route.version = 1;
        route.screening.sanctions_oracle = Address::repeat_byte(9);
        ScreenRoute::new(
            route,
            Arc::new(FixedSanctions(SanctionsResult {
                block_hash: None,
                provider_a,
                provider_b,
                block_number: 123,
            })),
        )
    }

    fn bounds() -> Bounds {
        Bounds {
            min_atomic: amount(10),
            max_atomic: amount(20),
        }
    }

    fn pauses(customer: &[&str], account: &[&str], route: &[&str]) -> PauseScopeSources {
        let customer = customer.iter().map(ToString::to_string).collect::<Vec<_>>();
        let account = account.iter().map(ToString::to_string).collect::<Vec<_>>();
        let route = route.iter().map(ToString::to_string).collect::<Vec<_>>();
        PauseScopeSources::from_codes(&customer, &account, &route, &[]).expect("valid pause scopes")
    }

    #[tokio::test]
    async fn sanctions_truth_table_maps_to_step_results() {
        use SanctionsAnswer::{Clear, Sanctioned, Unavailable};

        let cases = [
            (
                Sanctioned,
                Sanctioned,
                StepOutcome::Reject(RejectReason::Sanctioned),
            ),
            (Clear, Clear, StepOutcome::Advance),
            (
                Sanctioned,
                Clear,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (
                Clear,
                Sanctioned,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (
                Sanctioned,
                Unavailable,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (
                Unavailable,
                Sanctioned,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (
                Clear,
                Unavailable,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (
                Unavailable,
                Clear,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (
                Unavailable,
                Unavailable,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
        ];

        for (provider_a, provider_b, expected) in cases {
            let deposit = deposit(amount(15));
            let result = route(provider_a, provider_b)
                .evaluate(&deposit, pauses(&[], &[], &[]), bounds())
                .await;
            assert_eq!(result.outcome, expected);
            assert_eq!(result.evidence["block_number"], 123);
            if matches!(expected, StepOutcome::Reject(_)) {
                assert_eq!(result.events.len(), 1);
                assert_eq!(result.events[0].event_type, "deposit.rejected");
                assert_eq!(
                    result.events[0].id,
                    event_id("deposit.rejected", deposit.id)
                );
                assert_eq!(result.events[0].account_id, deposit.account_id);
                assert_eq!(result.events[0].livemode, deposit.livemode);
                assert_eq!(result.events[0].object, EventObject::Deposit(deposit.id));
            } else {
                assert!(result.events.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn bounds_and_pause_outcomes_preserve_evidence_and_event_rules() {
        let out_of_bounds = route(SanctionsAnswer::Clear, SanctionsAnswer::Clear)
            .evaluate(&deposit(amount(21)), pauses(&[], &[], &[]), bounds())
            .await;
        assert_eq!(
            out_of_bounds.outcome,
            StepOutcome::Reject(RejectReason::OutOfBounds)
        );
        assert_eq!(out_of_bounds.events[0].event_type, "deposit.rejected");
        assert_eq!(out_of_bounds.evidence["bounds"]["min_atomic"], "10");
        assert_eq!(out_of_bounds.evidence["bounds"]["max_atomic"], "20");

        let waiting = route(SanctionsAnswer::Clear, SanctionsAnswer::Clear)
            .evaluate(
                &deposit(amount(15)),
                pauses(&["settlement"], &[], &[]),
                bounds(),
            )
            .await;
        assert_eq!(
            waiting.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Paused,
            }
        );
        assert!(waiting.events.is_empty());
        assert_eq!(
            waiting.evidence["pause_scopes"]["customer"][0],
            "settlement"
        );

        let route_paused = route(SanctionsAnswer::Clear, SanctionsAnswer::Clear)
            .evaluate(
                &deposit(amount(15)),
                pauses(&[], &[], &["settlement"]),
                bounds(),
            )
            .await;
        assert_eq!(
            route_paused.outcome,
            StepOutcome::Wait {
                reason: WaitReason::Paused,
            }
        );
        assert_eq!(
            route_paused.evidence["pause_scopes"]["route"][0],
            "settlement"
        );

        let non_settlement_route_pause = route(SanctionsAnswer::Clear, SanctionsAnswer::Clear)
            .evaluate(
                &deposit(amount(15)),
                pauses(&[], &[], &["refunds"]),
                bounds(),
            )
            .await;
        assert_eq!(non_settlement_route_pause.outcome, StepOutcome::Advance);
    }

    #[tokio::test]
    async fn source_cannot_substitute_a_different_evidence_block() {
        let mut route = route(SanctionsAnswer::Clear, SanctionsAnswer::Clear);
        route.sanctions = Arc::new(FixedSanctions(SanctionsResult {
            block_hash: None,
            provider_a: SanctionsAnswer::Clear,
            provider_b: SanctionsAnswer::Clear,
            block_number: 124,
        }));
        let result = route
            .evaluate(&deposit(amount(15)), pauses(&[], &[], &[]), bounds())
            .await;
        assert_eq!(
            result.outcome,
            StepOutcome::Retry {
                error: RetryError::InvariantViolation,
            }
        );
        assert_eq!(result.evidence["error"], "sanctions_block_mismatch");
    }
}
