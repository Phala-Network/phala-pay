//! Process-local authentication protection. Neither source addresses nor key hashes are logged.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::rate_limit::trial_admission;

const CAPACITY: usize = 4096;
const UNKNOWN_TTL: Duration = Duration::from_secs(60);
const FAILURES_PER_MINUTE: u32 = 20;
const REFILL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct AuthLimits {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    unknown: HashMap<[u8; 32], Instant>,
    generation: u64,
    sources: HashMap<IpAddr, Bucket>,
}

struct Bucket {
    next: Instant,
    pending: u32,
}

impl AuthLimits {
    /// Reserve a possible failure before waiting for a database authentication slot.
    pub(super) fn reserve(&self, source: IpAddr) -> Option<FailurePermit<'_>> {
        self.reserve_at(source, Instant::now())
    }

    fn reserve_at(&self, source: IpAddr, now: Instant) -> Option<FailurePermit<'_>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.sources.contains_key(&source) && state.sources.len() >= CAPACITY {
            state.sources.retain(|_, b| b.pending > 0 || b.next > now);
            if state.sources.len() >= CAPACITY {
                return None;
            }
        }
        let previous = state.sources.get(&source);
        let pending = previous.map_or(0, |b| b.pending).checked_add(1)?;
        let next = trial_admission(
            previous.map(|b| b.next),
            now,
            FAILURES_PER_MINUTE,
            Duration::from_secs(60),
        )
        .ok()?;
        state.sources.insert(source, Bucket { next, pending });
        Some(FailurePermit {
            limits: self,
            source,
            failed: false,
        })
    }

    /// A miss returns a ticket; issuance invalidation also prevents stale in-flight insertions.
    pub(super) fn unknown_key(&self, hash: &[u8; 32], now: Instant) -> Result<u64, ()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.unknown.get(hash).is_some_and(|until| *until > now) {
            return Err(());
        }
        state.unknown.remove(hash);
        Ok(state.generation)
    }

    pub(super) fn remember_unknown(&self, hash: [u8; 32], generation: u64, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.generation != generation {
            return;
        }
        if state.unknown.len() >= CAPACITY {
            state.unknown.retain(|_, until| *until > now);
        }
        if state.unknown.len() < CAPACITY {
            state.unknown.insert(hash, now + UNKNOWN_TTL);
        }
    }

    pub(super) fn invalidate(&self, hash: &[u8; 32]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.unknown.remove(hash);
        state.generation = state.generation.wrapping_add(1);
    }
}

/// A canceled request, database error or successful authentication automatically refunds its token.
pub(super) struct FailurePermit<'a> {
    limits: &'a AuthLimits,
    source: IpAddr,
    failed: bool,
}

impl FailurePermit<'_> {
    pub(super) fn fail(mut self) {
        self.failed = true;
    }
}

impl Drop for FailurePermit<'_> {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut state = self
            .limits
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(bucket) = state.sources.get_mut(&self.source) {
            bucket.pending -= 1;
            if !self.failed {
                bucket.next = bucket
                    .next
                    .checked_sub(REFILL)
                    .map_or(now, |next| next.max(now));
            }
            if bucket.pending == 0 && bucket.next <= now {
                state.sources.remove(&self.source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn failures_refill_and_refunded_reservations_do_not_spend_tokens() {
        let limits = AuthLimits::default();
        let source = Ipv4Addr::LOCALHOST.into();
        let now = Instant::now();
        // Success, server failure and cancellation all drop an uncommitted reservation.
        for _ in 0..100 {
            drop(limits.reserve_at(source, now).unwrap());
        }
        let mut pending: Vec<_> = (0..20)
            .map(|_| limits.reserve_at(source, now).unwrap())
            .collect();
        assert!(limits.reserve_at(source, now).is_none());
        pending.pop().unwrap().fail();
        drop(pending); // Refunds must preserve the one committed failure, even out of order.
        for _ in 0..19 {
            limits.reserve_at(source, now).unwrap().fail();
        }
        assert!(limits.reserve_at(source, now).is_none());
        assert!(
            limits
                .reserve_at(Ipv4Addr::new(127, 0, 0, 2).into(), now)
                .is_some()
        );
        assert!(
            limits
                .reserve_at(source, now + REFILL - Duration::from_nanos(1))
                .is_none()
        );
        limits.reserve_at(source, now + REFILL).unwrap().fail();
        assert!(limits.reserve_at(source, now + REFILL).is_none());
    }

    #[test]
    fn cache_ttl_invalidation_and_inflight_misses_are_bounded() {
        let limits = AuthLimits::default();
        let now = Instant::now();
        for index in 0..=CAPACITY {
            let mut hash = [0; 32];
            hash[..8].copy_from_slice(&index.to_le_bytes());
            limits.remember_unknown(hash, 0, now);
        }
        assert_eq!(limits.state.lock().unwrap().unknown.len(), CAPACITY);
        let hash = [0; 32];
        assert!(limits.unknown_key(&hash, now).is_err());
        assert!(limits.unknown_key(&hash, now + UNKNOWN_TTL).is_ok());
        limits.remember_unknown(hash, 0, now);
        limits.invalidate(&hash);
        assert!(limits.unknown_key(&hash, now).is_ok());
        limits.remember_unknown(hash, 0, now); // Query begun before issuance completed late.
        assert!(limits.unknown_key(&hash, now).is_ok());
    }

    #[test]
    fn source_capacity_rejects_new_sources_without_evicting_active_buckets() {
        let limits = AuthLimits::default();
        let now = Instant::now();
        for index in 0..CAPACITY {
            limits
                .reserve_at(Ipv4Addr::from(u32::try_from(index).unwrap()).into(), now)
                .unwrap()
                .fail();
        }
        assert_eq!(limits.state.lock().unwrap().sources.len(), CAPACITY);
        let extra = Ipv4Addr::BROADCAST.into();
        assert!(limits.reserve_at(extra, now).is_none());
        assert!(
            limits
                .reserve_at(extra, now + Duration::from_secs(60))
                .is_some()
        );
        assert!(limits.state.lock().unwrap().sources.len() <= CAPACITY);
    }
}
