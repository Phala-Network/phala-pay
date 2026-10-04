import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { ADDRESS, API_BASE, CLIENT_SECRET, quote } from "../test/fixtures.js";

for (const width of [390, 1280]) {
  for (const theme of ["light", "dark"]) {
    for (const frameless of [false, true]) {
      test(`icons at ${width}px, ${theme}, ${frameless ? "frameless" : "framed"}, under strict style CSP`, async ({ page }) => {
        const cspErrors: string[] = [];
        page.on("console", (message) => {
          if (message.type() === "error" && /content security policy/i.test(message.text())) cspErrors.push(message.text());
        });
        await page.setViewportSize({ width, height: 900 });
        await page.route((url) => url.pathname === "/", async (route) => {
          const response = await route.fetch();
          await route.fulfill({ response, headers: { ...response.headers(), "content-security-policy": "style-src 'self'" } });
        });
        await page.route(`${API_BASE}/**`, async (route) => {
          await route.fulfill({ json: quote({ expires_at: Math.floor(Date.now() / 1000) + 900 }) });
        });
        const params = new URLSearchParams({ theme, client_secret: CLIENT_SECRET, expected_address: ADDRESS, api_base: API_BASE });
        if (frameless) params.set("frameless", "");
        const screenshots = process.env["PHALA_ICON_SCREENSHOTS"];
        if (screenshots !== undefined) mkdirSync(screenshots, { recursive: true });
        for (const surface of ["checkout", "deposit"]) {
          if (surface === "deposit") params.set("deposit", "");
          await page.goto(`/?${params}`);
          if (surface === "checkout") await expect(page.getByRole("status")).toContainText("Waiting for your payment");
          await expect(page.locator(".pp-icon").first()).toBeVisible();
          await expect(page.locator(".pp-root style, .pp-root [style], .pp-icon image, .pp-icon use")).toHaveCount(0);
          expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
          const icon = page.locator(".pp-icon").first();
          const bounds = await icon.boundingBox();
          expect(bounds?.width).toBe(surface === "checkout" ? 20 : 16);
          expect(bounds?.height).toBe(surface === "checkout" ? 20 : 16);
          expect(cspErrors).toEqual([]);
          if (screenshots !== undefined) await page.screenshot({ path: join(screenshots, `${surface}-${width}-${theme}-${frameless ? "frameless" : "framed"}.png`), fullPage: true });
        }
      });
    }
  }
}
