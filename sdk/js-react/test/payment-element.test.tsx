import axe from "axe-core";
import { StrictMode } from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
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
  expect(onChange).toHaveBeenLastCalledWith({ step: "options", checkout: null });
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

it("shows the customer's deposit address instead, at the chosen option", async () => {
  const createQuote = vi.fn<CreateQuote>(() => Promise.resolve(PARAMS));
  const networks = OPTIONS.map(({ chain_id, asset }) => ({
    chain_id,
    address: ADDRESS,
    assets: [{ asset, contract: TOKEN, decimals: 18, payment_uri: `ethereum:${TOKEN}@${chain_id}/transfer?address=${ADDRESS}` }],
  }));
  const createDepositAddress = vi.fn(() => Promise.resolve({ address: ADDRESS, networks }));
  render(<PaymentElement options={OPTIONS} defaultValue={OPTIONS[1]} createQuote={createQuote} createDepositAddress={createDepositAddress} />);
  fireEvent.click(screen.getByRole("button", { name: "Use a deposit address" }));
  expect(createDepositAddress).toHaveBeenCalledWith(OPTIONS[1]);
  expect((await screen.findByRole("img")).getAttribute("aria-label")).toBe("Deposit address for USDC on Base Sepolia");
  expect(createQuote).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Back to payment options" }));
  expect(screen.getByRole("button", { name: "Pay in USDC" })).toBeDefined();
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
