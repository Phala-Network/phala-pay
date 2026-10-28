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
  // An open listbox's place is the one style set, through the CSSOM.
  await expect.soft(page.locator(".pp-root style, .pp-root [style]:not(.pp-select__listbox:popover-open)")).toHaveCount(0);
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

/** The component's accessibility violations. */
async function axeViolations(page: Page) {
  await page.addScriptTag({ content: axe.source });
  return (await page.evaluate(() => axe.run(document.querySelector(".pp-root") ?? document))).violations;
}

/** The controls whose target, around their center, is under 44 × 44 px. */
function smallTargets(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const small: string[] = [];
    for (const control of document.querySelectorAll<HTMLElement>(".pp-root button, .pp-root input")) {
      // A control inside a collapsed (`hidden`) region is no target.
      if (control.getClientRects().length === 0) continue;
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
      await page.getByRole("button", { name: "Pay with Test Wallet" }).click();
      await expect(page.getByText(/^Transaction sent:/)).toBeVisible();
      // Sent but not yet seen: nothing offers to pay again.
      await expect(page.getByRole("status")).toHaveText("Sent, waiting for confirmation");
      await expect(page.getByRole("tablist")).toHaveCount(0);
      await check(page, `sent-${suffix}`);
      served = waiting({ payment_status: "seen", confirmations: 1 });
      await expect(page.getByRole("status")).toHaveText(/^Received, 1 confirmation/);
      await check(page, `seen-${suffix}`);
      served = waiting({ status: "complete", payment_status: "credited", amount_credited: 2500 });
      await expect(page.getByRole("status")).toHaveText("Payment credited");
      await check(page, `credited-${suffix}`);

      served = waiting({ expires_at: Math.floor(Date.now() / 1000) - 1 });
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText(/expired. Do not send funds/);
      await check(page, `expired-${suffix}`);

      served = 404;
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText("This payment link is not valid.");
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
      await expect(page.getByRole("combobox", { name: "Network" })).toHaveText("Sepolia Testnet");
      await check(page, `deposit-single-${suffix}`);

      await page.goto(`/?picker&theme=${theme}`);
      await expect(page.getByRole("combobox", { name: "Network" })).toHaveText("Base");
      await check(page, `picker-${suffix}`);
      await page.getByRole("combobox", { name: "Network" }).click();
      await check(page, `picker-open-${suffix}`);
      await page.keyboard.press("Escape");
      await page.getByRole("combobox", { name: "Token" }).click();
      await expect(page.getByRole("listbox", { name: "Token" }).getByRole("option")).toHaveText(["USDC", "USDT", "PHA", "ETH"]);
      await check(page, `picker-token-open-${suffix}`);
    });

    test(`payment element has no way back from a sent payment until it is final at ${suffix}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      let served = waiting();
      await serve(page, () => served);
      await installWallet(page);
      await page.goto(checkoutUrl(theme).replace("/?", "/?element&"));
      await page.getByRole("button", { name: "Pay $25.00 in USDC" }).click();
      const pay = page.getByRole("button", { name: "Pay with Test Wallet" });
      await expect(pay).toBeVisible();
      // Flags any DOM state, even one render's, in which "Back" or a wallet button shows beside "Sent".
      await page.evaluate(() => {
        new MutationObserver(() => {
          const buttons = [...document.querySelectorAll("button")].map((b) => b.textContent);
          if (document.body.textContent.includes("Sent, waiting") && buttons.some((t) => /^(Back|Pay with)/.test(t))) {
            document.documentElement.dataset["besideSent"] = "true";
          }
        }).observe(document.body, { childList: true, subtree: true, characterData: true });
      });
      await pay.click();
      await expect(page.getByText(/^Transaction sent:/)).toBeVisible();
      await expect(page.getByRole("status")).toHaveText("Sent, waiting for confirmation");
      await expect(page.getByRole("button", { name: /^(Back|Pay with .*)$/ })).toHaveCount(0);
      await expect(page.locator("html")).not.toHaveAttribute("data-beside-sent");
      expect(await axeViolations(page), "sent").toEqual([]);
      await check(page, `element-sent-${suffix}`);
      served = waiting({ status: "complete", payment_status: "credited", amount_credited: 2500 });
      await expect(page.getByRole("button", { name: "Start a new payment" })).toBeVisible({ timeout: 10_000 });
    });

    test(`payment element steps at ${suffix}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await serve(page, () => waiting({ asset: "usdc", chain_id: 84532, decimals: 6, amount_atomic: "25000000" }));
      await page.goto(checkoutUrl(theme).replace("/?", "/?element&"));
      await expect(page.getByRole("combobox", { name: "Network" })).toHaveText("Base");
      expect(await axeViolations(page), "options").toEqual([]);
      await check(page, `element-options-${suffix}`);
      await page.getByRole("button", { name: "Pay $25.00 in USDC" }).click();
      await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
      expect(await axeViolations(page), "checkout").toEqual([]);
      await check(page, `element-checkout-${suffix}`);
      await page.getByRole("button", { name: "Back", exact: true }).click();
      // The host's summary and submit remain in the same form.
      await page.goto(checkoutUrl(theme).replace("/?", "/?element&host&"));
      await expect(page.getByRole("combobox", { name: "Network" })).toHaveText("Base");
      const buttons = page.locator(".pp-element button");
      await expect(buttons).toHaveText(["Continue to pay $25.00"]);
      expect(await axeViolations(page), "host").toEqual([]);
      await check(page, `element-host-${suffix}`);
      await page.getByRole("button", { name: "Continue to pay $25.00" }).click();
      await expect(page.getByRole("status")).toHaveText("Waiting for your payment");

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
    await expect(page.getByRole("status")).toHaveText("Payment credited");
    expect(await violations(), "credited").toEqual([]);
  });
}

