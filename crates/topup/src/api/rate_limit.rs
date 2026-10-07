//! Rate limits of authenticated merchant requests (design §12): per account and mode, 100
//! requests per second live and 25 test, Stripe's global numbers, and a platform-wide test-mode
//! ceiling of 500 per second that keeps test traffic from loading the service.
//!
//! Each limit is a GCRA (the generic cell rate algorithm, the token bucket's equivalent): a
//! request is allowed while its theoretical arrival time is at most one second ahead of now, so a
//! limit of `n` per second admits bursts of `n`. State is held in this process; the service runs
//! one instance.
//!
//! Governor's persistent keyed limiters commit each successful `check_key` immediately. Two
//! successive checks cannot preserve our contract that a platform refusal consumes no account
//! permit (and vice versa); the API exposes no two-key transaction or rollback. We therefore
//! hold the existing outer state mutex, evaluate isolated governor `StateStore` trials, and
//! commit their returned arrival times only after both quotas admit. Governor supplies the
//! complete GCRA decision, including rejection wait time; the adapter stores state only.
//!
//! Each trial's clock returns the same supplied `Instant` for construction and checking. It is
//! a stack-only clock, without allocations or a second wall-clock read. The stored arrival is
//! translated to nanoseconds relative to that instant; an idle arrival becomes zero, equivalent
//! to governor's `max(tat, now)` refill. `StateStore::measure_and_replace` replaces state only on
//! `Ok`, following governor 0.10.4's documented contract:
//! <https://docs.rs/governor/0.10.4/governor/state/trait.StateStore.html>.
//! Its implementation and quota arithmetic are verified against that version's `state.rs`,
//! `gcra.rs` and `quota.rs` sources. Replenishment periods round down to integer nanoseconds;
//! governor's tolerance is `(burst - 1) * interval`. All configured limits (100/25/500 per second
//! and 120 per minute) divide their periods exactly, preserving their existing refill and burst
//! boundaries. Nondivisible custom limits follow governor's rounding. Zero counts or periods
//! below one nanosecond per permit fail closed before constructing a quota.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::tenancy::Scope;

const SECOND: Duration = Duration::from_secs(1);
/// Tracked scopes beyond which idle ones are dropped; an idle scope is indistinguishable from a
/// new one.
pub(super) const PRUNE_ABOVE: usize = 4_096;

/// The time source of a limiter: [`Instant::now`], or a test's clock.
pub(super) type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// Requests per second of each limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateLimits {
    /// Per live-mode account.
    pub live: u32,
    /// Per test-mode account.
    pub test: u32,
    /// All test-mode requests together.
    pub test_platform: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            live: 100,
            test: 25,
            test_platform: 500,
        }
    }
}

/// The per-account and platform limits of authenticated merchant requests.
pub struct ApiRateLimiter {
    limits: RateLimits,
    clock: Clock,
    state: Mutex<State>,
}

impl std::fmt::Debug for ApiRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiRateLimiter")
            .field("limits", &self.limits)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct State {
    /// Theoretical arrival time of each scope's next request.
    scopes: HashMap<Scope, Instant>,
    test_platform: Option<Instant>,
}

impl Default for ApiRateLimiter {
    fn default() -> Self {
        Self::new(RateLimits::default())
    }
}

impl ApiRateLimiter {
    /// A limiter enforcing `limits`.
    #[must_use]
    pub fn new(limits: RateLimits) -> Self {
        Self::with_clock(limits, Instant::now)
    }

    /// A limiter enforcing `limits` at the instants `clock` reads, so a test decides how much
    /// time passes between its requests instead of the machine it runs on.
    #[must_use]
    pub fn with_clock(
        limits: RateLimits,
        clock: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Self {
        Self {
            limits,
            clock: Box::new(clock),
            state: Mutex::default(),
        }
    }

    /// Counts one request of `scope`; `false` means it is over a limit and must be refused with
    /// `429`. A refused request counts against no limit.
    pub fn allow(&self, scope: Scope) -> bool {
        self.allow_at((self.clock)(), scope)
    }

    fn allow_at(&self, now: Instant, scope: Scope) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rate = if scope.livemode() {
            self.limits.live
        } else {
            self.limits.test
        };
        let Some(scope_next) = next_arrival(state.scopes.get(&scope).copied(), now, rate) else {
            return false;
        };
        let platform_next = if scope.livemode() {
            None
        } else {
            let Some(next) = next_arrival(state.test_platform, now, self.limits.test_platform)
            else {
                return false;
            };
            Some(next)
        };
        if state.scopes.len() >= PRUNE_ABOVE {
            state.scopes.retain(|_, next| *next > now);
        }
        state.scopes.insert(scope, scope_next);
        if platform_next.is_some() {
            state.test_platform = platform_next;
        }
        true
    }
}

