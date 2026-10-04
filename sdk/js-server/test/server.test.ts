// @vitest-environment node
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
  AddressMismatchError,
  WebhookSignatureError,
  batchChecksum,
  constructEvent,
  depositAddress,
  flushTransaction,
  flushTransactions,
  forwarderAddress,
  quoteSalt,
  quoteAddress,
  safeBatch,
  verifyDepositAddress,
  verifyQuoteAddress,
  type AddressPins,
  type BatchFile,
} from "../src/index.js";

// Tests run from sdk/js.
const repo = (path: string) => resolve(process.cwd(), "../..", path);
const vectors = JSON.parse(
  readFileSync(repo("contracts/test-vectors/create2.json"), "utf8"),
) as {
  factory: string;
  implementation: string;
  quote: { account: string; client_reference_id: string; quote_id: string; salt: `0x${string}`; treasury: string; predicted_address: string }[];
  deposit_address: {
    account: string;
    livemode: boolean;
    client_reference_id: string;
    version: number;
    treasury: string;
    predicted_address: string;
  }[];
};
const forwarder = { factory: vectors.factory, implementation: vectors.implementation };

describe("address recomputation", () => {
  it("reproduces the contract vectors of quotes", () => {
    expect(vectors.quote.length).toBeGreaterThan(0);
    for (const vector of vectors.quote) {
      expect(quoteSalt(vector.account, vector.client_reference_id, vector.quote_id)).toBe(vector.salt);
      expect(forwarderAddress(vectors.factory, vectors.implementation, vector.treasury, vector.salt)).toBe(
        vector.predicted_address,
      );
      const quote = { client_reference_id: vector.client_reference_id, id: vector.quote_id };
      expect(quoteAddress(forwarder, quote, vector.treasury, vector.account)).toBe(vector.predicted_address);
    }
  });

  it("reproduces the contract vectors of deposit addresses", () => {
    expect(vectors.deposit_address.length).toBeGreaterThan(0);
    for (const vector of vectors.deposit_address) {
      expect(depositAddress(forwarder, vector, vector.treasury, vector.account)).toBe(
        vector.predicted_address,
      );
    }
  });
});

describe("address verification from your own pins", () => {
  const [vector] = vectors.quote;
  if (vector === undefined) throw new Error("no quote vector");
  const pins: AddressPins = { ...forwarder, account: vector.account, treasuries: { 1: vector.treasury } };
  const quote = {
    id: vector.quote_id,
    livemode: true,
    chain_id: 1,
    treasury: vector.treasury,
    address: vector.predicted_address,
    client_reference_id: vector.client_reference_id,
  };
  const attacker = "0x" + "ee".repeat(20);
  // A compromised service returns an attacker's treasury and the address that really derives
  // from it, so recomputing over the response's treasury would pass.
  const spoofed = {
    ...quote,
    treasury: attacker,
    address: quoteAddress(forwarder, { client_reference_id: vector.client_reference_id, id: vector.quote_id }, attacker, vector.account),
  };

  it("returns the address derived from the pinned treasury", () => {
    expect(verifyQuoteAddress(pins, quote)).toBe(vector.predicted_address);
  });

  it("refuses a spoofed treasury with its valid address", () => {
    expect(() => verifyQuoteAddress(pins, spoofed)).toThrow(AddressMismatchError);
    // Naming the pinned treasury while showing the attacker's address fails too.
    expect(() => verifyQuoteAddress(pins, { ...spoofed, treasury: vector.treasury })).toThrow(
      /cannot derive/,
    );
  });

  it("fails closed in live mode without the chain's pinned treasury", () => {
    for (const treasuries of [undefined, {}, { 10: vector.treasury }]) {
      expect(() => verifyQuoteAddress({ ...pins, treasuries }, spoofed)).toThrow(/live mode requires/);
      expect(() => verifyQuoteAddress({ ...pins, treasuries }, quote)).toThrow(/live mode requires/);
    }
    expect(() => verifyQuoteAddress({ ...pins, account: "" }, quote)).toThrow(/pinned account/);
  });

  it("trusts the response's treasury only in test mode, with a warning", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    try {
      expect(verifyQuoteAddress({ ...pins, treasuries: undefined }, { ...spoofed, livemode: false })).toBe(
        spoofed.address,
      );
      expect(warn).toHaveBeenCalledWith(expect.stringContaining("test mode only"));
    } finally {
      warn.mockRestore();
    }
  });

  it("checks every network of a deposit address", () => {
    const [deposit] = vectors.deposit_address;
    if (deposit === undefined) throw new Error("no deposit address vector");
    const addressPins: AddressPins = {
      ...forwarder,
      account: deposit.account,
      treasuries: { 1: deposit.treasury, 8453: deposit.treasury },
    };
    const network = { treasury: deposit.treasury, address: deposit.predicted_address };
    const address = {
      id: "da_1",
      livemode: true,
      client_reference_id: deposit.client_reference_id,
      version: deposit.version,
      networks: [
        { chain_id: 1, ...network },
        { chain_id: 8453, ...network },
      ],
    };
    const live = { ...address, livemode: deposit.livemode };
    expect(verifyDepositAddress(addressPins, live)).toBe(deposit.predicted_address);
    const forged = {
      ...live,
      networks: [{ chain_id: 1, ...network }, { chain_id: 8453, treasury: attacker, address: deposit.predicted_address }],
    };
    expect(() => verifyDepositAddress(addressPins, forged)).toThrow(/chain 8453 pays a treasury/);
    expect(() =>
      verifyDepositAddress({ ...addressPins, treasuries: { 1: deposit.treasury } }, { ...address, livemode: true }),
    ).toThrow(AddressMismatchError);
  });
});