/** The contrast of each element's `side` border against the page behind it, as WCAG 1.4.11 measures a control's edge. */
function edgeContrast(locator: Locator, side: "Top" | "Bottom" = "Top"): Promise<number[]> {
  return locator.evaluateAll((elements, side) => {
    const rgba = (color: string) => {
      const [r = 0, g = 0, b = 0, a = 1] = (color.match(/[\d.]+/g) ?? []).map(Number);
      return { r, g, b, a };
    };
    const luminance = ({ r, g, b }: { r: number; g: number; b: number }) => {
      const [lr = 0, lg = 0, lb = 0] = [r, g, b].map((value) => {
        const c = value / 255;
        return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
      });
      return 0.2126 * lr + 0.7152 * lg + 0.0722 * lb;
    };
    return elements.map((element) => {
      let behind = { r: 255, g: 255, b: 255, a: 1 };
      for (let node = element.parentElement; node !== null; node = node.parentElement) {
        const background = rgba(getComputedStyle(node).backgroundColor);
        if (background.a > 0) {
          behind = background;
          break;
        }
      }
      const edge = rgba(getComputedStyle(element)[`border${side}Color`]);
      const over = (front: number, back: number) => front * edge.a + back * (1 - edge.a);
      const drawn = { r: over(edge.r, behind.r), g: over(edge.g, behind.g), b: over(edge.b, behind.b) };
      const [light, dark] = [luminance(drawn), luminance(behind)].sort((x, y) => y - x);
      return ((light ?? 0) + 0.05) / ((dark ?? 0) + 0.05);
    });
  }, side);
}

for (const theme of ["light", "dark"]) {
  test(`fields' edges keep 3:1 against the page, ${theme}`, async ({ page }) => {
    await page.goto(`/?deposit&theme=${theme}`);
    const fields = page.getByRole("combobox");
    await expect(fields).toHaveCount(2);
    for (const ratio of await edgeContrast(fields)) expect(ratio).toBeGreaterThanOrEqual(3);
  });
}

// A host whose primary color is a light fill (lime, 1.31:1 on white) keeps it for the pay button
// and gives the selection a color of its own with 3:1 or more against its page.
const LIME_HOST = `.pp-root[data-theme] { --pp-color-primary: #cdfa50; --pp-color-primary-foreground: #161616;
  --pp-color-selected: #4d7c0f; }
.pp-root[data-theme="dark"] { --pp-color-selected: #cdfa50; }`;
const LIME = "rgb(205, 250, 80)";

for (const width of [390, 1280]) {
  for (const theme of ["light", "dark"]) {
    const suffix = `${width}-${theme}`;

    test(`the selected color follows the primary color unless a host sets it at ${suffix}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      const served = waiting();
      await serve(page, () => served);
      await installWallet(page);
      await page.goto(checkoutUrl(theme));
      await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
      const button = page.getByRole("button", { name: "Pay with Test Wallet" });
      const tab = page.getByRole("tab", { selected: true });
      const primary = await button.evaluate((element) => getComputedStyle(element).backgroundColor);
      await expect(tab).toHaveCSS("border-bottom-color", primary);

      const selected = theme === "dark" ? LIME : "rgb(77, 124, 15)";
      const violations = async () => {
        await page.addScriptTag({ content: axe.source });
        return (await page.evaluate(() => axe.run(document.querySelector(".pp-root") ?? document))).violations;
      };
      await page.addStyleTag({ content: LIME_HOST });
      await expect(button).toHaveCSS("background-color", LIME);
      await expect(tab).toHaveCSS("border-bottom-color", selected);
      for (const ratio of await edgeContrast(tab, "Bottom")) expect(ratio).toBeGreaterThanOrEqual(3);
      expect(await violations(), "checkout").toEqual([]);
      await check(page, `lime-wallet-${suffix}`);

      // The fields' edges are the input color, which a light primary color leaves at 3:1.
      await page.goto(`/?deposit&theme=${theme}`);
      await page.addStyleTag({ content: LIME_HOST });
      for (const ratio of await edgeContrast(page.getByRole("combobox"))) expect(ratio).toBeGreaterThanOrEqual(3);
      expect(await violations(), "deposit address").toEqual([]);
      await check(page, `lime-deposit-${suffix}`);
    });
  }
}

// A host's reset, as Tailwind's preflight has it: SVGs and images are blocks.
const PREFLIGHT = "svg, img, video, canvas { display: block; vertical-align: middle; }";

for (const width of [390, 1280]) {
  test(`glyphs stay on their line of text under a host's CSS reset at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    const served = waiting();
    await serve(page, () => served);
    await installWallet(page);

    await page.goto(checkoutUrl("light"));
    await page.addStyleTag({ content: PREFLIGHT });
    await page.getByRole("button", { name: "Pay with Test Wallet" }).click();
    const hash = page.locator("a.pp-tx__hash");
    await expect(hash).toBeVisible();
    // The hash wraps at narrow widths; its glyph stays inline, after its last character.
    expect(await hash.evaluate((element) => getComputedStyle(element.querySelector("svg") ?? element).display)).toBe("inline-block");
  });
}

