import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import type { Quote } from "../src/index.js";
import { parsePins, quoteAddress, type Pins } from "../src/helpers.js";
export const apiKey = "ppay_rk_test_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA3s89I1";
// Typed fixture loading is the JSON boundary for tests.
// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters
export function fixture<T>(name: string): T {
  return JSON.parse(readFileSync(resolve(process.cwd(), "../fixtures", name), "utf8")) as T;
}
export const pinsFixture = fixture<{
  canonical_encoding: string;
  canonical_json: string;
  valid: Pins[];
  rejections: { name: string; input: string }[];
}>("pins-v1.json");
export const pins = parsePins(pinsFixture.canonical_encoding);
const api = JSON.parse(
  readFileSync(resolve(process.cwd(), "../../crates/topup/openapi.json"), "utf8"),
) as { components: { schemas: Record<string, { example?: Record<string, unknown> }> } };
export function example(name: string): Record<string, unknown> {
  const value = api.components.schemas[name]?.example;
  if (!value) throw new Error(`Missing OpenAPI example ${name}`);
  return structuredClone(value);
}
export function quoteResponse(overrides: Partial<Quote> = {}): Quote {
  const quote = {
    ...example("Quote"),
    expires_at: Math.floor(Date.now() / 1000) + 900,
    treasury: pins.treasuries["11155111"],
    chain_id: 11155111,
    ...overrides,
  } as Quote;
  return { ...quote, address: quoteAddress(pins, quote, quote.treasury, pins.account) };
}
export function jsonResponse(value: unknown, status = 200, headers: HeadersInit = {}): Response {
  const normalized = new Headers(headers);
  normalized.set("content-type", "application/json");
  return new Response(JSON.stringify(value), { status, headers: normalized });
}
export function errorResponse(
  status = 503,
  code = "service_unavailable",
  headers: Record<string, string> = {},
): Response {
  return jsonResponse(
    { error: { type: "api_error", code, message: "Payment unavailable" } },
    status,
    headers,
  );
}

export function requestUrl(input: RequestInfo | URL | undefined): string {
  return input instanceof Request ? input.url : input instanceof URL ? input.href : (input ?? "");
}
