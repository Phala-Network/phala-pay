import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CheckoutError, PhalaPay, checkoutStatus, createCheckout, pollDelay, type CheckoutSession, type CheckoutState } from "../src/index.js";
import { ADDRESS, API_BASE, CLIENT_SECRET, QUOTE_ID, fakeFetch, quote } from "./fixtures.js";

const NOW = (quote().expires_at - 600) * 1000;
const sessions = new Set<CheckoutSession>();

beforeEach(() => {
  vi.useFakeTimers({ now: NOW });
  vi.spyOn(Math, "random").mockReturnValue(0.5);
});
afterEach(() => {
  for (const session of sessions) session.destroy();
  sessions.clear();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
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
  sessions.add(checkout);
  return { checkout, states };
}

describe("createCheckout", () => {
  it("jitters normal reads and failed-read backoff", async () => {
    vi.mocked(Math.random).mockReturnValue(0);
    const { fetch, calls } = fakeFetch(quote(), 503, quote());
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(799);
    expect(calls).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1599);
    expect(calls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls).toHaveLength(3);
    expect(checkout.getState().reconnecting).toBeUndefined();
  });

  it("makes no hidden-tab requests and reads immediately on becoming visible", async () => {
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    const { fetch, calls } = fakeFetch(quote());
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls).toHaveLength(0);
    visibility.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toHaveLength(1);
    visibility.mockReturnValue("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls).toHaveLength(1);
    visibility.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1000);
    expect(calls).toHaveLength(3);
    checkout.destroy();
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls).toHaveLength(3);
  });

  it("allows an explicit refresh while hidden without restarting automatic polling", async () => {
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    const { fetch, calls } = fakeFetch(quote(), quote({ payment_status: "seen" }));
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls).toHaveLength(0);
    await checkout.refresh();
    expect(calls).toHaveLength(1);
    expect(checkout.getState().status).toBe("waiting");
    await checkout.refresh();
    expect(calls).toHaveLength(2);
    expect(checkout.getState().status).toBe("seen");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls).toHaveLength(2);
    visibility.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(1000);
    expect(calls).toHaveLength(4);
  });

  it("does not duplicate polls when visibility returns during an active read", async () => {
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    let respond: ((response: Response) => void) | undefined;
    const fetch = vi.fn<typeof globalThis.fetch>()
      .mockImplementationOnce(() => new Promise<Response>((resolve) => { respond = resolve; }))
      .mockImplementation(() => Promise.resolve(Response.json(quote())));
    start(fetch);
    visibility.mockReturnValue("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    visibility.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    document.dispatchEvent(new Event("visibilitychange"));
    expect(fetch).toHaveBeenCalledTimes(1);
    respond?.(Response.json(quote()));
    await vi.advanceTimersByTimeAsync(4000);
    expect(fetch).toHaveBeenCalledTimes(5);
  });

  it("clears polling timers and visibility listeners when the external signal aborts", async () => {
    const controller = new AbortController();
    const remove = vi.spyOn(document, "removeEventListener");
    const { fetch, calls } = fakeFetch(quote());
    const checkout = createCheckout({
      clientSecret: CLIENT_SECRET,
      expectedAddress: ADDRESS,
      apiBase: API_BASE,
      pollInterval: 1000,
      fetch,
      signal: controller.signal,
    });
    sessions.add(checkout);
    await vi.advanceTimersByTimeAsync(0);
    expect(vi.getTimerCount()).toBe(1);
    controller.abort();
    expect(vi.getTimerCount()).toBe(0);
    expect(remove).toHaveBeenCalledWith("visibilitychange", expect.any(Function));
    document.dispatchEvent(new Event("visibilitychange"));
    await checkout.refresh();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls).toHaveLength(1);
  });

  it("polls without a browser document", async () => {
    vi.stubGlobal("document", undefined);
    const { fetch, calls } = fakeFetch(quote());
    start(fetch);
    await vi.advanceTimersByTimeAsync(2000);
    expect(calls).toHaveLength(3);
  });

  it.each([400, 401, 403, 409, 410, 422, 499])(
    "stops after three consecutive %s responses using the existing error state",
    async (status) => {
      const initial = quote();
      const { fetch, calls } = fakeFetch(initial, status);
      const { checkout } = start(fetch);
      await vi.advanceTimersByTimeAsync(6999);
      expect(calls).toHaveLength(3);
      expect(checkout.getState().status).toBe("waiting");
      await vi.advanceTimersByTimeAsync(1);
      expect(checkout.getState()).toMatchObject({ status: "error", quote: initial, error: { code: "api_error" } });
      await vi.advanceTimersByTimeAsync(120_000);
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
      expect(calls).toHaveLength(4);
    },
  );

  it("stops after three consecutive non-retryable 4xx responses before any quote is read", async () => {
    const { fetch, calls } = fakeFetch(401, 403, 422);
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(calls).toHaveLength(3);
    expect(checkout.getState()).toMatchObject({ status: "error", quote: null, error: { code: "api_error" } });
  });

  it.each([408, 429, 503, 302, new TypeError("offline"), quote()])(
    "resets consecutive client errors after %s",
    async (interruption) => {
      const { fetch, calls } = fakeFetch(401, 403, interruption, 401, 403, 422);
      const { checkout } = start(fetch);
      await vi.advanceTimersByTimeAsync(0);
      for (let read = 0; read < 4; read += 1) await checkout.refresh();
      expect(checkout.getState().status).not.toBe("error");
      await checkout.refresh();
      expect(checkout.getState().status).toBe("error");
      await vi.advanceTimersByTimeAsync(120_000);
      expect(calls).toHaveLength(6);
    },
  );

  it.each([408, 429])("keeps polling repeated %s responses", async (status) => {
    const { fetch, calls } = fakeFetch(status);
    const { checkout } = start(fetch);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(calls.length).toBeGreaterThan(3);
    expect(checkout.getState().status).not.toBe("error");
  });
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

describe("pollDelay", () => {
  it.each([[1000, 0, 1000], [1000, 1, 2000], [3000, 10, 30_000]])(
    "keeps jitter between 80%% and 120%% for interval %s and %s failures",
    (interval, failures, base) => {
      for (const sample of [0, 0.25, 0.5, 0.75, 1]) {
        const random = vi.fn(() => sample);
        const delay = pollDelay(interval, failures, null, random);
        expect(delay).toBeCloseTo(Math.min(base * (0.8 + 0.4 * sample), 30_000));
        expect(random).toHaveBeenCalledTimes(1);
      }
    },
  );

  it("never retries before Retry-After, including with negative jitter", () => {
    const error = new CheckoutError("rate_limited", "limited", { retryAfter: 30 });
    expect(pollDelay(1000, 1, error, () => 0)).toBe(30_000);
    expect(pollDelay(1000, 1, error, () => 1)).toBe(30_000);
  });

  it("caps positive jitter at 30 seconds while honoring a longer Retry-After", () => {
    expect(pollDelay(3000, 10, null, () => 1)).toBe(30_000);
    const error = new CheckoutError("rate_limited", "limited", { retryAfter: 60 });
    expect(pollDelay(3000, 10, error, () => 1)).toBe(60_000);
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
