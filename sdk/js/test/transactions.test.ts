import { describe, expect, it, vi } from "vitest";
import { PhalaPay, submitTransaction } from "../src/index.js";
import { API_BASE, CLIENT_SECRET } from "./fixtures.js";

const hash = `0x${"ab".repeat(32)}` as const;
describe("transaction hints", () => {
  it("sends a quote hint to the configured origin with only the hash and object secret", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(new Response("{}", { status: 202 }));
    const pay = new PhalaPay({ apiBase: API_BASE, fetch });
    await pay.submitTransaction(CLIENT_SECRET, hash, { chainId: 1 });
    const [input, init] = fetch.mock.calls[0] ?? [];
    if (!(input instanceof URL)) throw new TypeError("Expected URL");
    const url = input;
    expect(url.origin).toBe(API_BASE);
    expect(url.pathname).toMatch(/^\/v1\/quotes\/qt_.+\/transactions$/);
    expect(url.searchParams.get("client_secret")).toBe(CLIENT_SECRET);
    expect(init?.body).toBe(JSON.stringify({ transaction_hash: hash }));
    expect(init?.credentials).toBe("omit");
    expect(init?.redirect).toBe("error");
  });
  it("requires the chain for deposit-address hints and rejects forged hash shapes locally", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(new Response("{}", { status: 202 }));
    const clientSecret = `da_${"cd".repeat(16)}_secret_${"ef".repeat(24)}`;
    await expect(submitTransaction({ apiBase: API_BASE, clientSecret, transactionHash: hash, fetch })).rejects.toThrow("chainId");
    expect(fetch).not.toHaveBeenCalled();
    await submitTransaction({ apiBase: API_BASE, clientSecret, transactionHash: hash, chainId: 84532, fetch });
    expect(fetch.mock.calls[0]?.[1]?.body).toBe(JSON.stringify({ transaction_hash: hash, chain_id: 84532 }));
    await expect(submitTransaction({ apiBase: API_BASE, clientSecret: CLIENT_SECRET, transactionHash: "0x00", fetch })).rejects.toThrow("hash");
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