test("keyboard focus is visible and differs from the selected state", async ({ page }) => {
  await page.goto("/?deposit");
  await expect(page.getByRole("group", { name: "Pay with" })).toBeVisible();
  const outline = (locator: Locator) =>
    locator.evaluate((element) => {
      const style = getComputedStyle(element);
      return { style: style.outlineStyle, width: style.outlineWidth, color: style.outlineColor, border: style.borderColor };
    });

  await page.keyboard.press("Tab");
  const token = page.getByRole("combobox", { name: "Token" });
  await expect(token).toBeFocused();
  const focused = await outline(token);
  expect(focused.style).toBe("solid");
  expect(focused.width).toBe("2px");
  expect(focused.color).not.toBe(focused.border);
  if (screenshots !== undefined) await page.locator(".pp-root").screenshot({ caret: "initial", path: join(screenshots, "focus-field.png") });

  // Unfocused: the field alone, without the focus ring.
  await page.keyboard.press("Tab");
  await expect(token).not.toBeFocused();
  expect((await outline(token)).style).toBe("none");

  // In forced colors, the listbox's active option keeps a system color of its own.
  await page.emulateMedia({ forcedColors: "active", reducedMotion: "reduce" });
  await expect.poll(() => page.evaluate(() => matchMedia("(forced-colors: active)").matches)).toBe(true);
  await token.focus();
  await page.keyboard.press("ArrowDown");
  const backgrounds = await page.getByRole("listbox", { name: "Token" }).getByRole("option").evaluateAll((options) =>
    options.map((option) => getComputedStyle(option).backgroundColor));
  expect(new Set(backgrounds).size).toBe(2);
  // A logo's ring is a system color too: GrayText, whatever the color around it.
  const rings = await page.locator(".pp-icon:has(image)").evaluateAll((icons) => {
    const probe = document.body.appendChild(document.createElement("span"));
    probe.style.setProperty("color", "GrayText");
    const grayText = getComputedStyle(probe).color;
    probe.remove();
    return icons.map((icon) => getComputedStyle(icon).outlineColor === grayText);
  });
  expect(rings.length).toBeGreaterThan(0);
  expect(rings).not.toContain(false);
  await page.keyboard.press("Escape");
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

for (const theme of ["light", "dark"]) {
  test(`a listbox opens over a clipping dialog near the window's foot, upward, ${theme}`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 640 });
    await page.goto(`/?modal&theme=${theme}`);
    const field = page.getByRole("combobox", { name: "Network" });
    await field.click();
    const listbox = page.getByRole("listbox", { name: "Network" });
    await expect(listbox).toBeVisible();
    const [fieldBox, listBox, dialogBox] = await Promise.all([field, listbox, page.getByRole("dialog")].map((each) => each.boundingBox()));
    // No room under the field: the listbox is over it, inside the window, past the dialog's edge.
    expect((listBox?.y ?? 0) + (listBox?.height ?? 0)).toBeLessThanOrEqual((fieldBox?.y ?? 0) + 1);
    expect(listBox?.y ?? -1).toBeGreaterThanOrEqual(0);
    expect(listBox?.y ?? 0).toBeLessThan(dialogBox?.y ?? 0);
    // Not clipped: each option is what is under its own centre, and takes a press.
    for (const option of await listbox.getByRole("option").all()) {
      expect(await option.evaluate((element) => {
        const box = element.getBoundingClientRect();
        return element.contains(document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2));
      })).toBe(true);
    }
    expect(await axeViolations(page)).toEqual([]);
    if (screenshots !== undefined) await page.screenshot({ path: join(screenshots, `modal-open-${theme}.png`) });
    await listbox.getByRole("option", { name: "Ethereum", exact: true }).click();
    await expect(field).toHaveText("Ethereum");
    await expect(listbox).toBeHidden();
    await expect(page.locator(".pp-root [style]")).toHaveCount(0);
  });
}
