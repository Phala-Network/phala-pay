// @vitest-environment node
import { describe, expect, it } from "vitest";
import {
  PhalaPay,
  parsePins,
  encodePins,
  ConfigurationError,
  LedgerSnapshotError,
  balanceDelta,
  depositNetAmount,
  type LedgerSnapshot,
} from "../src/index.js";
import { apiKey, pins, pinsFixture, fixture } from "./merchant-fixtures.js";
const wrap = (text: string) => `ppay_pins_v1.${Buffer.from(text).toString("base64url")}`;
describe("shared pins fixtures", () => {
  it("encodes canonically, freezes recursively, accepts key order/case/origin normalization", () => {
    for (const value of pinsFixture.valid)
      expect(encodePins(value)).toBe(pinsFixture.canonical_encoding);
    expect(encodePins(parsePins(pinsFixture.canonical_encoding))).toBe(
      pinsFixture.canonical_encoding,
    );
    expect(Object.isFrozen(pins)).toBe(true);
    expect(Object.isFrozen(pins.treasuries)).toBe(true);
    expect(Object.isFrozen(pins.webhook_keys[0])).toBe(true);
    expect(
      parsePins(
        wrap(
          JSON.stringify({
            ...pins,
            api_base: "HTTPS://API.PHALA-PAY.EXAMPLE:443/",
            factory: pins.factory.toUpperCase().replace("0X", "0x"),
          }),
        ),
      ),
    ).toEqual(pins);
  });
  for (const vector of pinsFixture.rejections)
    it(`rejects ${vector.name}`, () => {
      if (vector.name === "mode_mismatch") {
        expect(() => new PhalaPay({ apiKey, pins: { ...pins, livemode: true } })).toThrow(
          ConfigurationError,
        );
        return;
      }
      let input = vector.input;
      if (input.startsWith("{")) input = wrap(input);
      else if (vector.name === "oversize") input = wrap(" ".repeat(17 * 1024));
      else if (
        ["account_format", "address_format", "chain_format", "webhook_key_format"].includes(
          vector.name,
        )
      ) {
        const value = {
          ...pins,
          ...(vector.name === "account_format"
            ? { account: input }
            : vector.name === "address_format"
              ? { factory: input }
              : vector.name === "chain_format"
                ? { treasuries: { [input]: pins.factory } }
                : { webhook_keys: [{ version: 1, public_key: input }] }),
        };
        input = wrap(JSON.stringify(value));
      }
      expect(() => parsePins(input)).toThrow(ConfigurationError);
    });
  it("rejects padding, unused bits, zero addresses, duplicate keys, unsafe chains and invalid origins", () => {
    const changes = [
      { factory: `0x${"0".repeat(40)}` },
      { treasuries: {} },
      { treasuries: { "9007199254740992": pins.factory } },
      { webhook_keys: [...pins.webhook_keys, ...pins.webhook_keys] },
      { webhook_keys: [{ version: 0, public_key: pins.webhook_keys[0]?.public_key }] },
      ...[
        "http://api.example",
        "https://api.example/path",
        "https://user:pass@api.example",
        "https://api.example?",
        "https://api.example#",
      ].map((api_base) => ({ api_base })),
    ];
    for (const change of changes)
      expect(() => parsePins(wrap(JSON.stringify({ ...pins, ...change })))).toThrow(
        ConfigurationError,
      );
    for (const input of [pinsFixture.canonical_encoding + "=", "ppay_pins_v1.e31"])
      expect(() => parsePins(input)).toThrow(ConfigurationError);
    expect(() =>
      parsePins(
        wrap(
          pinsFixture.canonical_json.replace(
            '"livemode":false',
            '"livemode":false,"livemode":false',
          ),
        ),
      ),
    ).toThrow(ConfigurationError);
  });
  it("sorts textual chain keys and webhook versions and rejects duplicate public keys", () => {
    const secondKey = `whpk_${Buffer.alloc(32, 7).toString("base64")}`;
    const value = {
      ...pins,
      treasuries: { "2": pins.factory, "10": pins.implementation },
      webhook_keys: [{ version: 2, public_key: secondKey }, ...pins.webhook_keys],
    };
    const encoded = encodePins(value);
    const json = Buffer.from(encoded.slice(13), "base64url").toString("utf8");
    expect(json.indexOf('"10"')).toBeLessThan(json.indexOf('"2"'));
    expect(parsePins(encoded).webhook_keys.map((key) => key.version)).toEqual([1, 2]);
    expect(() =>
      encodePins({
        ...pins,
        webhook_keys: [
          { version: 2, public_key: pins.webhook_keys[0]?.public_key ?? "" },
          ...pins.webhook_keys,
        ],
      }),
    ).toThrow(ConfigurationError);
  });
  it("rejects missing pins in either mode and exposes readonly pins/mode", () => {
    for (const key of [apiKey, "ppay_rk_live_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA3hNyVt"])
      expect(() => new PhalaPay({ apiKey: key, pins: "" })).toThrow(ConfigurationError);
    const pay = new PhalaPay({ apiKey, pins });
    expect(Reflect.set(pay, "pins", {})).toBe(false);
    expect(Reflect.set(pay, "livemode", true)).toBe(false);
    expect(pay.pins).toEqual(pins);
    expect(() =>
      parsePins(wrap(JSON.stringify({ ...pins, livemode: true, api_base: "http://127.0.0.1" }))),
    ).toThrow(ConfigurationError);
  });
  it("reads exactly two env values and rejects bad keys, mode and alternate origins", () => {
    const reads: string[] = [];
    const env = new Proxy(
      { PHALA_PAY_API_KEY: apiKey, PHALA_PAY_PINS: pinsFixture.canonical_encoding },
      {
        get(target, key: string) {
          reads.push(key);
          return Reflect.get(target, key) as string | undefined;
        },
      },
    );
    expect(PhalaPay.fromEnv(env).pins).toEqual(pins);
    expect(reads).toEqual(["PHALA_PAY_API_KEY", "PHALA_PAY_PINS"]);
    for (const key of [apiKey.slice(0, -1) + "0", "ppay_sk_test_", ""])
      expect(() => new PhalaPay({ apiKey: key, pins })).toThrow(ConfigurationError);
    expect(() => new PhalaPay({ apiKey, pins, apiBase: "https://other.example" })).toThrow(
      ConfigurationError,
    );
    expect(() => PhalaPay.fromEnv({})).toThrow(ConfigurationError);
  });
});
const vectors = fixture<{
  deposit_net_amount_cases: { deposit: LedgerSnapshot; expected: number }[];
  convergence_cases: {
    name: string;
    events: LedgerSnapshot[];
    expected_delta?: number;
    error?: string;
  }[];
}>("ledger-v1.json");
describe("shared ledger fixtures", () => {
  for (const [index, vector] of vectors.deposit_net_amount_cases.entries())
    it(`net amount ${index}`, () => {
      expect(depositNetAmount(vector.deposit)).toBe(vector.expected);
    });
  for (const vector of vectors.convergence_cases)
    it(vector.name, () => {
      const apply = (events: LedgerSnapshot[]) => {
        let snapshot: LedgerSnapshot | null = null;
        let balance = 0;
        for (const event of events) {
          const result = balanceDelta(snapshot, event);
          snapshot = result.snapshot;
          balance += result.delta;
        }
        return balance;
      };
      if (vector.error) expect(() => apply(vector.events)).toThrow(LedgerSnapshotError);
      else {
        expect(apply(vector.events)).toBe(vector.expected_delta);
        expect(apply([...vector.events].reverse())).toBe(vector.expected_delta);
      }
    });
  const valued = vectors.deposit_net_amount_cases[0]?.deposit;
  if (!valued) throw new Error("Missing ledger fixture");
  it("converges refund maxima and duplicates without mutation", () => {
    const refunded = { ...valued, amount_refunded: 1000 };
    const before = structuredClone(refunded);
    const a = balanceDelta(null, refunded);
    const b = balanceDelta(a.snapshot, { ...valued, amount_refunded: 500 });
    expect(a.delta).toBe(1500);
    expect(b.delta).toBe(0);
    expect(b.contribution).toBe(1500);
    expect(refunded).toEqual(before);
    expect(Object.isFrozen(b.snapshot)).toBe(true);
  });
  it("values an unvalued reversal when an older valuation arrives", () => {
    const pending = { ...valued, amount: null, status: "pending" };
    const reversed = { ...pending, status: "reversed" };
    const credited = balanceDelta(pending, valued);
    expect(credited).toMatchObject({ contribution: 2500, delta: 2500 });
    expect(balanceDelta(credited.snapshot, pending)).toMatchObject({
      contribution: 2500,
      delta: 0,
    });
    const first = balanceDelta(null, reversed);
    expect(balanceDelta(first.snapshot, valued)).toMatchObject({
      contribution: 0,
      delta: 0,
      snapshot: { status: "reversed", amount: 2500, amount_reversed: 2500 },
    });
    expect(balanceDelta(null, pending).delta).toBe(0);
    expect(balanceDelta(null, { ...pending, status: "rejected" }).delta).toBe(0);
  });
  it("rejects unknown statuses, conflicts, unsafe/negative values and impossible deductions", () => {
    const bad = [
      { status: "future" },
      { status: "refunded" },
      { amount: 1.5 },
      { amount: -1 },
      { amount: NaN },
      { amount: Infinity },
      { amount_refunded: -1 },
      { amount_refunded: 0.5 },
      { amount_reversed: Number.MAX_SAFE_INTEGER + 1 },
      { amount: Number.MAX_SAFE_INTEGER + 1 },
      { amount: null },
      { amount_refunded: 2501 },
      { amount_reversed: 2500 },
      { status: "reversed", amount_reversed: 1 },
      { status: "reversed", amount_reversed: 2500, amount_refunded: 1 },
      { amount: null, status: "pending", amount_refunded: 1 },
    ];
    for (const change of bad)
      expect(() => depositNetAmount({ ...valued, ...change })).toThrow(LedgerSnapshotError);
    for (const change of [
      { id: "other" },
      { livemode: true },
      { client_reference_id: "other" },
      { currency: "eur" },
      { amount: 3000 },
      { status: "rejected" },
    ])
      expect(() => balanceDelta(valued, { ...valued, ...change })).toThrow(LedgerSnapshotError);
    expect(() =>
      balanceDelta(
        { ...valued, amount_refunded: 500 },
        { ...valued, status: "reversed", amount_reversed: 2500 },
      ),
    ).toThrow(LedgerSnapshotError);
  });
});
