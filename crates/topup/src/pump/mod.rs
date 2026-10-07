//! Concurrent deposit state-machine pumps and state-age alerts.
//!
//! A step panic is not caught: release builds abort the process, the outstanding lease expires,
//! and another pump re-claims the deposit after the process restarts. Step timeouts remain durable
//! retry outcomes.

mod age;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::time::{Instant, sleep, timeout_at};
use tokio_util::sync::CancellationToken;
use topup_core::deposit::{
    DepositState, RetryError, StepOutcome, TransitionKind, WaitReason, next,
};
use topup_core::retry::backoff;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::db::{
    self, ApplyTransitionError, ApplyTransitionResult, Deposit, OutboxEvent, TransitionUpdate,
};
use crate::jitter::{JitterSource, OsJitter};
use crate::routes::RouteSet;

pub use age::{AgeAlertConfig, AgeAlertConfigError, AgeAlerter};

/// Total step budget, including chain checks and pricing; shorter than the five-minute lease.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(240);

const LEASE_DURATION: Duration = Duration::from_secs(5 * 60);
/// Wait while a provider has not finalized the deposit block yet: about one slot, so a deposit
/// confirms soon after the lagging provider catches up instead of a full wait interval later.
const FINALITY_WAIT_INTERVAL: Duration = Duration::from_secs(12);
/// Wait while a provider has not reached the route's depth or `safe` confirmation: the head
/// poll interval, so a lagging provider delays a fast credit by seconds, not a slot.
const CONFIRMATION_WAIT_INTERVAL: Duration = Duration::from_secs(2);

/// One asynchronous operation for a non-terminal deposit state.
#[async_trait]
pub trait Step: Send + Sync {
    /// Runs the state-specific operation and returns its atomic persistence result.
    async fn run(&self, deposit: &Deposit) -> StepResult;
    /// Runs with the pump's absolute step deadline.
    async fn run_with_deadline(&self, deposit: &Deposit, _deadline: Instant) -> StepResult {
        self.run(deposit).await
    }
}

/// State-machine outcome and the evidence and events committed with it.
#[derive(Clone, Debug, PartialEq)]
pub struct StepResult {
    /// Domain outcome used by [`topup_core::deposit::next`].
    pub outcome: StepOutcome,
    /// Evidence written to the transition timeline.
    pub evidence: Value,
    /// Outbox events committed in the same transaction as the transition.
    pub events: Vec<OutboxEvent>,
    /// Additional database writes committed atomically with the transition.
    pub effects: db::TransitionEffects,
}

impl StepResult {
    /// Creates a result without outbox events.
    #[must_use]
    pub const fn new(outcome: StepOutcome, evidence: Value) -> Self {
        Self {
            outcome,
            evidence,
            events: Vec::new(),
            effects: db::TransitionEffects {
                dual_verified: false,
                canonical_evidence: None,
                valuation: None,
                lock_consumption: None,
                mark_final: false,
                sanctions_hit: false,
            },
        }
    }
}

/// Registry containing exactly one step for every state the pump claims.
///
/// A credited deposit has no step: the finalized scanner and the finality watch mark it `swept`
/// ([`crate::db::commit_factory_logs`]), so the pump never claims it.
pub struct StepSet {
    detected: Box<dyn Step>,
    confirmed: Box<dyn Step>,
}

impl StepSet {
    /// Creates a complete state-to-step registry.
    #[must_use]
    pub fn new(detected: Box<dyn Step>, confirmed: Box<dyn Step>) -> Self {
        Self {
            detected,
            confirmed,
        }
    }

    /// Replaces the step registered for `detected` deposits.
    #[must_use]
    pub fn with_detected(mut self, detected: Box<dyn Step>) -> Self {
        self.detected = detected;
        self
    }

    /// Replaces the step registered for `confirmed` deposits.
    #[must_use]
    pub fn with_confirmed(mut self, confirmed: Box<dyn Step>) -> Self {
        self.confirmed = confirmed;
        self
    }

    fn get(&self, state: DepositState) -> Option<&dyn Step> {
        match state {
            DepositState::Detected => Some(self.detected.as_ref()),
            DepositState::Confirmed => Some(self.confirmed.as_ref()),
            DepositState::Credited
            | DepositState::Swept
            | DepositState::Rejected
            | DepositState::Reversed => None,
        }
    }
}

fn wait_delay(outcome: &StepOutcome, wait_interval: Duration) -> Duration {
    match outcome {
        StepOutcome::Wait {
            reason: WaitReason::Finality,
        } => wait_interval.min(FINALITY_WAIT_INTERVAL),
        StepOutcome::Wait {
            reason: WaitReason::Confirmations,
        } => wait_interval.min(CONFIRMATION_WAIT_INTERVAL),
        _ => wait_interval,
    }
}