/// The next theoretical arrival time after admitting a request at `now`, or `None` when the
/// request is over `rate` per second.
fn next_arrival(current: Option<Instant>, now: Instant, rate: u32) -> Option<Instant> {
    trial_admission(current, now, rate, SECOND).ok()
}

/// Governor admission of `count` requests per `period`, in bursts of up to `count`: the next
/// theoretical arrival time after admitting a request at `now`, from `current`, or how long until
/// a request would be admitted.
pub(super) fn trial_admission(
    current: Option<Instant>,
    now: Instant,
    count: u32,
    period: Duration,
) -> Result<Instant, Duration> {
    use governor::{
        Quota, RateLimiter,
        clock::Clock as GovernorClock,
        nanos::Nanos,
        state::{NotKeyed, StateStore},
    };
    use std::num::NonZeroU32;

    // A trial state is committed by the caller only once every applicable quota admits.
    // Governor owns the algorithm; this store only adapts its state to our joint admission.
    struct Trial(Mutex<Option<Nanos>>);
    impl StateStore for Trial {
        type Key = NotKeyed;
        fn measure_and_replace<T, F, E>(&self, _: &NotKeyed, f: F) -> Result<T, E>
        where
            F: Fn(Option<Nanos>) -> Result<(T, Nanos), E>,
        {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (result, next) = f(*state)?;
            *state = Some(next);
            Ok(result)
        }
    }
    let count = NonZeroU32::new(count).ok_or(period)?;
    let interval = period
        .checked_div(count.get())
        .filter(|value| !value.is_zero())
        .ok_or(period)?;
    let quota = Quota::with_period(interval)
        .ok_or(period)?
        .allow_burst(count);
    let trial = Trial(Mutex::new(
        current.map(|value| value.saturating_duration_since(now).into()),
    ));
    struct DecisionClock(Instant);
    impl GovernorClock for DecisionClock {
        type Instant = Instant;
        fn now(&self) -> Instant {
            self.0
        }
    }
    let limiter =
        RateLimiter::<NotKeyed, Trial, DecisionClock>::new(quota, trial, DecisionClock(now));
    limiter
        .check()
        .map_err(|denied| denied.wait_time_from(now))?;
    let next = limiter
        .into_state_store()
        .0
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ok_or(period)?;
    now.checked_add(next.into()).ok_or(period)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn admitted(limiter: &ApiRateLimiter, now: Instant, scope: Scope, requests: u32) -> u32 {
        (0..requests)
            .filter(|_| limiter.allow_at(now, scope))
            .count()
            .try_into()
            .unwrap()
    }

    #[test]
    fn trials_match_persistent_governor_for_every_production_quota() {
        use governor::{Quota, RateLimiter};
        use std::{num::NonZeroU32, sync::Arc};
        struct TestClock(Arc<Mutex<Instant>>);
        impl governor::clock::Clock for TestClock {
            type Instant = Instant;
            fn now(&self) -> Instant {
                *self.0.lock().unwrap()
            }
        }
        for (count, period, quota) in [
            (
                100,
                SECOND,
                Quota::per_second(NonZeroU32::new(100).unwrap()),
            ),
            (25, SECOND, Quota::per_second(NonZeroU32::new(25).unwrap())),
            (
                500,
                SECOND,
                Quota::per_second(NonZeroU32::new(500).unwrap()),
            ),
            (
                120,
                Duration::from_secs(60),
                Quota::per_minute(NonZeroU32::new(120).unwrap()),
            ),
        ] {
            let start = Instant::now();
            let clock = Arc::new(Mutex::new(start));
            let persistent = RateLimiter::direct_with_clock(quota, TestClock(clock.clone()));
            let mut arrival = None;
            for offset in [
                Duration::ZERO,
                Duration::from_millis(1),
                Duration::from_millis(10),
                Duration::from_millis(500),
                SECOND,
                Duration::from_secs(120),
            ] {
                let now = start + offset;
                *clock.lock().unwrap() = now;
                for _ in 0..=count {
                    let trial = trial_admission(arrival, now, count, period);
                    let direct = persistent
                        .check()
                        .map_err(|denied| denied.wait_time_from(now));
                    assert_eq!(trial.is_ok(), direct.is_ok());
                    match (trial, direct) {
                        (Ok(next), Ok(())) => arrival = Some(next),
                        (Err(trial_wait), Err(direct_wait)) => assert_eq!(trial_wait, direct_wait),
                        _ => panic!("trial and persistent governor differ"),
                    }
                }
            }
        }
    }

    #[test]
    fn nondivisible_period_uses_governor_nanosecond_rounding_and_short_periods_deny() {
        let limiter = ApiRateLimiter::new(RateLimits {
            live: 3,
            test: 3,
            test_platform: 3,
        });
        let start = Instant::now();
        let scope = Scope::new(Uuid::from_u128(1), true);
        assert_eq!(admitted(&limiter, start, scope, 4), 3);
        assert!(!limiter.allow_at(start + Duration::from_nanos(333_333_332), scope));
        assert!(limiter.allow_at(start + Duration::from_nanos(333_333_333), scope));
        assert!(trial_admission(None, start, 2, Duration::from_nanos(1)).is_err());
        assert!(trial_admission(None, start, 1, Duration::ZERO).is_err());
    }

    #[test]
    fn each_account_and_mode_has_its_own_limit_per_second() {
        let limiter = ApiRateLimiter::default();
        let start = Instant::now();
        let account = Uuid::from_u128(1);
        let live = Scope::new(account, true);
        let test = Scope::new(account, false);

        assert_eq!(admitted(&limiter, start, live, 150), 100);
        assert_eq!(admitted(&limiter, start, test, 50), 25);
        // Another account is not affected.
        assert_eq!(
            admitted(&limiter, start, Scope::new(Uuid::from_u128(2), false), 30),
            25
        );
        // The limit refills at its rate: a tenth of a second admits a tenth of it.
        assert_eq!(
            admitted(&limiter, start + Duration::from_millis(100), live, 50),
            10
        );
        assert_eq!(admitted(&limiter, start + SECOND * 2, live, 150), 100);
    }

    #[test]
    fn refusals_charge_neither_scope_nor_platform() {
        let limiter = ApiRateLimiter::new(RateLimits {
            live: 2,
            test: 2,
            test_platform: 3,
        });
        let now = Instant::now();
        let a = Scope::new(Uuid::from_u128(1), false);
        let b = Scope::new(Uuid::from_u128(2), false);
        assert_eq!(admitted(&limiter, now, a, 10), 2);
        // The rejected account requests did not consume the remaining platform permit.
        assert!(limiter.allow_at(now, b));
        assert!(!limiter.allow_at(now, b));
        // A platform rejection did not consume b's remaining permit.
        assert!(limiter.allow_at(now + Duration::from_millis(334), b));
        assert!(!limiter.allow_at(now + Duration::from_millis(334), b));
    }

    #[test]
    fn zero_limits_deny_without_panicking() {
        let limiter = ApiRateLimiter::new(RateLimits {
            live: 0,
            test: 1,
            test_platform: 0,
        });
        assert!(!limiter.allow(Scope::new(Uuid::from_u128(1), true)));
        assert!(!limiter.allow(Scope::new(Uuid::from_u128(1), false)));
    }

    #[test]
    fn test_mode_shares_a_platform_ceiling_that_live_mode_does_not() {
        let limiter = ApiRateLimiter::new(RateLimits {
            live: 10,
            test: 10,
            test_platform: 25,
        });
        let now = Instant::now();
        let admitted_test: u32 = (0..5)
            .map(|index| admitted(&limiter, now, Scope::new(Uuid::from_u128(index), false), 10))
            .sum();
        assert_eq!(admitted_test, 25);
        assert_eq!(
            admitted(&limiter, now, Scope::new(Uuid::from_u128(9), true), 10),
            10
        );
    }
}
