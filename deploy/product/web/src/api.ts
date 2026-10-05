// The product's demo API (deploy/product/reference_product/demo.py), at its own origin, fixed at
// build time: VITE_DEMO_API_ORIGIN (.env.production; the e2e builds against its local product). The
// browser only talks to the product, which sends every service request itself with its API key, and
// to the service's public quote and deposit address views, which the SDK components read with a
// client secret.

import type { DepositAddressDetails } from "@phala/pay";

export interface Account {
  account_id: string;
  /** Cents: the console's ledger, credits less their refunded and reversed shares. */
  balance: number;
  ledger: LedgerLine[];
  presets: number[];
  min_amount: number;
  max_amount: number;
  api_base: string;
  factory: string;
  deposit_address: string | null;
  payments: PaymentRow[];
}

/**
 * A network the customer can pay on, with its tokens: the service's `GET /v1/config` `assets` on
 * the chains the product has pins for (a chain appears once the service serves it).
 */
export interface Network {
  chain_id: number;
  /** For display, for example `Sepolia testnet`. */
  name: string;
  testnet: boolean;
  /** The block explorer's origin, for transaction and address links. */
  explorer: string | null;
  /** A public list of faucets for the network's gas, on a testnet. */
  faucet: string | null;
  /** The merchant's treasury on this network, which every address pays. */
  treasury: string;
  assets: Asset[];
}

export interface Asset {
  /** The service's asset code, for example `pha`. */
  asset: string;
  symbol: string;
  contract: string;
  decimals: number;
  pricing: string;
  /** Cents. */
  min_amount: number;
  quote_ttl_seconds: number;
  /** Typical seconds from paying to the credit at the account's confirmation on the chain. */
  typical_credit_seconds: number;
  /** A test token anyone can mint: the visitor's own wallet mints it. */
  mintable: boolean;
  /**
   * The faucet contract whose public `mint(token, to, amount)` mints it (Aave's, for its test
   * USDT), or null when the token's own `mint(address,uint256)` does.
   */
  minter: string | null;
  /** The token issuer's testnet faucet, for a test token that does not mint. */
  faucet: string | null;
  /** The demo merchant's own promotion on credits paid in this token, in basis points. */
  bonus_bps: number;
}

export interface LedgerLine {
  deposit: string;
  amount: number;
  /**
   * `deposit.credited`, or the event that adjusted the credit (`deposit.refunded`, …); for a bonus
   * line, its grant (`PHA bonus +10%`) or the event that took it back.
   */
  reason: string;
  at: number;
  /** A credit and its claw-backs, or the demo merchant's own bonus. */
  kind: "credit" | "bonus";
}

export interface PaymentRow {
  kind: "quote" | "address";
  /** The deposit's `dep_` id, or the quote's `qt_` id while it has no deposit. */
  id: string;
  quote: string | null;
  created: number;
  amount: number | null;
  amount_atomic: string;
  chain_id: number;
  /** `null` for a token without a route. */
  asset: string | null;
  /** USD per token: the quote's locked price, or the deposit's valuation. */
  exchange_rate: string | null;
  status: string;
  final: boolean;
  swept: boolean;
  tx_hash: string | null;
  amount_refunded_atomic: string;
  net: number | null;
  /** The demo merchant's bonus on the credit, net of claw-backs; `null` before it is credited. */
  bonus: number | null;
}

export type StepKey =
  | "quote_created"
  | "sent"
  | "received"
  | "credited"
  | "webhook_received"
  | "final"
  | "reversed"
  | "swept";

export interface Detail {
  label: string;
  value: string | number | null;
  kind?: "address" | "tx" | "time" | "usd" | "usd_delta" | "atomic" | "rate";
  /** A rate's token symbol: `1 PHA = $0.25`. */
  unit?: string;
  mono?: boolean;
}

export interface Step {
  key: StepKey;
  state: "complete" | "current" | "upcoming" | "failed";
  at: number | null;
  details: Detail[];
}

export interface WebhookEvent {
  id: string;
  type: string;
  received_at: number;
  verified: boolean;
  data: { object?: Record<string, unknown> };
}

export interface ApiExchange {
  method: string;
  url: string;
  status: number;
  request: { headers: Record<string, string>; body: unknown };
  response: unknown;
}

