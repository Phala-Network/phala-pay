import { createWalletClient, custom, decodeFunctionData, encodeFunctionResult, erc20Abi } from "viem";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  INJECTED_WALLET_UUID,
  WalletError,
  payWithWallet,
  retrieveQuote,
  watchWallets,
  type EthereumProvider,
  type Wallet,
} from "../src/index.js";
import { ADDRESS, TOKEN, quote, API_BASE, CLIENT_SECRET } from "./fixtures.js";

const ACCOUNT = "0x3333333333333333333333333333333333333333";
const HASH = `0x${"ab".repeat(32)}`;

interface Call {
  method: string;
  params?: unknown;
}

/** A wallet on `chainId` that knows `known` chains and fails requests listed in `failures`; it
 * refuses to switch to an unknown chain with `unknownChain`. Its account holds `wallet.balance` of
 * any token, plenty unless a test sets it. */
function mockProvider(
  chainId: number,
  known: number[],
  failures: Record<string, number> = {},
  unknownChain: object = { code: 4902 },
) {
  const calls: Call[] = [];
  const wallet = { balance: 10n ** 30n };
  let current = chainId;
  const provider: EthereumProvider = {
    request: ({ method, params }: Call) => {
      calls.push({ method, params });
      const failure = failures[method];
      if (failure !== undefined) {
        return Promise.reject(Object.assign(new Error(method), { code: failure }));
      }
      switch (method) {
        case "eth_requestAccounts":
          return Promise.resolve([ACCOUNT]);
        case "eth_chainId":
          return Promise.resolve(`0x${current.toString(16)}`);
        case "wallet_switchEthereumChain": {
          const target = Number((params as [{ chainId: string }])[0].chainId);
          if (!known.includes(target)) {
            return Promise.reject(Object.assign(new Error("unknown chain"), unknownChain));
          }
          current = target;
          return Promise.resolve(null);
        }
        case "wallet_addEthereumChain":
          known.push(Number((params as [{ chainId: string }])[0].chainId));
          return Promise.resolve(null);
        case "eth_call":
          return Promise.resolve(
            encodeFunctionResult({ abi: erc20Abi, functionName: "balanceOf", result: wallet.balance }),
          );
        case "eth_sendTransaction":
          return Promise.resolve(HASH);
        default:
          return Promise.reject(new Error(`unexpected ${method}`));
      }
    },
  };
  return { provider, calls, wallet };
}

function sentTransfer(calls: Call[]) {
  const send = calls.find((c) => c.method === "eth_sendTransaction");
  const [tx] = send?.params as [{ from: string; to: string; data: `0x${string}` }];
  return { tx, decoded: decodeFunctionData({ abi: erc20Abi, data: tx.data }) };
}