// The Rust core's and the Python SDK's signing vector (sdk/python/tests/test_webhooks.py): the
// seed [7; 32] signs `{id}.{timestamp}.{body}`.
const PUBLIC_KEY = "whpk_6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";
const PUBLIC_KEY_HEX = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
const RUST_ID = "evt_018d5f8e8a7b7d65bc442c4f5f0a6d31";
const RUST_TIMESTAMP = 1_674_087_231;
const RUST_BODY = '{"type":"deposit.confirmed","data":{"deposit_id":"dep_123"}}';
const RUST_SIGNATURE =
  "v1a,YuPb4kzXzDJqX8EcTFjrfDziMBFmzlPS3V/ISzdG/7R3KS7G1TVLRBF7DOJGnAtOjjvfeFm1G32KO67JiiY0BQ==";

async function signed(body: string, id: string, timestamp = Math.floor(Date.now() / 1000)) {
  const pair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
  const signature = await crypto.subtle.sign(
    "Ed25519",
    pair.privateKey,
    new TextEncoder().encode(`${id}.${timestamp}.${body}`),
  );
  const raw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
  return {
    publicKey: `whpk_${Buffer.from(raw).toString("base64")}`,
    headers: {
      "webhook-id": id,
      "webhook-timestamp": String(timestamp),
      "webhook-signature": `v1a,${Buffer.from(signature).toString("base64")}`,
    },
  };
}

const ACCOUNT = `acct_${"a1".repeat(16)}`;
const EVENT_ID = `evt_${"26".repeat(16)}`;
function event(overrides: Record<string, unknown> = {}) {
  return JSON.stringify({
    id: EVENT_ID,
    object: "event",
    account: ACCOUNT,
    livemode: false,
    type: "deposit.credited",
    created: 1_790_000_000,
    actor: "system",
    request: null,
    data: { object: { id: `dep_${"01".repeat(16)}`, object: "deposit" } },
    ...overrides,
  });
}

