import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PhalaPay, checkoutStatus, createCheckout, type CheckoutState } from "../src/index.js";
import { ADDRESS, API_BASE, CLIENT_SECRET, QUOTE_ID, fakeFetch, quote } from "./fixtures.js";

const NOW = (quote().expires_at - 600) * 1000;

beforeEach(() => {
  vi.useFakeTimers({ now: NOW });
});
afterEach(() => {
  vi.useRealTimers();
});

function start(fetch: typeof globalThis.fetch) {
  const states: CheckoutState[] = [];
  const checkout = createCheckout({
    clientSecret: CLIENT_SECRET,
    expectedAddress: ADDRESS.toUpperCase().replace("0X", "0x"),
    apiBase: `${API_BASE}/`,
    fetch,
    pollInterval: 1000,
  });
  checkout.subscribe((state) => states.push(state));
  return { checkout, states };
}

describe("createCheckout", () => {
  it("retains payment state through a three-minute outage even past local expiry", async () => {
    const initial = quote({ payment_status: "seen", confirmations: 1, expires_at: NOW / 1000 + 30 });
    const fetch = vi.fn<typeof globalThis.fetch>().mockImplementation(() => {
      if (Date.now() === NOW) return Promise.resolve(Response.json(initial));
      if (Date.now() < NOW + 180000) return Promise.resolve(new Response("offline", { status: 503 }));
      return Promise.resolve(Response.json(quote({ status: "complete", payment_status: "credited" })));
    });
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(179999);
    expect(checkout.getState()).toMatchObject({ status: "seen", quote: initial, error: null, reconnecting: true });
    await vi.advanceTimersByTimeAsync(30001);
    expect(checkout.getState()).toMatchObject({ status: "credited", error: null });
    expect(checkout.getState().reconnecting).toBeUndefined();
    checkout.destroy();
  });

  it("polls the public view until the payment is credited", async () => {
    const { fetch, calls } = fakeFetch(
      quote(),
      quote({ payment_status: "seen", confirmations: 1 }),
      quote({ payment_status: "confirming" }),
      quote({ status: "complete", payment_status: "credited" }),
    );
    const { states } = start(fetch);
    await vi.advanceTimersByTimeAsync(10_000);

    expect(states.map((s) => s.status)).toEqual(["waiting", "seen", "confirming", "credited"]);
    expect(calls).toHaveLength(4);
    expect(calls[0]).toBe(
      `${API_BASE}/v1/quotes/${QUOTE_ID}?client_secret=${encodeURIComponent(CLIENT_SECRET)}`,
    );
  });

  it("does not notify when nothing changed", async () => {
    const { fetch, calls } = fakeFetch(quote());
    const { states } = start(fetch);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(calls.length).toBeGreaterThan(3);
    expect(states).toHaveLength(1);
  });

  it("stops with an error on an unknown client secret", async () => {
    const { fetch, calls } = fakeFetch(404);
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(checkout.getState()).toMatchObject({
      status: "error",
      error: { code: "invalid_client_secret" },
    });
    expect(calls).toHaveLength(1);
  });

  it("keeps the last quote and backs off while the service is unreachable", async () => {
    const { fetch, calls } = fakeFetch(quote(), new TypeError("offline"), 503, quote());
    const { checkout, states } = start(fetch);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(checkout.getState()).toMatchObject({ status: "waiting", error: null, reconnecting: true });
    await vi.advanceTimersByTimeAsync(2_000);
    expect(checkout.getState()).toMatchObject({ status: "waiting", error: null, reconnecting: true });
    // The third failure-free read waits 2^2 intervals after two failures.
    await vi.advanceTimersByTimeAsync(3_999);
    expect(calls).toHaveLength(3);
    await vi.advanceTimersByTimeAsync(1);
    expect(checkout.getState()).toMatchObject({ status: "waiting", error: null });
    expect(states.every((s) => s.quote !== null)).toBe(true);
  });

  it("turns expired at expires_at and stops once the service expires the quote", async () => {
    const { fetch, calls } = fakeFetch(quote(), quote(), quote({ status: "expired" }));
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(0);
    vi.setSystemTime(quote().expires_at * 1000);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(checkout.getState().status).toBe("expired");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls).toHaveLength(3);
  });

  it("rejects an invalid response body", async () => {
    const { fetch } = fakeFetch({ ...quote(), status: "unknown" } as never);
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(0);
    expect(checkout.getState()).toMatchObject({
      status: "loading",
      error: { code: "invalid_response" },
    });
  });

  it("stops polling when destroyed", async () => {
    const { fetch, calls } = fakeFetch(quote());
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(0);
    checkout.destroy();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls).toHaveLength(1);
  });
});

