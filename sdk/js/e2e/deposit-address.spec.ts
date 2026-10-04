import { expect, test } from "@playwright/test";
import axe from "axe-core";

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