describe("payWithWallet", () => {
  it("sends the quote's exact ERC-20 transfer on the quote's chain", async () => {
    const { provider, calls } = mockProvider(11155111, [11155111]);
    await expect(payWithWallet(provider, quote())).resolves.toBe(HASH);
    const { tx, decoded } = sentTransfer(calls);
    expect(tx.from).toBe(ACCOUNT);
    expect(tx.to.toLowerCase()).toBe(TOKEN.toLowerCase());
    expect(decoded).toEqual({
      functionName: "transfer",
      args: [ADDRESS, 100502512562814070352n],
    });
    expect(calls.map((c) => c.method)).not.toContain("wallet_switchEthereumChain");
  });

  it("switches the wallet to the quote's chain", async () => {
    const { provider, calls } = mockProvider(1, [1, 11155111]);
    await payWithWallet(provider, quote());
    expect(calls.map((c) => c.method)).toContain("wallet_switchEthereumChain");
    expect(calls.map((c) => c.method)).not.toContain("wallet_addEthereumChain");
  });

  it("adds a known chain the wallet lacks, then switches", async () => {
    const { provider, calls } = mockProvider(1, [1]);
    await payWithWallet(provider, quote());
    const methods = calls.map((c) => c.method);
    expect(methods.indexOf("wallet_addEthereumChain")).toBeGreaterThan(
      methods.indexOf("wallet_switchEthereumChain"),
    );
    expect(methods.at(-1)).toBe("eth_sendTransaction");
  });

  it("refuses to add a chain it cannot describe", async () => {
    const { provider, calls } = mockProvider(1, [1]);
    const unknownChain = quote({ chain_id: 31337 });
    await expect(payWithWallet(provider, unknownChain)).rejects.toMatchObject({ code: "wrong_chain" });
    expect(calls.map((c) => c.method)).not.toContain("eth_sendTransaction");
  });

  it("reports a rejection in the wallet", async () => {
    const { provider } = mockProvider(11155111, [11155111], { eth_sendTransaction: 4001 });
    const error = await payWithWallet(provider, quote()).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(WalletError);
    expect(error).toMatchObject({ code: "rejected" });
  });

  it("adds the chain when MetaMask Mobile nests 4902 in data.originalError", async () => {
    const nested = { code: -32603, data: { originalError: { code: 4902 } } };
    const { provider, calls } = mockProvider(1, [1], {}, nested);
    await expect(payWithWallet(provider, quote())).resolves.toBe(HASH);
    expect(calls.map((c) => c.method)).toContain("wallet_addEthereumChain");
  });

  it("reports a rejected chain switch", async () => {
    const { provider } = mockProvider(1, [1, 11155111], { wallet_switchEthereumChain: 4001 });
    await expect(payWithWallet(provider, quote())).rejects.toMatchObject({ code: "rejected" });
  });

  it("pays with a viem WalletClient's account, switching its chain, without asking to connect", async () => {
    const { provider, calls } = mockProvider(1, [1, 11155111]);
    const client = createWalletClient({ account: ACCOUNT, transport: custom(provider) });
    await expect(payWithWallet(client, quote())).resolves.toBe(HASH);
    const methods = calls.map((c) => c.method);
    expect(methods).not.toContain("eth_requestAccounts");
    expect(methods).toContain("wallet_switchEthereumChain");
    expect(sentTransfer(calls).tx.from).toBe(ACCOUNT);
  });

  it("connects a viem WalletClient that has no account", async () => {
    const { provider, calls } = mockProvider(11155111, [11155111]);
    const client = createWalletClient({ transport: custom(provider) });
    await expect(payWithWallet(client, quote())).resolves.toBe(HASH);
    expect(calls.map((c) => c.method)).toContain("eth_requestAccounts");
  });

  it("sends nothing when the wallet holds less than the quote, saying how much it holds", async () => {
    const { provider, calls, wallet } = mockProvider(11155111, [11155111]);
    wallet.balance = 12500000000000000000n;
    const error = await payWithWallet(provider, quote()).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(WalletError);
    expect(error).toMatchObject({
      code: "insufficient_balance",
      message: "Your wallet holds 12.5 PHA, less than the 100.502512562814070352 PHA to pay. Nothing was sent.",
    });
    expect(calls.map((c) => c.method)).not.toContain("eth_sendTransaction");
  });

  it("compares the balance in the token's own units (6 decimals)", async () => {
    const usdc = quote({ asset: "usdc", decimals: 6, amount_atomic: "20000000" });
    const short = mockProvider(11155111, [11155111]);
    short.wallet.balance = 19999999n;
    await expect(payWithWallet(short.provider, usdc)).rejects.toMatchObject({
      code: "insufficient_balance",
      message: "Your wallet holds 19.999999 USDC, less than the 20 USDC to pay. Nothing was sent.",
    });
    const exact = mockProvider(11155111, [11155111]);
    exact.wallet.balance = 20000000n;
    await expect(payWithWallet(exact.provider, usdc)).resolves.toBe(HASH);
  });

  it("still pays when the wallet cannot read the balance", async () => {
    const { provider } = mockProvider(11155111, [11155111], { eth_call: -32601 });
    await expect(payWithWallet(provider, quote())).resolves.toBe(HASH);
  });

  it("never sends when the payment URI disagrees with the quote", async () => {
    const { provider, calls } = mockProvider(11155111, [11155111]);
    const tampered = quote({ payment_uri: quote({ address: TOKEN }).payment_uri });
    await expect(payWithWallet(provider, tampered)).rejects.toThrow(TypeError);
    expect(calls).toHaveLength(0);
  });
});

