import { valid, type Schema } from "./validation.js";
import {
  ApiError,
  ConfigurationError,
  PhalaPayError,
  ResponseValidationError,
  TransportError,
} from "./errors.js";
import { isRecord, parseJson, validateNumbers } from "./json.js";
export interface RequestOptions {
  idempotencyKey?: string;
  signal?: AbortSignal;
  requestDeadlineMs?: number;
  /** Opt in to up to five minutes of retries during maintenance or a gateway/network outage. */
  upgradeTolerance?: boolean;
}
export interface TransportOptions {
  apiBase: string;
  apiKey: string;
  timeoutMs?: number;
  maxAttempts?: number;
  requestDeadlineMs?: number;
  /** Opt in to up to five minutes of retries during maintenance or a gateway/network outage. */
  upgradeTolerance?: boolean;
  fetch?: typeof globalThis.fetch;
}
export function positive(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0)
    throw new ConfigurationError("Timeouts and deadlines must be finite and positive");
  return value;
}
function retryAfter(value: string | null): number | null {
  if (value === null) return null;
  if (/^\d+(?:\.\d+)?$/.test(value)) {
    const seconds = Number(value);
    return Number.isFinite(seconds) ? seconds : null;
  }
  const date = Date.parse(value);
  return Number.isFinite(date) ? Math.max(0, (date - Date.now()) / 1000) : null;
}
function redacted(value: string, key: string): string {
  return value
    .split(key)
    .join("[redacted]")
    .replace(/ppay_(?:sk|rk)_(?:test|live)_[A-Za-z0-9]+/g, "[redacted]")
    .replace(/(?:qt|da)_[A-Za-z0-9]+_secret_[A-Za-z0-9]+/g, "[redacted]");
}
function checkCancellation(signal: AbortSignal): void {
  if (signal.aborted) throw new TransportError("cancelled");
}
function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const abort = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      reject(new TransportError("cancelled"));
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", abort);
      resolve();
    }, ms);
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
  });
}
/** Races injected transports too: implementations need not honor the passed signal. */
function bounded<T>(operation: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const abort = () => {
      signal.removeEventListener("abort", abort);
      reject(new TransportError("timeout"));
    };
    signal.addEventListener("abort", abort, { once: true });
    operation.then(
      (value) => {
        signal.removeEventListener("abort", abort);
        resolve(value);
      },
      () => {
        signal.removeEventListener("abort", abort);
        reject(new TransportError("network"));
      },
    );
    if (signal.aborted) abort();
  });
}
export class Transport {
  readonly #options: TransportOptions;
  readonly #shutdown = new AbortController();
  readonly #timeout: number;
  readonly #deadline: number;
  readonly #attempts: number;
  readonly #upgradeTolerance: boolean;
  readonly #upgradeDeadline: number;
  constructor(options: TransportOptions) {
    this.#options = options;
    if (options.upgradeTolerance !== undefined && typeof options.upgradeTolerance !== "boolean")
      throw new ConfigurationError("upgradeTolerance must be a boolean");
    this.#upgradeTolerance = options.upgradeTolerance ?? false;
    this.#timeout = positive(options.timeoutMs === undefined ? 15000 : options.timeoutMs);
    this.#deadline = positive(
      options.requestDeadlineMs === undefined ? 60000 : options.requestDeadlineMs,
    );
    this.#upgradeDeadline = options.requestDeadlineMs === undefined ? 300000 : this.#deadline;
    this.#attempts = options.maxAttempts === undefined ? 4 : options.maxAttempts;
    if (!Number.isInteger(this.#attempts) || this.#attempts < 1 || this.#attempts > 10)
      throw new ConfigurationError("maxAttempts must be an integer from 1 to 10");
  }
  close(): Promise<void> {
    this.#shutdown.abort();
    return Promise.resolve();
  }
  async request(
    method: string,
    path: string,
    params: unknown,
    options: RequestOptions = {},
    responseSchema?: Schema,
  ): Promise<unknown> {
    if (!isRecord(options) || (method === "GET" && params !== undefined && !isRecord(params)))
      throw new ConfigurationError("Invalid request controls or query parameters");
    if (options.signal !== undefined && !(options.signal instanceof AbortSignal))
      throw new ConfigurationError("Invalid cancellation signal");
    let deadline = positive(
      options.requestDeadlineMs === undefined ? this.#deadline : options.requestDeadlineMs,
    );
    if (options.upgradeTolerance !== undefined && typeof options.upgradeTolerance !== "boolean")
      throw new ConfigurationError("upgradeTolerance must be a boolean");
    const upgradeTolerance = (options.upgradeTolerance ?? this.#upgradeTolerance)
      && (method === "GET" || method === "POST");
    // Explicit deadlines remain hard limits, including during an upgrade.
    const upgradeDeadline = positive(options.requestDeadlineMs ?? this.#upgradeDeadline);
    const started = performance.now();
    const caller = AbortSignal.any([
      this.#shutdown.signal,
      ...(options.signal ? [options.signal] : []),
    ]);
    let body: string | undefined;
    try {
      validateNumbers(params);
      if (method === "POST" && params !== undefined)
        body = JSON.stringify(params, (_key, value: unknown) => {
          if (typeof value === "number") validateNumbers(value);
          return value;
        });
    } catch {
      throw new ConfigurationError("Invalid request parameters");
    }
    const key = method === "POST" ? (options.idempotencyKey ?? crypto.randomUUID()) : undefined;
    if (
      key !== undefined &&
      (typeof key !== "string" ||
        !key.length ||
        key.length > 255 ||
        key !== key.trim() ||
        !/^[\x20-\x7e]+$/.test(key))
    )
      throw new ConfigurationError("Invalid idempotency key");
    const url = new URL(path, this.#options.apiBase);
    if (method === "GET" && isRecord(params)) {
      for (const [name, value] of Object.entries(params)) {
        if (value === undefined || value === null) continue;
        for (const item of Array.isArray(value) ? value : [value]) {
          if (!["string", "number", "boolean"].includes(typeof item))
            throw new ConfigurationError("Invalid query parameter");
          url.searchParams.append(name, String(item));
        }
      }
    }
    let last: PhalaPayError = new TransportError("timeout");
    for (let attempt = 0; ; attempt++) {
      checkCancellation(caller);
      const remaining = deadline - (performance.now() - started);
      if (remaining <= 0) throw last;
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), Math.min(this.#timeout, remaining));
      const signal = AbortSignal.any([caller, controller.signal]);
      let retry: boolean;
      let minimum = 0;
      let replayed = false;
      let upgradeFailure: boolean;
      try {
        const response = await bounded(
          (this.#options.fetch ?? globalThis.fetch)(url, {
            method,
            headers: {
              Authorization: `Bearer ${this.#options.apiKey}`,
              Accept: "application/json",
              ...(method === "POST" ? { "Content-Type": "application/json" } : {}),
              ...(key ? { "Idempotency-Key": key } : {}),
            },
            ...(body === undefined ? {} : { body }),
            redirect: "manual",
            signal,
          }),
          signal,
        );
        const rawId = response.headers.get("request-id") ?? response.headers.get("x-request-id");
        const requestId = rawId === null ? null : redacted(rawId, this.#options.apiKey);
        replayed = response.headers.get("idempotent-replayed")?.toLowerCase() === "true";
        minimum = (retryAfter(response.headers.get("retry-after")) ?? 0) * 1000;
        upgradeFailure = upgradeTolerance && [502, 503, 504].includes(response.status) && !replayed;
        const invalid = () => new ResponseValidationError(undefined, response.status, requestId);
        if ((response.status >= 300 && response.status < 400) || response.redirected) {
          controller.abort();
          throw invalid();
        }
        const text = await bounded(response.text(), signal);
        let data: unknown;
        try {
          data = parseJson(text);
        } catch {
          if (upgradeFailure) throw new TransportError("network");
          throw invalid();
        }
        if (!isRecord(data)) {
          if (upgradeFailure) throw new TransportError("network");
          throw invalid();
        }
        if (response.ok) {
          if (responseSchema && !valid(data, responseSchema)) throw invalid();
          return data;
        }
        const error = data["error"];
        if (
          !isRecord(error) ||
          typeof error["code"] !== "string" ||
          typeof error["message"] !== "string"
        ) {
          if (upgradeFailure) throw new TransportError("network");
          throw invalid();
        }
        const optional = (name: string): string | null => {
          const value = error[name];
          if (value === null || value === undefined) return null;
          if (typeof value !== "string") throw invalid();
          return redacted(value, this.#options.apiKey);
        };
        const delay = retryAfter(response.headers.get("retry-after"));
        last = new ApiError(
          response.status,
          redacted(error["code"], this.#options.apiKey),
          redacted(error["message"], this.#options.apiKey),
          optional("type"),
          optional("param"),
          optional("doc_url"),
          requestId,
          delay,
        );
        minimum = (delay ?? 0) * 1000;
        retry =
          [429, 500, 502, 503, 504].includes(response.status) ||
          (response.status === 409 && error["code"] === "idempotency_key_in_use");
        if (replayed) retry = false;
        upgradeFailure = upgradeFailure || (upgradeTolerance && error["code"] === "service_maintenance" && !replayed);
      } catch (error) {
        checkCancellation(caller);
        if (error instanceof ResponseValidationError || error instanceof ConfigurationError)
          throw error;
        last = controller.signal.aborted
          ? new TransportError("timeout")
          : error instanceof TransportError
            ? error
            : new TransportError("network");
        retry = !replayed;
        upgradeFailure = upgradeTolerance && !replayed;
      } finally {
        clearTimeout(timer);
      }
      if (upgradeFailure) deadline = Math.max(deadline, upgradeDeadline);
      if (!retry || method === "DELETE" || (!upgradeFailure && attempt + 1 >= this.#attempts)) throw last;
      const cap = Math.min(upgradeFailure ? 10000 : 5000, 500 * 2 ** Math.min(attempt, 10));
      const delay = Math.max(minimum, cap * (0.5 + Math.random() * 0.5));
      if (delay >= deadline - (performance.now() - started)) throw last;
      await sleep(delay, caller);
    }
  }
}
