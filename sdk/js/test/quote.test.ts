import { describe, expect, it } from "vitest";
import { parseClientQuote, quoteIdFromClientSecret, quoteTransfer } from "../src/index.js";
import { ADDRESS, CLIENT_SECRET, QUOTE_ID, TOKEN, quote } from "./fixtures.js";

describe("quoteIdFromClientSecret", () => {
  it("returns the quote id prefix", () => {
    expect(quoteIdFromClientSecret(CLIENT_SECRET)).toBe(QUOTE_ID);
  });

  it.each(["", QUOTE_ID, "qt_x_secret_y", `${CLIENT_SECRET}/../admin`, `pi_12_secret_34`])(
    "rejects %j",
    (secret) => {
      expect(() => quoteIdFromClientSecret(secret)).toThrow(TypeError);
    },
  );
});

describe("parseClientQuote", () => {
  it("accepts the public view", () => {
    expect(parseClientQuote(JSON.parse(JSON.stringify(quote())))).toEqual(quote());
  });

  it.each([null, 1_790_410_499])("preserves the deferred cancellation request %j while still open", (requestedAt) => {
    const pending = { ...quote(), cancel_requested_at: requestedAt };
    expect(parseClientQuote(pending)).toEqual(pending);
    expect(parseClientQuote(pending).status).toBe("open");
  });

  it.each([
    ["a non-object", null],
    ["an unknown status", { ...quote(), status: "consumed" }],
    ["an unknown payment status", { ...quote(), payment_status: "final" }],
    ["a numeric token amount", { ...quote(), amount_atomic: 100 }],
    ["a malformed address", { ...quote(), address: "0x1234" }],
    ["fractional cents", { ...quote(), amount: 25.5 }],
    ["fractional credited cents", { ...quote(), amount_credited: 10.5 }],
    ["no typical credit time", { ...quote(), typical_credit_seconds: undefined }],
    ["a malformed cancellation time", { ...quote(), cancel_requested_at: "now" }],
    ["a fractional cancellation time", { ...quote(), cancel_requested_at: 1.5 }],
  ])("rejects %s", (_, value) => {
    expect(() => parseClientQuote(value)).toThrow(TypeError);
  });
});

describe("quoteTransfer", () => {
  it("reads the ERC-20 transfer of the payment URI", () => {
    expect(quoteTransfer(quote())).toEqual({
      chainId: 11155111,
      token: TOKEN,
      to: ADDRESS,
      amount: 100502512562814070352n,
    });
  });

  it.each([
    ["another recipient", quote({ payment_uri: quote({ address: `0x${"2".repeat(40)}` }).payment_uri })],
    ["another amount", quote({ payment_uri: quote({ amount_atomic: "1" }).payment_uri })],
    ["another chain", quote({ payment_uri: quote({ chain_id: 1 }).payment_uri })],
    ["a native transfer", quote({ payment_uri: `ethereum:${ADDRESS}@11155111?value=1` })],
    ["no chain", quote({ payment_uri: `ethereum:${TOKEN}/transfer?address=${ADDRESS}&uint256=1` })],
    ["no amount", quote({ payment_uri: `ethereum:${TOKEN}@11155111/transfer?address=${ADDRESS}` })],
    ["a scientific amount", quote({ payment_uri: `ethereum:${TOKEN}@11155111/transfer?address=${ADDRESS}&uint256=1e20` })],
  ])("refuses a payment URI with %s", (_, value) => {
    expect(() => quoteTransfer(value)).toThrow(TypeError);
  });
});
