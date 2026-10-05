import { requestSignal, trimTrailingSlashes } from "./request.js";
import { getAddress, isAddress, isAddressEqual } from "viem";
import { parseClientQuote, quoteIdFromClientSecret, type ClientQuote } from "./quote.js";

/**
 * What the payer sees:
 * - `loading` until the quote is first read;
 * - `waiting` for a payment, until `expires_at`;
 * - `seen` once a transfer is in a block, `confirming` once it reaches the route's confirmation
 *   (two blocks on Ethereum) and is being valued;
 * - `credited` (typically the quote's `typical_credit_seconds` after paying), or `rejected` for a
 *   payment that will not be credited;
 * - `reversed` when a credited payment's transaction left the chain before finality;
 * - `expired` or `canceled` without a payment;
 * - `error` when the client secret is not valid, or the quote's address is not the one your
 *   backend expects (`address_mismatch`): nothing is shown to pay.
 */
export type CheckoutStatus =
  | "loading"
  | "waiting"
  | "seen"
  | "confirming"
  | "credited"
  | "rejected"
  | "reversed"
  | "expired"
  | "canceled"
  | "error";

export type CheckoutErrorCode =
  | "invalid_client_secret"
  | "address_mismatch"
  | "rate_limited"
  | "network_error"
  | "service_unavailable"
  | "invalid_response"
  | "api_error";

export interface CheckoutErrorOptions extends ErrorOptions {
  /** The failed response's `Request-Id`, `req_…`, to quote to support. */
  requestId?: string;
  /** For `rate_limited`: the seconds the service asked to wait (`Retry-After`). */
  retryAfter?: number;
}

export class CheckoutError extends Error {
  override readonly name = "CheckoutError";
  /** The failed response's `Request-Id`, when the service answered. */
  readonly requestId: string | undefined;
  /** For `rate_limited`: the seconds to wait before the next read. */
  readonly retryAfter: number | undefined;

  constructor(
    readonly code: CheckoutErrorCode,
    message: string,
    options?: CheckoutErrorOptions,
  ) {
    super(message, options);
    this.requestId = options?.requestId;
    this.retryAfter = options?.retryAfter;
  }
}

// Response classification stays internal; consumers keep the existing CheckoutError contract.
const nonRetryableClientErrors = new WeakSet<CheckoutError>();

/**
 * The error of a failed read of a public view, with the response's `Request-Id` and, on `429`,
 * its `Retry-After`.
 */
export function responseError(response: Response, notFound: string): CheckoutError {
  const header = response.headers.get("request-id");
  const requestId = header === null ? {} : { requestId: header };
  if (response.status === 404) {
    return new CheckoutError("invalid_client_secret", notFound, requestId);
  }
  if (response.status >= 500) {
    const seconds = Number(response.headers.get("retry-after"));
    return new CheckoutError("service_unavailable", "the payment service is temporarily unavailable", {
      ...requestId,
      ...(Number.isFinite(seconds) && seconds > 0 ? { retryAfter: seconds } : {}),
    });
  }
  if (response.status === 429) {
    const seconds = Number(response.headers.get("retry-after"));
    return new CheckoutError("rate_limited", "too many status requests", {
      ...requestId,
      ...(Number.isFinite(seconds) && seconds > 0 ? { retryAfter: seconds } : {}),
    });
  }
  const error = new CheckoutError(
    "api_error",
    `the payment service answered ${response.status}`,
    requestId,
  );
  if (response.status >= 400 && response.status < 500 && response.status !== 408) {
    nonRetryableClientErrors.add(error);
  }
  return error;
}

/**
 * The next read's delay with uniform ±20% jitter, at least what `error` asks via Retry-After.
 * `random` defaults to Math.random and can be injected for deterministic tests.
 */
export function pollDelay(
  interval: number,
  failures: number,
  error: CheckoutError | null,
  random: () => number = Math.random,
): number {
  const backoff = failures === 0 ? interval : Math.min(interval * 2 ** failures, MAX_BACKOFF);
  const retryAfter = (error?.retryAfter ?? 0) * 1000;
  return Math.max(Math.max(backoff, retryAfter) * (0.8 + 0.4 * random()), retryAfter);
}

