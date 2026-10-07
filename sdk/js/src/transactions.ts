import type { Hash } from "viem";
import { quoteIdFromClientSecret, type ClientQuote } from "./quote.js";
import { requestSignal, trimTrailingSlashes } from "./request.js";

export interface SubmitTransactionOptions {
  apiBase: string;
  clientSecret: string;
  transactionHash: Hash;
  /** Required for a deposit-address secret; quotes select their stored chain. */
  chainId?: number;
  signal?: AbortSignal;
  requestTimeout?: number;
  fetch?: typeof globalThis.fetch;
}

/** Sends a best-effort detection hint. Receipt of a hint says nothing about payment status. */
export async function submitTransaction(options: SubmitTransactionOptions): Promise<void> {
  if (!/^0x[0-9a-fA-F]{64}$/.test(options.transactionHash)) {
    throw new TypeError("Invalid transaction hash");
  }
  const addressId = /^(da_[0-9a-f]{32})_secret_/.exec(options.clientSecret)?.[1];
  const id = addressId ?? quoteIdFromClientSecret(options.clientSecret);
  if (addressId !== undefined && (!Number.isSafeInteger(options.chainId) || (options.chainId ?? 0) <= 0)) {
    throw new TypeError("A deposit-address hint requires chainId");
  }
  const resource = addressId === undefined ? "quotes" : "deposit_addresses";
  const url = new URL(`${trimTrailingSlashes(options.apiBase)}/v1/${resource}/${encodeURIComponent(id)}/transactions`);
  url.searchParams.set("client_secret", options.clientSecret);
  const response = await (options.fetch ?? globalThis.fetch)(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ transaction_hash: options.transactionHash, ...(addressId === undefined ? {} : { chain_id: options.chainId }) }),
    signal: requestSignal(options),
    credentials: "omit",
    redirect: "error",
  });
  if (response.status !== 202) throw new Error("Transaction hint was not received");
}

type QuoteSubmission = Omit<SubmitTransactionOptions, "transactionHash" | "chainId">;
const submissions = new WeakMap<ClientQuote, QuoteSubmission>();

/** Keep the checkout's origin and secret out of its public, serializable quote view. */
export function bindQuoteSubmission(quote: ClientQuote, options: QuoteSubmission): void {
  submissions.set(quote, options);
}

/** Automatic wallet hints never delay or change the result of an already broadcast payment. */
export function submitWalletTransaction(quote: ClientQuote, transactionHash: Hash): void {
  const options = submissions.get(quote);
  if (options !== undefined) {
    void submitTransaction({ ...options, transactionHash }).catch(() => undefined);
  }
}
