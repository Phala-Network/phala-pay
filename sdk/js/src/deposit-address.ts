import { requestSignal, trimTrailingSlashes } from "./request.js";
import { CheckoutError, responseError } from "./checkout.js";

/** A payment to a deposit address as the customer's page sees it. Display only. */
export interface DepositAddressPayment {
  /**
   * `seen` (in a block, may still disappear), `confirming` (at the route's confirmation, being
   * valued and screened), `credited`, `rejected` (contact support), or `reversed` (its
   * transaction left the chain before finality: the payment did not happen).
   */
  status: "seen" | "confirming" | "credited" | "rejected" | "reversed";
  chain_id: number;
  /** The token's code, or `null` for a token without a route. */
  asset: string | null;
  decimals: number | null;
  /** Token amount in the smallest unit, as a decimal string. */
  amount_atomic: string;
  tx_hash: string;
  /** Block confirmations while `seen`, otherwise `null`. */
  confirmations: number | null;
  /** Unix seconds. */
  created: number;
}

/** A deposit address on one network, as the customer's page sees it. */
export interface ClientDepositAddressNetwork {
  chain_id: number;
  /** The forwarder address to pay on this chain. */
  address: string;
  /**
   * Typical seconds from payment to credit at the confirmation this chain's payments are credited
   * at: about 30 on Ethereum (two blocks), 7 on an OP-stack chain (three blocks), 300 under a
   * `safe` policy, 900 at finality.
   */
  typical_credit_seconds: number;
}

/**
 * The public view of a deposit address, as `GET /v1/deposit_addresses/{id}?client_secret=…`
 * returns it to a browser: its networks, and its payments in the last 24 hours, newest first.
 */
export interface ClientDepositAddress {
  id: string;
  object: "deposit_address";
  livemode: boolean;
  status: "active" | "retired";
  networks: ClientDepositAddressNetwork[];
  payments: DepositAddressPayment[];
}

export interface RetrieveDepositAddressOptions {
  /** A `client_secret` from your backend's `POST /v1/deposit_addresses` or `…/rotate`. */
  clientSecret: string;
  apiBase: string;
  signal?: AbortSignal;
  /** Request deadline in milliseconds; default 10000. */
  requestTimeout?: number;
  fetch?: typeof globalThis.fetch;
}

const PAYMENT_STATUSES = ["seen", "confirming", "credited", "rejected", "reversed"] as const;

/** Returns the deposit address a client secret belongs to (`da_…_secret_…`), or throws. */
export function depositAddressIdFromClientSecret(clientSecret: string): string {
  const match = /^(da_[0-9a-f]{32})_secret_[0-9a-f]+$/.exec(clientSecret);
  if (match?.[1] === undefined) {
    throw new TypeError("clientSecret is not a deposit address client secret");
  }
  return match[1];
}

/**
 * Reads a deposit address's public view once. Rejects with a `CheckoutError`:
 * `invalid_client_secret` for an unknown address or secret.
 */
export async function retrieveDepositAddress(
  options: RetrieveDepositAddressOptions,
): Promise<ClientDepositAddress> {
  const id = depositAddressIdFromClientSecret(options.clientSecret);
  const base = trimTrailingSlashes(options.apiBase);
  const url = `${base}/v1/deposit_addresses/${id}?client_secret=${encodeURIComponent(options.clientSecret)}`;
  const fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  // A simple GET with no custom headers, so the browser sends no CORS preflight.
  const response = await fetchImpl(url, {
    cache: "no-store",
    credentials: "omit",
    signal: requestSignal(options),
  });
  if (!response.ok) {
    throw responseError(response, "the address or its client secret is unknown");
  }
  try {
    return parseClientDepositAddress(await response.json());
  } catch (cause) {
    throw new CheckoutError("invalid_response", "unexpected response from the payment service", {
      cause,
      status: response.status,
    });
  }
}

/** Validates a response body against the public deposit address view; throws on anything else. */
export function parseClientDepositAddress(value: unknown): ClientDepositAddress {
  const v = record(value);
  const networks = v["networks"];
  const payments = v["payments"];
  if (
    typeof v["id"] !== "string" ||
    v["object"] !== "deposit_address" ||
    typeof v["livemode"] !== "boolean" ||
    (v["status"] !== "active" && v["status"] !== "retired") ||
    !Array.isArray(networks) ||
    !Array.isArray(payments)
  ) {
    throw new TypeError("response does not match the public deposit address view");
  }
  return {
    id: v["id"],
    object: "deposit_address",
    livemode: v["livemode"],
    status: v["status"],
    networks: networks.map(parseNetwork),
    payments: payments.map(parsePayment),
  };
}

function parseNetwork(value: unknown): ClientDepositAddressNetwork {
  const n = record(value);
  const seconds = n["typical_credit_seconds"];
  if (
    !Number.isSafeInteger(n["chain_id"]) ||
    typeof n["address"] !== "string" ||
    !Number.isSafeInteger(seconds) ||
    (seconds as number) < 0
  ) {
    throw new TypeError("network does not match the public deposit address view");
  }
  return {
    chain_id: n["chain_id"] as number,
    address: n["address"],
    typical_credit_seconds: seconds as number,
  };
}

function parsePayment(value: unknown): DepositAddressPayment {
  const p = record(value);
  const status = p["status"];
  const asset = p["asset"];
  const decimals = p["decimals"];
  const confirmations = p["confirmations"];
  if (
    typeof status !== "string" ||
    !(PAYMENT_STATUSES as readonly string[]).includes(status) ||
    !Number.isSafeInteger(p["chain_id"]) ||
    !(asset === null || typeof asset === "string") ||
    !(decimals === null || Number.isSafeInteger(decimals)) ||
    typeof p["amount_atomic"] !== "string" ||
    !/^\d+$/.test(p["amount_atomic"]) ||
    typeof p["tx_hash"] !== "string" ||
    !(confirmations === null || Number.isSafeInteger(confirmations)) ||
    !Number.isSafeInteger(p["created"])
  ) {
    throw new TypeError("payment does not match the public deposit address view");
  }
  return {
    status: status as DepositAddressPayment["status"],
    chain_id: p["chain_id"] as number,
    asset,
    decimals: decimals as number | null,
    amount_atomic: p["amount_atomic"],
    tx_hash: p["tx_hash"],
    confirmations: confirmations as number | null,
    created: p["created"] as number,
  };
}

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError("response is not an object");
  }
  return value as Record<string, unknown>;
}