describe("watchWallets", () => {
  afterEach(() => {
    delete (window as { ethereum?: unknown }).ethereum;
  });

  const announce = (detail: unknown) =>
    window.dispatchEvent(new CustomEvent("eip6963:announceProvider", { detail }));

  it("lists wallets that announce themselves with EIP-6963, before and after it starts", () => {
    const { provider } = mockProvider(1, [1]);
    const seen: Wallet[][] = [];
    const onRequest = () =>
      announce({
        info: { uuid: "u-1", name: "Test Wallet", icon: "javascript:alert(1)", rdns: "t.w" },
        provider,
      });
    window.addEventListener("eip6963:requestProvider", onRequest);
    const stop = watchWallets((wallets) => seen.push(wallets));
    window.removeEventListener("eip6963:requestProvider", onRequest);
    expect(seen.at(-1)?.map((w) => w.info)).toEqual([
      { uuid: "u-1", name: "Test Wallet", icon: "", rdns: "t.w" },
    ]);

    const icon = "data:image/svg+xml,%3Csvg/%3E";
    announce({ info: { uuid: "u-2", name: "Late Wallet", icon, rdns: "l.w" }, provider });
    expect(seen.at(-1)?.map((w) => w.info.icon)).toEqual(["", icon]);

    stop();
    const count = seen.length;
    announce({ info: { uuid: "u-3", name: "After", icon, rdns: "a.w" }, provider });
    expect(seen).toHaveLength(count);
  });

  it("ignores malformed announcements", () => {
    const seen: Wallet[][] = [];
    const stop = watchWallets((wallets) => seen.push(wallets));
    announce({ info: { uuid: "u-4", name: "No provider" }, provider: {} });
    announce({ info: { name: "No uuid" }, provider: mockProvider(1, [1]).provider });
    stop();
    expect(seen.at(-1)).toEqual([]);
  });

  it("falls back to window.ethereum", () => {
    const { provider } = mockProvider(1, [1]);
    (window as { ethereum?: unknown }).ethereum = provider;
    const seen: Wallet[][] = [];
    watchWallets((wallets) => seen.push(wallets))();
    expect(seen.at(-1)?.map((w) => w.info.uuid)).toEqual([INJECTED_WALLET_UUID]);
  });
});


describe("automatic transaction hints", () => {
  it.each(["received", "network failure", "pending"])("returns the broadcast hash when hint submission is %s", async (result) => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(Response.json(quote()));
    if (result === "received") fetch.mockResolvedValueOnce(new Response("{}", { status: 202 }));
    else if (result === "network failure") fetch.mockRejectedValueOnce(new Error("offline"));
    else fetch.mockImplementationOnce(() => new Promise<Response>(() => undefined));
    const payable = await retrieveQuote({ apiBase: API_BASE, clientSecret: CLIENT_SECRET, expectedAddress: ADDRESS, fetch });
    const wallet = mockProvider(11155111, [11155111]);
    await expect(payWithWallet(wallet.provider, payable)).resolves.toBe(HASH);
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(fetch.mock.calls[1]?.[1]?.body).toBe(JSON.stringify({ transaction_hash: HASH }));
    expect(JSON.stringify(payable)).not.toContain(CLIENT_SECRET);
  });
  it("sends no hint if the wallet rejects the transfer", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(Response.json(quote()));
    const payable = await retrieveQuote({ apiBase: API_BASE, clientSecret: CLIENT_SECRET, expectedAddress: ADDRESS, fetch });
    const wallet = mockProvider(11155111, [11155111], { eth_sendTransaction: 4001 });
    await expect(payWithWallet(wallet.provider, payable)).rejects.toBeInstanceOf(WalletError);
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