export interface Deposit {
  id: string;
  status: string;
  final: boolean;
  /** When the service's finality watch found it final, Unix seconds; null until final. */
  final_at?: number | null;
  swept: boolean;
  amount: number | null;
  amount_atomic: string;
  chain_id: number;
  asset: string | null;
  /** USD per token the deposit was valued at, once valued. */
  exchange_rate: string | null;
  price_source: "quote" | "spot" | null;
  amount_refunded_atomic: string;
  amount_refunded: number;
  amount_reversed: number;
  from_address: string;
  asset_contract: string;
  tx_hash: string;
  metadata: Record<string, string>;
}

export interface Refund {
  id: string;
  status: "pending" | "succeeded" | "failed" | "canceled";
  amount_atomic: string;
  destination_address: string;
  treasury: string;
  transaction_hash: string | null;
  receipt_log_index: number | null;
  failure_reason: string | null;
  failure_explanation: string | null;
  created: number;
  /** The exact transfer that pays the refund, while it awaits one. */
  transfer: { from: string; token: string; to: string; amount_atomic: string; data: string } | null;
}

export interface LedgerView {
  status: string;
  amount: number | null;
  amount_refunded: number;
  amount_reversed: number;
  /** What the snapshot rule nets the deposit to, from the service's deposit. */
  nets_to: number;
  product: {
    status: string | null;
    reason: string | null;
    credit: number | null;
    net: number | null;
    /** The demo merchant's bonus on the credit, net of its claw-backs. */
    bonus: number | null;
    adjustments: { amount: number; reason: string; at: number }[];
  } | null;
}

export interface Timeline {
  kind: "quote" | "address";
  quote: {
    id: string;
    status: string;
    chain_id: number;
    asset: string;
    exchange_rate: string;
    expires_at: number;
    metadata: Record<string, string>;
  } | null;
  deposit: Deposit | null;
  sent: { tx_hash: string; block_number: number; at: number } | null;
  steps: Step[];
  refunds: Refund[];
  ledger: LedgerView | null;
  events: WebhookEvent[];
  api: ApiExchange[];
}

export interface Trust {
  attestation: {
    binding_verified: boolean;
    account?: string;
    livemode?: boolean;
    webhook_public_key?: string;
    report_data?: string;
    quote_bytes?: number;
  };
  tls_evidence: { app_id: string; compose_hash?: string; os_image_hash?: string; url: string } | null;
  verify_docs: string;
  dstack_verifier: string;
}

export interface CreatedQuote {
  quote: string;
  client_secret: string;
  /** The quote's address as the product's SDK recomputed it from the pins. */
  expected_address: string;
  order_id: string;
  /** Cents. */
  amount: number;
  chain_id: number;
  asset: string;
  amount_atomic: string;
  /** The price the quote locks until `expires_at`, in USD per token. */
  exchange_rate: string;
  expires_at: number;
  api: ApiExchange[];
}

export interface AddressPayment {
  status: string;
  chain_id: number;
  asset: string | null;
  tx_hash: string;
  amount_atomic: string;
  confirmations: number | null;
  /** The deposit's `dep_` id, known before it is recorded. */
  deposit: string;
}

export interface DepositAddressView extends DepositAddressDetails {
  id: string;
  version: number;
  status: string;
  metadata: Record<string, string>;
  networks: (DepositAddressDetails["networks"][number] & { treasury: string })[];
  payments: AddressPayment[];
}

export interface DepositAddressResponse {
  deposit_address: DepositAddressView;
  /** Only from `POST api/deposit_address`, for `<DepositAddress>`. */
  client_secret?: string;
  /** The product's SDK recomputed every network's address from the pins. */
  verified: boolean;
  api: ApiExchange[];
}

export interface FlushCall {
  to: string;
  data: string;
  value: string;
}

/** One token's sweep on one network. */
export interface SweepGroup {
  unavailable: boolean;
  chain_id: number;
  network: string;
  asset: string;
  symbol: string;
  decimals: number;
  token: string;
  treasury: string;
  unswept_atomic: string;
  final_unswept_atomic: string;
  sweepable_forwarders: number;
  refused_forwarders: number;
  flush: FlushCall[];
  safe_batch: unknown;
  sweeps: { id: string; address: string; amount_atomic: string; tx_hash: string; created: number }[];
}

export interface Sweeps {
  factory: string;
  groups: SweepGroup[];
  api: ApiExchange[];
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
  ) {
    super(code);
  }
}

const API = `${import.meta.env.VITE_DEMO_API_ORIGIN}/api/`;

