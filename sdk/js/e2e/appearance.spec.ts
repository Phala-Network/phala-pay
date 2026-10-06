import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { expect, test, type Locator, type Page } from "@playwright/test";
import axe from "axe-core";
import jsqr from "jsqr";
import type { ClientQuote } from "../src/index.js";
import { API_BASE, CLIENT_SECRET, quote } from "../test/fixtures.js";

// jsqr is CommonJS; its function is `module.exports.default`.
const jsQR = jsqr.default;
const ADDRESS = "0x52908400098527886E0F7030069857D2E4169EE7";
const HASH = `0x${"3f9a".repeat(16)}`;
const DEPOSIT_SECRET = `da_${"0d".repeat(16)}_secret_${"ab".repeat(24)}`;
// Set to a directory to keep a screenshot of every state.
const screenshots = process.env["PHALA_APPEARANCE_SCREENSHOTS"];

/** A waiting quote that expires in 15 minutes, with `overrides`. */
function waiting(overrides: Partial<ClientQuote> = {}): ClientQuote {
  return quote({ address: ADDRESS, expires_at: Math.floor(Date.now() / 1000) + 900, ...overrides });
}

/** Serves `current()` as the quote's public view on every read. */
async function serve(page: Page, current: () => ClientQuote | number) {
  await page.route(`${API_BASE}/v1/quotes/**`, async (route) => {
    const body = current();
    const headers = { "access-control-allow-origin": "*" };
    await route.fulfill(typeof body === "number" ? { status: body, headers, json: {} } : { headers, json: body });
  });
}

/** Serves the deposit address's public view, or fails the read while `online()` is false. */
async function serveDeposit(page: Page, online: () => boolean = () => true) {
  await page.route(`${API_BASE}/v1/deposit_addresses/**`, async (route) => {
    if (!online()) return route.abort("connectionfailed");
    const payment = { chain_id: 11155111, asset: "pha", decimals: 18, created: 1790000000 };
    return route.fulfill({
      headers: { "access-control-allow-origin": "*" },
      json: {
        id: `da_${"0d".repeat(16)}`, object: "deposit_address", livemode: false, status: "active",
        networks: [11155111, 84532].map((chain_id) => ({ chain_id, address: ADDRESS, typical_credit_seconds: 30 })),
        payments: [
          { ...payment, status: "seen", amount_atomic: "1500000000000000000", tx_hash: HASH, confirmations: 1 },
          { ...payment, status: "credited", amount_atomic: "20000000000000000000", tx_hash: `0x${"ab".repeat(32)}`, confirmations: 3 },
        ],
      },
    });
  });
}

/** An EIP-6963 wallet on the quote's chain that holds enough and sends at once. */
async function installWallet(page: Page) {
  await page.addInitScript((hash) => {
    const provider = {
      request({ method }: { method: string }): Promise<unknown> {
        switch (method) {
          case "eth_requestAccounts":
          case "eth_accounts":
            return Promise.resolve(["0x52908400098527886E0F7030069857D2E4169EE7"]);
          case "eth_chainId":
            return Promise.resolve("0xaa36a7");
          case "eth_call":
            return Promise.resolve(`0x${"f".repeat(64)}`);
          case "eth_sendTransaction":
            return Promise.resolve(hash);
          default:
            return Promise.reject(new Error(`unexpected ${method}`));
        }
      },
      on: () => undefined,
      removeListener: () => undefined,
    };
    const info = {
      uuid: "7f2b7a2c-2f5a-4d7e-9a0e-5b5c1a7d3e10",
      name: "Test Wallet",
      icon: "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 20 20'%3E%3Crect width='20' height='20' rx='5' fill='%236366f1'/%3E%3C/svg%3E",
      rdns: "test.wallet",
    };
    const announce = () =>
      window.dispatchEvent(new CustomEvent("eip6963:announceProvider", { detail: Object.freeze({ info, provider }) }));
    window.addEventListener("eip6963:requestProvider", announce);
    announce();
  }, HASH);
}

