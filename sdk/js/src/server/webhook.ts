import { parseJson } from "./json.js";
/**
 * Standard Webhooks `v1a` verification of Phala Pay deliveries (design D11): an ed25519 signature
 * over `{webhook-id}.{webhook-timestamp}.{body}` by your account's webhook key in the endpoint's
 * mode, pinned from `GET /v1/attestation`. Uses WebCrypto, so it runs on Node 20+, Deno, Bun, and
 * edge runtimes.
 */

/** A delivery that did not verify: answer `400` and do nothing. */
export { SignatureVerificationError as WebhookSignatureError } from "./errors.js";
import { SignatureVerificationError as WebhookSignatureError } from "./errors.js";

/** A verified event. `data.object` is the object the event is about, such as a deposit. */
export interface WebhookEvent {
  /** `evt_…`, stable across retries and resends: process each id once. */
  id: string;
  object: "event";
  /** Your account, `acct_…`. */
  account: string;
  livemode: boolean;
  /** Such as `deposit.credited`; claw back a `deposit.reversed` deposit's credit. */
  type: string;
  created: number;
  /** Who caused it: an API key id (`key_…`), `admin`, or `system`. */
  actor: string;
  /**
   * The API request that caused it, with the `Idempotency-Key` it sent; `null` when the
   * service's own workers did (a payment credited, a quote expired).
   */
  request: { id: string; idempotency_key: string | null } | null;
  /**
   * `object` is the object as it was when the event happened, never re-rendered; `*.updated`
   * events add `previous_attributes`, the former values of what changed.
   */
  data: { object: Record<string, unknown>; previous_attributes?: Record<string, unknown> };
}

export interface ConstructEventOptions {
  /** Your account id, `acct_…`: an event of another account fails closed. */
  expectedAccount: string;
  /** The mode of the endpoint receiving it: an event of the other mode fails closed. */
  expectedLivemode: boolean;
  /** Finite, non-negative seconds a delivery's timestamp may differ from now; default 300. */
  tolerance?: number;
  /** Finite current time in Unix seconds; for tests. */
  now?: number;
}

/**
 * Verifies a delivery and returns its event, as Stripe's `constructEvent` does, failing closed
 * unless a `v1a` signature verifies with one of `publicKeys` (ed25519 keys in Standard Webhooks'
 * `whpk_` form, as `GET /v1/attestation` lists them; pass both while a rotation overlaps), the
 * timestamp is within tolerance, the body's id is the `webhook-id`, and the event is
 * `expectedAccount`'s in `expectedLivemode`.
 *
 * `payload` is the raw request body, before any JSON parsing.
 */
