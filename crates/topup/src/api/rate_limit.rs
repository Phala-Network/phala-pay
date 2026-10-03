//! Rate limits of authenticated merchant requests (design §12): per account and mode, 100
//! requests per second live and 25 test, Stripe's global numbers, and a platform-wide test-mode
//! ceiling of 500 per second that keeps test traffic from loading the service.
//!
//! Each limit is a GCRA (the generic cell rate algorithm, the token bucket's equivalent): a
//! request is allowed while its theoretical arrival time is at most one second ahead of now, so a
//! limit of `n` per second admits bursts of `n`. State is held in this process; the service runs
//! one instance.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::tenancy::Scope;
use governor::{DefaultKeyedRateLimiter, Quota};
use std::num::NonZeroU32;

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
    source_limiter: DefaultKeyedRateLimiter<String>,
    source_housekeeping: AtomicUsize,
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
    /// Counts an unauthenticated request by its transport peer.
    pub fn allow_source(&self, source: &str) -> bool {
        if self
            .source_housekeeping
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(1024)
        {
            self.source_limiter.retain_recent();
            self.source_limiter.shrink_to_fit();
        }
        self.source_limiter.check_key(&source.to_owned()).is_ok()
    }
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
            source_limiter: DefaultKeyedRateLimiter::keyed(Quota::per_second(
                NonZeroU32::new(10_000).unwrap(),
            )),
            source_housekeeping: AtomicUsize::new(0),
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
    gcra(current, now, rate, SECOND).ok()
}

/// The GCRA of `count` requests per `period`, admitted in bursts of up to `count`: the next
/// theoretical arrival time after admitting a request at `now`, from `current`, or how long until
/// a request would be admitted.
pub(super) fn gcra(
    current: Option<Instant>,
    now: Instant,
    count: u32,
    period: Duration,
) -> Result<Instant, Duration> {
    let interval = period.checked_div(count).ok_or(period)?;
    let tolerance = period.saturating_sub(interval);
    let arrival = current.map_or(now, |current| current.max(now));
    let ahead = arrival.saturating_duration_since(now);
    if ahead > tolerance {
        return Err(ahead.saturating_sub(tolerance));
    }
    arrival.checked_add(interval).ok_or(period)
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

    #[test]
    fn ingress_budget_is_keyed_by_source() {
        let limiter = ApiRateLimiter::default();
        assert_eq!(
            (0..10_000)
                .filter(|_| limiter.allow_source("198.51.100.1"))
                .count(),
            10_000
        );
        assert!(limiter.allow_source("198.51.100.2"));
        assert!(limiter.allow_source("198.51.100.2"));
    }
}
