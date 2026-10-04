import axe from "axe-core";
import { userEvent } from "@testing-library/user-event";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  depositAddressIdFromClientSecret,
  depositAddressTransfer,
  parseClientDepositAddress,
  type DepositAddressAsset,
  type DepositAddressDetails,
  type DepositAddressNetwork,
} from "../../js/src/index.js";
import { DepositAddress } from "../src/index.js";
import { ADDRESS, TOKEN } from "./fixtures.js";

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

afterEach(cleanup);

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
    expect(screen.getByText(ADDRESS)).toBeDefined();
    expect(screen.getByText(TOKEN)).toBeDefined();
    expect(screen.getByRole("button", { name: "Copy Deposit address" })).toBeDefined();
    expect(screen.getByText(/Send PHA on Sepolia/)).toBeDefined();
    // Without the address's public view it knows no network's credit time, and names none.
    expect(screen.getByText(/credited at the market rate after confirmation\./)).toBeDefined();
    expect(screen.queryByText(/seconds|minutes/)).toBeNull();

    fireEvent.click(screen.getByRole("radio", { name: "USDC" }));
    expect(qr().getAttribute("aria-label")).toBe("Deposit address for USDC on Sepolia");
    expect(screen.getByText(USDC)).toBeDefined();

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
    expect(screen.getByText(OTHER)).toBeDefined();
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
    expect(screen.queryByRole("status")).toBeNull();
    unmount();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  /** A network of the public view. */
  function clientNetwork(chainId: number, seconds = 30) {
    return { chain_id: chainId, address: ADDRESS, assets: [asset(chainId, "usdc", USDC)], typical_credit_seconds: seconds };
  }

  /** The address's message once its public view, served with `networks`, is read. */
  async function messageWith(networks: unknown[]): Promise<string> {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.stubGlobal("fetch", () => Promise.resolve(Response.json(view([], networks))));
    const { container } = render(
      <DepositAddress depositAddress={details()} clientSecret={SECRET} apiBase="https://pay.example" />,
    );
    await act(() => vi.advanceTimersByTimeAsync(0));
    return container.querySelector(".pp-message")?.textContent ?? "";
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

  it("states each network's credit time from the public view", async () => {
    expect(await messageWith([clientNetwork(11155111, 30), clientNetwork(84532, 300)])).toContain(
      "credited at the market rate on arrival, usually in about 30 seconds on Sepolia and about 5 minutes on Base Sepolia.",
    );
  });

  it("states one time when every network shares it, as under a finalized policy", async () => {
    expect(await messageWith([clientNetwork(11155111, 900), clientNetwork(84532, 900)])).toContain(
      "credited at the market rate on arrival, usually in about 15 minutes.",
    );
  });

  it("parses the public view and refuses anything else", () => {
    expect(parseClientDepositAddress(view([payment({ status: "reversed" })])).payments[0]?.status).toBe(
      "reversed",
    );
    expect(
      parseClientDepositAddress(view([], [clientNetwork(84532, 300), clientNetwork(11155111, 30)])).networks,
    ).toEqual([
      { chain_id: 84532, address: ADDRESS, typical_credit_seconds: 300 },
      { chain_id: 11155111, address: ADDRESS, typical_credit_seconds: 30 },
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
