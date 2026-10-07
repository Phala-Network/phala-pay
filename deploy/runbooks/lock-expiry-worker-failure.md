# Lock expiry worker failure

**Trigger:** `TopupLockExpiryFailing` (an expiry scan failed; the event carries the error), or the
`topup-lock-expiry` monitor missing its check-ins (no successful scan for five minutes; it checks
in only after a successful scan).

**Impact:** every five seconds the worker expires, in batches of 100, open locks whose chain's
finalized scan has passed `expires_at` and that no in-window payment still awaits; that releases
their exposure and queues `quote.expired`. While it fails, overdue locks keep holding exposure
(new quotes hit `400 exposure_cap_exceeded` early) and merchants are not told checkouts expired. A
failing batch rolls back and the same oldest locks are picked again, so nothing progresses until
the cause is fixed. Payments are unaffected: the confirm step judges them by `expires_at`.

A lock past `expires_at` by wall clock is not overdue until finality, about 15 minutes; a stalled
scanner holds locks open by design and pages as `topup-coverage-scanner-<chain_id>`, not as this alert.

## First steps

1. Read the error in the Sentry event.
2. Check `topup-coverage-scanner-<chain_id>` and, for locks with an in-window payment, a
   `TopupDepositStateAgeExceeded` `state:detected` alert: either holds locks open by design.
3. Watch `exposure_minor` in the daily report across a few minutes.

## Decide

- Scanner stalled or a payment stuck in `detected`: follow [scanner lag](scanner-lag.md) or
  [provider disagreement](provider-disagreement.md); the locks expire once that clears.
- Database connection or timeout errors: the worker retries every tick; restore database health.
- `rate-lock database invariant failed` on every tick: stored lock data breaks an invariant and a
  restart will not help; escalate to Engineering.
- No error but the monitor is silent: the worker task stopped. **HUMAN-ONLY:** restart the CVM
  (`deploy/phala cvms restart "$TOPUP_CVM_ID"`); it resumes from the database.

## Fix

Never close or re-open locks by hand: the caps are enforced against open reserved locks, and a
lock closed outside the worker never emits `quote.expired`. Pausing `quotes` does not stop
consumption, cancellation, or expiry.

## Done when

`topup-lock-expiry` checks in again, the Sentry issue stops receiving events, and `exposure_minor`
falls as overdue locks expire.
