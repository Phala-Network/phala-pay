import { describe, expect, it } from "vitest";
import {
  addressUrl, transactionUrl, stripeReceiptInvoice, stripeReceiptInvoiceItem,
  stripeReceiptCreditNote, type ReceiptDeposit,
} from "../src/helpers.js";
import {
  addressUrl as browserAddressUrl, transactionUrl as browserTransactionUrl,
  networkName as browserNetworkName,
} from "../../js/src/chains.js";
import { fixture, quoteResponse } from "./merchant-fixtures.js";

function deposit(changes: Partial<ReceiptDeposit> = {}): ReceiptDeposit {
  return {
    id: "dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516", status: "credited", amount: 2500,
    currency: "usd", chain_id: 8453, asset: "usdc", asset_contract: `0x${"11".repeat(20)}`,
    tx_hash: `0x${"22".repeat(32)}`, livemode: true,
    quote: "qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10", amount_refunded: 0, amount_reversed: 0,
    ...changes,
  };
}
const creditOptions = { invoice: "in_123", previousRefunded: 0, previousReversed: 0 };

describe("Stripe receipts", () => {
  it("matches the browser viem explorer registry and the Python shared fixture", () => {
    const networks = fixture<{ chain_id: number; name: string; explorer: string }[]>("explorers.json");
    for (const { chain_id: id, name, explorer } of networks) {
      expect(browserNetworkName(id)).toBe(name);
      expect(transactionUrl(id, "0xabc")).toBe(`${explorer}/tx/0xabc`);
      expect(addressUrl(id, "0x123")).toBe(`${explorer}/address/0x123`);
      expect(transactionUrl(id, "0xabc")).toBe(browserTransactionUrl(id, "0xabc"));
      expect(addressUrl(id, "0x123")).toBe(browserAddressUrl(id, "0x123"));
    }
    expect(transactionUrl(999, "0xabc")).toBeUndefined();
    expect(addressUrl(999, "0x123")).toBeUndefined();
  });
  it.each([true, false])("preserves the amount, transaction and mode (%s) without charging", (livemode) => {
    const dep = deposit({ livemode });
    const invoice = stripeReceiptInvoice(dep, { customer: "cus_123" });
    const item = stripeReceiptInvoiceItem(dep, { customer: "cus_123", invoice: "in_123" });
    expect(invoice).toMatchObject({ collection_method: "send_invoice", auto_advance: false,
      days_until_due: 0, customer: "cus_123", currency: "usd" });
    expect(item).toMatchObject({ customer: "cus_123", invoice: "in_123", amount: 2500,
      currency: "usd", tax_behavior: "inclusive", discountable: false });
    expect(item.description).toBe(`USDC on Base, ${dep.tx_hash}`);
    expect(invoice.metadata).toEqual(item.metadata);
    expect(invoice.metadata).toMatchObject({ phala_pay_deposit: dep.id, phala_pay_quote: dep.quote,
      tx_hash: dep.tx_hash, chain_id: "8453", asset: "usdc", livemode: String(livemode) });
    expect(invoice.footer).toContain(transactionUrl(8453, dep.tx_hash));
    expect(invoice.custom_fields.map((f) => f.name)).toEqual(["Network", "Token", "Transaction", "Explorer"]);
  });
  it("supports spot deposits and unknown networks without inventing links", () => {
    const dep = deposit({ quote: null, chain_id: 999, asset: null });
    const params = stripeReceiptInvoice(dep, { customer: "cus_123", description: "Account credits" });
    expect(params.description).toBe("Account credits");
    expect(params.metadata).not.toHaveProperty("phala_pay_quote");
    expect(params.metadata.asset).toBe(dep.asset_contract);
    expect(params.custom_fields).toContainEqual({ name: "Network", value: "Chain 999" });
    expect(params.custom_fields).toHaveLength(3);
    expect(params.footer).toContain(dep.tx_hash);
    expect(params.footer).not.toContain("https://");
  });
  it("records expanded quote ids", () => {
    const quote = quoteResponse();
    expect(stripeReceiptInvoice(deposit({ quote }), { customer: "cus_123" })
      .metadata.phala_pay_quote).toBe(quote.id);
  });
  it("obeys custom field limits while retaining complete references", () => {
    const dep = deposit({ asset: "a".repeat(200), tx_hash: `0x${"b".repeat(200)}` });
    const params = stripeReceiptInvoice(dep, { customer: "cus_123" });
    expect(params.custom_fields).toHaveLength(3);
    for (const field of params.custom_fields) {
      expect(Array.from(field.name).length).toBeLessThanOrEqual(40);
      expect(Array.from(field.value).length).toBeLessThanOrEqual(140);
    }
    expect(params.footer).toContain(dep.tx_hash);
    expect(params.metadata.tx_hash).toBe(dep.tx_hash);
  });
  it("credits growth only, ignoring replays and stale snapshots", () => {
    const dep = deposit({ amount_refunded: 600 });
    const credit = stripeReceiptCreditNote(dep, { ...creditOptions, previousRefunded: 200 });
    expect(credit).toMatchObject({ invoice: "in_123", amount: 400, out_of_band_amount: 400,
      metadata: { phala_pay_deposit: dep.id, amount_refunded: "600", refund_growth: "400" } });
    for (const previousRefunded of [600, 800])
      expect(stripeReceiptCreditNote(dep, { ...creditOptions, previousRefunded })).toBeNull();
    expect(stripeReceiptCreditNote(deposit({ amount_refunded: 900 }),
      { ...creditOptions, previousRefunded: 800 })?.amount).toBe(100);
  });
  it("records a reversal once", () => {
    const dep = deposit({ status: "reversed", amount_reversed: 2500 });
    expect(stripeReceiptCreditNote(dep, creditOptions)).toMatchObject({ amount: 2500,
      out_of_band_amount: 2500, metadata: { reversal_growth: "2500" } });
    expect(stripeReceiptCreditNote(dep, { ...creditOptions, previousReversed: 2500 })).toBeNull();
  });
  it.each<Partial<ReceiptDeposit>>([
    { status: "pending" }, { status: "rejected" }, { amount: null }, { amount: -1 },
    { amount: Number.MAX_SAFE_INTEGER + 1 }, { currency: "USD" }, { tx_hash: "a".repeat(501) },
  ])("rejects unreceiptable deposits (%j)", (changes) => {
    expect(() => stripeReceiptInvoice(deposit(changes), { customer: "cus_123" })).toThrow(RangeError);
  });
  it.each([-1, NaN, 2600])("rejects invalid prior totals (%s)", (previousRefunded) => {
    expect(() => stripeReceiptCreditNote(deposit(), { ...creditOptions, previousRefunded })).toThrow(RangeError);
  });
});