// Cross-origin with the visitor's demo account cookie, which the API allows only for the website.
async function request(path: string, init?: RequestInit): Promise<unknown> {
  const deadline = AbortSignal.timeout(10_000);
  const signal = init?.signal == null ? deadline : AbortSignal.any([init.signal, deadline]);
  const response = await fetch(`${API}${path}`, { credentials: "include", ...init, signal });
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const code = isRecord(body) && typeof body["code"] === "string" ? body["code"] : "error";
    throw new ApiError(response.status, code);
  }
  if (!isRecord(body)) {
    throw new ApiError(response.status, "invalid_response");
  }
  return body;
}

function post(path: string, body: unknown): Promise<unknown> {
  return request(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

export async function getAccount(signal?: AbortSignal): Promise<Account> {
  const body = await request("account", { signal: signal ?? null });
  return expect<Account>(body, ["account_id", "balance", "ledger", "payments"]);
}

export async function getNetworks(signal?: AbortSignal): Promise<Network[]> {
  const body = await request("assets", { signal: signal ?? null });
  return expect<{ networks: Network[] }>(body, ["networks"]).networks;
}

export async function createQuote({
  amount,
  chainId,
  asset,
}: {
  amount: number;
  chainId: number;
  asset: string;
}): Promise<CreatedQuote> {
  const body = await post("quotes", { amount, chain_id: chainId, asset });
  return expect<CreatedQuote>(body, ["quote", "client_secret", "expected_address", "exchange_rate", "expires_at"]);
}

export async function createDepositAddress(): Promise<DepositAddressResponse> {
  const body = await post("deposit_address", {});
  return expect<DepositAddressResponse>(body, ["deposit_address", "client_secret", "verified"]);
}

export async function getDepositAddress(signal?: AbortSignal): Promise<DepositAddressResponse> {
  const body = await request("deposit_address", { signal: signal ?? null });
  return expect<DepositAddressResponse>(body, ["deposit_address", "verified"]);
}

export async function getTimeline(selection: Selection, signal?: AbortSignal): Promise<Timeline> {
  const path = selection.kind === "quote" ? "quotes" : "deposits";
  const body = await request(`${path}/${encodeURIComponent(selection.id)}`, { signal: signal ?? null });
  return expect<Timeline>(body, ["steps", "refunds", "events", "api"]);
}

export async function createRefund({
  deposit,
  amountAtomic,
  destinationAddress,
}: {
  deposit: string;
  amountAtomic: string;
  destinationAddress: string;
}): Promise<Refund> {
  const body = await post("refunds", {
    deposit,
    amount_atomic: amountAtomic,
    destination_address: destinationAddress,
  });
  return expect<{ refund: Refund }>(body, ["refund"]).refund;
}

export async function markRefundPaid({
  refund,
  transactionHash,
  receiptLogIndex,
}: {
  refund: string;
  transactionHash: string;
  receiptLogIndex: number | null;
}): Promise<Refund> {
  const body = await post(`refunds/${encodeURIComponent(refund)}/mark_paid`, {
    transaction_hash: transactionHash,
    ...(receiptLogIndex === null ? {} : { receipt_log_index: receiptLogIndex }),
  });
  return expect<{ refund: Refund }>(body, ["refund"]).refund;
}

export async function cancelRefund(refund: string): Promise<Refund> {
  const body = await post(`refunds/${encodeURIComponent(refund)}/cancel`, {});
  return expect<{ refund: Refund }>(body, ["refund"]).refund;
}

export async function getSweeps(signal?: AbortSignal): Promise<Sweeps> {
  const body = await request("sweeps", { signal: signal ?? null });
  return expect<Sweeps>(body, ["groups", "api"]);
}

export async function getTrust(signal?: AbortSignal): Promise<Trust> {
  const body = await request("trust", { signal: signal ?? null });
  return expect<Trust>(body, ["attestation", "verify_docs"]);
}

/** What the behind-the-scenes panel follows: a quote (before and after its payment) or a deposit. */
export interface Selection {
  kind: "quote" | "deposit";
  id: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// The product is this page's own backend; a shape check on the fields read catches a mismatched
// deployment without duplicating every field's validation.
// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters -- the caller names the checked shape
function expect<T>(body: unknown, keys: string[]): T {
  if (!isRecord(body) || !keys.every((key) => key in body)) {
    throw new ApiError(200, "invalid_response");
  }
  return body as T;
}