/// Runtime timing policy for one pump worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PumpConfig {
    /// Maximum duration of one step, which must remain shorter than the lease.
    pub step_timeout: Duration,
    /// Delay used for an expected wait outcome.
    pub wait_interval: Duration,
    /// Delay before polling again when no due deposit is available.
    pub idle_poll_interval: Duration,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            step_timeout: STEP_TIMEOUT,
            wait_interval: Duration::from_secs(60),
            idle_poll_interval: Duration::from_millis(250),
        }
    }
}

impl PumpConfig {
    fn validate(self) -> Result<Self, PumpConfigError> {
        if self.step_timeout.is_zero() {
            return Err(PumpConfigError::ZeroStepTimeout);
        }
        if self.step_timeout >= LEASE_DURATION {
            return Err(PumpConfigError::TimeoutNotShorterThanLease);
        }
        if self.idle_poll_interval.is_zero() {
            return Err(PumpConfigError::ZeroIdlePollInterval);
        }
        Ok(self)
    }
}

/// Invalid pump timing configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PumpConfigError {
    /// A zero timeout would cancel every step immediately.
    #[error("step timeout must be positive")]
    ZeroStepTimeout,
    /// The step timeout must be strictly shorter than the five-minute lease.
    #[error("step timeout must be shorter than the five-minute lease")]
    TimeoutNotShorterThanLease,
    /// A zero idle interval would create a busy claim loop.
    #[error("idle poll interval must be positive")]
    ZeroIdlePollInterval,
}

/// Result of one claim-and-process attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOnceResult {
    /// No due deposit was claimable.
    Idle,
    /// The step result was persisted.
    Applied {
        /// Deposit whose transition was applied.
        deposit_id: Uuid,
    },
    /// A newer lease owner won and the late result was discarded.
    Stale {
        /// Deposit whose result was discarded.
        deposit_id: Uuid,
    },
    /// A selected single-use rate lock was consumed by another deposit.
    Contended {
        /// Deposit that must be re-run at spot pricing.
        deposit_id: Uuid,
    },
}

/// A worker that claims and advances one durable deposit at a time.
#[derive(Clone)]
pub struct Pump {
    pool: PgPool,
    routes: Arc<RouteSet>,
    steps: Arc<StepSet>,
    config: PumpConfig,
    jitter: Arc<dyn JitterSource>,
}

impl Pump {
    /// Creates a pump with operating-system retry jitter. `routes` render the objects of the
    /// events its steps write.
    pub fn new(
        pool: PgPool,
        routes: Arc<RouteSet>,
        steps: Arc<StepSet>,
        config: PumpConfig,
    ) -> Result<Self, PumpConfigError> {
        Self::with_jitter(pool, routes, steps, config, Arc::new(OsJitter))
    }

    /// Creates a pump with an explicit jitter source.
    pub fn with_jitter(
        pool: PgPool,
        routes: Arc<RouteSet>,
        steps: Arc<StepSet>,
        config: PumpConfig,
        jitter: Arc<dyn JitterSource>,
    ) -> Result<Self, PumpConfigError> {
        Ok(Self {
            pool,
            routes,
            steps,
            config: config.validate()?,
            jitter,
        })
    }

    /// Runs until cancellation, finishing an already claimed step before stopping.
    pub async fn run(&self, cancellation: CancellationToken) {
        self.run_with_instance("0".to_owned(), cancellation).await;
    }

    /// Runs one named worker until cancellation.
    pub async fn run_with_instance(&self, instance: String, cancellation: CancellationToken) {
        let monitor = crate::observability::CronMonitor::pump(&instance);
        loop {
            if cancellation.is_cancelled() {
                return;
            }
            monitor.check_in(true);
            match self.run_once().await {
                Ok(RunOnceResult::Idle) => {
                    tokio::select! {
                        () = cancellation.cancelled() => return,
                        () = sleep(self.config.idle_poll_interval) => {}
                    }
                }
                Ok(
                    RunOnceResult::Applied { .. }
                    | RunOnceResult::Stale { .. }
                    | RunOnceResult::Contended { .. },
                ) => {}
                Err(error) => {
                    tracing::error!(%error, "deposit pump iteration failed");
                    tokio::select! {
                        () = cancellation.cancelled() => return,
                        () = sleep(self.config.idle_poll_interval) => {}
                    }
                }
            }
        }
    }

