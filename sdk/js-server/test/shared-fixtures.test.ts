// @vitest-environment node
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  constructEvent,
  depositAddress,
  depositAddressSalt,
  forwarderAddress,
  quoteAddress,
  quoteSalt,
} from "../src/helpers.js";
type Manifest = {
  schema_version: number;
  groups: {
    addresses: { js: boolean };
    webhooks: { js: boolean };
    pins: { js: boolean };
    ledger: { js: boolean };
    transport: { js: boolean };
  };
};
type QuoteVector = {
  account: string;
  client_reference_id: string;
  quote_id: string;
  salt: `0x${string}`;
  treasury: string;
  predicted_address: string;
};
type DepositVector = QuoteVector & { livemode: boolean; version: number };
type Addresses = {
  factory: string;
  implementation: string;
  quote: QuoteVector[];
  deposit_address: DepositVector[];
  forwarders: { salt: `0x${string}`; treasury: string; predicted_address: string }[];
};
type WebhookCase = {
  headers: Record<string, string>;
  body: string;
  expected_account: string;
  expected_livemode: boolean;
  now: number;
  tolerance?: number;
  outcome: "accept" | "reject";
};
type Webhooks = { public_key: string; cases: WebhookCase[] };
function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(resolve(process.cwd(), "../fixtures", name), "utf8")) as Record<
    string,
    unknown
  >;
}
// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters
function typed<T>(value: Record<string, unknown>): T {
  return value as T;
}
describe("shared SDK fixtures", () => {
  it("declares all JS fixture groups implemented", () => {
    const manifest = typed<Manifest>(fixture("manifest.json"));
    expect(manifest.schema_version).toBe(1);
    expect(manifest.groups.addresses.js).toBe(true);
    expect(manifest.groups.webhooks.js).toBe(true);
    for (const group of [manifest.groups.pins, manifest.groups.ledger, manifest.groups.transport])
      expect(group.js).toBe(true);
  });
  it("matches existing address derivation", () => {
    const vectors = typed<Addresses>(fixture("addresses-v1.json"));
    const first = vectors.quote[0];
    if (!first) throw new Error("address fixture has no quote vectors");
    for (const vector of vectors.quote) {
      expect(quoteSalt(vector.account, vector.client_reference_id, vector.quote_id)).toBe(
        vector.salt,
      );
      expect(
        quoteAddress(
          { factory: vectors.factory, implementation: vectors.implementation },
          { client_reference_id: vector.client_reference_id, id: vector.quote_id },
          vector.treasury,
          vector.account,
        ),
      ).toBe(vector.predicted_address);
    }
    for (const vector of vectors.deposit_address) {
      expect(
        depositAddressSalt(
          vector.account,
          vector.livemode,
          vector.client_reference_id,
          vector.version,
        ),
      ).toBe(vector.salt);
      expect(
        depositAddress(
          { factory: vectors.factory, implementation: vectors.implementation },
          vector,
          vector.treasury,
          vector.account,
        ),
      ).toBe(vector.predicted_address);
    }
    expect(
      forwarderAddress(vectors.factory, vectors.implementation, first.treasury, first.salt),
    ).toBe(first.predicted_address);
    for (const vector of vectors.forwarders)
      expect(
        forwarderAddress(vectors.factory, vectors.implementation, vector.treasury, vector.salt),
      ).toBe(vector.predicted_address);
  });
  it("matches current webhook verification", async () => {
    const vectors = typed<Webhooks>(fixture("webhooks-v1.json"));
    for (const vector of vectors.cases) {
      const options = {
        expectedAccount: vector.expected_account,
        expectedLivemode: vector.expected_livemode,
        now: vector.now,
        ...(vector.tolerance === undefined ? {} : { tolerance: vector.tolerance }),
      };
      if (vector.outcome === "accept")
        await expect(
          constructEvent(vector.body, vector.headers, vectors.public_key, options),
        ).resolves.toMatchObject({ id: vector.headers["webhook-id"] });
      else
        await expect(
          constructEvent(vector.body, vector.headers, vectors.public_key, options),
        ).rejects.toThrow();
    }
  });
});
