import { expect, test } from "@playwright/test";
import axe from "axe-core";

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
  await page.getByRole("radio", { name: "Base Sepolia" }).check();
  const payments = await page.getByRole("list", { name: "Payments" }).textContent();
  await page.clock.pauseAt(new Date(Date.now() + 1000));
  online = false;
  await page.clock.runFor(180000);
  await expect(page.getByRole("status")).toHaveText("Reconnecting…");
  await expect(page.getByRole("radio", { name: "Base Sepolia" })).toBeChecked();
  await expect(page.getByRole("list", { name: "Payments" })).toHaveText(payments ?? "");
  online = true;
  // Fire the pending poll once, then let its real network response settle without
  // advancing the request timeout while Chromium is still processing the response.
  await page.clock.fastForward(30000);
  await expect(page.getByRole("status")).toHaveText("");
});

test("deposit network and token radio groups work by keyboard and pass axe", async ({ page }) => {
  await page.goto("/?deposit");
  const network = page.getByRole("group", { name: "Network" });
  const token = page.getByRole("group", { name: "Token" });
  await network.getByRole("radio", { name: "Sepolia", exact: true }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(network.getByRole("radio", { name: "Base Sepolia" })).toBeChecked();
  await expect(network.getByRole("radio", { name: "Base Sepolia" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(token.getByRole("radio", { name: "PHA" })).toBeFocused();
  await page.keyboard.press("ArrowRight");
  await expect(token.getByRole("radio", { name: "USDC" })).toBeChecked();
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