describe("constructEvent", () => {
  it("verifies either signature during rotation and keeps raw body bytes significant", async () => {
    const body = event();
    const timestamp = 1_790_000_000;
    const oldKey = await signed(body, EVENT_ID, timestamp);
    const newKey = await signed(body, EVENT_ID, timestamp);
    const headers = { ...newKey.headers, "webhook-signature":
      `v1,AAAA v1a,AAAA ${newKey.headers["webhook-signature"]} ${oldKey.headers["webhook-signature"]}` };
    const options = { expectedAccount: ACCOUNT, expectedLivemode: false, now: timestamp };
    for (const pinned of [oldKey.publicKey, newKey.publicKey, [oldKey.publicKey, newKey.publicKey]]) {
      expect((await constructEvent(body, headers, pinned, options)).id).toBe(EVENT_ID);
      await expect(constructEvent(body + " ", headers, pinned, options)).rejects.toThrow(
        "no valid webhook signature",
      );
    }
    await expect(constructEvent(body, newKey.headers, oldKey.publicKey, options)).rejects.toThrow(
      "no valid webhook signature",
    );
  });

  it("keeps the default, custom, and zero timestamp windows inclusive in both directions", async () => {
    const body = event();
    const timestamp = 1_790_000_000;
    const { publicKey, headers } = await signed(body, EVENT_ID, timestamp);
    for (const tolerance of [undefined, 0, 0.5, 30]) {
      const window = tolerance ?? 300;
      for (const direction of [-1, 1]) {
        const options = {
          expectedAccount: ACCOUNT, expectedLivemode: false,
          ...(tolerance === undefined ? {} : { tolerance }),
          now: timestamp + direction * window,
        };
        expect((await constructEvent(body, headers, publicKey, options)).id).toBe(EVENT_ID);
        await expect(constructEvent(body, headers, publicKey, {
          ...options, now: timestamp + direction * (window + 1),
        })).rejects.toThrow("outside tolerance");
      }
    }
  });

  it("rejects invalid time configuration even for an authentic stale event", async () => {
    const body = event();
    const timestamp = 1_790_000_000;
    const { publicKey, headers } = await signed(body, EVENT_ID, timestamp);
    for (const invalid of [NaN, Infinity, -Infinity]) {
      const options = { expectedAccount: ACCOUNT, expectedLivemode: false };
      await expect(constructEvent(body, headers, publicKey, {
        ...options, now: timestamp + 301, tolerance: invalid,
      })).rejects.toThrow(TypeError);
      await expect(constructEvent(body, headers, publicKey, {
        ...options, now: invalid,
      })).rejects.toThrow(TypeError);
    }
    await expect(constructEvent(body, headers, publicKey, {
      expectedAccount: ACCOUNT, expectedLivemode: false, now: timestamp, tolerance: -1,
    })).rejects.toThrow(TypeError);
  });

  it("rejects signed timestamps outside the interoperable integer range", async () => {
    const body = event();
    const { publicKey, headers } = await signed(body, EVENT_ID, 2 ** 53);
    await expect(constructEvent(body, headers, publicKey, {
      expectedAccount: ACCOUNT, expectedLivemode: false, now: 2 ** 53,
    })).rejects.toThrow("timestamp malformed");
  });

  it.each([
    { data: { object: [] } },
    { data: { object: [["amount", 1234]] } },
    { data: [["object", { object: "deposit" }]] },
    { id: 123 },
    { type: ["deposit.credited"] },
  ])("rejects an authentic malformed envelope: %j", async (overrides) => {
    const body = event(overrides);
    const { publicKey, headers } = await signed(body, EVENT_ID);
    await expect(constructEvent(body, headers, publicKey, {
      expectedAccount: ACCOUNT, expectedLivemode: false,
    })).rejects.toThrow("not an event");
  });

  it("keeps object snapshots and ignores non-object previous attributes", async () => {
    for (const previous of [{ status: "pending" }, [], null]) {
      const body = event({ data: { object: { object: "deposit" }, previous_attributes: previous } });
      const { publicKey, headers } = await signed(body, EVENT_ID);
      const verified = await constructEvent(new TextEncoder().encode(body), headers, publicKey, {
        expectedAccount: ACCOUNT, expectedLivemode: false,
      });
      expect(verified.data.previous_attributes).toEqual(Array.isArray(previous) || previous === null
        ? undefined : previous);
    }
  });

  it("verifies the service's v1a signature vector", async () => {
    const options = { expectedAccount: ACCOUNT, expectedLivemode: false, now: RUST_TIMESTAMP };
    const headers = new Headers({
      "Webhook-Id": RUST_ID,
      "Webhook-Timestamp": String(RUST_TIMESTAMP),
      "Webhook-Signature": RUST_SIGNATURE,
    });
    // The signature verifies; the vector's body is no event, which is refused after it.
    await expect(constructEvent(RUST_BODY, headers, PUBLIC_KEY, options)).rejects.toThrow(
      "not an event",
    );
    await expect(
      constructEvent(RUST_BODY.replace("123", "124"), headers, PUBLIC_KEY, options),
    ).rejects.toBeInstanceOf(WebhookSignatureError);
  });

  it("returns the event of the expected account and mode", async () => {
    const body = event();
    const { publicKey, headers } = await signed(body, EVENT_ID);
    const verified = await constructEvent(body, headers, [PUBLIC_KEY, publicKey], {
      expectedAccount: ACCOUNT,
      expectedLivemode: false,
    });
    expect(verified.type).toBe("deposit.credited");
    expect(verified.data.object["object"]).toBe("deposit");
  });

  it("returns the causing request", async () => {
    const request = { id: `req_${"3c".repeat(12)}`, idempotency_key: "order-17" };
    const body = event({ request });
    const { publicKey, headers } = await signed(body, EVENT_ID);
    const verified = await constructEvent(body, headers, publicKey, {
      expectedAccount: ACCOUNT,
      expectedLivemode: false,
    });
    expect(verified.request).toEqual(request);
    expect(verified.actor).toBe("system");
    // A malformed request, or an envelope without its actor or request, is not an event.
    for (const malformed of [
      event({ request: { id: 7 } }),
      event({ actor: undefined }),
      event({ request: undefined }),
    ]) {
      const other = await signed(malformed, EVENT_ID);
      await expect(
        constructEvent(malformed, other.headers, other.publicKey, {
          expectedAccount: ACCOUNT,
          expectedLivemode: false,
        }),
      ).rejects.toThrow("not an event");
    }
  });

  it("accepts a public key only in the whpk_ form", async () => {
    const body = event();
    const { publicKey, headers } = await signed(body, EVENT_ID);
    const options = { expectedAccount: ACCOUNT, expectedLivemode: false };
    for (const refused of [
      PUBLIC_KEY_HEX,
      publicKey.slice("whpk_".length),
      `whpk_${PUBLIC_KEY_HEX}`,
      `whpk_${Buffer.from(new Uint8Array(16)).toString("base64")}`,
    ]) {
      await expect(constructEvent(body, headers, refused, options)).rejects.toThrow("whpk_");
    }
  });

  it.each([
    ["another account", event({ account: `acct_${"b2".repeat(16)}` }), EVENT_ID, "another account"],
    ["the other mode", event({ livemode: true }), EVENT_ID, "other mode"],
    ["another id", event(), `evt_${"00".repeat(16)}`, "does not match"],
  ])("fails closed for %s", async (_, body, id, message) => {
    const { publicKey, headers } = await signed(body, id);
    await expect(
      constructEvent(body, headers, publicKey, { expectedAccount: ACCOUNT, expectedLivemode: false }),
    ).rejects.toThrow(message);
  });

  it("refuses a stale, unsigned, or foreign delivery", async () => {
    const body = event();
    const { publicKey, headers } = await signed(body, EVENT_ID);
    const options = { expectedAccount: ACCOUNT, expectedLivemode: false };
    await expect(
      constructEvent(body, headers, publicKey, { ...options, now: Date.now() / 1000 + 301 }),
    ).rejects.toThrow("tolerance");
    await expect(constructEvent(body, {}, publicKey, options)).rejects.toThrow("headers missing");
    await expect(constructEvent(body, headers, PUBLIC_KEY, options)).rejects.toThrow(
      "no valid webhook signature",
    );
    await expect(
      constructEvent(body, headers, publicKey, { ...options, expectedAccount: "" }),
    ).rejects.toThrow(TypeError);
  });
});

