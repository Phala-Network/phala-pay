//! Atomic two-level admission using governor's GCRA and transactional in-memory state.
use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use governor::{
    Quota, RateLimiter,
    clock::{Clock, DefaultClock},
    nanos::Nanos,
    state::{NotKeyed, StateStore},
};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep_until};

/// One shared rate/burst limit, independent of RPC method.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BudgetSpec {
    /// Sustained sends per second.
    pub requests_per_second: u32,
    /// Maximum burst.
    pub burst: u32,
    /// Burst capacity kept available for interactive requests (zero disables the reserve).
    #[serde(default)]
    pub interactive_reserve: u32,
}

/// Priority of an RPC send sharing account and credential budgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Priority {
    /// API work may use the reserved capacity.
    Interactive,
    /// Workers preserve the reserved capacity.
    #[default]
    Background,
}

impl Priority {
    /// Reads the current task's RPC priority, defaulting to background outside a scope.
    pub fn current() -> Self {
        RPC_PRIORITY
            .try_with(|priority| *priority)
            .unwrap_or_default()
    }
}

tokio::task_local! {
    pub(super) static RPC_PRIORITY: Priority;
}

/// Runs a future with interactive RPC priority in the current task, including joined futures.
/// Spawned tasks do not inherit this scope.
pub async fn interactive<F: Future>(f: F) -> F::Output {
    RPC_PRIORITY.scope(Priority::Interactive, f).await
}

#[derive(Clone, Default)]
struct State(Arc<Mutex<Option<Nanos>>>);
impl StateStore for State {
    type Key = NotKeyed;
    fn measure_and_replace<T, F, E>(&self, _: &NotKeyed, f: F) -> Result<T, E>
    where
        F: Fn(Option<Nanos>) -> Result<(T, Nanos), E>,
    {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (result, next) = f(*state)?;
        *state = Some(next);
        Ok(result)
    }
}
struct Budget {
    limiter: RateLimiter<NotKeyed, State, DefaultClock>,
    state: State,
    paused: Instant,
    rate: u32,
    reserve: u32,
}

/// Process-wide registry; no limiter can be mutated outside its admission transaction.
pub struct Budgets(Mutex<BTreeMap<String, Budget>>);
impl Budgets {
    /// Builds validated account and credential scopes.
    pub fn new(specs: &BTreeMap<String, BudgetSpec>) -> Result<Self, &'static str> {
        let mut budgets = BTreeMap::new();
        for (id, spec) in specs {
            let rate =
                NonZeroU32::new(spec.requests_per_second).ok_or("RPC rate must be positive")?;
            let burst = NonZeroU32::new(spec.burst).ok_or("RPC burst must be positive")?;
            if spec.interactive_reserve >= spec.burst {
                return Err("RPC interactive reserve must be below burst");
            }
            let state = State::default();
            budgets.insert(
                id.clone(),
                Budget {
                    limiter: RateLimiter::new(
                        Quota::per_second(rate).allow_burst(burst),
                        state.clone(),
                        DefaultClock::default(),
                    ),
                    state,
                    paused: Instant::now(),
                    rate: spec.requests_per_second,
                    reserve: spec.interactive_reserve,
                },
            );
        }
        Ok(Self(Mutex::new(budgets)))
    }
    /// Admits both scopes in one transaction immediately before dispatch. On a denied scope,
    /// rolls back every tentative GCRA update, releases the lock, waits and rechecks both.
    pub async fn admit(
        &self,
        account: &str,
        key: &str,
        deadline: Instant,
        priority: Priority,
    ) -> Result<(), &'static str> {
        loop {
            let wake = {
                let mut budgets = self.0.lock().unwrap_or_else(PoisonError::into_inner);
                let ids = if account == key {
                    vec![account]
                } else {
                    vec![account, key]
                };
                let mut snapshots = Vec::new();
                let mut wake = Instant::now();
                for id in &ids {
                    let b = budgets.get(*id).ok_or("unknown RPC budget")?;
                    snapshots.push((
                        *id,
                        *b.state.0.lock().unwrap_or_else(PoisonError::into_inner),
                    ));
                    wake = wake.max(b.paused);
                }
                if wake <= Instant::now() {
                    let mut denied = false;
                    if priority == Priority::Background {
                        for (id, previous) in &snapshots {
                            let b = budgets.get(*id).ok_or("unknown RPC budget")?;
                            if b.reserve == 0 {
                                continue;
                            }
                            // Construction guarantees reserve + 1 fits within the burst.
                            let cells = NonZeroU32::new(b.reserve.saturating_add(1))
                                .ok_or("invalid RPC interactive reserve")?;
                            let admission = b.limiter.check_n(cells);
                            // This is only a capacity probe, even when check_n succeeds.
                            *b.state.0.lock().unwrap_or_else(PoisonError::into_inner) = *previous;
                            if let Err(until) =
                                admission.map_err(|_| "RPC reserve exceeds burst")?
                            {
                                denied = true;
                                wake = wake.max(
                                    Instant::now()
                                        .checked_add(until.wait_time_from(b.limiter.clock().now()))
                                        .unwrap_or(deadline),
                                );
                            }
                        }
                    }
                    if !denied {
                        for id in &ids {
                            let b = budgets.get(*id).ok_or("unknown RPC budget")?;
                            if let Err(until) = b.limiter.check() {
                                denied = true;
                                wake = wake.max(
                                    Instant::now()
                                        .checked_add(until.wait_time_from(b.limiter.clock().now()))
                                        .unwrap_or(deadline),
                                );
                            }
                        }
                    }
                    if !denied {
                        return Ok(());
                    }
                    for (id, previous) in snapshots {
                        if let Some(b) = budgets.get_mut(id) {
                            *b.state.0.lock().unwrap_or_else(PoisonError::into_inner) = previous;
                        }
                    }
                }
                wake
            };
            if wake >= deadline || Instant::now() >= deadline {
                return Err("RPC admission deadline");
            }
            sleep_until(wake).await;
        }
    }
    /// Conservative admission time for a bounded verification plan, across both scopes.
    pub fn planned_time(
        &self,
        account: &str,
        key: &str,
        sends: u64,
    ) -> Result<Duration, &'static str> {
        let budgets = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let rate = [account, key]
            .iter()
            .map(|id| budgets.get(*id).map(|b| b.rate).ok_or("unknown RPC budget"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min()
            .ok_or("missing RPC rate")?;
        Ok(Duration::from_millis(
            sends.saturating_mul(2000).div_ceil(u64::from(rate)),
        ))
    }
    /// Selection skips explicit quota pauses; ordinary rate admission still waits fairly.
    pub fn paused(&self, account: &str, key: &str) -> bool {
        let budgets = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        [account, key]
            .iter()
            .any(|id| budgets.get(*id).is_none_or(|b| b.paused > Instant::now()))
    }
    /// Conservatively pauses an account on an unknown 429; classified key limits may narrow it.
    pub fn pause(&self, id: &str, duration: Duration) {
        if let Some(b) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            b.paused = b.paused.max(
                Instant::now()
                    .checked_add(duration)
                    .unwrap_or_else(Instant::now),
            );
        }
    }
}
