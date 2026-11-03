/**
 * Pure Stripe parameters, with no Stripe dependency or API calls. Contracts:
 * https://docs.stripe.com/api/invoices/create
 * https://docs.stripe.com/api/invoiceitems/create
 * https://docs.stripe.com/api/credit_notes/create (2026-10-28.endive).
 */
import type { Deposit } from "./types.js";
import { networkName, transactionUrl } from "./chains.js";

export type ReceiptDeposit = Pick<Deposit,
  "id" | "status" | "amount" | "currency" | "chain_id" | "asset" | "asset_contract" |
  "tx_hash" | "livemode" | "quote" | "amount_refunded" | "amount_reversed"
>;

export interface StripeReceiptInvoiceParams {
  customer: string;
  currency: string;
  collection_method: "send_invoice";
  days_until_due: number;
  auto_advance: false;
  description: string;
  footer: string;
  custom_fields: { name: string; value: string }[];
  metadata: Record<string, string>;
}
export interface StripeReceiptInvoiceItemParams {
  invoice: string;
  customer: string;
  amount: number;
  currency: string;
  tax_behavior: "inclusive";
  discountable: false;
  description: string;
  metadata: Record<string, string>;
}
export interface StripeReceiptCreditNoteParams {
  invoice: string;
  amount: number;
  out_of_band_amount: number;
  metadata: Record<string, string>;
}

function amount(value: unknown, name: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
    throw new RangeError(`${name} must be a nonnegative safe integer in minor units`);
  return value;
}
function metadata(deposit: ReceiptDeposit): Record<string, string> {
  if (!["credited", "reversed"].includes(deposit.status) || amount(deposit.amount, "amount") === 0)
    throw new RangeError("receipt requires a deposit with a positive credited amount");
  if (!/^[a-z]{3}$/.test(deposit.currency))
    throw new RangeError("currency must be a lowercase three-letter currency code");
  const result: Record<string, string> = {
    phala_pay_deposit: deposit.id,
    chain_id: String(deposit.chain_id),
    asset: deposit.asset ?? deposit.asset_contract,
    tx_hash: deposit.tx_hash,
    livemode: String(deposit.livemode),
  };
  if (deposit.quote != null)
    result.phala_pay_quote = typeof deposit.quote === "string" ? deposit.quote : deposit.quote.id;
  // https://docs.stripe.com/api/metadata: never truncate identifiers to the 500-char limit.
  if (Object.values(result).some((value) => !value || Array.from(value).length > 500))
    throw new RangeError("receipt metadata values must contain 1 to 500 characters");
  return result;
}
function token(deposit: ReceiptDeposit): string {
  return deposit.asset?.toUpperCase() ?? deposit.asset_contract;
}
function description(deposit: ReceiptDeposit): string {
  return `${token(deposit)} on ${networkName(deposit.chain_id)}, ${deposit.tx_hash}`;
}

/** Draft receipt; finalize with auto_advance=false, then pay with paid_out_of_band=true. */
export function stripeReceiptInvoice(
  deposit: ReceiptDeposit,
  options: { customer: string; description?: string },
): StripeReceiptInvoiceParams {
  const meta = metadata(deposit);
  // Stripe: <=4 fields, name <=40 and value <=140 Unicode characters.
  const short = (value: string): string => Array.from(value).slice(0, 140).join("");
  const customFields = [
    { name: "Network", value: short(networkName(deposit.chain_id)) },
    { name: "Token", value: short(token(deposit)) },
    { name: "Transaction", value: short(deposit.tx_hash) },
  ];
  const url = transactionUrl(deposit.chain_id, deposit.tx_hash);
  if (url !== undefined && Array.from(url).length <= 140)
    customFields.push({ name: "Explorer", value: url });
  return {
    customer: options.customer,
    currency: deposit.currency,
    collection_method: "send_invoice",
    days_until_due: 0,
    auto_advance: false,
    description: options.description ?? description(deposit),
    footer: `Paid in cryptocurrency. Transaction: ${url ?? deposit.tx_hash}`,
    custom_fields: customFields,
    metadata: meta,
  };
}

/** Tax-inclusive amount/currency line; no Product/Price needed. No currency conversion. */
export function stripeReceiptInvoiceItem(
  deposit: ReceiptDeposit,
  options: { invoice: string; customer: string },
): StripeReceiptInvoiceItemParams {
  const meta = metadata(deposit);
  return {
    ...options,
    amount: amount(deposit.amount, "amount"),
    currency: deposit.currency,
    tax_behavior: "inclusive",
    discountable: false,
    description: description(deposit),
    metadata: meta,
  };
}

/**
 * Credit only refund/reversal growth on an already paid receipt invoice. Prior totals must
 * be those successfully recorded in Stripe; persist componentwise maxima after success.
 */
export function stripeReceiptCreditNote(
  deposit: ReceiptDeposit,
  options: { invoice: string; previousRefunded: number; previousReversed: number },
): StripeReceiptCreditNoteParams | null {
  const meta = metadata(deposit);
  const refunded = amount(deposit.amount_refunded, "amount_refunded");
  const reversed = amount(deposit.amount_reversed, "amount_reversed");
  const previousRefunded = amount(options.previousRefunded, "previousRefunded");
  const previousReversed = amount(options.previousReversed, "previousReversed");
  if (Math.max(refunded, previousRefunded) + Math.max(reversed, previousReversed) >
      amount(deposit.amount, "amount"))
    throw new RangeError("refund and reversal totals exceed the credited amount");
  const refundGrowth = Math.max(0, refunded - previousRefunded);
  const reversalGrowth = Math.max(0, reversed - previousReversed);
  const growth = refundGrowth + reversalGrowth;
  if (growth === 0) return null;
  return {
    invoice: options.invoice,
    amount: growth,
    out_of_band_amount: growth,
    metadata: {
      ...meta,
      amount_refunded: String(Math.max(refunded, previousRefunded)),
      amount_reversed: String(Math.max(reversed, previousReversed)),
      refund_growth: String(refundGrowth),
      reversal_growth: String(reversalGrowth),
    },
  };
}
