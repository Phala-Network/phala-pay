import axe from "axe-core";
import { useState } from "react";
import { userEvent } from "@testing-library/user-event";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { PaymentOptionPicker } from "../src/index.js";

afterEach(cleanup);

const OPTIONS = [
  { chain_id: 1, asset: "usdc", bonus: 0 },
  { chain_id: 8453, asset: "usdc", bonus: 0 },
  { chain_id: 1, asset: "usdt", bonus: 0 },
  { chain_id: 84532, asset: "pha", bonus: 10 },
];

/** The picker, controlled as a host would, reporting each choice. */
function Picker({ onChange }: { onChange: (option: (typeof OPTIONS)[number]) => void }) {
  const [value, setValue] = useState(OPTIONS[1]);
  return (
    <PaymentOptionPicker
      options={OPTIONS}
      value={value}
      onChange={(option) => {
        setValue(option);
        onChange(option);
      }}
      renderDetail={(option) => (option.bonus > 0 ? `+${option.bonus}% bonus` : "$1.00")}
    />
  );
}

const token = () => screen.getByRole("combobox", { name: "Token" });
const network = () => screen.getByRole("combobox", { name: "Network" });
const listbox = (field: HTMLElement) => document.getElementById(field.getAttribute("aria-controls") ?? "") ?? field;

it("offers the token, then the networks that take it, and returns the caller's own object", async () => {
  const onChange = vi.fn();
  const { container } = render(<Picker onChange={onChange} />);
  expect(screen.getByRole("group", { name: "Pay with" })).toBeDefined();
  expect(token().textContent).toBe("USDC");
  expect(network().textContent).toBe("Base");
  expect(screen.getByText("$1.00").id).toBe(token().getAttribute("aria-describedby"));

  fireEvent.click(network());
  expect(within(listbox(network())).getAllByRole("option").map((option) => option.textContent)).toEqual(["Ethereum", "Base"]);
  fireEvent.click(network());
  fireEvent.click(token());
  expect(within(listbox(token())).getAllByRole("option").map((option) => option.textContent)).toEqual(["USDC", "USDT", "PHA"]);

  // Another token keeps the network where it takes the token, else takes its first network.
  fireEvent.click(within(listbox(token())).getByRole("option", { name: "PHA" }));
  expect(onChange).toHaveBeenLastCalledWith(OPTIONS[3]);
  expect(onChange.mock.lastCall?.[0]).toBe(OPTIONS[3]);
  expect(network().textContent).toBe("Base Sepolia Testnet");
  expect(screen.getByText("+10% bonus")).toBeDefined();
  fireEvent.click(token());
  fireEvent.click(within(listbox(token())).getByRole("option", { name: "USDC" }));
  expect(onChange).toHaveBeenLastCalledWith(OPTIONS[0]);
  fireEvent.click(network());
  fireEvent.click(within(listbox(network())).getByRole("option", { name: "Ethereum" }));
  fireEvent.click(token());
  fireEvent.click(within(listbox(token())).getByRole("option", { name: "USDT" }));
  expect(onChange).toHaveBeenLastCalledWith(OPTIONS[2]);
  expect(network().textContent).toBe("Ethereum");

  const result = await axe.run(container, { rules: { "color-contrast": { enabled: false } } });
  expect(result.violations).toEqual([]);
});

it("works by keyboard as a select-only combobox", async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<Picker onChange={onChange} />);
  await user.tab();
  expect(token()).toBe(document.activeElement);
  expect(token().getAttribute("aria-expanded")).toBe("false");

  // Down opens on the chosen token; Down moves; Enter chooses and closes.
  await user.keyboard("{ArrowDown}");
  expect(token().getAttribute("aria-expanded")).toBe("true");
  const active = () => document.getElementById(token().getAttribute("aria-activedescendant") ?? "")?.textContent;
  expect(active()).toBe("USDC");
  await user.keyboard("{ArrowDown}");
  expect(active()).toBe("USDT");
  await user.keyboard("{Enter}");
  expect(token().getAttribute("aria-expanded")).toBe("false");
  expect(onChange).toHaveBeenLastCalledWith(OPTIONS[2]);

  // Typing finds a token; End and Home move to the ends; Escape closes without choosing.
  await user.keyboard("p");
  expect(active()).toBe("PHA");
  await user.keyboard("{Home}");
  expect(active()).toBe("USDC");
  await user.keyboard("{End}");
  expect(active()).toBe("PHA");
  await user.keyboard("{Escape}");
  expect(token().getAttribute("aria-expanded")).toBe("false");
  expect(onChange).toHaveBeenCalledTimes(1);

  // Tab moves on to the network.
  await user.tab();
  expect(network()).toBe(document.activeElement);
});

it("shows a single option, and takes the host's label or disabled state", () => {
  render(
    <>
      <h3 id="pay-label">Top up with</h3>
      <PaymentOptionPicker options={OPTIONS.slice(0, 1)} value={OPTIONS[0]} onChange={() => undefined} aria-labelledby="pay-label" disabled />
    </>,
  );
  expect(screen.getByRole("group", { name: "Top up with" })).toBeDefined();
  expect(token().textContent).toBe("USDC");
  expect(network().textContent).toBe("Ethereum");
  expect(within(listbox(network())).getAllByRole("option", { hidden: true })).toHaveLength(1);
  expect(token().getAttribute("aria-disabled")).toBe("true");
  fireEvent.click(token());
  expect(token().getAttribute("aria-expanded")).toBe("false");
});
