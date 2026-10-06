import { expect, expectTypeOf, it } from "vitest";
import * as server from "../src/index.js";
import type { Forwarder, PhalaPay } from "../src/index.js";
import * as helpers from "../src/helpers.js";

it("exports offline functions only from the helpers entry", () => {
  for (const name of [
    "parsePins",
    "encodePins",
    "balanceDelta",
    "depositNetAmount",
    "constructEvent",
    "depositAddress",
    "depositAddressSalt",
    "forwarderAddress",
    "quoteSalt",
    "quoteAddress",
    "verifyDepositAddress",
    "verifyQuoteAddress",
    "batchChecksum",
    "flushTransaction",
    "flushTransactions",
    "safeBatch",
  ] as const) {
    expect(helpers[name]).toBeTypeOf("function");
    expect(server).not.toHaveProperty(name);
  }
  expect(helpers.SignatureVerificationError).toBe(server.SignatureVerificationError);
});

it("exports Forwarder as the forwarders API response type", () => {
  expectTypeOf<Forwarder>().toEqualTypeOf<
    Awaited<ReturnType<PhalaPay["forwarders"]["listPage"]>>["data"][number]
  >();
});
