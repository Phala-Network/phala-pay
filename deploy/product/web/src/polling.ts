import type { PaymentRow, Refund, Timeline } from "./api.js";

// Idle accounts need only occasional balance refreshes.
export const ACCOUNT_IDLE_INTERVAL_MS = 15_000;
// Pending payments need prompt account updates without duplicating the active timeline rate.
export const ACCOUNT_PENDING_INTERVAL_MS = 4000;
// Transfers and refunds in flight need prompt settlement updates.
export const TIMELINE_ACTIVE_INTERVAL_MS = 3000;
// Waiting payments and webhook delivery need a modest background refresh rate.
export const TIMELINE_INTERVAL_MS = 10_000;
// Expired quotes can still receive late payments valued at spot.
export const EXPIRED_QUOTE_INTERVAL_MS = 30_000;
// Failed configuration reads should recover without repeatedly hitting an unavailable service.
export const QUERY_RECOVERY_INTERVAL_MS = 15_000;
// The merchant's sweeps list follows finalized transfers at a modest refresh rate.
export const SWEEPS_INTERVAL_MS = 10_000;
// Match the product's five-minute trust/configuration cache.
export const CONFIG_STALE_TIME_MS = 5 * 60_000;
// Two retries recover brief outages without keeping failed views pending indefinitely.
export const QUERY_RETRY_LIMIT = 2;

// Use the product's response types so statuses from a different resource cannot enter a set.
export const TERMINAL_DEPOSIT_STATUSES: ReadonlySet<NonNullable<Timeline["deposit"]>["status"]> = new Set(["credited", "rejected", "reversed"]);
export const TERMINAL_QUOTE_STATUSES: ReadonlySet<NonNullable<Timeline["quote"]>["status"]> = new Set(["complete", "expired", "canceled"]);
export const TERMINAL_REFUND_STATUSES: ReadonlySet<Refund["status"]> = new Set(["succeeded", "failed", "canceled"]);
export const TERMINAL_ACCOUNT_STATUSES: ReadonlySet<PaymentRow["status"]> = new Set([
  ...TERMINAL_DEPOSIT_STATUSES,
  "complete",
  "expired",
  "canceled",
]);
