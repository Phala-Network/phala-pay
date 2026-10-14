import axe from "axe-core";
import { StrictMode } from "react";
import { userEvent } from "@testing-library/user-event";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  depositAddressIdFromClientSecret,
  depositAddressTransfer,
  parseClientDepositAddress,
  type ClientDepositAddress,
  type DepositAddressAsset,
  type DepositAddressDetails,
  type DepositAddressNetwork,
} from "../../js/src/index.js";
import { DepositAddress } from "../src/index.js";
import { ADDRESS, TOKEN, shownValue } from "./fixtures.js";

const USDC = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";
const OTHER = `0x${"2".repeat(40)}`;

function asset(chainId: number, symbol: string, contract: string, to = ADDRESS): DepositAddressAsset {
  return {
    asset: symbol,
    contract,
    decimals: 18,
    payment_uri: `ethereum:${contract}@${chainId}/transfer?address=${to}`,
  };
}

function network(chainId: number, assets: DepositAddressAsset[], address = ADDRESS): DepositAddressNetwork {
  return { chain_id: chainId, address, assets };
}

function details(overrides: Partial<DepositAddressDetails> = {}): DepositAddressDetails {
  return {
    address: ADDRESS,
    networks: [
      network(11155111, [asset(11155111, "pha", TOKEN), asset(11155111, "usdc", USDC)]),
      network(84532, [asset(84532, "usdc", USDC)]),
    ],
    ...overrides,
  };
}

beforeEach(() => {
  vi.spyOn(Math, "random").mockReturnValue(0.5);
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("depositAddressTransfer", () => {
  it("reads one token's transfer request, which names no amount", () => {
    const sepolia = network(11155111, [asset(11155111, "pha", TOKEN)]);
    expect(depositAddressTransfer(sepolia, asset(11155111, "pha", TOKEN))).toEqual({
      chainId: 11155111,
      token: TOKEN,
      to: ADDRESS,
      amount: undefined,
    });
  });

  it.each([
    ["another recipient", network(11155111, [], OTHER), asset(11155111, "pha", TOKEN)],
    ["another chain", network(1, []), asset(11155111, "pha", TOKEN)],
    ["another token", network(11155111, []), { ...asset(11155111, "pha", TOKEN), contract: USDC }],
    [
      "an amount",
      network(11155111, []),
      { ...asset(11155111, "pha", TOKEN), payment_uri: `ethereum:${TOKEN}@11155111/transfer?address=${ADDRESS}&uint256=1` },
    ],
    [
      "a native transfer",
      network(11155111, []),
      { ...asset(11155111, "pha", TOKEN), payment_uri: `ethereum:${ADDRESS}@11155111?value=1` },
    ],
  ])("refuses a payment URI with %s", (_, onNetwork, token) => {
    expect(() => depositAddressTransfer(onNetwork, token)).toThrow(TypeError);
  });
});

describe("DepositAddress", () => {
  it("supports native radio keyboard selection and has no accessibility violations", async () => {
    const user = userEvent.setup();
    const { container } = render(<DepositAddress depositAddress={details()} />);
    const first = screen.getByRole("radio", { name: "Sepolia" });
    first.focus();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("radio", { name: "Base Sepolia" })).toBe(document.activeElement);
    expect(screen.getByRole("img").getAttribute("aria-label")).toBe("Deposit address for USDC on Base Sepolia");
    await user.keyboard("{ArrowRight}");
    expect(first).toBe(document.activeElement);
    await user.tab();
    expect(screen.getByRole("radio", { name: "PHA" })).toBe(document.activeElement);
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("radio", { name: "USDC" })).toBe(document.activeElement);
    const result = await axe.run(container, { rules: { "color-contrast": { enabled: false } } });
    expect(result.violations).toEqual([]);
    expect(container.querySelector("style, [style]")).toBeNull();
  });

  it("shows one address for every network and token, with a QR code per network and token", () => {
    render(<DepositAddress depositAddress={details()} />);
    expect(screen.getByText("One reusable address for supported tokens and networks")).toBeDefined();
    const qr = () => screen.getByRole("img");
    expect(qr().getAttribute("aria-label")).toBe("Deposit address for PHA on Sepolia");
    expect(qr().querySelector("path")?.getAttribute("d")).toMatch(/^M\d+ \d+h1v1h-1z/);
    expect(screen.getByText("Sepolia (chain ID 11155111)")).toBeDefined();
    expect(screen.getByText(shownValue(ADDRESS))).toBeDefined();
    expect(screen.getByText(shownValue(TOKEN))).toBeDefined();
    expect(screen.getByRole("button", { name: "Copy Deposit address" })).toBeDefined();
    expect(screen.getByText(/Send PHA on Sepolia/)).toBeDefined();
    // Without the address's public view it knows no network's credit time, and names none.
    expect(screen.getByText(/credited at the market rate after confirmation\./)).toBeDefined();
    expect(screen.queryByText(/seconds|minutes/)).toBeNull();

    fireEvent.click(screen.getByRole("radio", { name: "USDC" }));
    expect(qr().getAttribute("aria-label")).toBe("Deposit address for USDC on Sepolia");
    expect(screen.getByText(shownValue(USDC))).toBeDefined();

    fireEvent.click(screen.getByRole("radio", { name: "Base Sepolia" }));
    expect(qr().getAttribute("aria-label")).toBe("Deposit address for USDC on Base Sepolia");
    expect(screen.getByText("Base Sepolia (chain ID 84532)")).toBeDefined();
    // One token on this network: no token tabs.
    expect(screen.queryByRole("group", { name: "Token" })).toBeNull();
  });

  it("shows each network's own address when they differ", () => {
    render(
      <DepositAddress
        chainId={84532}
        depositAddress={details({
          address: null,
          networks: [
            network(11155111, [asset(11155111, "pha", TOKEN)]),
            network(84532, [asset(84532, "usdc", USDC, OTHER)], OTHER),
          ],
        })}
      />,
    );
    expect(screen.getByText(/varies by network/)).toBeDefined();
    expect(screen.getByText(shownValue(OTHER))).toBeDefined();
  });

  it("refuses details whose payment URI pays another address", () => {
    expect(() =>
      render(
        <DepositAddress
          depositAddress={details({ networks: [network(11155111, [asset(11155111, "pha", TOKEN)], OTHER)] })}
        />,
      ),
    ).toThrow(TypeError);
  });
});

