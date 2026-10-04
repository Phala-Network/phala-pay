// @vitest-environment node
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Transport } from "../src/server/transport.js";
import {
  ApiError,
  TransportError,
  ConfigurationError,
  ResponseValidationError,
} from "../src/server/index.js";
import {
  requestUrl,
  apiKey,
  pins,
  fixture,
  errorResponse,
  jsonResponse,
} from "./merchant-fixtures.js";
const scenarios = fixture<{
  scenarios: {
    name: string;
    method: string;
    responses: number[];
    expected_attempts: number;
    retry_after_seconds?: number;
    deadline_seconds?: number;
  }[];
}>("transport-v1.json").scenarios;
describe("merchant transport", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date", "performance", "setTimeout", "clearTimeout"] });
    vi.spyOn(Math, "random").mockReturnValue(0);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });
  const transport = (fetch: typeof globalThis.fetch, options = {}) =>
    new Transport({ apiKey, apiBase: pins.api_base, fetch, ...options });
  for (const method of ["GET", "POST"]) {
    it(`rides out a three-minute upgrade for ${method} with one key and body`, async () => {
      const started = performance.now();
      const fetch = vi.fn<typeof globalThis.fetch>().mockImplementation(() => {
        const elapsed = performance.now() - started;
        if (elapsed < 5000)
          return Promise.resolve(errorResponse(503, "service_maintenance", { "retry-after": "5" }));
        if (elapsed < 60000) return Promise.reject(new TypeError("connection refused"));
        if (elapsed < 180000) return Promise.resolve(new Response("Bad Gateway", { status: 502 }));
        return Promise.resolve(jsonResponse({ ok: true }));
      });
      const promise = transport(fetch, { upgradeTolerance: true }).request(method, "/v1/quotes", { value: 42 });
      const assertion = expect(promise).resolves.toEqual({ ok: true });
      await vi.advanceTimersByTimeAsync(179999);
      expect(fetch.mock.calls.length).toBeGreaterThan(10);
      expect(performance.now() - started).toBeLessThan(180000);
      await vi.advanceTimersByTimeAsync(10001);
      await assertion;
      if (method === "POST") {
        expect(new Set(fetch.mock.calls.map(([, init]) => new Headers(init?.headers).get("idempotency-key"))).size).toBe(1);
        expect(new Set(fetch.mock.calls.map(([, init]) => init?.body))).toEqual(new Set(['{"value":42}']));
      }
    });
  }
  it("bounds upgrade retries at five minutes", async () => {
    const started = performance.now();
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(new Response("down", { status: 503 }));
    const assertion = expect(transport(fetch, { upgradeTolerance: true }).request("GET", "/v1/config", undefined)).rejects.toBeInstanceOf(TransportError);
    await vi.runAllTimersAsync();
    await assertion;
    expect(performance.now() - started).toBeLessThanOrEqual(300000);
    expect(performance.now() - started).toBeGreaterThan(290000);
  });
  it("keeps explicit deadlines and cancellation effective during upgrades", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockRejectedValue(new TypeError("offline"));
    const assertion = expect(transport(fetch, { upgradeTolerance: true }).request("GET", "/v1/config", undefined, { requestDeadlineMs: 2000 })).rejects.toBeInstanceOf(TransportError);
    await vi.runAllTimersAsync();
    await assertion;
    const controller = new AbortController();
    const pending = transport(fetch, { upgradeTolerance: true }).request("GET", "/v1/config", undefined, { signal: controller.signal });
    const cancelled = expect(pending).rejects.toMatchObject({ code: "cancelled" });
    await vi.advanceTimersByTimeAsync(100);
    controller.abort();
    await cancelled;
  });
  it("does not prolong DELETE or replayed execution failures", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(errorResponse(503, "service_maintenance", { "idempotent-replayed": "true" }));
    await expect(transport(fetch, { upgradeTolerance: true }).request("POST", "/v1/quotes", {})).rejects.toBeInstanceOf(ApiError);
    expect(fetch).toHaveBeenCalledTimes(1);
    fetch.mockClear().mockRejectedValue(new TypeError("offline"));
    await expect(transport(fetch, { upgradeTolerance: true }).request("DELETE", "/v1/test", undefined)).rejects.toBeInstanceOf(TransportError);
    expect(fetch).toHaveBeenCalledTimes(1);
  });
  for (const vector of scenarios)
    it(`shared fixture: ${vector.name}`, async () => {
      const fetch = vi.fn<typeof globalThis.fetch>();
      for (const status of vector.responses)
        fetch.mockResolvedValueOnce(
          status < 300
            ? jsonResponse({ ok: true }, status)
            : errorResponse(
                status,
                "service_unavailable",
                vector.retry_after_seconds
                  ? { "retry-after": String(vector.retry_after_seconds) }
                  : {},
              ),
        );
      const result = transport(fetch).request(
        vector.method,
        "/v1/test",
        { value: 42 },
        {
          ...(vector.deadline_seconds ? { requestDeadlineMs: vector.deadline_seconds * 1000 } : {}),
        },
      );
      const assertion =
        vector.responses.at(vector.expected_attempts - 1) === 201
          ? expect(result).resolves.toEqual({ ok: true })
          : expect(result).rejects.toBeInstanceOf(ApiError);
      await vi.runAllTimersAsync();
      await assertion;
      expect(fetch).toHaveBeenCalledTimes(vector.expected_attempts);
      if (vector.method === "POST") {
        const keys = fetch.mock.calls.map(([, init]) =>
          new Headers(init?.headers).get("idempotency-key"),
        );
        expect(keys[0]).toMatch(/^[a-f0-9-]{36}$/);
        expect(new Set(keys).size).toBe(1);
      }
    });
  it("freezes serialized body and explicit key before first attempt", async () => {
    const params = { metadata: { order: "first" } };
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockImplementationOnce(() => {
        params.metadata.order = "changed";
        return Promise.resolve(errorResponse());
      })
      .mockResolvedValueOnce(jsonResponse({ ok: true }));
    const promise = transport(fetch).request("POST", "/v1/quotes", params, {
      idempotencyKey: "order-42",
    });
    const assertion = expect(promise).resolves.toEqual({ ok: true });
    await vi.runAllTimersAsync();
    await assertion;
    expect(fetch.mock.calls.map(([, init]) => init?.body)).toEqual([
      JSON.stringify({ metadata: { order: "first" } }),
      JSON.stringify({ metadata: { order: "first" } }),
    ]);
    for (const [, init] of fetch.mock.calls)
      expect(new Headers(init?.headers).get("idempotency-key")).toBe("order-42");
  });
  it("uses distinct automatic keys for distinct invocations", async () => {
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockImplementation(() => Promise.resolve(jsonResponse({ ok: true })));
    const client = transport(fetch);
    await client.request("POST", "/v1/test", {});
    await client.request("POST", "/v1/test", {});
    expect(new Headers(fetch.mock.calls[0]?.[1]?.headers).get("idempotency-key")).not.toBe(
      new Headers(fetch.mock.calls[1]?.[1]?.headers).get("idempotency-key"),
    );
  });
  it("bounds retry count, exponential half/full jitter, and elapsed deadline", async () => {
    const calls: number[] = [];
    const fetch = vi.fn<typeof globalThis.fetch>().mockImplementation(() => {
      calls.push(performance.now());
      return Promise.resolve(errorResponse());
    });
    const promise = transport(fetch, { maxAttempts: 10, requestDeadlineMs: 1800 }).request(
      "GET",
      "/v1/test",
      undefined,
    );
    const assertion = expect(promise).rejects.toBeInstanceOf(ApiError);
    await vi.runAllTimersAsync();
    await assertion;
    expect(calls).toEqual([0, 250, 750, 1750]);
    expect(performance.now()).toBe(1750);
    vi.mocked(Math.random).mockReturnValue(1);
    calls.length = 0;
    const second = transport(fetch, { maxAttempts: 10 }).request("GET", "/v1/test", undefined);
    const secondAssertion = expect(second).rejects.toBeInstanceOf(ApiError);
    await vi.runAllTimersAsync();
    await secondAssertion;
    expect(calls.map((t) => t - (calls[0] ?? 0))).toEqual([
      0, 500, 1500, 3500, 7500, 12500, 17500, 22500, 27500, 32500,
    ]);
  });
  it.each(["2", "date"])("honors Retry-After minimum (%s)", async (value) => {
    vi.setSystemTime(new Date("2026-10-04T00:00:00Z"));
    const retryAfter = value === "date" ? new Date(Date.now() + 2000).toUTCString() : value;
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockResolvedValueOnce(errorResponse(429, "rate_limited", { "retry-after": retryAfter }))
      .mockResolvedValueOnce(jsonResponse({ ok: true }));
    const promise = transport(fetch).request("GET", "/v1/test", undefined);
    const assertion = expect(promise).resolves.toEqual({ ok: true });
    await vi.advanceTimersByTimeAsync(1999);
    expect(fetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    await assertion;
    expect(fetch).toHaveBeenCalledTimes(2);
  });
  it.each([429, 500, 502, 503, 504, 409])("retries eligible status %i", async (status) => {
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockResolvedValueOnce(
        errorResponse(status, status === 409 ? "idempotency_key_in_use" : "unavailable"),
      )
      .mockResolvedValueOnce(jsonResponse({ ok: true }));
    const assertion = expect(transport(fetch).request("POST", "/v1/test", {})).resolves.toEqual({
      ok: true,
    });
    await vi.runAllTimersAsync();
    await assertion;
    expect(fetch).toHaveBeenCalledTimes(2);
  });
  it.each([400, 401, 403, 404, 409, 422, 501])(
    "does not retry terminal status %i",
    async (status) => {
      const fetch = vi
        .fn<typeof globalThis.fetch>()
        .mockResolvedValueOnce(errorResponse(status, "invalid_request"));
      await expect(transport(fetch).request("POST", "/v1/test", {})).rejects.toMatchObject({
        statusCode: status,
      });
      expect(fetch).toHaveBeenCalledTimes(1);
    },
  );
  it("ends retries on replayed errors", async () => {
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockResolvedValueOnce(errorResponse(503, "unavailable", { "idempotent-replayed": "true" }));
    await expect(transport(fetch).request("POST", "/v1/test", {})).rejects.toBeInstanceOf(ApiError);
    expect(fetch).toHaveBeenCalledTimes(1);
  });
  it("maps fields and redacts credentials without raw causes", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(
      jsonResponse(
        {
          error: {
            code: "bad",
            message: `bad ${apiKey}`,
            type: "invalid_request_error",
            param: "amount",
            doc_url: "https://docs.example",
          },
        },
        400,
        { "request-id": "req_42", "retry-after": "3" },
      ),
    );
    await expect(transport(fetch).request("GET", "/v1/test", undefined)).rejects.toMatchObject({
      statusCode: 400,
      code: "bad",
      message: "bad [redacted]",
      errorType: "invalid_request_error",
      param: "amount",
      docUrl: "https://docs.example",
      requestId: "req_42",
      retryAfter: 3,
    });
    const minimal = vi
      .fn<typeof globalThis.fetch>()
      .mockResolvedValue(jsonResponse({ error: { code: "bad", message: "Payment unavailable" } }, 400));
    await expect(transport(minimal).request("GET", "/v1/test", undefined)).rejects.toMatchObject({
      errorType: null,
      param: null,
      docUrl: null,
      requestId: null,
      retryAfter: null,
    });
    const network = vi.fn<typeof globalThis.fetch>().mockRejectedValue(new Error(apiKey));
    await expect(
      transport(network, { maxAttempts: 1 }).request("GET", "/v1/test", undefined),
    ).rejects.toMatchObject({ code: "network", message: "Request network" });
  });
  it("retries network failures and times out hanging fetch/body reads within deadline", async () => {
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockRejectedValueOnce(new Error("private"))
      .mockResolvedValueOnce(jsonResponse({ ok: true }));
    const assertion = expect(
      transport(fetch).request("GET", "/v1/test", undefined),
    ).resolves.toEqual({ ok: true });
    await vi.runAllTimersAsync();
    await assertion;
    for (const kind of ["fetch", "body"]) {
      const slow = vi.fn<typeof globalThis.fetch>().mockImplementation(() =>
        kind === "fetch"
          ? new Promise<Response>(() => {})
          : Promise.resolve(
              new Response(
                new ReadableStream({
                  start(controller) {
                    controller.enqueue(new TextEncoder().encode('{"ok":'));
                  },
                }),
              ),
            ),
      );
      const failure = expect(
        transport(slow, { timeoutMs: 5000, requestDeadlineMs: 1000 }).request(
          "GET",
          "/v1/test",
          undefined,
        ),
      ).rejects.toMatchObject({ code: "timeout" });
      await vi.runAllTimersAsync();
      await failure;
      expect(slow).toHaveBeenCalledTimes(1);
    }
  });
  it("caller cancellation and close abort requests and retry sleeps", async () => {
    const fetch = vi
      .fn<typeof globalThis.fetch>()
      .mockImplementation(() => new Promise<Response>(() => {}));
    const controller = new AbortController();
    const injectedClose = vi.fn();
    Object.assign(fetch, { close: injectedClose });
    const client = transport(fetch);
    const cancelled = expect(
      client.request("GET", "/v1/test", undefined, { signal: controller.signal }),
    ).rejects.toMatchObject({ code: "cancelled" });
    controller.abort();
    await cancelled;
    expect(fetch).toHaveBeenCalledTimes(1);
    const closed = expect(client.request("GET", "/v1/test", undefined)).rejects.toMatchObject({
      code: "cancelled",
    });
    await client.close();
    await closed;
    expect(injectedClose).not.toHaveBeenCalled();
    await expect(client.request("GET", "/v1/test", undefined)).rejects.toBeInstanceOf(
      TransportError,
    );
    const retrying = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(errorResponse());
    const sleeping = transport(retrying);
    const failure = expect(sleeping.request("GET", "/v1/test", undefined)).rejects.toMatchObject({
      code: "cancelled",
    });
    await vi.advanceTimersByTimeAsync(1);
    await sleeping.close();
    await failure;
    expect(retrying).toHaveBeenCalledTimes(1);
  });
  it("refuses redirects and malformed/unsafe responses without exposing bodies", async () => {
    for (const response of [
      new Response(null, { status: 302, headers: { location: "https://evil.example" } }),
      new Response(apiKey),
      new Response('{"amount":9007199254740993}', { headers: { "request-id": "req_bad" } }),
      new Response('{"amount":9007199254740990.5}'),
      jsonResponse({ error: "bad" }, 503),
    ]) {
      const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(response);
      await expect(transport(fetch).request("POST", "/v1/test", {})).rejects.toBeInstanceOf(
        ResponseValidationError,
      );
      expect(fetch).toHaveBeenCalledTimes(1);
      expect(fetch.mock.calls[0]?.[1]?.redirect).toBe("manual");
    }
  });
  it("validates controls, unsafe request numbers and header limits before IO, encodes queries", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(jsonResponse({ ok: true }));
    for (const options of [
      { maxAttempts: 0 },
      { maxAttempts: 11 },
      { maxAttempts: 1.5 },
      { timeoutMs: 0 },
      { timeoutMs: -1 },
      { timeoutMs: NaN },
      { maxAttempts: Infinity },
      { requestDeadlineMs: 0 },
      { requestDeadlineMs: Infinity },
    ])
      expect(() => transport(fetch, options)).toThrow(ConfigurationError);
    for (const idempotencyKey of ["", "x".repeat(256), "bad\nheader", "秘密"])
      await expect(
        transport(fetch).request("POST", "/v1/test", {}, { idempotencyKey }),
      ).rejects.toBeInstanceOf(ConfigurationError);
    await expect(
      transport(fetch).request("POST", "/v1/test", { amount: Number.MAX_SAFE_INTEGER + 1 }),
    ).rejects.toBeInstanceOf(ConfigurationError);
    await expect(
      transport(fetch).request("POST", "/v1/test", {
        toJSON() {
          return { amount: Number.MAX_SAFE_INTEGER + 1 };
        },
      }),
    ).rejects.toBeInstanceOf(ConfigurationError);
    await expect(
      transport(fetch).request("GET", "/v1/test", ["not-a-query"]),
    ).rejects.toBeInstanceOf(ConfigurationError);
    expect(fetch).not.toHaveBeenCalled();
    await transport(fetch).request("GET", "/v1/test", {
      "expand[]": ["deposit", "x&key=y"],
      client_reference_id: "a?#&b",
    });
    const url = new URL(requestUrl(fetch.mock.calls[0]?.[0]));
    expect(url.searchParams.getAll("expand[]")).toEqual(["deposit", "x&key=y"]);
    expect(url.searchParams.get("client_reference_id")).toBe("a?#&b");
  });
});