    /// Claims at most one due deposit, runs one step, and persists one transition.
    pub async fn run_once(&self) -> Result<RunOnceResult, PumpError> {
        let lease_token = Uuid::new_v4();
        let Some(deposit) = db::claim_deposit(&self.pool, lease_token).await? else {
            return Ok(RunOnceResult::Idle);
        };
        let result = if crate::reconciler::chain_is_blocked(&self.pool, deposit.chain_id).await? {
            tracing::warn!(
                deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                chain = deposit.chain_id,
                "deposit step deferred because reconciliation froze the chain"
            );
            StepResult::new(
                StepOutcome::Wait {
                    reason: WaitReason::Paused,
                },
                json!({"outcome": "wait", "reason": "chain_frozen"}),
            )
        } else {
            self.run_step(&deposit).await
        };
        let mut result = result;
        loop {
            match self.persist(&deposit, lease_token, result).await? {
                Persisted::Done(outcome) => return Ok(outcome),
                // Another credit won the cap since the step checked it: the deposit waits for
                // finality instead.
                Persisted::Capped(exposure) => {
                    let credit = deposit.credit_minor.map_or(0, |credit| credit.value());
                    result = unfinalized_cap_wait(exposure, credit);
                }
            }
        }
    }

    /// Validates the step's outcome and persists it in one transaction.
    async fn persist(
        &self,
        deposit: &Deposit,
        lease_token: Uuid,
        result: StepResult,
    ) -> Result<Persisted, PumpError> {
        let deposit_id = deposit.id;
        let result = match next(deposit.state, &result.outcome) {
            Ok(transition) => (result, transition),
            Err(error) => {
                tracing::error!(
                    deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                    state = ?deposit.state,
                    %error,
                    "step returned an invalid outcome"
                );
                let result = StepResult::new(
                    StepOutcome::Retry {
                        error: RetryError::InvariantViolation,
                    },
                    json!({
                        "outcome": "retry",
                        "error": "invalid_step_outcome",
                    }),
                );
                let transition = next(deposit.state, &result.outcome)
                    .map_err(|_| PumpError::MissingStep(deposit.state))?;
                (result, transition)
            }
        };
        let (result, transition) = result;
        let now = Utc::now();
        let attempt = match transition.kind {
            TransitionKind::Advanced => 0,
            TransitionKind::Retry => deposit.attempt.saturating_add(1),
            TransitionKind::Wait | TransitionKind::Rejected | TransitionKind::Reversed => {
                deposit.attempt
            }
        };
        let delay = match transition.kind {
            TransitionKind::Retry => {
                let retry_attempt = u32::try_from(deposit.attempt).unwrap_or(u32::MAX);
                backoff(retry_attempt, self.jitter.next_u64())
            }
            TransitionKind::Wait => wait_delay(&result.outcome, self.config.wait_interval),
            TransitionKind::Advanced | TransitionKind::Rejected | TransitionKind::Reversed => {
                Duration::ZERO
            }
        };
        let chrono_delay =
            chrono::Duration::from_std(delay).map_err(|_| PumpError::ScheduleOutsideChronoRange)?;
        let next_attempt_at = now
            .checked_add_signed(chrono_delay)
            .ok_or(PumpError::ScheduleOutsideChronoRange)?;
        let update = TransitionUpdate {
            transition,
            rejection_reason: match result.outcome {
                StepOutcome::Reject(reason) => Some(reason),
                _ => None,
            },
            attempt,
            next_attempt_at,
        };
        let mut transaction = self.pool.begin().await?;
        let applied = db::apply_transition(
            &mut transaction,
            &self.routes,
            deposit.id,
            deposit.state,
            lease_token,
            update,
            db::TransitionWrites {
                evidence: &result.evidence,
                effects: &result.effects,
                outbox_events: &result.events,
            },
        )
        .await?;

        match applied {
            ApplyTransitionResult::Applied => {
                transaction.commit().await?;
                tracing::info!(
                    deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                    chain_id = deposit.chain_id,
                    state = ?deposit.state,
                    attempt,
                    "deposit step persisted"
                );
                Ok(Persisted::Done(RunOnceResult::Applied { deposit_id }))
            }
            ApplyTransitionResult::Stale => {
                transaction.commit().await?;
                tracing::debug!(
                    deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                    state = ?deposit.state,
                    "discarded stale deposit step result"
                );
                Ok(Persisted::Done(RunOnceResult::Stale { deposit_id }))
            }
            ApplyTransitionResult::LockUnavailable => {
                transaction.rollback().await?;
                db::release_deposit_lease(&self.pool, deposit.id, lease_token).await?;
                tracing::info!(
                    deposit_id = %crate::ids::format(crate::ids::DEPOSIT, deposit.id),
                    "rate lock was consumed concurrently; deposit will retry at spot"
                );
                Ok(Persisted::Done(RunOnceResult::Contended { deposit_id }))
            }
            ApplyTransitionResult::UnfinalizedCreditCapped(exposure) => {
                transaction.rollback().await?;
                Ok(Persisted::Capped(exposure))
            }
        }
    }

