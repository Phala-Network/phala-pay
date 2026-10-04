// @vitest-environment node
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { PhalaPay, ApiError, type RequestOptions } from "../src/server/index.js";
import { apiKey, pins, requestUrl, errorResponse } from "./merchant-fixtures.js";

// Independent inventory of the normative resource contract, checked against service OpenAPI.
// A normal API error lets these tests inspect the actual request without inventing response objects.
const id = "resource/a?b";
const query = { limit: 1, starting_after: "cursor?&" };
const metadata = { metadata: "" } as const;
const quote = {
  client_reference_id: "team-42",
  amount: 2500,
  currency: "usd",
  chain_id: 11155111,
  asset: "pha",
};
type Case = {
  name: string;
  operation: string;
  call: (pay: PhalaPay) => Promise<unknown>;
  body?: unknown;
};
const cases: Case[] = [
  {
    name: "quotes.create",
    operation: "create_quote",
    call: (pay) => pay.quotes.create(quote),
    body: quote,
  },
  { name: "quotes.retrieve", operation: "get_quote", call: (pay) => pay.quotes.retrieve(id) },
  { name: "quotes.listPage", operation: "list_quotes", call: (pay) => pay.quotes.listPage(query) },
  {
    name: "quotes.update",
    operation: "update_quote",
    call: (pay) => pay.quotes.update(id, metadata),
    body: metadata,
  },
  { name: "quotes.cancel", operation: "cancel_quote", call: (pay) => pay.quotes.cancel(id) },
  {
    name: "depositAddresses.create",
    operation: "create_deposit_address",
    call: (pay) => pay.depositAddresses.create({ client_reference_id: "team-42" }),
    body: { client_reference_id: "team-42" },
  },
  {
    name: "depositAddresses.retrieve",
    operation: "get_deposit_address",
    call: (pay) => pay.depositAddresses.retrieve(id),
  },
  {
    name: "depositAddresses.listPage",
    operation: "list_deposit_addresses",
    call: (pay) => pay.depositAddresses.listPage(query),
  },
  {
    name: "depositAddresses.update",
    operation: "update_deposit_address",
    call: (pay) => pay.depositAddresses.update(id, metadata),
    body: metadata,
  },
  {
    name: "depositAddresses.rotate",
    operation: "rotate_deposit_address",
    call: (pay) => pay.depositAddresses.rotate(id),
  },
  { name: "deposits.retrieve", operation: "get_deposit", call: (pay) => pay.deposits.retrieve(id) },
  {
    name: "deposits.listPage",
    operation: "list_deposits",
    call: (pay) => pay.deposits.listPage(query),
  },
  {
    name: "deposits.update",
    operation: "update_deposit",
    call: (pay) => pay.deposits.update(id, metadata),
    body: metadata,
  },
  {
    name: "refunds.create",
    operation: "create_refund",
    call: (pay) =>
      pay.refunds.create({
        deposit: "dep_example",
        destination_address: pins.factory,
        amount_atomic: "202510000000000000000",
      }),
    body: {
      deposit: "dep_example",
      destination_address: pins.factory,
      amount_atomic: "202510000000000000000",
    },
  },
  { name: "refunds.retrieve", operation: "get_refund", call: (pay) => pay.refunds.retrieve(id) },
  {
    name: "refunds.listPage",
    operation: "list_refunds",
    call: (pay) => pay.refunds.listPage(query),
  },
  {
    name: "refunds.update",
    operation: "update_refund",
    call: (pay) => pay.refunds.update(id, metadata),
    body: metadata,
  },
  { name: "refunds.cancel", operation: "cancel_refund", call: (pay) => pay.refunds.cancel(id) },
  {
    name: "refunds.markPaid",
    operation: "mark_refund_paid",
    call: (pay) => pay.refunds.markPaid(id, { transaction_hash: `0x${"12".repeat(32)}` }),
    body: { transaction_hash: `0x${"12".repeat(32)}` },
  },
  {
    name: "paymentSettings.retrieve",
    operation: "get_payment_settings",
    call: (pay) => pay.paymentSettings.retrieve(),
  },
  {
    name: "paymentSettings.update",
    operation: "update_payment_settings",
    call: (pay) => pay.paymentSettings.update({}),
    body: {},
  },
  { name: "config.retrieve", operation: "get_config", call: (pay) => pay.config.retrieve() },
  { name: "account.retrieve", operation: "get_account", call: (pay) => pay.account.retrieve() },
  {
    name: "account.pauseQuotes",
    operation: "pause_account",
    call: (pay) => pay.account.pauseQuotes({ scopes: ["quotes"] }),
    body: { scopes: ["quotes"] },
  },
  {
    name: "account.resumeQuotes",
    operation: "resume_account",
    call: (pay) => pay.account.resumeQuotes({ scopes: ["quotes"] }),
    body: { scopes: ["quotes"] },
  },
  {
    name: "account.rollWebhookKey",
    operation: "roll_webhook_key",
    call: (pay) => pay.account.rollWebhookKey({ expires_in: 3600 }),
    body: { expires_in: 3600 },
  },
  {
    name: "treasuries.challenge",
    operation: "create_treasury_challenge",
    call: (pay) => pay.treasuries.challenge({ chain_id: 11155111, address: pins.factory }),
    body: { chain_id: 11155111, address: pins.factory },
  },
  {
    name: "treasuries.create",
    operation: "create_treasury",
    call: (pay) =>
      pay.treasuries.create({
        chain_id: 11155111,
        message: "signed authorization",
        signature: "0x00",
      }),
    body: { chain_id: 11155111, message: "signed authorization", signature: "0x00" },
  },
  {
    name: "treasuries.retrieve",
    operation: "get_treasury",
    call: (pay) => pay.treasuries.retrieve(id),
  },
  {
    name: "treasuries.listPage",
    operation: "list_treasuries",
    call: (pay) => pay.treasuries.listPage(query),
  },
  {
    name: "treasuries.cancel",
    operation: "cancel_treasury",
    call: (pay) => pay.treasuries.cancel(id),
  },
  {
    name: "treasuries.pause",
    operation: "pause_treasury",
    call: (pay) => pay.treasuries.pause(id),
  },
  {
    name: "treasuries.resume",
    operation: "resume_treasury",
    call: (pay) => pay.treasuries.resume(id),
  },
  {
    name: "apiKeys.create secret",
    operation: "create_api_key",
    call: (pay) => pay.apiKeys.create({}),
    body: {},
  },
  {
    name: "apiKeys.create restricted",
    operation: "create_api_key",
    call: (pay) => pay.apiKeys.create({ permissions: ["quotes.write"] }),
    body: { permissions: ["quotes.write"] },
  },
  { name: "apiKeys.retrieve", operation: "get_api_key", call: (pay) => pay.apiKeys.retrieve(id) },
  {
    name: "apiKeys.listPage",
    operation: "list_api_keys",
    call: (pay) => pay.apiKeys.listPage(query),
  },
  {
    name: "apiKeys.roll",
    operation: "roll_api_key",
    call: (pay) => pay.apiKeys.roll(id, {}),
    body: {},
  },
  { name: "apiKeys.revoke", operation: "revoke_api_key", call: (pay) => pay.apiKeys.revoke(id) },
  {
    name: "webhookEndpoints.create",
    operation: "create_webhook_endpoint",
    call: (pay) =>
      pay.webhookEndpoints.create({
        url: "https://merchant.example/webhook",
        enabled_events: ["deposit.credited"],
      }),
    body: { url: "https://merchant.example/webhook", enabled_events: ["deposit.credited"] },
  },
  {
    name: "webhookEndpoints.retrieve",
    operation: "get_webhook_endpoint",
    call: (pay) => pay.webhookEndpoints.retrieve(id),
  },
  {
    name: "webhookEndpoints.listPage",
    operation: "list_webhook_endpoints",
    call: (pay) => pay.webhookEndpoints.listPage(query),
  },
  {
    name: "webhookEndpoints.update",
    operation: "update_webhook_endpoint",
    call: (pay) => pay.webhookEndpoints.update(id, { disabled: true }),
    body: { disabled: true },
  },
  {
    name: "webhookEndpoints.delete",
    operation: "delete_webhook_endpoint",
    call: (pay) => pay.webhookEndpoints.delete(id),
  },
  {
    name: "webhookEndpoints.test",
    operation: "test_webhook_endpoint",
    call: (pay) => pay.webhookEndpoints.test(id),
  },
  { name: "events.retrieve", operation: "get_event", call: (pay) => pay.events.retrieve(id) },
  { name: "events.listPage", operation: "list_events", call: (pay) => pay.events.listPage(query) },
  {
    name: "events.resend",
    operation: "resend_event",
    call: (pay) => pay.events.resend(id, { webhook_endpoint: "we_example" }),
    body: { webhook_endpoint: "we_example" },
  },
  { name: "balance.retrieve", operation: "get_balance", call: (pay) => pay.balance.retrieve() },
  { name: "sweeps.listPage", operation: "list_sweeps", call: (pay) => pay.sweeps.listPage(query) },
  {
    name: "forwarders.listPage",
    operation: "list_forwarders",
    call: (pay) => pay.forwarders.listPage(query),
  },
];
const api = JSON.parse(
  readFileSync(resolve(process.cwd(), "../../crates/topup/openapi.json"), "utf8"),
) as { paths: Record<string, Record<string, { operationId: string }>> };
describe("normative resource wire contract", () => {
  it.each(cases)(
    "$name matches OpenAPI operationId, wire fields and POST idempotency",
    async (item) => {
      const fetch = vi
        .fn<typeof globalThis.fetch>()
        .mockImplementation(() => Promise.resolve(errorResponse(400, "contract_probe")));
      const pay = new PhalaPay({ apiKey, pins, fetch });
      await expect(item.call(pay)).rejects.toBeInstanceOf(ApiError);
      const route = Object.entries(api.paths)
        .flatMap(([path, methods]) =>
          Object.entries(methods).map(([method, operation]) => ({
            path,
            method,
            operation: operation.operationId,
          })),
        )
        .find((operation) => operation.operation === item.operation);
      if (!route) throw new Error(`Missing service operation ${item.operation}`);
      const [input, init] = fetch.mock.calls[0] ?? [];
      const url = new URL(requestUrl(input));
      expect(init?.method).toBe(route.method.toUpperCase());
      expect(url.pathname).toBe(route.path.replace("{id}", encodeURIComponent(id)));
      expect(init?.body).toBe(item.body === undefined ? undefined : JSON.stringify(item.body));
      const headers = new Headers(init?.headers);
      expect(headers.get("authorization")).toBe(`Bearer ${apiKey}`);
      expect(headers.get("idempotency-key")).toEqual(
        route.method === "post" ? expect.stringMatching(/^[a-f0-9-]{36}$/) : null,
      );
      if (item.name.endsWith("listPage")) {
        expect(url.searchParams.get("limit")).toBe("1");
        expect(url.searchParams.get("starting_after")).toBe(query.starting_after);
      }
      expect(fetch).toHaveBeenCalledTimes(1);
      await pay.close();
    },
  );
  it("covers every merchant OpenAPI operation without including attestation/setup", () => {
    const expected = Object.values(api.paths)
      .flatMap((methods) => Object.values(methods).map((operation) => operation.operationId))
      .filter((operation) => operation !== "get_attestation");
    expect(new Set(cases.map((item) => item.operation))).toEqual(new Set(expected));
  });
  const iterators: {
    name: string;
    list: (pay: PhalaPay, options?: RequestOptions) => AsyncIterable<unknown>;
  }[] = [
    { name: "quotes", list: (pay, options) => pay.quotes.list(query, options) },
    { name: "depositAddresses", list: (pay, options) => pay.depositAddresses.list(query, options) },
    { name: "deposits", list: (pay, options) => pay.deposits.list(query, options) },
    { name: "refunds", list: (pay, options) => pay.refunds.list(query, options) },
    { name: "treasuries", list: (pay, options) => pay.treasuries.list(query, options) },
    { name: "apiKeys", list: (pay, options) => pay.apiKeys.list(query, options) },
    { name: "webhookEndpoints", list: (pay, options) => pay.webhookEndpoints.list(query, options) },
    { name: "events", list: (pay, options) => pay.events.list(query, options) },
    { name: "sweeps", list: (pay, options) => pay.sweeps.list(query, options) },
    { name: "forwarders", list: (pay, options) => pay.forwarders.list(query, options) },
  ];
  it.each(iterators)(
    "$name.list is a lazy AsyncIterable with query/options forwarding",
    async (item) => {
      const fetch = vi
        .fn<typeof globalThis.fetch>()
        .mockImplementation(() => Promise.resolve(errorResponse(400, "contract_probe")));
      const pay = new PhalaPay({ apiKey, pins, fetch });
      const iterable = item.list(pay);
      expect(fetch).not.toHaveBeenCalled();
      await expect(iterable[Symbol.asyncIterator]().next()).rejects.toBeInstanceOf(ApiError);
      const url = new URL(requestUrl(fetch.mock.calls[0]?.[0]));
      expect(url.searchParams.get("starting_after")).toBe(query.starting_after);
      expect(fetch.mock.calls[0]?.[1]?.method).toBe("GET");
      fetch.mockClear();
      const controller = new AbortController();
      controller.abort();
      await expect(
        item.list(pay, { signal: controller.signal })[Symbol.asyncIterator]().next(),
      ).rejects.toMatchObject({ code: "cancelled" });
      expect(fetch).not.toHaveBeenCalled();
      await pay.close();
    },
  );
});