export async function constructEvent(
  payload: string | Uint8Array,
  headers: Headers | Record<string, string | string[] | undefined>,
  publicKeys: string | readonly string[],
  options: ConstructEventOptions,
): Promise<WebhookEvent> {
  if (options.expectedAccount === "") {
    throw new TypeError("expectedAccount is required");
  }
  const now = options.now ?? Math.floor(Date.now() / 1000);
  const tolerance = options.tolerance ?? 300;
  if (!Number.isFinite(now) || !Number.isFinite(tolerance) || tolerance < 0) {
    throw new TypeError("webhook now must be finite and tolerance finite and non-negative");
  }
  const body = typeof payload === "string" ? new TextEncoder().encode(payload) : payload;
  if (
    typeof payload === "string" &&
    new TextDecoder("utf-8", { fatal: true }).decode(body) !== payload
  )
    throw new WebhookSignatureError("webhook body is not exact UTF-8");
  const id = header(headers, "webhook-id");
  const timestamp = header(headers, "webhook-timestamp");
  const signatures = header(headers, "webhook-signature");
  if (id === undefined || timestamp === undefined || signatures === undefined) {
    throw new WebhookSignatureError("webhook headers missing");
  }
  if (!/^\d+$/.test(timestamp) || !Number.isSafeInteger(Number(timestamp))) {
    throw new WebhookSignatureError("webhook timestamp malformed");
  }
  if (Math.abs(now - Number(timestamp)) > tolerance) {
    throw new WebhookSignatureError("webhook timestamp outside tolerance");
  }
  const signed = concat(new TextEncoder().encode(`${id}.${timestamp}.`), body);
  const keys = await Promise.all(
    (typeof publicKeys === "string" ? [publicKeys] : publicKeys).map(importKey),
  );
  if (keys.length === 0) {
    throw new TypeError("no webhook public key pinned");
  }
  let verified = false;
  for (const entry of signatures.split(" ")) {
    if (entry.split(",").length !== 2)
      throw new WebhookSignatureError("duplicate or malformed webhook signature");
    const [version, encoded] = entry.split(",", 2);
    const signature = version === "v1a" && encoded !== undefined ? base64(encoded) : undefined;
    if (signature === undefined) {
      continue;
    }
    for (const key of keys) {
      if (await crypto.subtle.verify("Ed25519", key, signature, signed)) {
        verified = true;
      }
    }
  }
  if (!verified) {
    throw new WebhookSignatureError("no valid webhook signature");
  }
  let event: unknown;
  try {
    event = parseJson(new TextDecoder("utf-8", { fatal: true }).decode(body));
  } catch {
    throw new TypeError("webhook body is not an event");
  }
  if (!isRecord(event)) {
    throw new TypeError("webhook body is not an event");
  }
  const e = event;
  const data = e["data"];
  const object: unknown = isRecord(data) ? data["object"] : null;
  const { id: eventId, type, account, livemode, created, actor, request } = e;
  if (
    e["object"] !== "event" ||
    typeof eventId !== "string" ||
    typeof type !== "string" ||
    typeof account !== "string" ||
    typeof livemode !== "boolean" ||
    typeof created !== "number" ||
    !Number.isSafeInteger(created) ||
    !isRecord(object) ||
    typeof actor !== "string" ||
    !(request === null || isRequest(request))
  ) {
    throw new TypeError("webhook body is not an event");
  }
  if (eventId !== id) {
    throw new WebhookSignatureError("webhook id does not match the event");
  }
  if (account !== options.expectedAccount) {
    throw new WebhookSignatureError("webhook event is for another account");
  }
  if (livemode !== options.expectedLivemode) {
    throw new WebhookSignatureError("webhook event is for the other mode");
  }
  const previous = isRecord(data) ? data["previous_attributes"] : undefined;
  return {
    ...e,
    id: eventId,
    object: "event",
    account,
    livemode,
    type,
    created,
    actor,
    request,
    data: {
      ...(isRecord(data)
        ? Object.fromEntries(Object.entries(data).filter(([key]) => key !== "previous_attributes"))
        : {}),
      object,
      ...(isRecord(previous) ? { previous_attributes: previous } : {}),
    },
  };
}

function header(
  headers: Headers | Record<string, string | string[] | undefined>,
  name: string,
): string | undefined {
  if (headers instanceof Headers) {
    return headers.get(name)?.trim() ?? undefined;
  }
  const matches = Object.entries(headers).filter(([key]) => key.toLowerCase() === name);
  if (matches.length > 1) throw new WebhookSignatureError("duplicate webhook header");
  const value = matches[0]?.[1];
  if (Array.isArray(value)) throw new WebhookSignatureError("duplicate webhook header");
  return value?.trim();
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRequest(value: unknown): value is { id: string; idempotency_key: string | null } {
  if (!isRecord(value)) {
    return false;
  }
  const { id, idempotency_key: key } = value;
  return typeof id === "string" && (key === null || typeof key === "string");
}

async function importKey(encoded: string): Promise<CryptoKey> {
  const raw = encoded.startsWith("whpk_") ? base64(encoded.slice("whpk_".length)) : undefined;
  if (raw?.length !== 32) {
    throw new TypeError("a webhook public key is whpk_ and the base64 of 32 bytes");
  }
  return crypto.subtle.importKey("raw", raw, "Ed25519", false, ["verify"]);
}

function base64(value: string): Uint8Array<ArrayBuffer> | undefined {
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(value) || value.length % 4 !== 0) {
    return undefined;
  }
  return Uint8Array.from(atob(value), (char) => char.charCodeAt(0));
}

function concat(left: Uint8Array, right: Uint8Array): Uint8Array<ArrayBuffer> {
  const joined = new Uint8Array(left.length + right.length);
  joined.set(left);
  joined.set(right, left.length);
  return joined;
}
