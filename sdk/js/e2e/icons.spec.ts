import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { ADDRESS, API_BASE, CLIENT_SECRET, quote } from "../test/fixtures.js";

for (const width of [390, 1280]) {
  for (const theme of ["light", "dark"]) {
    test(`icons at ${width}px, ${theme}, under the documented CSP`, async ({ page }) => {
      const cspErrors: string[] = [];
      page.on("console", (message) => {
        if (message.type() === "error" && /content security policy/i.test(message.text())) cspErrors.push(message.text());
      });
      await page.setViewportSize({ width, height: 900 });
      await page.route((url) => url.pathname === "/", async (route) => {
        const response = await route.fetch();
        await route.fulfill({ response, headers: { ...response.headers(), "content-security-policy": "style-src 'self'; img-src 'self' data:" } });
      });
      await page.route(`${API_BASE}/**`, async (route) => {
        await route.fulfill({ json: quote({ expires_at: Math.floor(Date.now() / 1000) + 900 }) });
      });
      const params = new URLSearchParams({ theme, client_secret: CLIENT_SECRET, expected_address: ADDRESS, api_base: API_BASE });
      const screenshots = process.env["PHALA_ICON_SCREENSHOTS"];
      if (screenshots !== undefined) mkdirSync(screenshots, { recursive: true });
      for (const surface of ["checkout", "deposit"]) {
        if (surface === "deposit") params.set("deposit", "");
        await page.goto(`/?${params}`);
        if (surface === "checkout") await expect(page.getByRole("status")).toContainText("Waiting for your payment");
        await expect(page.locator(".pp-icon").first()).toBeVisible();
        await expect(page.locator(".pp-root style, .pp-root [style], .pp-icon use")).toHaveCount(0);
        await expect(page.locator(".pp-icon image").first()).toHaveAttribute("href", /^data:image\/png;base64,/);
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
        const icon = page.locator(".pp-icon").first();
        const bounds = await icon.boundingBox();
        expect(bounds?.width).toBe(20);
        expect(bounds?.height).toBe(20);
        // Every logo has a faint ring inside its edge in the surrounding text color, so that a disc
        // the color of the page (PHA's black on dark, Ethereum's white on light) keeps its edge.
        const rings = await page.locator(".pp-icon:has(image)").evaluateAll((icons) => icons.map((icon) => {
          const style = getComputedStyle(icon);
          return { style: style.outlineStyle, width: style.outlineWidth, offset: style.outlineOffset,
            color: style.outlineColor, text: style.color };
        }));
        expect(rings.length).toBeGreaterThan(0);
        for (const ring of rings) {
          expect(ring).toMatchObject({ style: "solid", width: "1px", offset: "-1px" });
          const [color, text] = [ring.color, ring.text].map(channels);
          for (const [index, value] of (text ?? []).slice(0, 3).entries()) expect(color?.[index]).toBeCloseTo(value, 2);
          expect(color?.[3]).toBeCloseTo(0.12, 2);
        }
        expect(cspErrors).toEqual([]);
        if (screenshots !== undefined) await page.screenshot({ path: join(screenshots, `${surface}-${width}-${theme}.png`), fullPage: true });
        if (surface === "deposit") {
          // The open listbox is placed through the CSSOM, which the policy allows: it opens under its field.
          const field = page.getByRole("combobox", { name: "Token" });
          await field.click();
          const listbox = page.getByRole("listbox", { name: "Token" });
          await expect(listbox).toBeVisible();
          const [fieldBox, listBox] = await Promise.all([field.boundingBox(), listbox.boundingBox()]);
          expect(listBox?.y ?? 0).toBeGreaterThanOrEqual((fieldBox?.y ?? 0) + (fieldBox?.height ?? 0));
          expect(Math.round(listBox?.width ?? 0)).toBe(Math.round(fieldBox?.width ?? -1));
          expect(cspErrors).toEqual([]);
        }
      }
    });
  }
}

/** A computed `rgb()`, `rgba()`, or `color(srgb …)` (which `color-mix()` computes to) as 0–1 channels and alpha. */
function channels(color: string): number[] {
  const [r = 0, g = 0, b = 0, alpha = 1] = color.match(/[\d.]+/g)?.map(Number) ?? [];
  const scale = color.startsWith("color(") ? 1 : 255;
  return [r / scale, g / scale, b / scale, alpha];
}