export interface CheckoutState {
  status: CheckoutStatus;
  /** The last known view is retained while polling reconnects after a temporary outage. */
  reconnecting?: boolean;
  quote: ClientQuote | null;
  /** Permanent/validation failure details; temporary outages use `reconnecting` instead. */
  error: CheckoutError | null;
}

export interface CheckoutOptions {
  /** The quote's `client_secret`, from your backend's `POST /v1/quotes`. */
  clientSecret: string;
  /**
   * The quote's address as your backend's SDK recomputed it from the pinned forwarder. The
   * checkout fails closed, showing nothing to pay, when the quote read from the service names
   * another address.
   */
  expectedAddress: string;
  /** The service origin, for example `https://topup.example.com`. */
  apiBase: string;
  /** Milliseconds between status reads; default 3000. */
  pollInterval?: number;
  /** Cancels this request/session together with its own deadline. */
  signal?: AbortSignal;
  /** Request deadline in milliseconds; default 10000. */
  requestTimeout?: number;
  fetch?: typeof globalThis.fetch;
  /** Current time in milliseconds; for tests. */
  now?: () => number;
}

export interface CheckoutSession {
  getState(): CheckoutState;
  /** Calls `listener` on every state change; returns the unsubscribe function. */
  subscribe(listener: (state: CheckoutState) => void): () => void;
  /** Reads the quote now instead of at the next poll. */
  refresh(): Promise<void>;
  /** Stops polling and aborts the active request. */
  destroy(): void;
}

const DEFAULT_POLL_INTERVAL = 3000;
const MAX_BACKOFF = 30_000;

export interface RetrieveQuoteOptions {
  clientSecret: string;
  /** The address your backend recomputed; a quote naming another one is refused. */
  expectedAddress: string;
  apiBase: string;
  /** Cancels this request/session together with its own deadline. */
  signal?: AbortSignal;
  /** Request deadline in milliseconds; default 10000. */
  requestTimeout?: number;
  fetch?: typeof globalThis.fetch;
}

/**
 * Reads a quote's public view once, as Stripe.js's `retrievePaymentIntent(clientSecret)` does.
 * Rejects with a `CheckoutError`: `invalid_client_secret` for an unknown quote or secret,
 * `address_mismatch` for a quote whose address is not `expectedAddress`.
 */
export async function retrieveQuote(options: RetrieveQuoteOptions): Promise<ClientQuote> {
  const quoteId = quoteIdFromClientSecret(options.clientSecret);
  const expected = expectedAddress(options.expectedAddress);
  const base = trimTrailingSlashes(options.apiBase);
  const url = `${base}/v1/quotes/${quoteId}?client_secret=${encodeURIComponent(options.clientSecret)}`;
  const fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  // A simple GET with no custom headers, so the browser sends no CORS preflight.
  const response = await fetchImpl(url, {
    cache: "no-store",
    credentials: "omit",
    signal: requestSignal(options),
  });
  if (!response.ok) {
    throw responseError(response, "the quote or its client secret is unknown");
  }
  let quote: ClientQuote;
  try {
    quote = parseClientQuote(await response.json());
  } catch (cause) {
    throw new CheckoutError("invalid_response", "unexpected response from the payment service", {
      cause,
    });
  }
  if (!isAddressEqual(getAddress(quote.address), expected)) {
    throw new CheckoutError(
      "address_mismatch",
      "the quote's address is not the one the merchant expects",
    );
  }
  return quote;
}

function expectedAddress(value: string): `0x${string}` {
  if (!isAddress(value, { strict: false })) {
    throw new TypeError("expectedAddress is not an address");
  }
  return getAddress(value);
}

/** The payer-facing status of a quote at `nowSeconds`. */
export function checkoutStatus(quote: ClientQuote, nowSeconds: number): CheckoutStatus {
  if (quote.payment_status !== "none") {
    return quote.payment_status;
  }
  if (quote.status === "canceled") {
    return "canceled";
  }
  if (quote.status === "expired" || nowSeconds >= quote.expires_at) {
    return "expired";
  }
  return quote.status === "open" ? "waiting" : "confirming";
}

/**
 * Follows a quote from the browser by polling its public view until it is credited, rejected,
 * canceled, or expired by the service. The status turns `expired` at `expires_at` even while the
 * service, which expires quotes by chain time, still reports the quote open; polling continues
 * until then, so a payment sent just before expiry still shows.
 */