    async fn run_step(&self, deposit: &Deposit) -> StepResult {
        let state = deposit.state;
        let Some(step) = self.steps.get(state) else {
            tracing::error!(state = ?state, "no step registered for claimed state");
            return StepResult::new(
                StepOutcome::Retry {
                    error: RetryError::InvariantViolation,
                },
                json!({"outcome": "retry", "error": "missing_step"}),
            );
        };

        let span = crate::observability::deposit_step_span(deposit);
        let deadline = Instant::now() + self.config.step_timeout;
        match timeout_at(deadline, step.run_with_deadline(deposit, deadline))
            .instrument(span)
            .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(state = ?state, "deposit step timed out");
                StepResult::new(
                    StepOutcome::Retry {
                        error: RetryError::Transient,
                    },
                    json!({"outcome": "retry", "error": "step_timeout"}),
                )
            }
        }
    }
}

enum Persisted {
    Done(RunOnceResult),
    /// The transition would credit past the unfinalized-credit cap; nothing was written.
    Capped(db::UnfinalizedCredit),
}

/// The wait of a deposit whose credit, `credit` cents, would take its account's unfinalized
/// credit past the cap: it is credited once final, or once earlier credits are.
pub(crate) fn unfinalized_cap_wait(exposure: db::UnfinalizedCredit, credit: u64) -> StepResult {
    StepResult::new(
        StepOutcome::Wait {
            reason: WaitReason::UnfinalizedCreditCap,
        },
        json!({
            "outcome": "wait",
            "reason": "unfinalized_credit_cap",
            "credit_minor": credit,
            "unfinalized_credit_minor": exposure.credited,
            "max_unfinalized_credit": exposure.cap,
        }),
    )
}

/// Failure while claiming, scheduling, or persisting one pump iteration.
#[derive(Debug, thiserror::Error)]
pub enum PumpError {
    /// PostgreSQL failed outside the transition writer.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
    /// The transition writer rejected or failed the persistence operation.
    #[error("{0}")]
    ApplyTransition(#[from] ApplyTransitionError),
    /// A terminal state was unexpectedly claimed without a registered step.
    #[error("no pump step for state {0:?}")]
    MissingStep(DepositState),
    /// A configured delay could not be represented as a UTC timestamp.
    #[error("next attempt time is outside the supported UTC range")]
    ScheduleOutsideChronoRange,
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use topup_adapters::signer::actor::SignerHandle;
    use topup_core::deposit::{StepOutcome, WaitReason};
    use topup_core::{Signer as _, WebhookKeyId};

    use super::{Deposit, Step, StepResult};

    struct SignerBackedStep {
        signer: SignerHandle,
    }

    #[async_trait]
    impl Step for SignerBackedStep {
        async fn run(&self, _deposit: &Deposit) -> StepResult {
            if let Some(key) = WebhookKeyId::new("acct_a", false, 1) {
                let _ = self.signer.webhook_public_key(&key).await;
            }
            StepResult::new(
                StepOutcome::Wait {
                    reason: WaitReason::Paused,
                },
                json!({"outcome": "wait"}),
            )
        }
    }

    #[test]
    fn finality_wait_retries_after_about_one_slot_and_other_waits_keep_the_interval() {
        let interval = std::time::Duration::from_secs(60);
        let wait = |reason| super::wait_delay(&StepOutcome::Wait { reason }, interval);
        assert_eq!(
            wait(WaitReason::Finality),
            std::time::Duration::from_secs(12)
        );
        assert_eq!(
            wait(WaitReason::Confirmations),
            std::time::Duration::from_secs(2)
        );
        assert_eq!(wait(WaitReason::Paused), interval);
        assert_eq!(
            super::wait_delay(
                &StepOutcome::Wait {
                    reason: WaitReason::Finality
                },
                std::time::Duration::from_secs(5)
            ),
            std::time::Duration::from_secs(5)
        );
    }

    #[test]
    fn signer_handle_satisfies_step_send_bounds() {
        fn assert_step<T: Step>() {}
        assert_step::<SignerBackedStep>();
    }
}
