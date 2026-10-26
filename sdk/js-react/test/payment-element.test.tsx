import axe from "axe-core";
import { StrictMode } from "react";
import { createWalletClient, custom, decodeFunctionData, encodeFunctionResult, erc20Abi } from "viem";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DepositAddressPayment } from "../../js/src/index.js";
import { PaymentElement, type PaymentElementState } from "../src/index.js";
import { ADDRESS, API_BASE, CLIENT_SECRET, TOKEN, choose, chosen, quote } from "./fixtures.js";

const OPTIONS = [
  { chain_id: 11155111, asset: "pha", price: "Market rate" },
  { chain_id: 84532, asset: "usdc", price: "$1.00" },
];
const PARAMS = { clientSecret: CLIENT_SECRET, expectedAddress: ADDRESS, apiBase: API_BASE };
type CreateQuote = (option: (typeof OPTIONS)[number], request: { idempotencyKey: string }) => Promise<typeof PARAMS>;

beforeEach(() => {
  vi.spyOn(Math, "random").mockReturnValue(0.5);
  const served = quote({ expires_at: Math.floor(Date.now() / 1000) + 900 });
  vi.stubGlobal("fetch", () => Promise.resolve(Response.json(served)));
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it("creates the quote once, on submit, with the chosen option, then shows its checkout", async () => {
  const createQuote = vi.fn<CreateQuote>(() => Promise.resolve(PARAMS));
  const onChange = vi.fn<(state: PaymentElementState) => void>();
  const { container } = render(
    <StrictMode>
      <PaymentElement options={OPTIONS} amount={1000} createQuote={createQuote} onChange={onChange} renderDetail={(option) => option.price} />
    </StrictMode>,
  );
  expect(createQuote).not.toHaveBeenCalled();
  expect((await axe.run(container, { rules: { "color-contrast": { enabled: false } } })).violations).toEqual([]);
  choose("Token", "USDC");
  const pay = screen.getByRole("button", { name: "Pay $10.00 in USDC" });
  fireEvent.click(pay);
  fireEvent.click(pay);
  fireEvent.submit(pay.closest("form") ?? pay);
  expect(createQuote).toHaveBeenCalledTimes(1);
  expect(createQuote.mock.calls[0]?.[0]).toBe(OPTIONS[1]);
  expect(screen.getByRole("button", { name: "Starting your payment…" })).toHaveProperty("disabled", true);

  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
  await waitFor(() => expect(onChange.mock.lastCall?.[0].checkout?.status).toBe("waiting"));
  fireEvent.click(screen.getByRole("button", { name: "Choose another token or network" }));
  expect(chosen()).toBe("USDC · Base Sepolia");
  expect(onChange).toHaveBeenLastCalledWith({ step: "options", checkout: null, payment: null });
});

it("shows a failed quote and lets the payer try again", async () => {
  const error = new Error("backend down");
  const createQuote = vi.fn().mockRejectedValueOnce(error).mockResolvedValueOnce(PARAMS);
  const onError = vi.fn();
  render(<PaymentElement options={OPTIONS} createQuote={createQuote} onError={onError} />);
  fireEvent.click(screen.getByRole("button", { name: "Pay in PHA" }));
  expect((await screen.findByRole("alert")).textContent).toBe("Could not start the payment. Try again.");
  expect(onError).toHaveBeenCalledWith(error);
  fireEvent.click(screen.getByRole("button", { name: "Pay in PHA" }));
  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
  expect(createQuote).toHaveBeenCalledTimes(2);
  expect(screen.queryByRole("alert")).toBeNull();
});

it("shows the host's message for a failure, else its own", async () => {
  const createQuote = vi
    .fn<CreateQuote>()
    .mockRejectedValueOnce(new Error("amount_too_small"))
    .mockRejectedValueOnce(new Error("timeout"))
    .mockResolvedValue(PARAMS);
  const createDepositAddress = vi.fn().mockRejectedValue(new Error("treasury_not_set"));
  const errorMessage = (cause: unknown) =>
    cause instanceof Error && cause.message === "amount_too_small" ? "The minimum top-up is $5.00." : null;
  render(
    <PaymentElement options={OPTIONS} createQuote={createQuote} createDepositAddress={createDepositAddress} errorMessage={errorMessage} />,
  );
  const pay = screen.getByRole("button", { name: "Pay in PHA" });
  fireEvent.click(pay);
  expect((await screen.findByRole("alert")).textContent).toBe("The minimum top-up is $5.00.");
  fireEvent.click(pay);
  await waitFor(() => expect(screen.getByRole("alert").textContent).toBe("Could not start the payment. Try again."));
  fireEvent.click(screen.getByRole("button", { name: "Use a deposit address" }));
  await waitFor(() => expect(screen.getByRole("alert").textContent).toBe("Could not get your deposit address. Try again."));
  // Only one alert at a time, and the retry is the same request.
  expect(screen.getAllByRole("alert")).toHaveLength(1);
  expect(createQuote.mock.calls[1]?.[1].idempotencyKey).toBe(createQuote.mock.calls[0]?.[1].idempotencyKey);
});

it("shows the customer's deposit address instead, at the chosen option", async () => {
  const createQuote = vi.fn<CreateQuote>(() => Promise.resolve(PARAMS));
  const networks = OPTIONS.map(({ chain_id, asset }) => ({
    chain_id,
    address: ADDRESS,
    assets: [{ asset, contract: TOKEN, decimals: 18, payment_uri: `ethereum:${TOKEN}@${chain_id}/transfer?address=${ADDRESS}` }],
  }));
  const createDepositAddress = vi.fn(() => Promise.resolve({ address: ADDRESS, networks }));
  render(<PaymentElement options={OPTIONS} defaultValue={OPTIONS[1]} createQuote={createQuote} createDepositAddress={createDepositAddress} />);
  // One quiet line, with no amount: the address states its own limits.
  expect(screen.getByText(/Paying from an exchange\?/).textContent).toBe("Paying from an exchange? Use a deposit address");
  fireEvent.click(screen.getByRole("button", { name: "Use a deposit address" }));
  expect(createDepositAddress).toHaveBeenCalledWith(OPTIONS[1]);
  expect((await screen.findByRole("img")).getAttribute("aria-label")).toBe("Deposit address for USDC on Base Sepolia");
  expect(createQuote).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Back to payment options" }));
  expect(screen.getByRole("button", { name: "Pay in USDC" })).toBeDefined();
});

it("places the host's summary and button before the deposit-address line, only while choosing", async () => {
  const createQuote = vi.fn<CreateQuote>(() => Promise.resolve(PARAMS));
  render(
    <PaymentElement hideSubmit options={OPTIONS} createQuote={createQuote} createDepositAddress={vi.fn()}>
      <p>Total $10.00</p>
      <button type="submit">Top up</button>
    </PaymentElement>,
  );
  expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual(["Top up", "Use a deposit address"]);
  fireEvent.click(screen.getByRole("button", { name: "Top up" }));
  expect(createQuote.mock.calls[0]?.[0]).toBe(OPTIONS[0]);
  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
  expect(screen.queryByText("Total $10.00")).toBeNull();
});

it("is submitted by the host's own button when its submit is hidden", async () => {
  const createQuote = vi.fn<CreateQuote>(() => Promise.resolve(PARAMS));
  render(
    <>
      <PaymentElement id="pay" hideSubmit options={OPTIONS} createQuote={createQuote} />
      <button type="submit" form="pay">
        Top up
      </button>
    </>,
  );
  expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual(["Top up"]);
  fireEvent.click(screen.getByRole("button", { name: "Top up" }));
  expect(createQuote.mock.calls[0]?.[0]).toBe(OPTIONS[0]);
  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
});

it("creates each quote with its own idempotency key, kept only to retry a failure", async () => {
  const createQuote = vi
    .fn<CreateQuote>()
    .mockRejectedValueOnce(new Error("timeout"))
    .mockRejectedValueOnce(new Error("timeout"))
    .mockResolvedValue(PARAMS);
  render(<PaymentElement options={OPTIONS} createQuote={createQuote} />);
  const key = (call: number) => createQuote.mock.calls[call]?.[1].idempotencyKey;

  // A retry of the same option after a failure is the same request.
  fireEvent.click(screen.getByRole("button", { name: "Pay in PHA" }));
  await screen.findByRole("alert");
  fireEvent.click(screen.getByRole("button", { name: "Pay in PHA" }));
  await screen.findByRole("alert");
  expect(key(1)).toBe(key(0));

  // Another option is another quote.
  choose("Token", "USDC");
  fireEvent.click(screen.getByRole("button", { name: "Pay in USDC" }));
  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
  expect(key(2)).not.toBe(key(1));

  // Back from a created quote, the same option asks for a new one.
  fireEvent.click(screen.getByRole("button", { name: "Choose another token or network" }));
  fireEvent.click(screen.getByRole("button", { name: "Pay in USDC" }));
  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
  expect(key(3)).not.toBe(key(2));
  expect(new Set([key(0), key(2), key(3)]).size).toBe(3);
  expect(key(0)).toMatch(/^[0-9a-f-]{36}$/);
});

it("asks with a new idempotency key once the amount changed after a failure", async () => {
  const createQuote = vi.fn<CreateQuote>().mockRejectedValueOnce(new Error("amount_too_small")).mockResolvedValue(PARAMS);
  const { rerender } = render(<PaymentElement options={OPTIONS} amount={300} createQuote={createQuote} />);
  fireEvent.click(screen.getByRole("button", { name: "Pay $3.00 in PHA" }));
  await screen.findByRole("alert");
  rerender(<PaymentElement options={OPTIONS} amount={2000} createQuote={createQuote} />);
  fireEvent.click(screen.getByRole("button", { name: "Pay $20.00 in PHA" }));
  expect(await screen.findByText("Waiting for your payment")).toBeDefined();
  const [first, second] = createQuote.mock.calls.map((call) => call[1].idempotencyKey);
  expect(second).not.toBe(first);
});

describe("without createQuote", () => {
  const SECRET = `da_${"0d".repeat(16)}_secret_${"ab".repeat(24)}`;
  const HASH = `0x${"cd".repeat(32)}`;
  const EARLIER = `0x${"ef".repeat(32)}`;
  const ACCOUNT = "0x3333333333333333333333333333333333333333";
  const NETWORKS = OPTIONS.map(({ chain_id, asset }) => ({
    chain_id,
    address: ADDRESS,
    assets: [
      {
        asset,
        contract: TOKEN,
        decimals: asset === "usdc" ? 6 : 18,
        payment_uri: `ethereum:${TOKEN}@${chain_id}/transfer?address=${ADDRESS}`,
      },
    ],
  }));
  const created = { address: ADDRESS, networks: NETWORKS, clientSecret: SECRET, apiBase: API_BASE };
  // 10 of the address's token, in its units.
  const amountOf = (_: (typeof OPTIONS)[number], token: { decimals: number }) => 10n * 10n ** BigInt(token.decimals);

  function row(tx_hash: string, status: DepositAddressPayment["status"], created = 1_790_000_000): DepositAddressPayment {
    return {
      status,
      chain_id: 84532,
      asset: "usdc",
      decimals: 6,
      amount_atomic: "10000000",
      tx_hash,
      confirmations: status === "seen" ? 1 : null,
      created,
    };
  }

  /** Serves the address's public view with `payments`, and records transaction hints. */
  function serve(payments: DepositAddressPayment[]) {
    const hints: string[] = [];
    vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(input instanceof Request ? input.url : input.toString());
      if (url.pathname.endsWith("/transactions")) {
        hints.push(typeof init?.body === "string" ? init.body : "");
        return Promise.resolve(new Response("{}", { status: 202 }));
      }
      return Promise.resolve(
        Response.json({
          id: `da_${"0d".repeat(16)}`,
          object: "deposit_address",
          livemode: false,
          status: "active",
          networks: [{ chain_id: 84532, address: ADDRESS, typical_credit_seconds: 7 }],
          payments,
        }),
      );
    });
    return hints;
  }

  it("shows the deposit address on submit, created once, with no secondary deposit line", async () => {
    const createDepositAddress = vi.fn(() => Promise.resolve(created));
    serve([]);
    const { container } = render(
      <StrictMode>
        <PaymentElement
          options={OPTIONS}
          amount={1000}
          createDepositAddress={createDepositAddress}
          depositAmountAtomic={amountOf}
          pollInterval={20}
        />
      </StrictMode>,
    );
    expect(screen.queryByText(/Paying from an exchange/)).toBeNull();
    choose("Token", "USDC");
    const pay = screen.getByRole("button", { name: "Pay $10.00 in USDC" });
    fireEvent.click(pay);
    fireEvent.click(pay);
    fireEvent.submit(pay.closest("form") ?? pay);
    expect(createDepositAddress).toHaveBeenCalledTimes(1);
    expect(createDepositAddress).toHaveBeenCalledWith(OPTIONS[1]);
    expect(screen.getByRole("button", { name: "Starting your payment…" })).toHaveProperty("disabled", true);

    // No wallet in this browser: the QR code and the address are the way to pay, stated once.
    expect(await screen.findByText("10 USDC")).toBeDefined();
    expect(screen.getByRole("img", { name: "Payment request for 10 USDC on Base Sepolia" })).toBeDefined();
    expect(screen.getByText("Send exactly this amount in USDC on Base Sepolia, in one transfer.")).toBeDefined();
    expect(screen.getByRole("button", { name: "Copy Deposit address" })).toBeDefined();
    expect(screen.queryByRole("combobox")).toBeNull();
    expect((await axe.run(container, { rules: { "color-contrast": { enabled: false } } })).violations).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "Back to payment options" }));
    expect(screen.getByRole("button", { name: "Pay $10.00 in USDC" })).toBeDefined();
  });

  it("pays the amount from the wallet, follows that transaction, and reports it credited", async () => {
    const payments: DepositAddressPayment[] = [row(EARLIER, "credited")];
    const hints = serve(payments);
    const sent: `0x${string}`[] = [];
    const provider = {
      request: ({ method, params }: { method: string; params?: unknown }): Promise<unknown> => {
        switch (method) {
          case "eth_chainId":
            return Promise.resolve("0x14a34");
          case "eth_call":
            return Promise.resolve(encodeFunctionResult({ abi: erc20Abi, functionName: "balanceOf", result: 10n ** 12n }));
          case "eth_sendTransaction":
            sent.push((params as [{ data: `0x${string}` }])[0].data);
            return Promise.resolve(HASH);
          default:
            return Promise.reject(new Error(`unexpected ${method}`));
        }
      },
    };
    const walletClient = createWalletClient({ account: ACCOUNT, transport: custom(provider) });
    const onChange = vi.fn<(state: PaymentElementState) => void>();
    const onDepositSuccess = vi.fn<(payment: DepositAddressPayment) => void>();
    const onDepositSent = vi.fn<(txHash: string, chainId: number) => void>();
    render(
      <PaymentElement
        options={OPTIONS}
        defaultValue={OPTIONS[1]}
        createDepositAddress={() => Promise.resolve(created)}
        depositAmountAtomic={amountOf}
        onDepositSent={onDepositSent}
        walletClient={walletClient}
        onChange={onChange}
        onDepositSuccess={onDepositSuccess}
        pollInterval={20}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Pay in USDC" }));
    const wallet = await screen.findByRole("button", { name: "Pay with your wallet" });
    // The QR code and the address wait behind one quiet control.
    const more = screen.getByRole("button", { name: "Pay from an exchange or another wallet" });
    expect(more.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByRole("img", { name: /Payment request/ })).toBeNull();
    fireEvent.click(more);
    expect(screen.getByRole("img", { name: "Payment request for 10 USDC on Base Sepolia" })).toBeDefined();

    // The payment already credited before the address was shown is not this one.
    await waitFor(() => expect(onChange.mock.lastCall?.[0].step).toBe("deposit_address"));
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(onDepositSuccess).not.toHaveBeenCalled();

    fireEvent.click(wallet);
    await waitFor(() => expect(screen.getByRole("status").textContent).toMatch(/^Sent, waiting for it on chain…/));
    expect(decodeFunctionData({ abi: erc20Abi, data: sent[0] ?? "0x" })).toEqual({
      functionName: "transfer",
      args: [ADDRESS, 10_000_000n],
    });
    expect(hints).toEqual([JSON.stringify({ transaction_hash: HASH, chain_id: 84532 })]);
    expect(onDepositSent).toHaveBeenCalledExactlyOnceWith(HASH, 84532);
    // The ways to pay give way to the payment's status.
    expect(screen.queryByRole("button", { name: "Pay with your wallet" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Pay from an exchange or another wallet" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Back to payment options" })).not.toBeNull();

    payments.unshift(row(HASH, "seen"));
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toBe("Received, 1 confirmation. Crediting in about 7 seconds View transaction"),
    );
    await waitFor(() => expect(onChange.mock.lastCall?.[0].payment?.status).toBe("seen"));
    // While the payment is on its way, the element stays on it.
    expect(screen.queryByRole("button", { name: "Back to payment options" })).toBeNull();

    payments[0] = row(HASH, "credited");
    await waitFor(() => expect(onDepositSuccess).toHaveBeenCalledTimes(1));
    expect(onDepositSuccess.mock.calls[0]?.[0]).toMatchObject({ tx_hash: HASH, status: "credited" });
    expect(screen.getByRole("status").textContent).toBe("Payment credited View transaction");
    await waitFor(() =>
      expect(onChange.mock.lastCall?.[0]).toMatchObject({ step: "deposit_address", payment: { status: "credited" } }),
    );
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(onDepositSuccess).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "Start a new payment" })).toBeDefined();
  });

  it("follows a payment sent by QR code or by hand once it arrives", async () => {
    const payments: DepositAddressPayment[] = [row(EARLIER, "seen")];
    serve(payments);
    const onDepositSuccess = vi.fn<(payment: DepositAddressPayment) => void>();
    render(
      <PaymentElement
        options={OPTIONS}
        defaultValue={OPTIONS[1]}
        createDepositAddress={() => Promise.resolve(created)}
        depositAmountAtomic={amountOf}
        onDepositSuccess={onDepositSuccess}
        pollInterval={20}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Pay in USDC" }));
    await screen.findByText("10 USDC");
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(screen.getByRole("status").textContent).toBe("");
    // Another amount than asked for is named as it arrived.
    payments.unshift({ ...row(HASH, "credited"), amount_atomic: "4000000" });
    await waitFor(() => expect(onDepositSuccess).toHaveBeenCalledTimes(1));
    expect(onDepositSuccess.mock.calls[0]?.[0].tx_hash).toBe(HASH);
    expect(screen.getByRole("status").textContent).toBe("4 USDC on Base Sepolia credited");
  });

  it("resumes following a sent payment after a reload, without offering to pay again", async () => {
    const payments: DepositAddressPayment[] = [row(EARLIER, "credited")];
    serve(payments);
    const createDepositAddress = vi.fn(() => Promise.resolve(created));
    const onDepositSuccess = vi.fn<(payment: DepositAddressPayment) => void>();
    render(
      <StrictMode>
        <PaymentElement
          options={OPTIONS}
          defaultValue={OPTIONS[1]}
          createDepositAddress={createDepositAddress}
          depositAmountAtomic={amountOf}
          depositTxHash={{ txHash: HASH, chainId: 84532 }}
          onDepositSuccess={onDepositSuccess}
          pollInterval={20}
        />
      </StrictMode>,
    );
    await waitFor(() => expect(screen.getByRole("status").textContent).toMatch(/^Sent, waiting for it on chain…/));
    expect(createDepositAddress).toHaveBeenLastCalledWith(OPTIONS[1]);
    expect(screen.queryByRole("button", { name: /^Pay / })).toBeNull();
    expect(screen.queryByRole("img", { name: /Payment request/ })).toBeNull();
    // The earlier credited payment is not this one.
    await new Promise((resolve) => setTimeout(resolve, 60));
    expect(onDepositSuccess).not.toHaveBeenCalled();
    payments.unshift(row(HASH, "credited"));
    await waitFor(() => expect(onDepositSuccess).toHaveBeenCalledTimes(1));
    expect(onDepositSuccess.mock.calls[0]?.[0].tx_hash).toBe(HASH);
    expect(screen.getByRole("status").textContent).toBe("Payment credited View transaction");
  });

  it("computes the amount in the units of the address's token, once it is created", async () => {
    serve([]);
    const depositAmountAtomic = vi.fn(amountOf);
    const createDepositAddress = vi.fn(() => Promise.resolve(created));
    render(
      <PaymentElement options={OPTIONS} createDepositAddress={createDepositAddress} depositAmountAtomic={depositAmountAtomic} />,
    );
    // Nothing is computed before the address, whose token states the decimals.
    expect(depositAmountAtomic).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Pay in PHA" }));
    expect(await screen.findByText("10 PHA")).toBeDefined();
    expect(depositAmountAtomic).toHaveBeenCalledExactlyOnceWith(OPTIONS[0], NETWORKS[0]?.assets[0]);
    fireEvent.click(screen.getByRole("button", { name: "Back to payment options" }));
    choose("Token", "USDC");
    fireEvent.click(screen.getByRole("button", { name: "Pay in USDC" }));
    // 10 × 10^6, not 10 × 10^18: the QR code requests exactly that.
    expect(await screen.findByText("10 USDC")).toBeDefined();
    expect(depositAmountAtomic.mock.results[1]?.value).toBe(10_000_000n);
  });

  it("refuses an address that does not take the token to pay", async () => {
    const onError = vi.fn();
    serve([]);
    render(
      <PaymentElement
        options={OPTIONS}
        defaultValue={OPTIONS[1]}
        createDepositAddress={() => Promise.resolve({ ...created, networks: NETWORKS.slice(0, 1) })}
        depositAmountAtomic={amountOf}
        onError={onError}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Pay in USDC" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Could not get your deposit address. Try again.");
    expect(onError.mock.calls[0]?.[0]).toBeInstanceOf(TypeError);
  });
});
