import axe from "axe-core";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { PaymentOptionPicker } from "../src/index.js";

afterEach(cleanup);

const OPTIONS = [
  { chain_id: 1, asset: "usdc", bonus: 0 },
  { chain_id: 8453, asset: "usdc", bonus: 0 },
  { chain_id: 84532, asset: "pha", bonus: 10 },
];

it("offers each option as a radio and returns the caller's own object", async () => {
  const onChange = vi.fn();
  const { container } = render(
    <PaymentOptionPicker
      options={OPTIONS}
      value={{ chain_id: 8453, asset: "USDC" }}
      onChange={onChange}
      renderDetail={(option) => (option.bonus > 0 ? `+${option.bonus}% bonus` : null)}
    />,
  );
  const group = screen.getByRole("group", { name: "Pay with" });
  expect(Array.from(group.querySelectorAll("input"), (radio) => radio.getAttribute("aria-label"))).toEqual([
    "USDC · Ethereum",
    "USDC · Base",
    "PHA · Base Sepolia",
  ]);
  expect(screen.getByRole("radio", { name: "USDC · Base" })).toHaveProperty("checked", true);
  const testnet = screen.getByRole("radio", { name: "PHA · Base Sepolia" });
  expect(testnet.getAttribute("aria-describedby")?.split(" ").map((id) => document.getElementById(id)?.textContent))
    .toEqual(["Testnet", "+10% bonus"]);

  fireEvent.click(testnet);
  expect(onChange).toHaveBeenCalledWith(OPTIONS[2]);
  expect(onChange.mock.calls[0]?.[0]).toBe(OPTIONS[2]);
  const result = await axe.run(container, { rules: { "color-contrast": { enabled: false } } });
  expect(result.violations).toEqual([]);
});

it("shows a single option, and takes the host's label or disabled state", () => {
  render(
    <>
      <h3 id="pay-label">Token</h3>
      <PaymentOptionPicker options={OPTIONS.slice(0, 1)} value={null} onChange={() => undefined} aria-labelledby="pay-label" disabled />
    </>,
  );
  const radio = screen.getByRole("radio", { name: "USDC · Ethereum" });
  expect(radio).toHaveProperty("checked", false);
  expect(radio.matches(":disabled")).toBe(true);
  expect(screen.getByRole("group", { name: "Token" })).toBeDefined();
  expect(document.querySelector("legend")).toBeNull();
});