export function createCheckout(options: CheckoutOptions): CheckoutSession {
  quoteIdFromClientSecret(options.clientSecret);
  expectedAddress(options.expectedAddress);
  const controller = new AbortController();
  const signal =
    options.signal === undefined
      ? controller.signal
      : AbortSignal.any([controller.signal, options.signal]);
  const now = options.now ?? Date.now;
  const interval = options.pollInterval ?? DEFAULT_POLL_INTERVAL;
  const listeners = new Set<(state: CheckoutState) => void>();
  const visibilityDocument = typeof document === "undefined" ? undefined : document;

  let state: CheckoutState = { status: "loading", quote: null, error: null };
  let timer: ReturnType<typeof setTimeout> | undefined;
  let failures = 0;
  let clientFailures = 0;
  let lastFailure: CheckoutError | null = null;
  let destroyed = false;
  let inFlight: Promise<void> | undefined;

  function setState(next: CheckoutState): void {
    if (
      next.status === state.status &&
      next.reconnecting === state.reconnecting &&
      next.error === state.error &&
      JSON.stringify(next.quote) === JSON.stringify(state.quote)
    ) {
      return;
    }
    state = next;
    for (const listener of listeners) {
      listener(state);
    }
  }

  function finished(): boolean {
    const { status, quote } = state;
    return (
      destroyed ||
      signal.aborted ||
      status === "error" ||
      status === "credited" ||
      status === "rejected" ||
      status === "reversed" ||
      status === "canceled" ||
      (status === "expired" && quote?.status === "expired")
    );
  }

  async function load(): Promise<void> {
    try {
      const quote = await retrieveQuote({ ...options, signal });
      if (destroyed || signal.aborted) {
        return;
      }
      failures = 0;
      clientFailures = 0;
      lastFailure = null;
      setState({ status: checkoutStatus(quote, now() / 1000), quote, error: null });
    } catch (cause) {
      if (destroyed || signal.aborted) {
        return;
      }
      const error =
        cause instanceof CheckoutError
          ? cause
          : new CheckoutError("network_error", "could not reach the payment service", { cause });
      failures += 1;
      clientFailures = nonRetryableClientErrors.has(error) ? clientFailures + 1 : 0;
      lastFailure = error;
      const quote = state.quote;
      if (error.code === "address_mismatch") {
        // Fail closed: forget the quote, so nothing about its address is shown.
        setState({ status: "error", quote: null, error });
      } else if (error.code === "invalid_client_secret") {
        setState({ status: "error", quote, error });
      } else if (clientFailures >= 3) {
        setState({ status: "error", quote, error });
      } else if (error.code === "network_error" || error.code === "service_unavailable") {
        // Do not infer expiry or discard the payer's view while chain progress is unknown.
        setState({ status: state.status, quote, error: null, reconnecting: true });
      } else {
        setState({
          status: quote === null ? state.status : checkoutStatus(quote, now() / 1000),
          quote,
          error,
        });
      }
    }
  }

  function refresh(): Promise<void> {
    if (destroyed || signal.aborted) {
      return Promise.resolve();
    }
    inFlight ??= load().finally(() => {
      inFlight = undefined;
    });
    return inFlight;
  }

  function schedule(): void {
    clearTimeout(timer);
    if (finished() || visibilityDocument?.visibilityState === "hidden") {
      return;
    }
    const delay = pollDelay(interval, failures, lastFailure);
    timer = setTimeout(() => {
      if (!finished() && visibilityDocument?.visibilityState !== "hidden") {
        void refresh().then(schedule);
      }
    }, delay);
  }

  function visibilityChanged(): void {
    clearTimeout(timer);
    if (visibilityDocument?.visibilityState !== "hidden" && !finished()) {
      void refresh().then(schedule);
    }
  }

  function stopPolling(): void {
    clearTimeout(timer);
    visibilityDocument?.removeEventListener("visibilitychange", visibilityChanged);
  }

  visibilityDocument?.addEventListener("visibilitychange", visibilityChanged);
  signal.addEventListener("abort", stopPolling, { once: true });
  if (visibilityDocument?.visibilityState !== "hidden") {
    void refresh().then(schedule);
  }

  return {
    getState: () => state,
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    refresh,
    destroy() {
      destroyed = true;
      controller.abort();
      stopPolling();
      listeners.clear();
    },
  };
}
