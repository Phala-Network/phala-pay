import { expect, test, type Page } from "@playwright/test";
import axe from "axe-core";

/** Chooses `name` in the payment options' `field` ("Token" or "Network"), as a pointer does. */
async function choose(page: Page, field: string, name: string) {
  await page.getByRole("combobox", { name: field }).click();
  await page.getByRole("listbox", { name: field }).getByRole("option", { name, exact: true }).click();
  await expect(page.getByRole("combobox", { name: field })).toContainText(name);
}

for (const width of [320, 1280]) {
  for (const theme of ["light", "dark"]) {
    test(`deposit limits are readable at ${width}px in ${theme} theme`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(`/?deposit&theme=${theme}`);
      const message = page.locator(".pp-message").last();
      await expect(message).toHaveText(
        "Send at least $5.00 and 0.1 PHA, up to 10,100 PHA per transfer on Sepolia. Unsupported tokens or networks are not credited. Eligible transfers are credited at the market rate after confirmation.",
      );
      await choose(page, "Token", "USDC");
      await choose(page, "Network", "Base Sepolia");
      await expect(message).toContainText("Send at least $5.00 and up to 10,100 USDC per transfer on Base Sepolia.");
      expect(await message.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
      await page.addScriptTag({ content: axe.source });
      const result = await page.evaluate(() => axe.run(document.querySelector(".pp-root") ?? document));
      expect(result.violations).toEqual([]);
    });
  }
}

test("retains deposit selections and payments through a three-minute network outage", async ({ page }) => {
  const secret = `da_${"0d".repeat(16)}_secret_${"ab".repeat(24)}`;
  let online = true;
  await page.route("http://topup.test/v1/deposit_addresses/**", async (route) => {
    if (!online) return route.abort("connectionfailed");
    return route.fulfill({
      headers: { "access-control-allow-origin": "*" },
      json: {
        id: `da_${"0d".repeat(16)}`, object: "deposit_address", livemode: false, status: "active",
        networks: [{ chain_id: 11155111, address: "0x1111111111111111111111111111111111111111", typical_credit_seconds: 30 }],
        payments: [{ status: "seen", chain_id: 11155111, asset: "pha", decimals: 18,
          amount_atomic: "1000000000000000000", tx_hash: `0x${"ab".repeat(32)}`, confirmations: 1, created: 1790000000 }],
      },
    });
  });
  await page.clock.install();
  await page.goto(`/?deposit&client_secret=${secret}&api_base=http://topup.test`);
  await expect(page.getByRole("list", { name: "Payments" })).toBeVisible();
  // A payer sending to the address gives no hint: the discovery scan, then Sepolia's 30 s.
  await expect(page.getByText(/credited at the market rate, usually within about 6 minutes\./)).toBeVisible();
  await choose(page, "Token", "USDC");
  await choose(page, "Network", "Base Sepolia");
  const payments = await page.getByRole("list", { name: "Payments" }).textContent();
  await page.clock.pauseAt(new Date(Date.now() + 1000));
  online = false;
  await page.clock.runFor(180000);
  await expect(page.getByRole("status")).toHaveText("Reconnecting…");
  await expect(page.getByRole("combobox", { name: "Token" })).toHaveText("USDC");
  await expect(page.getByRole("combobox", { name: "Network" })).toHaveText("Base Sepolia Testnet");
  await expect(page.getByRole("list", { name: "Payments" })).toHaveText(payments ?? "");
  online = true;
  // Fire the pending poll once, then let its real network response settle without
  // advancing the request timeout while Chromium is still processing the response.
  await page.clock.fastForward(30000);
  await expect(page.getByRole("status")).toHaveText("");
});

test("deposit token and network fields work by keyboard and pass axe", async ({ page }) => {
  await page.goto("/?deposit");
  const token = page.getByRole("combobox", { name: "Token" });
  const network = page.getByRole("combobox", { name: "Network" });
  await token.focus();
  await page.keyboard.press("ArrowDown");
  await expect(token).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await expect(token).toHaveText("USDC");
  await expect(network).toHaveText("Sepolia Testnet");
  await page.keyboard.press("Tab");
  await expect(network).toBeFocused();
  await page.keyboard.press("End");
  await page.keyboard.press("Enter");
  await expect(network).toHaveText("Base Sepolia Testnet");
  await expect(network).toBeFocused();
  await expect(page.getByRole("img")).toHaveAttribute(
    "aria-label",
    "Deposit address for USDC on Base Sepolia",
  );
  await page.addScriptTag({ content: axe.source });
  const results = await page.evaluate(() =>
    axe.run(document.querySelector(".pp-root") ?? document),
  );
  expect(results.violations).toEqual([]);
  await expect(page.locator(".pp-root style, .pp-root [style]")).toHaveCount(0);
});
