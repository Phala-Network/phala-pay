import { act, cleanup, render, screen, within } from "@testing-library/react";
import { userEvent } from "@testing-library/user-event";
import { createWalletClient, custom, encodeFunctionResult, erc20Abi } from "viem";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Checkout } from "../src/react/index.js";
import type { CheckoutState, ClientQuote, EthereumProvider, WalletError } from "../src/index.js";
import { ADDRESS, API_BASE, CLIENT_SECRET, TOKEN, quote } from "./fixtures.js";

const NOW = (quote().expires_at - 14 * 60 - 32) * 1000;
let served: ClientQuote;

beforeEach(() => {
  vi.useFakeTimers({ now: NOW, shouldAdvanceTime: true });
  served = quote();
  vi.stubGlobal("fetch", () => Promise.resolve(Response.json(served)));
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

async function renderCheckout(props: Partial<Parameters<typeof Checkout>[0]> = {}) {
  const view = render(
    <Checkout
      clientSecret={CLIENT_SECRET}
      expectedAddress={ADDRESS}
      apiBase={API_BASE}
      pollInterval={1000}
      {...props}
    />,
  );
  // Quote readiness does not imply wallet readiness: discovery runs in a child effect.
  await screen.findByText("Waiting for your payment");
  return view;
}

/** A browser wallet on the quote's chain whose account holds `balance` of the token; it records
 * the methods called. */
function browserWallet(hash: string, balance = 10n ** 30n) {
  const methods: string[] = [];
  const provider: EthereumProvider = {
    request: ({ method }) => {
      methods.push(method);
      switch (method) {
        case "eth_requestAccounts":
          return Promise.resolve([ADDRESS]);
        case "eth_chainId":
          return Promise.resolve(`0x${quote().chain_id.toString(16)}`);
        case "eth_call":
          return Promise.resolve(encodeFunctionResult({ abi: erc20Abi, functionName: "balanceOf", result: balance }));
        case "eth_sendTransaction":
          return Promise.resolve(hash);
        default:
          return Promise.reject(new Error(`unexpected ${method}`));
      }
    },
  };
  return { provider, methods };
}

async function poll() {
  await act(() => vi.advanceTimersByTimeAsync(1000));
}

describe("Checkout", () => {
  it("aborts a hung status read on unmount", () => {
    let signal: AbortSignal | null | undefined;
    vi.stubGlobal("fetch", (_: RequestInfo | URL, init?: RequestInit) => {
      signal = init?.signal;
      return new Promise((_, reject) => signal?.addEventListener("abort", () => reject(new DOMException("unmounted", "AbortError"))));
    });
    const { unmount } = render(<Checkout clientSecret={CLIENT_SECRET} expectedAddress={ADDRESS} apiBase={API_BASE} />);
    unmount();
    expect(signal?.aborted).toBe(true);
  });

  it("states the exact amount, the network, and the time left", async () => {
    await renderCheckout();
    expect(screen.getByText("100.502512562814070352 PHA")).toBeDefined();
    expect(screen.getByText(/\$25\.00 top-up ·/).textContent).toContain("Testnet Sepolia");
    expect(screen.getByText(/Test mode/)).toBeDefined();
    expect(screen.getByText(/exactly 100.502512562814070352 PHA/)).toBeDefined();
    expect(screen.getByLabelText("Time left to pay").textContent).toBe("14:32");
    expect(screen.getByRole("status").getAttribute("aria-live")).toBe("polite");
  });

  it("offers the three payment methods as keyboard-operable tabs", async () => {
    const user = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
    await renderCheckout();
    const tabs = screen.getAllByRole("tab");
    expect(tabs.map((t) => t.textContent)).toEqual(["Wallet", "QR code", "Manual"]);
    await user.click(screen.getByRole("tab", { name: "Browser wallet" }));
    expect(screen.getByRole("tabpanel").textContent).toMatch(/Install a browser wallet/);

    tabs[0]?.focus();
    await user.keyboard("{ArrowRight}");
    expect(document.activeElement).toBe(screen.getByRole("tab", { name: "QR code" }));
    const qr = within(screen.getByRole("tabpanel")).getByRole("img");
    expect(qr.getAttribute("aria-label")).toBe("Payment request for 100.502512562814070352 PHA");
    expect(qr.querySelector("path")?.getAttribute("d")).toMatch(/^M\d+ \d+h1v1h-1z/);

    await user.keyboard("{End}");
    const manual = screen.getByRole("tabpanel");
    expect(within(manual).getByText(ADDRESS)).toBeDefined();
    expect(within(manual).getByText(TOKEN)).toBeDefined();
    expect(within(manual).getByText("Sepolia (chain ID 11155111)")).toBeDefined();
  });

  it("defaults to QR without a browser wallet and keeps the wallet tab available", async () => {
    const user = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
    await renderCheckout();
    expect(screen.getByRole("tab", { name: "QR code" }).getAttribute("aria-selected")).toBe("true");
    expect(within(screen.getByRole("tabpanel")).getByRole("img")).toBeDefined();
    await user.click(screen.getByRole("tab", { name: "Browser wallet" }));
    expect(screen.getByRole("tabpanel").textContent).toBe("Install a browser wallet to pay here.");
    // Discovery stays active across tabs, without overriding the payer's choice.
    const { provider } = browserWallet(`0x${"ab".repeat(32)}`);
    act(() => {
      window.dispatchEvent(new CustomEvent("eip6963:announceProvider", {
        detail: { info: { uuid: "late", name: "Late wallet", icon: "", rdns: "wallet.test" }, provider },
      }));
    });
    expect(screen.getByRole("tab", { name: "Browser wallet" }).getAttribute("aria-selected")).toBe("true");
    expect(await screen.findByRole("button", { name: "Pay with crypto (Late wallet)" })).toBeDefined();
  });

  it("copies the address and the exact amount", async () => {
    const user = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    await renderCheckout();
    await user.click(screen.getByRole("tab", { name: "Manual transfer" }));
    await user.click(screen.getByRole("button", { name: "Copy Send to address" }));
    await user.click(screen.getByRole("button", { name: "Copy Exact amount" }));
    expect(writeText.mock.calls).toEqual([[ADDRESS], ["100.502512562814070352"]]);
    expect(screen.getAllByText("Copied")).toHaveLength(2);
  });

  it("follows the payment to credited, hides the payment options, and calls onSuccess once", async () => {
    const onSuccess = vi.fn();
    await renderCheckout({ onSuccess });

    served = quote({ payment_status: "seen", confirmations: 2 });
    await poll();
    expect(screen.getByRole("status").textContent).toBe(
      "Received, 2 confirmations. Crediting in about 30 seconds",
    );
    expect(screen.queryByRole("tablist")).toBeNull();

    served = quote({ status: "complete", payment_status: "credited", amount_credited: 2500 });
    await poll();
    await poll();
    expect(screen.getByRole("status").textContent).toBe("Payment credited: $25.00");
    expect(onSuccess).toHaveBeenCalledTimes(1);
    expect(onSuccess).toHaveBeenCalledWith(served);
  });

  it.each([
    [300, "Received, 1 confirmation. Crediting in about 5 minutes"],
    [900, "Received, 1 confirmation. Crediting in about 15 minutes"],
  ])("tells the payer the typical wait of the quote's confirmation (%i s)", async (seconds, message) => {
    served = quote({ typical_credit_seconds: seconds });
    await renderCheckout();
    served = quote({ typical_credit_seconds: seconds, payment_status: "seen", confirmations: 1 });
    await poll();
    expect(screen.getByRole("status").textContent).toBe(message);
  });

  it.each([
    [1000, "Payment credited: $10.00 of $25.00"],
    [3000, "Payment credited: $30.00 ($25.00 quoted)"],
  ])("states what a market-priced payment credited and passes it to onSuccess (%i)", async (credited, message) => {
    const onSuccess = vi.fn<(quote: ClientQuote) => void>();
    await renderCheckout({ onSuccess });
    served = quote({ payment_status: "credited", amount_credited: credited });
    await poll();
    expect(screen.getByRole("status").textContent).toBe(message);
    expect(onSuccess).toHaveBeenCalledWith(expect.objectContaining({ amount: 2500, amount_credited: credited }));
  });

  it("calls onChange once per status change, not on every poll", async () => {
    const onChange = vi.fn<(state: CheckoutState) => void>();
    await renderCheckout({ onChange });

    served = quote({ payment_status: "seen", confirmations: 2 });
    await poll();
    served = quote({ payment_status: "seen", confirmations: 3 });
    await poll();
    served = quote({ payment_status: "confirming" });
    await poll();

    const statuses = onChange.mock.calls.map(([state]) => state.status);
    expect(statuses.filter((status) => status !== "loading")).toEqual(["waiting", "seen", "confirming"]);
    expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ quote: served, error: null }));
  });

  it("tells the payer not to pay after expiry, and calls onExpire", async () => {
    const onExpire = vi.fn();
    await renderCheckout({ onExpire });
    vi.setSystemTime(quote().expires_at * 1000);
    await poll();
    expect(screen.getByRole("status").textContent).toMatch(/expired. Do not send funds/);
    expect(screen.queryByText(ADDRESS)).toBeNull();
    expect(onExpire).toHaveBeenCalledTimes(1);
  });

  it("sets the theme without inline styles", async () => {
    const { container } = await renderCheckout({ appearance: { theme: "dark" } });
    expect(container.querySelector(".pp-root")?.getAttribute("data-theme")).toBe("dark");
    expect(container.querySelector("style, [style]")).toBeNull();
  });

  it("notifies expiry and later credit once each for the same quote", async () => {
    const onExpire = vi.fn();
    const onSuccess = vi.fn();
    await renderCheckout({ onExpire, onSuccess });
    vi.setSystemTime(served.expires_at * 1000);
    await poll();
    expect(screen.getByRole("status").textContent).toMatch(/expired/);
    expect(onExpire).toHaveBeenCalledTimes(1);
    await poll();
    served = quote({ payment_status: "credited", amount_credited: 2500 });
    await poll();
    expect(screen.getByRole("status").textContent).toMatch(/Payment credited/);
    expect(onSuccess).toHaveBeenCalledExactlyOnceWith(served);
    expect(onExpire).toHaveBeenCalledTimes(1);
    await poll();
    expect(onSuccess).toHaveBeenCalledTimes(1);
  });

  it("shows the full transaction hash after a wallet payment, linked to the explorer", async () => {
    const hash = `0x${"ab".repeat(32)}`;
    vi.stubGlobal("ethereum", browserWallet(hash).provider);
    const user = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
    await renderCheckout();
    await user.click(await screen.findByRole("button", { name: "Pay with crypto (Browser wallet)" }));
    const link = await screen.findByRole("link", { name: hash });
    expect(link.getAttribute("href")).toBe(`https://sepolia.etherscan.io/tx/${hash}`);
  });

  it("pays with the page's own wallet client instead of discovered wallets", async () => {
    const hash = `0x${"cd".repeat(32)}`;
    const { provider, methods } = browserWallet(hash);
    vi.stubGlobal("ethereum", provider);
    const walletClient = createWalletClient({ account: ADDRESS, transport: custom(provider) });
    const user = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
    await renderCheckout({ walletClient });
    const panel = screen.getByRole("tabpanel");
    expect(within(panel).getAllByRole("button").map((b) => b.getAttribute("aria-label"))).toEqual([
      "Pay with crypto",
    ]);
    await user.click(within(panel).getByRole("button", { name: "Pay with crypto" }));
    await screen.findByRole("link", { name: hash });
    expect(methods).not.toContain("eth_requestAccounts");
  });

  it("sends nothing from a wallet holding too little, says so, and reports it to the page", async () => {
    const { provider, methods } = browserWallet(`0x${"ef".repeat(32)}`, 10n ** 18n);
    vi.stubGlobal("ethereum", provider);
    const errors: [WalletError, unknown][] = [];
    const user = userEvent.setup({ advanceTimers: (ms) => vi.advanceTimersByTime(ms) });
    await renderCheckout({ onWalletError: (error, wallet) => errors.push([error, wallet]) });
    await user.click(await screen.findByRole("button", { name: "Pay with crypto (Browser wallet)" }));
    await screen.findByText(
      "Your wallet holds 1 PHA, less than the 100.502512562814070352 PHA to pay. Nothing was sent.",
    );
    // With the wallet that tried, so the page funds that one.
    expect(errors.map(([error, wallet]) => [error.code, wallet])).toEqual([["insufficient_balance", provider]]);
    expect(methods).not.toContain("eth_sendTransaction");
  });
});