describe("DepositAddress payments", () => {
  it("aborts a hung payment read on unmount", () => {
    let signal: AbortSignal | null | undefined;
    vi.stubGlobal("fetch", (_: RequestInfo | URL, init?: RequestInit) => {
      signal = init?.signal;
      return new Promise((_, reject) => signal?.addEventListener("abort", () => reject(new DOMException("unmounted", "AbortError"))));
    });
    const { unmount } = render(<DepositAddress depositAddress={details()} clientSecret={`da_${"0d".repeat(16)}_secret_${"ab".repeat(24)}`} apiBase="https://pay.example" />);
    unmount();
    expect(signal?.aborted).toBe(true);
  });

  const SECRET = `da_${"0d".repeat(16)}_secret_${"ab".repeat(24)}`;

  function view(payments: unknown[], networks: unknown[] = []) {
    return {
      id: `da_${"0d".repeat(16)}`,
      object: "deposit_address",
      livemode: false,
      status: "active",
      address: ADDRESS,
      networks,
      payments,
    };
  }

  it.each([[0, 800], [1, 1200]])("jitters payment reads with random sample %s", async (sample, delay) => {
    vi.useFakeTimers();
    vi.mocked(Math.random).mockReturnValue(sample);
    const fetch = vi.fn(() => Promise.resolve(Response.json(view([]))));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} />);
    await act(() => vi.advanceTimersByTimeAsync(delay - 1));
    expect(fetch).toHaveBeenCalledTimes(1);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("jitters failed-read backoff", async () => {
    vi.useFakeTimers();
    vi.mocked(Math.random).mockReturnValue(0);
    const fetch = vi.fn(() => Promise.resolve(new Response("offline", { status: 503 })));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} />);
    await act(() => vi.advanceTimersByTimeAsync(1599));
    expect(fetch).toHaveBeenCalledTimes(1);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("makes no requests while hidden and reads immediately on visibility regain", async () => {
    vi.useFakeTimers();
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    const fetch = vi.fn(() => Promise.resolve(Response.json(view([]))));
    vi.stubGlobal("fetch", fetch);
    const { unmount } = render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" />);
    await act(() => vi.advanceTimersByTimeAsync(60_000));
    expect(fetch).not.toHaveBeenCalled();
    visibility.mockReturnValue("visible");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetch).toHaveBeenCalledTimes(1);
    visibility.mockReturnValue("hidden");
    fireEvent(document, new Event("visibilitychange"));
    await act(() => vi.advanceTimersByTimeAsync(60_000));
    expect(fetch).toHaveBeenCalledTimes(1);
    visibility.mockReturnValue("visible");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(fetch).toHaveBeenCalledTimes(3);
    unmount();
    document.dispatchEvent(new Event("visibilitychange"));
    await act(() => vi.advanceTimersByTimeAsync(60_000));
    expect(fetch).toHaveBeenCalledTimes(3);
  });

  it("keeps a single polling loop when visibility changes during a request", async () => {
    vi.useFakeTimers();
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    let respond: ((response: Response) => void) | undefined;
    const fetch = vi.fn<() => Promise<Response>>()
      .mockImplementationOnce(() => new Promise<Response>((resolve) => { respond = resolve; }))
      .mockImplementation(() => Promise.resolve(Response.json(view([]))));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} />);
    visibility.mockReturnValue("hidden");
    fireEvent(document, new Event("visibilitychange"));
    visibility.mockReturnValue("visible");
    fireEvent(document, new Event("visibilitychange"));
    fireEvent(document, new Event("visibilitychange"));
    expect(fetch).toHaveBeenCalledTimes(1);
    await act(async () => {
      respond?.(Response.json(view([])));
      await vi.advanceTimersByTimeAsync(4000);
    });
    expect(fetch).toHaveBeenCalledTimes(5);
  });

  it("uses a 15-second interval only after ten minutes without a view change", async () => {
    vi.useFakeTimers();
    let served = view([]);
    const fetch = vi.fn(() => Promise.resolve(Response.json(served)));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" />);
    await act(() => vi.advanceTimersByTimeAsync(600_000));
    expect(fetch).toHaveBeenCalledTimes(201);
    await act(() => vi.advanceTimersByTimeAsync(14_999));
    expect(fetch).toHaveBeenCalledTimes(201);
    served = view([payment()]);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(fetch).toHaveBeenCalledTimes(202);
    await act(() => vi.advanceTimersByTimeAsync(2999));
    expect(fetch).toHaveBeenCalledTimes(202);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(fetch).toHaveBeenCalledTimes(203);
  });

  it("keeps a 30-second interval after ten idle minutes instead of speeding up", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(() => Promise.resolve(Response.json(view([]))));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={30000} />);
    await act(() => vi.advanceTimersByTimeAsync(600_000));
    expect(fetch).toHaveBeenCalledTimes(21);
    await act(() => vi.advanceTimersByTimeAsync(29_999));
    expect(fetch).toHaveBeenCalledTimes(21);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(fetch).toHaveBeenCalledTimes(22);
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(fetch).toHaveBeenCalledTimes(23);
  });

  it("restarts the ten-minute idle window when any observed view field changes", async () => {
    vi.useFakeTimers();
    let served = view([], [clientNetwork(11155111)]);
    const fetch = vi.fn(() => Promise.resolve(Response.json(served)));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" />);
    await act(() => vi.advanceTimersByTimeAsync(597_000));
    served = view([], [clientNetwork(11155111, 60)]);
    await act(() => vi.advanceTimersByTimeAsync(3000));
    const reads = fetch.mock.calls.length;
    await act(() => vi.advanceTimersByTimeAsync(6000));
    expect(fetch).toHaveBeenCalledTimes(reads + 2);
    await act(() => vi.advanceTimersByTimeAsync(594_000));
    const idleReads = fetch.mock.calls.length;
    await act(() => vi.advanceTimersByTimeAsync(14_999));
    expect(fetch).toHaveBeenCalledTimes(idleReads);
    await act(() => vi.advanceTimersByTimeAsync(1));
    expect(fetch).toHaveBeenCalledTimes(idleReads + 1);
  });

  it("returns to normal polling on visibility regain even if the view stays the same", async () => {
    vi.useFakeTimers();
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    const fetch = vi.fn(() => Promise.resolve(Response.json(view([]))));
    vi.stubGlobal("fetch", fetch);
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" />);
    await act(() => vi.advanceTimersByTimeAsync(600_000));
    const reads = fetch.mock.calls.length;
    visibility.mockReturnValue("hidden");
    fireEvent(document, new Event("visibilitychange"));
    await act(() => vi.advanceTimersByTimeAsync(120_000));
    expect(fetch).toHaveBeenCalledTimes(reads);
    visibility.mockReturnValue("visible");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetch).toHaveBeenCalledTimes(reads + 1);
    await act(() => vi.advanceTimersByTimeAsync(3000));
    expect(fetch).toHaveBeenCalledTimes(reads + 2);
  });

  it("notifies once per observed view change and uses the latest callback without restarting polling", async () => {
    vi.useFakeTimers();
    let served = view([]);
    let online = true;
    const fetch = vi.fn(() => online ? Promise.resolve(Response.json(served)) : Promise.reject(new TypeError("offline")));
    vi.stubGlobal("fetch", fetch);
    const onChange = vi.fn();
    const props = { depositAddress: details(), clientSecret: SECRET, apiBase: "https://pay.example", pollInterval: 1000 };
    const { rerender } = render(<DepositAddress {...props} onChange={onChange} />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(onChange).toHaveBeenCalledExactlyOnceWith(parseClientDepositAddress(served));
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(onChange).toHaveBeenCalledTimes(1);
    served = view([payment()]);
    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenLastCalledWith(parseClientDepositAddress(served));
    const latest = vi.fn();
    const reads = fetch.mock.calls.length;
    rerender(<DepositAddress {...props} onChange={latest} />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(fetch).toHaveBeenCalledTimes(reads);
    expect(latest).not.toHaveBeenCalled();
    served = view([payment({ confirmations: 2 })]);
    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(latest).toHaveBeenCalledExactlyOnceWith(parseClientDepositAddress(served));
    online = false;
    await act(() => vi.advanceTimersByTimeAsync(1000));
    online = true;
    await act(() => vi.advanceTimersByTimeAsync(2000));
    expect(latest).toHaveBeenCalledTimes(1);
    served = view([payment({ status: "credited", confirmations: null })]);
    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(latest).toHaveBeenCalledTimes(2);
    expect(latest).toHaveBeenLastCalledWith(parseClientDepositAddress(served));
  });

  it("does not re-notify identical content when pollInterval changes", async () => {
    vi.useFakeTimers();
    let served = view([]);
    const fetch = vi.fn(() => Promise.resolve(Response.json(served)));
    vi.stubGlobal("fetch", fetch);
    const onChange = vi.fn();
    const props = { depositAddress: details(), clientSecret: SECRET, apiBase: "https://pay.example", onChange };
    const { rerender } = render(<DepositAddress {...props} pollInterval={1000} />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(onChange).toHaveBeenCalledExactlyOnceWith(parseClientDepositAddress(served));
    rerender(<DepositAddress {...props} pollInterval={30000} />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenCalledTimes(1);
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(fetch).toHaveBeenCalledTimes(3);
    expect(onChange).toHaveBeenCalledTimes(1);
    served = view([payment()]);
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenLastCalledWith(parseClientDepositAddress(served));
  });

  it("rethrows callback errors in a microtask while continuing to poll", async () => {
    vi.useFakeTimers();
    let served = view([payment()]);
    const fetch = vi.fn(() => Promise.resolve(Response.json(served)));
    vi.stubGlobal("fetch", fetch);
    const originalMicrotask = globalThis.queueMicrotask;
    const microtask = vi.fn<typeof queueMicrotask>(() => {
      vi.stubGlobal("queueMicrotask", originalMicrotask);
    });
    const error = new Error("consumer callback failed");
    const onChange = vi.fn(() => {
      vi.stubGlobal("queueMicrotask", microtask);
      throw error;
    });
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} onChange={onChange} />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(microtask).toHaveBeenCalledTimes(1);
    expect(() => microtask.mock.calls[0]?.[0]()).toThrow(error);
    expect(screen.getByText("1.5 PHA received on Sepolia, 1 confirmation")).toBeDefined();
    expect(screen.getByRole("status").textContent).toBe("");
    served = view([payment({ status: "credited" })]);
    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(microtask).toHaveBeenCalledTimes(2);
    expect(screen.getByText("1.5 PHA on Sepolia credited")).toBeDefined();
    expect(screen.getByRole("status").textContent).toBe("");
  });

  it("removes its visibilitychange listener on unmount", async () => {
    vi.useFakeTimers();
    const add = vi.spyOn(document, "addEventListener");
    const remove = vi.spyOn(document, "removeEventListener");
    const fetch = vi.fn(() => Promise.resolve(Response.json(view([]))));
    vi.stubGlobal("fetch", fetch);
    const { unmount } = render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(add).toHaveBeenCalledWith("visibilitychange", expect.any(Function));
    const registration = add.mock.calls.find(([type]) => type === "visibilitychange");
    unmount();
    expect(remove).toHaveBeenCalledWith("visibilitychange", registration?.[1]);
    document.dispatchEvent(new Event("visibilitychange"));
    await act(() => vi.advanceTimersByTimeAsync(60_000));
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("notifies the first successful view only once under StrictMode", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("fetch", () => Promise.resolve(Response.json(view([]))));
    const onChange = vi.fn();
    render(
      <StrictMode>
        <DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} onChange={onChange} />
      </StrictMode>,
    );
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(onChange).toHaveBeenCalledExactlyOnceWith(parseClientDepositAddress(view([])));
  });

  it("does not report an observed view without client credentials", () => {
    const onChange = vi.fn();
    render(<DepositAddress depositAddress={details()} onChange={onChange} />);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("notifies every observed change when React batches several polling updates", async () => {
    vi.useFakeTimers();
    const served = [view([]), view([payment()]), view([payment({ confirmations: 2 })]), view([payment({ status: "credited" })])];
    let reads = 0;
    vi.stubGlobal("fetch", () => Promise.resolve(Response.json(served[Math.min(reads++, served.length - 1)])));
    const onChange = vi.fn<(state: ClientDepositAddress) => void>();
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} onChange={onChange} />);
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(onChange.mock.calls.map(([state]) => state)).toEqual(served.map(parseClientDepositAddress));
  });

  it("keeps payments and selections during a three-minute outage, then resumes", async () => {
    vi.useFakeTimers();
    let online = true;
    vi.stubGlobal("fetch", () => online
      ? Promise.resolve(Response.json(view([payment({ status: "seen" })], [clientNetwork(11155111)])))
      : Promise.reject(new TypeError("offline")));
    const { unmount } = render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} />);
    await act(() => vi.advanceTimersByTimeAsync(0));
    fireEvent.click(screen.getByRole("radio", { name: "Base Sepolia" }));
    const payments = screen.getByRole("list", { name: "Payments" }).textContent;
    online = false;
    await act(() => vi.advanceTimersByTimeAsync(180000));
    expect(screen.getByRole("status").textContent).toBe("Reconnecting…");
    expect(screen.getByRole("radio", { name: "Base Sepolia" })).toHaveProperty("checked", true);
    expect(screen.getByRole("list", { name: "Payments" }).textContent).toBe(payments);
    online = true;
    await act(() => vi.advanceTimersByTimeAsync(30000));
    expect(screen.getByRole("status").textContent).toBe("");
    unmount();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  /** A network of the public view. */
  function clientNetwork(chainId: number, seconds = 30) {
    return { chain_id: chainId, address: ADDRESS, assets: [asset(chainId, "usdc", USDC)], typical_credit_seconds: seconds };
  }

  /** The address's message once its public view, served with `networks`, is read. */
  async function messageWith(networks: unknown[], initial = details()): Promise<string> {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.stubGlobal("fetch", () => Promise.resolve(Response.json(view([], networks))));
    const { container } = render(
      <DepositAddress depositAddress={initial} clientSecret={SECRET} apiBase="https://pay.example" />,
    );
    await act(() => vi.advanceTimersByTimeAsync(0));
    return [...container.querySelectorAll(".pp-message")].at(-1)?.textContent ?? "";
  }

  function payment(overrides: Record<string, unknown> = {}) {
    return {
      status: "seen",
      chain_id: 11155111,
      asset: "pha",
      decimals: 18,
      amount_atomic: "1500000000000000000",
      tx_hash: `0x${"ab".repeat(32)}`,
      confirmations: 1,
      created: 1_790_000_000,
      ...overrides,
    };
  }

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("shows a payment within a block of arriving, then its credit", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const served = [view([]), view([payment()]), view([payment({ status: "credited", confirmations: null })])];
    const urls: string[] = [];
    vi.stubGlobal("fetch", (url: string) => {
      urls.push(url);
      return Promise.resolve(Response.json(served[Math.min(urls.length - 1, served.length - 1)]));
    });
    render(
      <DepositAddress
        depositAddress={details()}
        clientSecret={SECRET}
        apiBase="https://pay.example/"
        pollInterval={1000}
      />,
    );
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(screen.queryByRole("list", { name: "Payments" })).toBeNull();
    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(screen.getByText("1.5 PHA received on Sepolia, 1 confirmation")).toBeDefined();
    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(screen.getByText("1.5 PHA on Sepolia credited")).toBeDefined();
    expect(urls[0]).toBe(
      `https://pay.example/v1/deposit_addresses/da_${"0d".repeat(16)}?client_secret=${encodeURIComponent(SECRET)}`,
    );
  });

  it("states each network's credit time after the discovery scan, as a payer sends no hint", async () => {
    // Up to 5 minutes to discover an unhinted transfer, then the view's 30 s or 7 s confirmation.
    expect(await messageWith([clientNetwork(11155111, 30), clientNetwork(84532, 7)])).toContain(
      "Eligible transfers are credited at the market rate, usually within about 6 minutes on Sepolia and about 5 minutes on Base Sepolia.",
    );
  });

  it("formats the selected token's exact per-transfer range and changes it with the selection", () => {
    const pha = {
      ...asset(11155111, "pha", TOKEN),
      min_deposit_atomic: "100000000000000001",
      max_deposit_atomic: "10100000000000000000000",
    };
    const usdc = {
      ...asset(11155111, "usdc", USDC), decimals: 6,
      min_deposit_atomic: "1500000", max_deposit_atomic: "10100000000",
    };
    const base = {
      ...asset(84532, "usdc", USDC), decimals: 6,
      min_deposit_atomic: "0", max_deposit_atomic: "50000000",
    };
    render(<DepositAddress depositAddress={details({ networks: [
      network(11155111, [pha, usdc]), network(84532, [base]),
    ] })} />);
    expect(screen.getByText(/Send 0\.100000000000000001–10100 PHA per transfer on Sepolia\./)).toBeDefined();
    fireEvent.click(screen.getByRole("radio", { name: "USDC" }));
    expect(screen.getByText(/Send 1\.5–10100 USDC per transfer on Sepolia\./)).toBeDefined();
    fireEvent.click(screen.getByRole("radio", { name: "Base Sepolia" }));
    expect(screen.getByText(/Send 0–50 USDC per transfer on Base Sepolia\./)).toBeDefined();
    expect(screen.queryByText(/Any amount/)).toBeNull();
  });

  it("uses current public-view limits instead of the original backend response", async () => {
    const current = {
      ...clientNetwork(11155111),
      assets: [{ ...asset(11155111, "pha", TOKEN),
        min_deposit_atomic: "25000000000000000000", max_deposit_atomic: "10100000000000000000000" }],
    };
    const initial = details({ networks: [network(11155111, [{
      ...asset(11155111, "pha", TOKEN), min_deposit_atomic: "0", max_deposit_atomic: "1000000000000000000",
    }])] });
    expect(await messageWith([current], initial)).toContain("Send 25–10100 PHA per transfer on Sepolia.");
    expect(screen.getByText(/Eligible transfers are credited at the market rate, usually within about 6 minutes\./)).toBeDefined();
  });

  it("uses neutral copy when older servers omit limits or only one bound is known", async () => {
    const old = clientNetwork(11155111);
    old.assets = [asset(11155111, "pha", TOKEN)];
    const message = await messageWith([old]);
    expect(message).toContain("Send PHA on Sepolia.");
    expect(message).toContain("Eligible transfers are credited at the market rate, usually within about 6 minutes.");
    expect(message).not.toMatch(/Any amount|per transfer/);
    cleanup();
    render(<DepositAddress depositAddress={details({ networks: [network(11155111, [
      { ...asset(11155111, "pha", TOKEN), max_deposit_atomic: "1" },
    ])] })} />);
    expect(screen.getByText(/Send PHA on Sepolia\./)).toBeDefined();
    expect(screen.queryByText(/per transfer|Any amount/)).toBeNull();
  });

  it("validates bounds before using public-view amounts", () => {
    const valid = { ...asset(11155111, "pha", TOKEN),
      min_deposit_atomic: "0", max_deposit_atomic: "10100000000000000000000" };
    const body = (overrides: Record<string, unknown>) => view([], [{
      ...clientNetwork(11155111), assets: [{ ...valid, ...overrides }],
    }]);
    expect(parseClientDepositAddress(body({})).networks[0]?.assets?.[0]).toEqual(valid);
    for (const invalid of [null, -1, "-1", "1.5", "1e18", "", "abc"]) {
      expect(() => parseClientDepositAddress(body({ min_deposit_atomic: invalid }))).toThrow(TypeError);
      expect(() => parseClientDepositAddress(body({ max_deposit_atomic: invalid }))).toThrow(TypeError);
    }
    expect(() => parseClientDepositAddress(body({ min_deposit_atomic: "2", max_deposit_atomic: "1" }))).toThrow(TypeError);
    expect(() => parseClientDepositAddress(body({ decimals: 256 }))).toThrow(TypeError);
    expect(() => parseClientDepositAddress(body({ decimals: -1 }))).toThrow(TypeError);
  });

  it("states one time when every network's rounds to the same, as under a finalized policy", async () => {
    expect(await messageWith([clientNetwork(11155111, 900), clientNetwork(84532, 900)])).toContain(
      "Eligible transfers are credited at the market rate, usually within about 20 minutes.",
    );
    expect(await messageWith([clientNetwork(11155111, 7), clientNetwork(84532, 2)])).toContain(
      "Eligible transfers are credited at the market rate, usually within about 5 minutes.",
    );
  });

  it("parses the public view and refuses anything else", () => {
    expect(parseClientDepositAddress(view([payment({ status: "reversed" })])).payments[0]?.status).toBe(
      "reversed",
    );
    expect(
      parseClientDepositAddress(view([], [clientNetwork(84532, 300), clientNetwork(11155111, 30)])).networks,
    ).toEqual([
      clientNetwork(84532, 300),
      clientNetwork(11155111, 30),
    ]);
    for (const seconds of [undefined, -1, 1.5, "300", null]) {
      expect(() =>
        parseClientDepositAddress(view([], [{ ...clientNetwork(84532), typical_credit_seconds: seconds }])),
      ).toThrow(TypeError);
    }
    expect(() => parseClientDepositAddress({ ...view([]), networks: undefined })).toThrow(TypeError);
    expect(() => parseClientDepositAddress(view([payment({ status: "final" })]))).toThrow(TypeError);
    expect(() => parseClientDepositAddress({ ...view([]), livemode: "no" })).toThrow(TypeError);
    expect(() => depositAddressIdFromClientSecret(`qt_${"0c".repeat(16)}_secret_ab`)).toThrow(TypeError);
  });

  it("keeps showing the address when the secret is not valid, and stops asking", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let calls = 0;
    vi.stubGlobal("fetch", () => {
      calls += 1;
      return Promise.resolve(new Response("{}", { status: 404 }));
    });
    render(<DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" pollInterval={1000} />);
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(calls).toBe(1);
    expect(screen.getByText("Deposit address")).toBeDefined();
  });
});