/**
 * Checks the layout every state shares: nothing overflows the page or the component, no inline
 * style, and every control's target is at least 44 × 44 px. Then keeps a screenshot when asked.
 */
async function check(page: Page, name: string) {
  await expect(page.locator(".pp-root")).toBeVisible();
  await expect.soft(page.locator(".pp-root style, .pp-root [style]")).toHaveCount(0);
  expect.soft(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${name}: page overflow`).toBe(true);
  expect.soft(await page.evaluate(() => {
    const root = document.querySelector(".pp-root");
    return root !== null && root.scrollWidth <= root.clientWidth;
  }), `${name}: component overflow`).toBe(true);
  expect.soft(await smallTargets(page), `${name}: targets under 44px`).toEqual([]);
  if (screenshots !== undefined) {
    mkdirSync(screenshots, { recursive: true });
    const box = await page.locator(".pp-root").boundingBox();
    const viewport = page.viewportSize();
    if (box !== null && viewport !== null) {
      await page.screenshot({
        path: join(screenshots, `${name}.png`),
        fullPage: true,
        // Playwright hides the caret through an inline style, which the check above would see.
        caret: "initial",
        clip: { x: 0, y: 0, width: Math.min(viewport.width, box.x + box.width + 16), height: box.y + box.height + 16 },
      });
    }
  }
}

/** The controls whose target, around their center, is under 44 × 44 px. */
function smallTargets(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const small: string[] = [];
    for (const control of document.querySelectorAll<HTMLElement>(".pp-root button, .pp-root input")) {
      control.scrollIntoView({ block: "center", inline: "center" });
      const box = control.getBoundingClientRect();
      const [x, y, reach] = [box.left + box.width / 2, box.top + box.height / 2, 21.5];
      const hits = [[x - reach, y], [x + reach, y], [x, y - reach], [x, y + reach]].map(([px = 0, py = 0]) => document.elementFromPoint(px, py));
      if (!hits.every((hit) => hit !== null && (control.contains(hit) || (control.closest("label")?.contains(hit) ?? false)))) {
        small.push(control.getAttribute("aria-label") ?? control.textContent);
      }
    }
    window.scrollTo(0, 0);
    return small;
  });
}

/** Decodes the QR code from a screenshot of it and `margin` px of the page around it. */
async function decodeOnPage(page: Page, name: string, margin = 24): Promise<string | undefined> {
  const qr = page.locator(".pp-qr__code");
  await qr.scrollIntoViewIfNeeded();
  const box = await qr.boundingBox();
  if (box === null) return undefined;
  const png = await page.screenshot({
    caret: "initial",
    clip: { x: box.x - margin, y: box.y - margin, width: box.width + 2 * margin, height: box.height + 2 * margin },
    ...(screenshots === undefined ? {} : { path: join(screenshots, `${name}.png`) }),
  });
  const image = await page.evaluate(async (base64) => {
    const bitmap = await createImageBitmap(await (await fetch(`data:image/png;base64,${base64}`)).blob());
    const context = new OffscreenCanvas(bitmap.width, bitmap.height).getContext("2d");
    if (context === null) return null;
    context.drawImage(bitmap, 0, 0);
    const { width, height, data } = context.getImageData(0, 0, bitmap.width, bitmap.height);
    return { width, height, data: Array.from(data) };
  }, png.toString("base64"));
  // Only the code as drawn: an inverted reading would hide a dark quiet zone.
  return image === null
    ? undefined
    : jsQR(Uint8ClampedArray.from(image.data), image.width, image.height, { inversionAttempts: "dontInvert" })?.data;
}

const checkoutUrl = (theme: string) =>
  `/?${new URLSearchParams({ theme, client_secret: CLIENT_SECRET, expected_address: ADDRESS, api_base: API_BASE })}`;

for (const width of [390, 1280]) {
  for (const theme of ["light", "dark"]) {
    const suffix = `${width}-${theme}`;

    test(`checkout states at ${suffix}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.route(`${API_BASE}/**`, () => new Promise(() => undefined));
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText("Loading payment details…");
      await check(page, `loading-${suffix}`);

      await page.unrouteAll({ behavior: "ignoreErrors" });
      let served: ClientQuote | number = waiting();
      await serve(page, () => served);
      await installWallet(page);
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
      await expect(page.getByRole("tab", { name: "Browser wallet" })).toHaveAttribute("aria-selected", "true");
      await check(page, `wallet-${suffix}`);
      await page.getByRole("tab", { name: "QR code" }).click();
      await check(page, `qr-${suffix}`);
      await page.getByRole("tab", { name: "Manual transfer" }).click();
      await check(page, `manual-${suffix}`);

      await page.getByRole("tab", { name: "Browser wallet" }).click();
      await page.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
      await expect(page.getByText(/^Transaction sent:/)).toBeVisible();
      served = waiting({ payment_status: "seen", confirmations: 1 });
      await expect(page.getByRole("status")).toHaveText(/^Received, 1 confirmation/);
      await check(page, `seen-${suffix}`);
      served = waiting({ status: "complete", payment_status: "credited", amount_credited: 2500 });
      await expect(page.getByRole("status")).toHaveText("Payment credited: $25.00");
      await check(page, `credited-${suffix}`);

      served = waiting({ expires_at: Math.floor(Date.now() / 1000) - 1 });
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText(/expired. Do not send funds/);
      await check(page, `expired-${suffix}`);

      served = 404;
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText("This payment link is not valid. Start a new top-up.");
      await check(page, `error-${suffix}`);

      served = waiting();
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
      served = 502;
      await expect(page.getByRole("status")).toContainText("reconnecting…", { timeout: 10_000 });
      await check(page, `reconnecting-${suffix}`);
    });

    test(`deposit address states at ${suffix}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(`/?deposit&networks=1&theme=${theme}`);
      await expect(page.getByRole("group", { name: "Network" })).toHaveCount(0);
      await check(page, `deposit-single-${suffix}`);

      let online = true;
      await serveDeposit(page, () => online);
      await page.goto(`/?deposit&theme=${theme}&client_secret=${DEPOSIT_SECRET}&api_base=${API_BASE}`);
      await expect(page.getByRole("list", { name: "Payments" })).toBeVisible();
      await check(page, `deposit-multi-${suffix}`);
      online = false;
      await expect(page.getByRole("status")).toHaveText("Reconnecting…", { timeout: 15_000 });
      await check(page, `deposit-reconnecting-${suffix}`);
    });

    test(`QR codes decode from the screen at ${suffix}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      const served = waiting();
      await serve(page, () => served);
      await page.goto(checkoutUrl(theme));
      await page.getByRole("tab", { name: "QR code" }).click();
      expect(await decodeOnPage(page, `qr-decode-checkout-${suffix}`)).toBe(served.payment_uri);

      await page.goto(`/?deposit&theme=${theme}`);
      const uri = "ethereum:0x2222222222222222222222222222222222222222@11155111/transfer?address=0x1111111111111111111111111111111111111111";
      expect(await decodeOnPage(page, `qr-decode-deposit-${suffix}`)).toBe(uri);
    });
  }
}