const FACTORY = "0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d";
const TREASURY = "0x0000000000000000000000000000000000007EA5";
const SAFE = "0xDF8a1Ce35c9a6ACE153B4e0767942f1E2291a1Aa";
const TOKEN = "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E";
const salt = (byte: string) => `0x${byte.repeat(32)}` as const;

describe("sweeps", () => {
  // The batch file the Python SDK writes for the same calls (sdk/python/tests/test_sweeps.py).
  const fixture: BatchFile = JSON.parse(
    readFileSync(repo("sdk/testdata/safe-batch.json"), "utf8"),
  ) as BatchFile;

  it("writes the same Transaction Builder batch file as the Python SDK", () => {
    const calls = [
      flushTransaction(FACTORY, TREASURY, [salt("01"), salt("02")], TOKEN),
      flushTransaction(FACTORY, TREASURY, [salt("03")], TOKEN),
    ];
    const batch: BatchFile = safeBatch(1, SAFE, calls, {
      name: "Phala Pay sweep",
      createdAt: 1_790_000_000_000,
    });
    expect(batch).toEqual(fixture);
    // The app's validateChecksum: drop the checksum, recompute, compare.
    expect(batchChecksum(fixture)).toBe(fixture.meta.checksum);
    expect(batchChecksum({ ...fixture, chainId: "10" })).not.toBe(fixture.meta.checksum);
  });

  it("groups sweepable forwarders into one flush per treasury", () => {
    const other = `0x${"99".repeat(20)}`;
    const forwarders = [
      { chain_id: 1, factory: FACTORY, treasury: TREASURY, salt: salt("01") },
      { chain_id: 1, factory: FACTORY, treasury: other, salt: salt("02") },
      { chain_id: 1, factory: FACTORY, treasury: TREASURY, salt: salt("03") },
    ];
    expect(flushTransactions(forwarders, TOKEN)).toEqual([
      flushTransaction(FACTORY, TREASURY, [salt("01"), salt("03")], TOKEN),
      flushTransaction(FACTORY, other, [salt("02")], TOKEN),
    ]);
    const otherChain = { chain_id: 10, factory: FACTORY, treasury: TREASURY, salt: salt("04") };
    expect(() => flushTransactions([...forwarders, otherChain], TOKEN)).toThrow(
      "one chain",
    );
    expect(() => flushTransaction(FACTORY, TREASURY, [], TOKEN)).toThrow("at least one salt");
  });
});
