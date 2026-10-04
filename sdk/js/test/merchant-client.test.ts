// @vitest-environment node
import { generateKeyPairSync, sign } from "node:crypto";
import { createServer } from "node:http";
import { once } from "node:events";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  PhalaPay,
  AddressMismatchError,
  ConfigurationError,
  ResponseValidationError,
  SignatureVerificationError,
  quoteAddress,
  depositAddress,
  type Quote,
} from "../src/server/index.js";
import type { CheckoutParams } from "../src/index.js";
import {
  requestUrl,
  apiKey,
  pins,
  quoteResponse,
  jsonResponse,
  example,
  fixture,
} from "./merchant-fixtures.js";
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});
const params = {
  client_reference_id: "team-42",
  amount: 2500,
  currency: "usd",
  chain_id: 11155111,
  asset: "pha",
};
const makeClient = (response: unknown) =>
  new PhalaPay({
    apiKey,
    pins,
    fetch: vi.fn<typeof fetch>().mockImplementation(() => Promise.resolve(jsonResponse(response))),
  });
describe("merchant client resources and checkout", () => {
  it("rejects non-Node compatibility shims before reading credentials", () => {
    vi.stubGlobal("Deno", {});
    const getter = vi.fn(() => apiKey);
    const options = {
      get apiKey() {
        return getter();
      },
      pins,
    };
    expect(() => new PhalaPay(options)).toThrow(ConfigurationError);
    expect(getter).not.toHaveBeenCalled();
    const env = new Proxy(
      {},
      {
        get() {
          throw new Error("Environment read");
        },
      },
    );
    expect(() => PhalaPay.fromEnv(env)).toThrow(ConfigurationError);
  });
  it("creates a verified quote and returns the shared browser handoff", async () => {
    const response = { ...quoteResponse(), future_field: { enabled: true } };
    const pay = makeClient(response);
    const quote = await pay.quotes.create(params, { idempotencyKey: "order-1" });
    const checkout: CheckoutParams = pay.checkoutParams(quote);
    expect(checkout).toEqual({
      clientSecret: response.client_secret,
      expectedAddress: response.address,
      apiBase: pins.api_base,
    });
    expect(quote).toMatchObject({ future_field: { enabled: true } });
    expect(() => pay.checkoutParams({ ...quote })).toThrow(ResponseValidationError);
    expect(() => makeClient(response).checkoutParams(quote)).toThrow(ResponseValidationError);
  });
  it("rejects retrieved, closed, expired, missing or changed-secret quotes", async () => {
    const response = quoteResponse();
    const pay = makeClient(response);
    const retrieved = await pay.quotes.retrieve(response.id);
    expect(() => pay.checkoutParams(retrieved as Quote)).toThrow(ResponseValidationError);
    for (const overrides of [{ status: "canceled" }, { expires_at: 0 }, { client_secret: null }]) {
      const client = makeClient(quoteResponse(overrides));
      const quote = await client.quotes.create(params);
      expect(() => client.checkoutParams(quote)).toThrow(ResponseValidationError);
    }
    const quote = await pay.quotes.create(params);
    Object.assign(quote, { client_secret: `${quote.id}_secret_changed` });
    expect(() => pay.checkoutParams(quote)).toThrow(ResponseValidationError);
  });
  it("rejects treasury/address substitution, missing pins, response identity/mode on every quote action/page", async () => {
    const quote = quoteResponse();
    const substitutions = [
      { address: pins.factory },
      { treasury: pins.factory },
      { chain_id: 1 },
      { livemode: true },
      { account: "acct_other" },
    ];
    for (const substitution of substitutions) {
      const bad = { ...quote, ...substitution };
      const pay = makeClient(bad);
      await expect(pay.quotes.create(params)).rejects.toBeInstanceOf(ResponseValidationError);
      await expect(pay.quotes.retrieve(quote.id)).rejects.toBeInstanceOf(ResponseValidationError);
      await expect(pay.quotes.update(quote.id, { metadata: {} })).rejects.toBeInstanceOf(
        ResponseValidationError,
      );
      await expect(pay.quotes.cancel(quote.id)).rejects.toBeInstanceOf(ResponseValidationError);
      const pagePay = makeClient({
        object: "list",
        url: "/v1/quotes",
        data: [bad],
        has_more: false,
      });
      await expect(pagePay.quotes.listPage()).rejects.toBeInstanceOf(ResponseValidationError);
    }
    const pay = makeClient(quote);
    const created = await pay.quotes.create(params);
    Object.assign(created, { address: pins.factory });
    expect(() => pay.checkoutParams(created)).toThrow(AddressMismatchError);
  });
  it("retains historical quotes without making them payable; ignores future response enums", async () => {
    const pay = makeClient({
      ...quoteResponse({ status: "future_closed" }),
      treasury: pins.factory,
      address: pins.factory,
    });
    const result = await pay.quotes.create(params);
    expect(result.status).toBe("future_closed");
    expect(() => pay.checkoutParams(result)).toThrow(ResponseValidationError);
  });
  it("verifies all active deposit-address networks on create/retrieve/update/rotate/page", async () => {
    const address: Record<string, unknown> = {
      ...example("DepositAddress"),
      client_reference_id: "team-42",
      livemode: false,
      version: 1,
    };
    const treasury = pins.treasuries["11155111"];
    if (!treasury) throw new Error("Missing treasury");
    const derived = depositAddress(
      pins,
      { client_reference_id: "team-42", livemode: false, version: 1 },
      treasury,
      pins.account,
    );
    const network = {
      ...(address["networks"] as Record<string, unknown>[])[0],
      chain_id: 11155111,
      treasury,
      address: derived,
    };
    const good = { ...address, address: derived, networks: [network] };
    const verified = await makeClient(good).depositAddresses.create({
      client_reference_id: "team-42",
    });
    expect(verified.networks[0]?.address).toBe(derived);
    for (const change of [{ treasury: pins.factory }, { address: pins.factory }, { chain_id: 1 }]) {
      const bad = { ...good, networks: [network, { ...network, ...change }] };
      const pay = makeClient(bad);
      await expect(
        pay.depositAddresses.create({ client_reference_id: "team-42" }),
      ).rejects.toBeInstanceOf(AddressMismatchError);
      await expect(pay.depositAddresses.retrieve(String(address["id"]))).rejects.toBeInstanceOf(
        AddressMismatchError,
      );
      await expect(
        pay.depositAddresses.update(String(address["id"]), { metadata: {} }),
      ).rejects.toBeInstanceOf(AddressMismatchError);
      await expect(pay.depositAddresses.rotate(String(address["id"]))).rejects.toBeInstanceOf(
        AddressMismatchError,
      );
      await expect(
        makeClient({
          object: "list",
          url: "/v1/deposit_addresses",
          data: [bad],
          has_more: false,
        }).depositAddresses.listPage(),
      ).rejects.toBeInstanceOf(AddressMismatchError);
    }
  });
  it("paginates automatically, encodes cursors and rejects empty/repeated continuing pages", async () => {
    const first = quoteResponse();
    const second = quoteResponse({ id: "qt_22222222222222222222222222222222" });
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockResolvedValueOnce(
        jsonResponse({ object: "list", url: "/v1/quotes", data: [first], has_more: true }),
      )
      .mockResolvedValueOnce(
        jsonResponse({ object: "list", url: "/v1/quotes", data: [second], has_more: false }),
      );
    const client = new PhalaPay({ apiKey, pins, fetch });
    const collected = [];
    for await (const quote of client.quotes.list({ limit: 1 })) collected.push(quote.id);
    expect(collected).toEqual([first.id, second.id]);
    expect(new URL(requestUrl(fetch.mock.calls[1]?.[0])).searchParams.get("starting_after")).toBe(
      first.id,
    );
    for (const data of [[], [first]]) {
      const pay = makeClient({ object: "list", url: "/v1/quotes", data, has_more: true });
      await expect(
        (async () => {
          for await (const quote of pay.quotes.list({ starting_after: first.id }))
            expect(quote.id).toBe(first.id);
        })(),
      ).rejects.toBeInstanceOf(ResponseValidationError);
    }
  });
  it("routes account actions, optional retrieve params and encodes resource IDs", async () => {
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockImplementation(() => Promise.resolve(jsonResponse(quoteResponse())));
    const pay = new PhalaPay({ apiKey, pins, fetch });
    await pay.quotes.retrieve("qt_a/b?#", { "expand[]": ["deposit"] }, { requestDeadlineMs: 1000 });
    expect(requestUrl(fetch.mock.calls[0]?.[0])).toBe(
      `${pins.api_base}/v1/quotes/qt_a%2Fb%3F%23?expand%5B%5D=deposit`,
    );
    await expect(pay.quotes.create({ ...params, amount: 0.1 })).rejects.toBeInstanceOf(
      ConfigurationError,
    );
    const account = { ...example("AccountObject"), id: pins.account };
    const accountFetch = vi
      .fn<typeof globalThis.fetch>()
      .mockImplementation(() => Promise.resolve(jsonResponse(account)));
    const accountPay = new PhalaPay({ apiKey, pins, fetch: accountFetch });
    await accountPay.account.pauseQuotes({ scopes: ["quotes"] });
    await accountPay.account.resumeQuotes({ scopes: ["quotes"] });
    await accountPay.account.rollWebhookKey({});
    expect(accountFetch.mock.calls.map(([url]) => new URL(requestUrl(url)).pathname)).toEqual([
      "/v1/account/pause",
      "/v1/account/resume",
      "/v1/account/webhook_keys/roll",
    ]);
  });
  it("attaches status/request ID to malformed schemas", async () => {
    const pay = new PhalaPay({
      apiKey,
      pins,
      fetch: vi
        .fn<typeof fetch>()
        .mockResolvedValue(jsonResponse({ object: "quote" }, 200, { "request-id": "req_bad" })),
    });
    await expect(pay.quotes.create(params)).rejects.toMatchObject({
      statusCode: 200,
      requestId: "req_bad",
    });
  });
});
describe("bound webhooks", () => {
  const pair = generateKeyPairSync("ed25519");
  const publicKey = `whpk_${pair.publicKey.export({ type: "spki", format: "der" }).subarray(-32).toString("base64")}`;
  const pay = new PhalaPay({
    apiKey,
    pins: { ...pins, webhook_keys: [{ version: 1, public_key: publicKey }] },
  });
  const envelope = (type = "deposit.credited") => ({
    id: "evt_26262626262626262626262626262626",
    object: "event",
    account: pins.account,
    livemode: false,
    type,
    created: Math.floor(Date.now() / 1000),
    actor: "system",
    request: null,
    data: { object: example("Deposit") },
    extra: "preserved",
  });
  const signed = (value: unknown, offset = 0) => {
    const body = JSON.stringify(value);
    const timestamp = Math.floor(Date.now() / 1000) + offset;
    const id = "evt_26262626262626262626262626262626";
    return {
      body: Buffer.from(body),
      headers: {
        "WebHook-ID": id,
        "Webhook-Timestamp": String(timestamp),
        "Webhook-Signature": `v1a,${sign(null, Buffer.from(`${id}.${timestamp}.${body}`), pair.privateKey).toString("base64")}`,
      },
    };
  };
  it("returns typed deposits, retains unknown types and fields; the method stays bound", async () => {
    const construct = pay.webhooks.constructEvent;
    const delivery = signed(envelope());
    const event = await construct(delivery.body, delivery.headers);
    expect(event.deposit?.amount).toBe(2500);
    expect(event).toMatchObject({ extra: "preserved" });
    const previousNull = signed({
      ...envelope(),
      data: { object: example("Deposit"), previous_attributes: null },
    });
    expect(
      (await construct(previousNull.body, previousNull.headers)).data.previous_attributes,
    ).toBeNull();
    const future = signed(envelope("deposit.future"));
    expect((await construct(future.body, future.headers)).deposit).toBeUndefined();
  });
  it("enforces inclusive bilateral tolerance and zero exact match", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-04T00:00:00Z"));
    for (const offset of [-300, 300, 0]) {
      const delivery = signed(envelope(), offset);
      await expect(
        pay.webhooks.constructEvent(delivery.body, delivery.headers),
      ).resolves.toHaveProperty("id");
    }
    for (const offset of [-301, 301]) {
      const delivery = signed(envelope(), offset);
      await expect(
        pay.webhooks.constructEvent(delivery.body, delivery.headers),
      ).rejects.toBeInstanceOf(SignatureVerificationError);
    }
    const exact = signed(envelope());
    await expect(
      pay.webhooks.constructEvent(exact.body, exact.headers, { tolerance: 0 }),
    ).resolves.toHaveProperty("id");
    const older = signed(envelope(), -1);
    await expect(
      pay.webhooks.constructEvent(older.body, older.headers, { tolerance: 0 }),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
    await expect(
      pay.webhooks.constructEvent(exact.body, exact.headers, { tolerance: NaN }),
    ).rejects.toBeInstanceOf(ConfigurationError);
  });
  it("rejects duplicate headers, altered bytes, wrong keys/account/mode/id/envelope/resource", async () => {
    const valid = signed(envelope());
    await expect(
      pay.webhooks.constructEvent(valid.body, {
        ...valid.headers,
        "webhook-id": valid.headers["WebHook-ID"],
      }),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
    await expect(
      pay.webhooks.constructEvent(valid.body, {
        ...valid.headers,
        "Webhook-Signature": [
          valid.headers["Webhook-Signature"],
          valid.headers["Webhook-Signature"],
        ],
      }),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
    const duplicate = new Headers(valid.headers);
    duplicate.append("webhook-signature", valid.headers["Webhook-Signature"]);
    await expect(pay.webhooks.constructEvent(valid.body, duplicate)).rejects.toBeInstanceOf(
      SignatureVerificationError,
    );
    await expect(
      pay.webhooks.constructEvent(Buffer.concat([valid.body, Buffer.from(" ")]), valid.headers),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
    await expect(
      new PhalaPay({ apiKey, pins }).webhooks.constructEvent(valid.body, valid.headers),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
    for (const change of [
      { account: "acct_other" },
      { livemode: true },
      { id: "evt_other" },
      { request: {} },
      { data: { object: {} } },
      { created: 0.1 },
      { pending_webhooks: "invalid" },
      { data: { object: example("Deposit"), previous_attributes: [] } },
    ]) {
      const bad = signed({ ...envelope(), ...change });
      await expect(pay.webhooks.constructEvent(bad.body, bad.headers)).rejects.toBeInstanceOf(
        SignatureVerificationError,
      );
    }
  });
  it("ignores clock/trust override fields and rejects lossy UTF-8 strings", async () => {
    const old = signed(envelope(), -1000);
    const overrides = {
      tolerance: 0,
      now: Number(old.headers["Webhook-Timestamp"]),
      expectedAccount: pins.account,
    };
    await expect(
      pay.webhooks.constructEvent(old.body, old.headers, overrides),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
    const value = envelope();
    const lossy = JSON.stringify(value).replace(
      '"extra":"preserved"',
      '"extra":"' + String.fromCharCode(0xd800) + '"',
    );
    const timestamp = Math.floor(Date.now() / 1000);
    const signature = sign(
      null,
      Buffer.from(`${value.id}.${timestamp}.${lossy}`),
      pair.privateKey,
    ).toString("base64");
    await expect(
      pay.webhooks.constructEvent(lossy, {
        "webhook-id": value.id,
        "webhook-timestamp": String(timestamp),
        "webhook-signature": `v1a,${signature}`,
      }),
    ).rejects.toBeInstanceOf(SignatureVerificationError);
  });
  it("verifies all shared webhook vectors using the pinned client identity", async () => {
    const vectors = fixture<{
      public_key: string;
      cases: {
        body: string;
        headers: Record<string, string>;
        expected_account: string;
        expected_livemode: boolean;
        now: number;
        tolerance?: number;
        outcome: string;
      }[];
    }>("webhooks-v1.json");
    vi.useFakeTimers();
    // This fixture key is public; this locally checksummed live key is never a service credential.
    const liveKey = "ppay_rk_live_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA3hNyVt";
    for (const vector of vectors.cases) {
      vi.setSystemTime(vector.now * 1000);
      const client = new PhalaPay({
        apiKey: vector.expected_livemode ? liveKey : apiKey,
        pins: {
          ...pins,
          account: vector.expected_account,
          livemode: vector.expected_livemode,
          webhook_keys: [{ version: 1, public_key: vectors.public_key }],
        },
      });
      const result = client.webhooks.constructEvent(
        vector.body,
        vector.headers,
        vector.tolerance === undefined ? {} : { tolerance: vector.tolerance },
      );
      if (vector.outcome === "accept") await expect(result).resolves.toHaveProperty("id");
      else await expect(result).rejects.toBeInstanceOf(SignatureVerificationError);
    }
  });
});
it("integrates native fetch with a local HTTP service, POST replay and redirect refusal", async () => {
  let attempts = 0;
  const keys: string[] = [];
  const bodies: string[] = [];
  const server = createServer((request, response) => {
    request.on("error", () => {
      response.destroy();
    });
    response.on("error", () => {});
    if (request.url === "/v1/config") {
      response.writeHead(302, { location: "https://evil.example" });
      response.end();
      return;
    }
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk: string) => {
      body += chunk;
    });
    request.on("end", () => {
      keys.push(String(request.headers["idempotency-key"]));
      bodies.push(body);
      attempts++;
      response.writeHead(attempts === 1 ? 503 : 200, { "content-type": "application/json" });
      response.end(
        JSON.stringify(
          attempts === 1
            ? { error: { type: "api_error", code: "unavailable", message: "Unavailable" } }
            : quoteResponse(),
        ),
      );
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("No local port");
  const pay = new PhalaPay({
    apiKey,
    pins: { ...pins, api_base: `http://127.0.0.1:${address.port}` },
  });
  try {
    const quote = await pay.quotes.create(params, { idempotencyKey: "local-order-1" });
    expect(pay.checkoutParams(quote).expectedAddress).toBe(
      quoteAddress(pins, quote, quote.treasury, pins.account),
    );
    expect(keys).toEqual(["local-order-1", "local-order-1"]);
    expect(bodies[0]).toBe(bodies[1]);
    await expect(pay.config.retrieve()).rejects.toBeInstanceOf(ResponseValidationError);
  } finally {
    await pay.close();
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => {
      server.close((error) => {
        if (error) reject(error);
        else resolve();
      });
    });
  }
});