describe("checkoutStatus", () => {
  const now = quote().expires_at - 1;
  it.each([
    [quote(), "waiting"],
    [quote({ payment_status: "seen" }), "seen"],
    [quote({ payment_status: "rejected" }), "rejected"],
    [quote({ status: "canceled" }), "canceled"],
    [quote({ status: "expired" }), "expired"],
    [quote({ expires_at: now }), "expired"],
    // A payment seen after local expiry still shows.
    [quote({ expires_at: now, payment_status: "seen" }), "seen"],
  ] as const)("%# is %s", (value, expected) => {
    expect(checkoutStatus(value, now)).toBe(expected);
  });
});

describe("expectedAddress", () => {
  it("fails closed when the quote names another address, and stops polling", async () => {
    const { fetch, calls } = fakeFetch(quote({ address: `0x${"22".repeat(20)}` }));
    const { states } = start(fetch);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(states).toHaveLength(1);
    expect(states[0]?.status).toBe("error");
    expect(states[0]?.error?.code).toBe("address_mismatch");
    // Nothing of the quote is kept to render.
    expect(states[0]?.quote).toBeNull();
    expect(calls).toHaveLength(1);
  });

  it("is required and must be an address", () => {
    expect(() =>
      createCheckout({ clientSecret: CLIENT_SECRET, expectedAddress: "nope", apiBase: API_BASE }),
    ).toThrow(TypeError);
  });

  it("waits as long as a rate limit asks, and names the request", async () => {
    const limited = () =>
      new Response("{}", {
        status: 429,
        headers: { "retry-after": "30", "request-id": "req_0123456789abcdef0123456789abcdef" },
      });
    const { fetch, calls } = fakeFetch(limited, quote());
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(0);
    expect(checkout.getState().error).toMatchObject({
      code: "rate_limited",
      retryAfter: 30,
      requestId: "req_0123456789abcdef0123456789abcdef",
    });
    // The backoff alone would read again after 2 s; Retry-After holds it for 30.
    await vi.advanceTimersByTimeAsync(29_000);
    expect(calls).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(calls).toHaveLength(2);
    expect(checkout.getState()).toMatchObject({ status: "waiting", error: null });
    checkout.destroy();
  });

  it("reports a reversed payment and stops polling", async () => {
    expect(checkoutStatus(quote({ payment_status: "reversed" }), NOW / 1000)).toBe("reversed");
    const { fetch, calls } = fakeFetch(quote({ payment_status: "reversed" }));
    const { states } = start(fetch);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(states.map((s) => s.status)).toEqual(["reversed"]);
    expect(calls).toHaveLength(1);
  });
});

describe("PhalaPay", () => {
  it("retrieves the public view with the client secret", async () => {
    const { fetch, calls } = fakeFetch(quote());
    const pay = new PhalaPay({ apiBase: API_BASE, fetch });
    await expect(pay.retrieveQuote(CLIENT_SECRET, ADDRESS)).resolves.toEqual(quote());
    expect(calls[0]).toBe(
      `${API_BASE}/v1/quotes/${QUOTE_ID}?client_secret=${encodeURIComponent(CLIENT_SECRET)}`,
    );
  });

  it("rejects an unknown client secret", async () => {
    const pay = new PhalaPay({ apiBase: API_BASE, fetch: fakeFetch(404).fetch });
    await expect(pay.retrieveQuote(CLIENT_SECRET, ADDRESS)).rejects.toMatchObject({
      code: "invalid_client_secret",
    });
  });

  it("follows a checkout session", async () => {
    const pay = new PhalaPay({ apiBase: API_BASE, fetch: fakeFetch(quote()).fetch });
    const session = pay.checkout(CLIENT_SECRET, { expectedAddress: ADDRESS, pollInterval: 1000 });
    await vi.advanceTimersByTimeAsync(0);
    expect(session.getState().status).toBe("waiting");
    session.destroy();
  });
});
