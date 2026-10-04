import { describe, expect, it } from "vitest";
import { createCheckout, retrieveDepositAddress, retrieveQuote } from "../src/index.js";
import { ADDRESS, API_BASE, CLIENT_SECRET, quote } from "./fixtures.js";

/** A stalled request that obeys the fetch cancellation contract. */
function stalled(signal: AbortSignal | null | undefined): Promise<Response> {
  if (signal == null) throw new Error("missing request signal");
  return new Promise((_, reject) => {
    signal.addEventListener(
      "abort",
      () =>
        reject(
          signal.reason instanceof Error
            ? signal.reason
            : new DOMException("aborted", "AbortError"),
        ),
      { once: true },
    );
    if (signal.aborted)
      reject(
        signal.reason instanceof Error ? signal.reason : new DOMException("aborted", "AbortError"),
      );
  });
}

const options = { clientSecret: CLIENT_SECRET, expectedAddress: ADDRESS, apiBase: API_BASE };
const addressOptions = {
  clientSecret: `da_${"0d".repeat(16)}_secret_${"ab".repeat(24)}`,
  apiBase: API_BASE,
};

describe("request cancellation", () => {
  it("applies deadlines to both public reads", async () => {
    const fetch: typeof globalThis.fetch = (_, init) => stalled(init?.signal);
    await expect(retrieveQuote({ ...options, fetch, requestTimeout: 10 })).rejects.toMatchObject({
      name: "TimeoutError",
    });
    await expect(
      retrieveDepositAddress({ ...addressOptions, fetch, requestTimeout: 10 }),
    ).rejects.toMatchObject({ name: "TimeoutError" });
  });

  it("composes caller cancellation with the deadline", async () => {
    const controller = new AbortController();
    const fetch: typeof globalThis.fetch = (_, init) => stalled(init?.signal);
    const quoteRead = retrieveQuote({ ...options, fetch, signal: controller.signal });
    const addressRead = retrieveDepositAddress({
      ...addressOptions,
      fetch,
      signal: controller.signal,
    });
    controller.abort();
    await expect(quoteRead).rejects.toMatchObject({ name: "AbortError" });
    await expect(addressRead).rejects.toMatchObject({ name: "AbortError" });
  });

  it("keeps the deadline active while reading the response body", async () => {
    const fetch: typeof globalThis.fetch = (_, init) =>
      Promise.resolve(
        new Response(
          new ReadableStream({
            start(controller) {
              init?.signal?.addEventListener("abort", () => controller.error(init.signal?.reason), {
                once: true,
              });
            },
          }),
        ),
      );
    await expect(retrieveQuote({ ...options, fetch, requestTimeout: 10 })).rejects.toMatchObject({
      code: "invalid_response",
      cause: { name: "TimeoutError" },
    });
  });

  it("retries after a hung request times out instead of keeping refresh blocked", async () => {
    let calls = 0;
    const fetch: typeof globalThis.fetch = (_, init) => {
      calls += 1;
      return calls === 1 ? stalled(init?.signal) : Promise.resolve(Response.json(quote()));
    };
    const session = createCheckout({
      ...options,
      fetch,
      requestTimeout: 10,
      pollInterval: 5,
      now: () => (quote().expires_at - 10) * 1000,
    });
    try {
      await session.refresh();
      expect(session.getState()).toMatchObject({ error: null, reconnecting: true });
      await new Promise((resolve) => setTimeout(resolve, 30));
      expect(calls).toBeGreaterThan(1);
      expect(session.getState().status).toBe("waiting");
      expect(session.getState().error).toBeNull();
    } finally {
      session.destroy();
    }
  });

  it("destroy aborts in-flight reads and prevents further refreshes and notifications", async () => {
    let signal: AbortSignal | null | undefined;
    let calls = 0;
    const fetch: typeof globalThis.fetch = (_, init) => {
      calls += 1;
      signal = init?.signal;
      return stalled(signal);
    };
    const session = createCheckout({ ...options, fetch });
    let notifications = 0;
    session.subscribe(() => {
      notifications += 1;
    });
    const active = session.refresh();
    session.destroy();
    expect(signal?.aborted).toBe(true);
    await active;
    await session.refresh();
    expect(calls).toBe(1);
    expect(notifications).toBe(0);
  });
});