for (const theme of ["light", "dark"]) {
  test(`checkout passes axe in each payment method and once credited, ${theme}`, async ({ page }) => {
    let served = waiting();
    await serve(page, () => served);
    await installWallet(page);
    await page.goto(checkoutUrl(theme));
    await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
    await page.addScriptTag({ content: axe.source });
    const violations = async () =>
      (await page.evaluate(() => axe.run(document.querySelector(".pp-root") ?? document))).violations;
    for (const name of ["Browser wallet", "QR code", "Manual transfer"]) {
      await page.getByRole("tab", { name }).click();
      expect(await violations(), name).toEqual([]);
    }
    served = waiting({ status: "complete", payment_status: "credited", amount_credited: 2500 });
    await expect(page.getByRole("status")).toHaveText("Payment credited: $25.00");
    expect(await violations(), "credited").toEqual([]);
  });
}

test("keyboard focus is visible and differs from the selected state", async ({ page }) => {
  await page.goto("/?deposit");
  await expect(page.getByRole("group", { name: "Network" })).toBeVisible();
  const outline = (locator: Locator) =>
    locator.evaluate((element) => {
      const style = getComputedStyle(element);
      return { style: style.outlineStyle, width: style.outlineWidth, color: style.outlineColor, border: style.borderColor };
    });

  await page.keyboard.press("Tab");
  const radio = page.getByRole("radio", { name: "Sepolia", exact: true });
  await expect(radio).toBeFocused();
  await expect(radio).toBeChecked();
  const focused = await outline(radio);
  expect(focused.style).toBe("solid");
  expect(focused.width).toBe("2px");
  expect(focused.color).not.toBe(focused.border);
  if (screenshots !== undefined) await page.locator(".pp-root").screenshot({ caret: "initial", path: join(screenshots, "focus-radio.png") });

  // Selected without focus: the selection alone, without the focus ring.
  await page.keyboard.press("Tab");
  await expect(radio).not.toBeFocused();
  const selected = await outline(radio);
  expect(selected.style).toBe("none");

  // In forced colors, the selection keeps a system color of its own.
  await page.emulateMedia({ forcedColors: "active", reducedMotion: "reduce" });
  await expect.poll(() => page.evaluate(() => matchMedia("(forced-colors: active)").matches)).toBe(true);
  const borders = await page.getByRole("group", { name: "Network" }).getByRole("radio").evaluateAll((radios) =>
    radios.map((radio) => getComputedStyle(radio).borderTopColor));
  expect(new Set(borders).size).toBe(2);
  await page.emulateMedia({ forcedColors: "none", reducedMotion: "no-preference" });

  const tabs = new URLSearchParams({ client_secret: CLIENT_SECRET, expected_address: ADDRESS, api_base: API_BASE });
  const served = waiting();
  await serve(page, () => served);
  await page.goto(`/?${tabs}`);
  await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
  await page.keyboard.press("Tab");
  await expect(page.getByRole("tab", { name: "QR code" })).toBeFocused();
  const tab = await outline(page.getByRole("tab", { name: "QR code" }));
  expect(tab.style).toBe("solid");
  expect(tab.width).toBe("2px");
  if (screenshots !== undefined) await page.locator(".pp-root").screenshot({ caret: "initial", path: join(screenshots, "focus-tab.png") });

  // In forced colors, the selected tab keeps a system color of its own. Measured on the selection
  // alone (Chromium paints a focused element apart), without transitions (none under reduced
  // motion), once the emulation applies, which forces the notice's muted text color.
  const noticeColor = () => page.locator(".pp-notice").evaluate((notice) => getComputedStyle(notice).color);
  await page.getByRole("tab", { name: "QR code" }).blur();
  const unforced = await noticeColor();
  await page.emulateMedia({ forcedColors: "active", reducedMotion: "reduce" });
  await expect.poll(noticeColor).not.toBe(unforced);
  const underlines = await page.getByRole("tab").evaluateAll((tabs) =>
    tabs.map((tab) => [tab.getAttribute("aria-selected"), getComputedStyle(tab).borderBottomColor] as const));
  const selectedUnderline = underlines.find(([selected]) => selected === "true")?.[1];
  expect(underlines.filter(([selected]) => selected !== "true").map(([, color]) => color))
    .not.toContain(selectedUnderline);
  await page.emulateMedia({ forcedColors: "none", reducedMotion: "no-preference" });

  // The copy icon stays 16px whatever the text size.
  await page.getByRole("tab", { name: "Manual transfer" }).click();
  await page.addStyleTag({ content: ".pp-root[data-theme] { --pp-font-size: 22px; }" });
  expect(await page.locator(".pp-copy svg").first().boundingBox()).toMatchObject({ width: 16, height: 16 });
});
