import AxeBuilder from "@axe-core/playwright";
import { existsSync, readdirSync } from "node:fs";
import { expect, test as base, type Locator, type Page } from "@playwright/test";
import {
  createPublicClient,
  createTestClient,
  encodeFunctionData,
  erc20Abi,
  getAddress,
  http,
  parseAbi,
  parseEther,
  parseGwei,
  parseUnits,
  publicActions,
  walletActions,
  type Address,
  type Hash,
} from "viem";
import { baseSepolia, sepolia } from "viem/chains";
import type { Timeline } from "../src/api.js";
import { DOCS, docPath } from "../src/content/docs.js";
import { tokens } from "../src/format.js";
import { EXPIRED_QUOTE_INTERVAL_MS, QUERY_RETRY_LIMIT, TIMELINE_ACTIVE_INTERVAL_MS, TIMELINE_INTERVAL_MS } from "../src/polling.js";

declare global {
  interface Window {
    /** The page's layout shifts, collected by a test. */
    layoutShifts: number[];
    /** The page's timeline reads (`GET /api/quotes/{id}`): how many it has begun, and ended. */
    timelineReads: { started: number; settled: number };
    anvilRequest(
      chainId: number,
      method: string,
      params: unknown,
    ): Promise<{ result: unknown } | { error: { code: number; message: string } }>;
  }
}

function env(name: string): string {
  const value = process.env[name];
  if (value === undefined) {
    throw new Error(`${name} is not set; global setup did not run`);
  }
  return value;
}

// Counts the page's timeline reads where they begin, in the page's own fetch, so that a count
// taken there includes every read the page has started, however late its network events reach
// the test (an intercepted request's can trail the page by a few milliseconds).
const test = base.extend<{ timelineReads: undefined }>({
  timelineReads: [async ({ page }, use) => {
    await page.addInitScript(() => {
      const reads = { started: 0, settled: 0 };
      window.timelineReads = reads;
      const fetch = window.fetch.bind(window);
      window.fetch = (input, init) => {
        const url = new URL(input instanceof Request ? input.url : String(input), location.href);
        const method = (init?.method ?? (input instanceof Request ? input.method : "GET")).toUpperCase();
        if (method !== "GET" || !/^\/api\/quotes\/[^/]+$/.test(url.pathname)) {
          return fetch(input, init);
        }
        reads.started += 1;
        return fetch(input, init).finally(() => {
          reads.settled += 1;
        });
      };
    });
    await use(undefined);
  }, { auto: true }],
});

/** Finish the visible page's initial timeline refresh before observing its polling interval. */
async function refetchTimeline(page: Page) {
  const response = page.waitForResponse("**/api/quotes/*");
  await page.evaluate(() => window.dispatchEvent(new Event("visibilitychange")));
  const current = await response;
  await current.finished();
  return current;
}

/**
 * The page has stopped polling its timeline. A first window lets every read already set in motion
 * (by loading, the checkout's status reports, a refocus) begin and end; over a second window of the
 * same length, the page begins no read. Polling would begin one in each.
 */
async function expectNoTimelineRequests(page: Page, windowMs: number) {
  await page.clock.runFor(windowMs);
  await page.waitForFunction(() => window.timelineReads.started === window.timelineReads.settled, undefined, { polling: 100 });
  const before = await page.evaluate(() => window.timelineReads.started);
  await page.clock.runFor(windowMs);
  expect(await page.evaluate(() => window.timelineReads.started), "timeline reads begun in the second window").toBe(before);
}

/** The owner's balance of a token: Sepolia's test PHA unless named. */
async function tokenBalance(
  owner: string,
  { rpc = env("ANVIL_URL"), token = env("TOKEN_ADDRESS") }: { rpc?: string; token?: string } = {},
): Promise<bigint> {
  const chain = createPublicClient({ transport: http(rpc) });
  return chain.readContract({
    address: getAddress(token),
    abi: erc20Abi,
    functionName: "balanceOf",
    args: [getAddress(owner)],
  });
}

/**
 * Pays `amount` of the test token from the treasury: on Anvil the test plays the merchant's
 * finance team, which controls the treasury (on staging it is Phala's finance Safe).
 */
async function payFromTreasury(to: string, amount: bigint): Promise<Hash> {
  const treasury = getAddress(env("TREASURY"));
  const chain = createTestClient({ mode: "anvil", chain: sepolia, transport: http(env("ANVIL_URL")) }).extend(
    walletActions,
  );
  await chain.impersonateAccount({ address: treasury });
  await chain.setBalance({ address: treasury, value: parseEther("1") });
  const hash = await chain.sendTransaction({
    account: treasury,
    to: getAddress(env("TOKEN_ADDRESS")),
    data: encodeFunctionData({ abi: erc20Abi, functionName: "transfer", args: [getAddress(to), amount] }),
  });
  await chain.stopImpersonatingAccount({ address: treasury });
  return hash;
}

/** The test USDC's public mint, from the test (the page offers Circle's faucet instead). */
async function mintUsdc(to: string, amount: bigint): Promise<void> {
  const chain = createTestClient({ mode: "anvil", chain: sepolia, transport: http(env("ANVIL_URL")) })
    .extend(walletActions)
    .extend(publicActions);
  const payer = getAddress(env("PAYER_ADDRESS"));
  await chain.waitForTransactionReceipt({
    hash: await chain.sendTransaction({
      account: payer,
      to: getAddress(env("USDC_ADDRESS")),
      data: encodeFunctionData({
        abi: parseAbi(["function mint(address account, uint256 amount)"]),
        functionName: "mint",
        args: [getAddress(to), amount],
      }),
    }),
  });
}

/**
 * Anvil's dev account `index` other than the payer's (0): gas on both chains, and none of the test
 * tokens until a test gives it some. Each test that needs one takes its own.
 */
async function emptyAccount(index: number): Promise<Address> {
  const accounts = await createTestClient({ mode: "anvil", chain: sepolia, transport: http(env("ANVIL_URL")) })
    .extend(walletActions)
    .getAddresses();
  const account = accounts[index];
  if (account === undefined) {
    throw new Error(`anvil has no dev account ${index}`);
  }
  return account;
}

/** The transactions `account` has sent on Sepolia. */
async function sentCount(account: Address): Promise<number> {
  return createPublicClient({ transport: http(env("ANVIL_URL")) }).getTransactionCount({ address: account });
}

/**
 * EIP-6963 wallets on the Anvils, announced in order, each holding its account: by default one,
 * "Test Wallet", holding the payer's. Each starts on mainnet. Resolves with the RPC methods they
 * forwarded to the Anvils, in order, and `holdSends`, which holds every transaction the wallets
 * send, as a wallet does while the payer confirms, until the function it returns is called.
 */
async function installWallet(
  page: Page,
  wallets: { name: string; account: string }[] = [{ name: "Test Wallet", account: env("PAYER_ADDRESS") }],
): Promise<{ forwarded: string[]; holdSends: () => () => void }> {
  const rpcs: Record<number, string> = { [sepolia.id]: env("ANVIL_URL"), [baseSepolia.id]: env("BASE_ANVIL_URL") };
  const forwarded: string[] = [];
  let held: Promise<void> | undefined;
  await page.exposeFunction("anvilRequest", async (chainId: number, method: string, params: unknown) => {
    const rpc = rpcs[chainId];
    if (rpc === undefined) {
      return { error: { code: 4901, message: "wallet is on another chain" } };
    }
    forwarded.push(method);
    if (method === "eth_sendTransaction") {
      await held;
    }
    const response = await fetch(rpc, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params: params ?? [] }),
    });
    const body = (await response.json()) as {
      result?: unknown;
      error?: { code: number; message: string };
    };
    return body.error === undefined ? { result: body.result } : { error: body.error };
  });
  await page.addInitScript(
    ({ wallets, chainIds }) => {
      const fail = (code: number, message: string) => Object.assign(new Error(message), { code });
      const announced = wallets.map(({ name, account }, index) => {
        let current = 1;
        const known = new Set([1]);
        const provider = {
          async request({ method, params }: { method: string; params?: unknown }): Promise<unknown> {
            const [first] = (params ?? []) as [{ chainId?: string }?];
            switch (method) {
              case "eth_requestAccounts":
              case "eth_accounts":
                return [account];
              case "eth_chainId":
                return `0x${current.toString(16)}`;
              case "wallet_switchEthereumChain": {
                const id = Number(first?.chainId);
                if (!known.has(id)) {
                  throw fail(4902, "Unrecognized chain ID");
                }
                current = id;
                return null;
              }
              case "wallet_addEthereumChain":
                known.add(Number(first?.chainId));
                return null;
            }
            if (!chainIds.includes(current)) {
              throw fail(4901, "wallet is on another chain");
            }
            const answer = await window.anvilRequest(current, method, params);
            if ("error" in answer) {
              throw fail(answer.error.code, answer.error.message);
            }
            return answer.result;
          },
          on: () => undefined,
          removeListener: () => undefined,
        };
        const info = {
          uuid: `0b6f1e1e-6f0c-4c43-9d7a-2f0d4b0f7a1${index}`,
          name,
          icon: "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'/%3E",
          rdns: `test.wallet${index}`,
        };
        return Object.freeze({ info, provider });
      });
      const announce = () => {
        for (const detail of announced) {
          window.dispatchEvent(new CustomEvent("eip6963:announceProvider", { detail }));
        }
      };
      window.addEventListener("eip6963:requestProvider", announce);
      announce();
    },
    { wallets, chainIds: [sepolia.id, baseSepolia.id] as number[] },
  );
  const holdSends = () => {
    let release = () => undefined;
    held = new Promise<void>((resolve) => {
      release = () => {
        held = undefined;
        resolve();
      };
    });
    return release;
  };
  return { forwarded, holdSends };
}

/**
 * The network, then the token, chosen by default: Sepolia, and the first of its two tokens, test
 * PHA, which earns the demo merchant's bonus; test USDC is a stablecoin.
 */
async function expectPaymentOptions(product: Locator) {
  await expect(product.getByRole("combobox", { name: "Network" })).toHaveValue(String(sepolia.id));
  const token = product.getByRole("radiogroup", { name: "Token" });
  await expect(token.getByRole("radio")).toHaveCount(2);
  await expect(token.getByRole("radio", { name: "Test PHA", exact: true })).toBeChecked();
  const rows = token.getByTestId("token-option");
  await expect(rows.filter({ hasText: "PHA" })).toContainText("+10% bonus");
  // A stablecoin is $1.00; a spot token is at the market rate, which a quote locks.
  await expect(rows.filter({ hasText: "USDC" }).getByTestId("token-price")).toHaveText("$1.00");
  await expect(rows.filter({ hasText: "PHA" }).getByTestId("token-price")).toHaveText("Market rate");
  await expect(rows.filter({ hasText: "USDC" })).not.toContainText("bonus");
}

/** Chooses a network in the product's network select. */
async function chooseNetwork(product: Locator, name: string) {
  const select = product.getByRole("combobox", { name: "Network" });
  await select.selectOption({ label: name });
  await expect(select.locator("option:checked")).toHaveText(name);
}

/** Collects the page's console errors and CSP violations; the flows expect none. */
async function watchConsole(page: Page): Promise<string[]> {
  const problems: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      problems.push(message.text());
    }
  });
  page.on("pageerror", (error) => problems.push(error.message));
  await page.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (event) => {
      console.error(`CSP violation: ${event.violatedDirective} ${event.blockedURI}`);
    });
  });
  return problems;
}

/**
 * The demo at a phone's width: every control is a 44px target around its centre (its own box, its
 * label's, or a hit area it draws), save a link inside a sentence (WCAG 2.5.8's inline exception);
 * and no card holds a boxed part that holds another (a card's parts are set apart by dividers).
 */
async function expectTouchLayout(page: Page, state: string): Promise<void> {
  const { small, nested } = await page.locator("#demo-root").evaluate((root) => {
    const small: string[] = [];
    const controls = root.querySelectorAll<HTMLElement>(
      "a[href], button, input, select, textarea, summary, [role=tab], [role=radio]",
    );
    for (const control of controls) {
      if (control.getAttribute("aria-hidden") === "true" || !control.checkVisibility()) {
        continue;
      }
      const sentence = control.closest("p");
      if (control instanceof HTMLAnchorElement && sentence !== null && sentence.textContent.trim() !== control.textContent.trim()) {
        continue;
      }
      // A control in a label or an input group is reached through it: the box is the target.
      const target = control.closest("label, [data-slot=input-group]") ?? control;
      target.scrollIntoView({ block: "center", inline: "center" });
      const box = target.getBoundingClientRect();
      // 21px either side of the centre: a 44px target, as hit testing rounds to whole pixels.
      const [x, y, reach] = [box.left + box.width / 2, box.top + box.height / 2, 21];
      const stray = [[x - reach, y], [x + reach, y], [x, y - reach], [x, y + reach]]
        .map(([px = 0, py = 0]) => document.elementFromPoint(px, py))
        .find((hit) => hit === null || !target.contains(hit));
      if (stray !== undefined) {
        const name = control.getAttribute("aria-label") ?? control.textContent.trim();
        const hit = stray === null ? "nothing" : `${stray.tagName.toLowerCase()}.${[...stray.classList].slice(0, 3).join(".")}`;
        small.push(`${control.tagName.toLowerCase()} "${name}" (${Math.round(box.width)}×${Math.round(box.height)}, reaches ${hit})`);
      }
    }
    window.scrollTo(0, 0);
    // A box: an element framed on every side, other than a control.
    const boxed = (element: Element) => {
      if (element.matches("a, button, input, select, textarea, label, summary, [role=radio], [role=tab], [data-slot=input-group]")) {
        return false;
      }
      const style = getComputedStyle(element);
      return (["Top", "Right", "Bottom", "Left"] as const).every(
        (side) => style[`border${side}Style`] !== "none" && parseFloat(style[`border${side}Width`]) > 0,
      ) && !/rgba\(.*, 0\)$/.test(style.borderTopColor);
    };
    const nested: string[] = [];
    for (const element of root.querySelectorAll("*")) {
      if (!boxed(element)) continue;
      let depth = 1;
      for (let parent = element.parentElement; parent !== null && parent !== root; parent = parent.parentElement) {
        if (boxed(parent)) depth += 1;
      }
      if (depth > 2) nested.push(`${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")} (${depth} deep)`);
    }
    return { small, nested };
  });
  expect(small, `${state}: targets under 44px at 390px`).toEqual([]);
  expect(nested, `${state}: cards nested more than one level`).toEqual([]);
}

/**
 * Checks the page with axe at its width and at a phone's (390px), each in the theme it is in, then
 * in the other (switched with the header's toggle, and back): no serious or critical violation. At
 * 390px, the demo's targets and cards too.
 */
async function expectAccessible(page: Page, state: string): Promise<void> {
  const viewport = page.viewportSize() ?? { width: 1360, height: 1000 };
  for (const width of [viewport.width, 390]) {
    await page.setViewportSize({ width, height: viewport.height });
    for (let pass = 0; pass < 2; pass++) {
      // Colours are read once the theme's colour transitions have settled. Polled on a timer, not
      // on animation frames, which axe's helper page can hold back.
      await page.waitForFunction(() => !document.getAnimations().some((animation) => animation instanceof CSSTransition), undefined, { polling: 100 });
      const theme = (await page.locator("html").getAttribute("class"))?.includes("dark") ? "dark" : "light";
      const { violations } = await new AxeBuilder({ page }).analyze();
      const serious = violations
        .filter((violation) => violation.impact === "serious" || violation.impact === "critical")
        .map((violation) => `${violation.id}: ${violation.nodes.map((node) => node.target.join(" ")).join(", ")}`);
      expect(serious, `${state}, ${width}px, ${theme} theme`).toEqual([]);
      if (width === 390 && pass === 0) {
        await expectTouchLayout(page, state);
      }
      await page.getByRole("button", { name: "Dark theme" }).click();
    }
  }
  await page.setViewportSize(viewport);
}

/**
 * The page's metadata for search results and link previews, and the files it links, served from
 * the page's origin.
 */
async function expectMetadata(page: Page): Promise<void> {
  const origin = "https://pay.phala.com/";
  const head = page.locator("head");
  const content = (selector: string) => head.locator(selector).getAttribute("content");
  await expect(page).toHaveTitle("Phala Pay: self-hosted, non-custodial crypto payments");
  const description = await content('meta[name="description"]');
  expect(description?.length).toBeLessThanOrEqual(160);
  await expect(head.locator('link[rel="canonical"]')).toHaveAttribute("href", origin);
  expect(await head.locator('meta[name="theme-color"]').evaluateAll((tags) => tags.map((tag) => tag.getAttribute("media")))).toEqual([
    "(prefers-color-scheme: light)",
    "(prefers-color-scheme: dark)",
  ]);
  expect(await content('meta[property="og:title"]')).toBe("Phala Pay: self-hosted, non-custodial crypto payments");
  expect(await content('meta[property="og:description"]')).toBe(description);
  expect(await content('meta[property="og:url"]')).toBe(origin);
  expect(await content('meta[property="og:image"]')).toBe(`${origin}og-image.png`);
  expect(await content('meta[property="og:image:width"]')).toBe("1200");
  expect(await content('meta[property="og:image:height"]')).toBe("630");
  expect(await content('meta[name="twitter:card"]')).toBe("summary_large_image");
  const structured = await head.locator('script[type="application/ld+json"]').textContent();
  expect(JSON.parse(structured ?? "null")).toMatchObject({
    "@context": "https://schema.org",
    "@graph": expect.arrayContaining([expect.objectContaining({ "@type": "WebSite", url: origin })]),
  });
  const links = await head
    .locator('link[rel="icon"], link[rel="apple-touch-icon"], link[rel="manifest"]')
    .evaluateAll((tags) => tags.map((tag) => (tag instanceof HTMLLinkElement ? tag.href : "")));
  for (const url of [...links, new URL("og-image.png", page.url()).href, new URL("robots.txt", page.url()).href]) {
    expect((await page.request.get(url)).status(), url).toBe(200);
  }
}

function step(timeline: Locator, key: string): Locator {
  return timeline.locator(`[data-step="${key}"]`);
}

/** An opened step's details: its hint, time, and data, shown under the stepper. */
function stepDetails(timeline: Locator, key: string): Locator {
  return timeline.page().locator(`[data-step-details="${key}"]`);
}

/** Opens a step to its hint, time, and data (closed steps render none), and returns them. */
async function openStep(timeline: Locator, key: string): Promise<Locator> {
  const trigger = step(timeline, key).getByRole("button");
  if ((await trigger.getAttribute("aria-expanded")) !== "true") {
    await trigger.click();
  }
  await expect(trigger).toHaveAttribute("aria-expanded", "true");
  return stepDetails(timeline, key);
}

async function expectComplete(timeline: Locator, keys: string[], timeout = 60_000) {
  for (const key of keys) {
    await expect(step(timeline, key)).toHaveAttribute("data-state", "complete", { timeout });
  }
}

/** Opens one of the backend's tabs and returns its panel. */
async function openTab(scenes: Locator, name: string): Promise<Locator> {
  await scenes.getByRole("tab", { name: new RegExp(`^${name}`) }).click();
  return scenes.getByRole("tabpanel", { name: new RegExp(`^${name}`) });
}

/** Opens every "Show … (N)" list in `scope`: "Show all (5)", "Show finalized sweeps (2)". */
async function showAll(scope: Locator): Promise<void> {
  const buttons = scope.getByRole("button", { name: /^Show [a-z ]+ \(\d+\)$/ });
  while ((await buttons.count()) > 0) {
    await buttons.first().click();
  }
}

/**
 * Declares a refund of `amount` PHA and returns its list item, opened to its transfer and forms
 * unless `open` is false (a refund's row starts closed).
 */
async function declareRefund(scenes: Locator, amount: string, open = true): Promise<Locator> {
  await openTab(scenes, "Refunds");
  const refunds = scenes.getByTestId("refund");
  const before = await refunds.count();
  const form = scenes.getByRole("form", { name: "Declare a refund" });
  await form.getByLabel(/^Amount/).fill(amount);
  await form.getByRole("button", { name: "Declare refund" }).click();
  await expect(refunds).toHaveCount(before + 1);
  // Newest first; followed by its id from here on.
  const id = await refunds.first().getAttribute("data-refund");
  const refund = scenes.locator(`[data-refund="${id ?? ""}"]`);
  await expect(refund).toHaveAttribute("data-status", "pending");
  if (open) {
    const trigger = refund.getByRole("button", { name: /^Refund / });
    await trigger.click();
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
  }
  return refund;
}

test("query outages show retrying states, recover, and preserve the last account data", async ({ page }) => {
  let available = false;
  await page.route("**/api/{account,assets,trust}", async (route) => {
    if (available) {
      await route.continue();
    } else {
      await route.fulfill({
        status: 503,
        json: { code: "service_unavailable" },
        headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
      });
    }
  });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await expect(product).toContainText("Account is unavailable right now; retrying…", { timeout: 15_000 });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const trust = await openTab(scenes, "Trust");
  await expect(trust).toContainText("Trust information is unavailable right now; retrying…");
  await expect(trust).toContainText("Networks are unavailable right now; retrying…");

  available = true;
  const recovered = page.waitForResponse("**/api/account");
  await page.evaluate(() => window.dispatchEvent(new Event("visibilitychange")));
  const recoveredResponse = await recovered;
  expect(recoveredResponse.status()).toBe(200);
  await recoveredResponse.finished();
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  await expect(product.getByRole("combobox", { name: "Network", exact: true })).toBeEnabled();
  await expect(trust).toContainText("Attestation verified");
  await expect(trust).not.toContainText("retrying…");

  available = false;
  for (let attempt = 0; attempt <= QUERY_RETRY_LIMIT; attempt += 1) {
    const unavailable = page.waitForResponse("**/api/account");
    if (attempt === 0) {
      await page.evaluate(() => window.dispatchEvent(new Event("visibilitychange")));
    }
    const response = await unavailable;
    expect(response.status()).toBe(503);
    await response.finished();
  }
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  await expect(product).not.toContainText("Account is unavailable");
});

test("a missing timeline shows a terminal message and stops polling", async ({ page }) => {
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    await route.fulfill({
      status: 404,
      json: { code: "not_found" },
      headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
    });
  });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  await expect(scenes).toContainText("Timeline is unavailable for this request.");
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  await expect(scenes.getByTestId("stream-status")).toHaveText("Unavailable");
  await expect(scenes.getByRole("list", { name: /^Loading/ })).toHaveCount(0);
  expect((await refetchTimeline(page)).status()).toBe(404);
  await expectNoTimelineRequests(page, 3 * TIMELINE_INTERVAL_MS);
});

test("a refused timeline keeps its cached data and shows paused updates", async ({ page }) => {
  let missing = false;
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    if (!missing) {
      await route.continue();
      return;
    }
    await route.fulfill({
      status: 404,
      json: { code: "not_found" },
      headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
    });
  });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  await expect(step(timeline, "quote_created")).toHaveAttribute("data-state", "complete");
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  expect((await refetchTimeline(page)).status()).toBe(200);
  missing = true;
  const timelineUrl = new URLPattern(`${env("API_URL")}/api/quotes/*`);
  const response = page.waitForResponse((response) => timelineUrl.test(response.url()) && response.status() === 404);
  await page.clock.runFor(TIMELINE_INTERVAL_MS);
  const refused = await response;
  expect(refused.status()).toBe(404);
  await refused.finished();
  await expect(scenes.getByText("Updates paused.", { exact: true })).toBeVisible();
  await expect(step(timeline, "quote_created")).toHaveAttribute("data-state", "complete");
  await expectNoTimelineRequests(page, 3 * TIMELINE_INTERVAL_MS);
});

for (const sent of [false, true]) {
  test(`a quote past its deadline polls ${sent ? "in-flight transfers" : "slowly for late payments"}`, async ({ page }) => {
    await page.clock.install();
    await page.route("**/api/quotes/*", async (route) => {
      const timeline: Timeline = {
        kind: "quote",
        quote: {
          id: new URL(route.request().url()).pathname.split("/").at(-1) ?? "",
          status: sent ? "open" : "expired",
          chain_id: sepolia.id,
          asset: "pha",
          exchange_rate: "0.25",
          expires_at: 1,
          metadata: {},
        },
        deposit: null,
        sent: sent ? { tx_hash: `0x${"1".repeat(64)}`, block_number: 1, at: 1 } : null,
        steps: [{ key: "sent", state: sent ? "current" : "failed", at: null, details: [] }],
        refunds: [], ledger: null, events: [], api: [],
      };
      await route.fulfill({
        json: timeline,
        headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
      });
    });
    await page.goto(env("SITE_URL"));
    const product = page.getByRole("region", { name: "Customer view" });
    await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
    await expect(page.getByRole("list", { name: "Payment timeline" })).toBeVisible();
    await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
    expect((await refetchTimeline(page)).status()).toBe(200);
    if (!sent) {
      await expectNoTimelineRequests(page, TIMELINE_INTERVAL_MS);
    }
    const response = page.waitForResponse("**/api/quotes/*");
    await page.clock.runFor(sent ? TIMELINE_ACTIVE_INTERVAL_MS : EXPIRED_QUOTE_INTERVAL_MS - TIMELINE_INTERVAL_MS);
    const update = await response;
    expect(update.status()).toBe(200);
    await update.finished();
  });
}

test("a swept payment keeps polling until its webhook arrives", async ({ page }) => {
  let delivered = false;
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    const timeline: Timeline = {
      kind: "quote", quote: null, sent: null, refunds: [], ledger: null, events: [], api: [],
      deposit: {
        id: `dep_${"1".repeat(32)}`,
        status: "credited", final: true, swept: true,
        amount: 2000, amount_atomic: (80n * 10n ** 18n).toString(),
        chain_id: sepolia.id, asset: "pha", exchange_rate: "0.25", price_source: "quote",
        amount_refunded_atomic: "0", amount_refunded: 0, amount_reversed: 0,
        from_address: env("PAYER_ADDRESS"), asset_contract: env("TOKEN_ADDRESS"),
        tx_hash: `0x${"1".repeat(64)}`, metadata: {},
      },
      steps: [{ key: "webhook_received", state: delivered ? "complete" : "current", at: delivered ? 1 : null, details: [] }],
    };
    await route.fulfill({
      json: timeline,
      headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
    });
  });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  await expect(step(scenes, "webhook_received")).toHaveAttribute("data-state", "current");
  await expect(scenes.getByTestId("stream-status")).toHaveText("Live");
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  expect((await refetchTimeline(page)).status()).toBe(200);
  delivered = true;
  const response = page.waitForResponse("**/api/quotes/*");
  await page.clock.runFor(TIMELINE_INTERVAL_MS);
  const update = await response;
  expect(update.status()).toBe(200);
  await update.finished();
  await expect(step(scenes, "webhook_received")).toHaveAttribute("data-state", "complete");
  await expect(scenes.getByTestId("stream-status")).toHaveText("Done");
  await expectNoTimelineRequests(page, 3 * TIMELINE_INTERVAL_MS);
});

/** A deposit the service reports `status`, and the webhook events the product has received. */
function depositTimeline(status: "reversed" | "rejected", events: Timeline["events"]): Timeline {
  const reversed = status === "reversed";
  return {
    kind: "quote", quote: null, sent: null, refunds: [], ledger: null, events, api: [],
    deposit: {
      id: `dep_${"1".repeat(32)}`, status, final: false, swept: false,
      amount: reversed ? 2000 : null, amount_atomic: (80n * 10n ** 18n).toString(),
      chain_id: sepolia.id, asset: "pha", exchange_rate: reversed ? "0.25" : null,
      price_source: reversed ? "quote" : null,
      amount_refunded_atomic: "0", amount_refunded: 0, amount_reversed: reversed ? 2000 : 0,
      from_address: env("PAYER_ADDRESS"), asset_contract: env("TOKEN_ADDRESS"),
      tx_hash: `0x${"1".repeat(64)}`, metadata: {},
    },
    steps: [{ key: reversed ? "reversed" : "credited", state: "failed", at: 1, details: [] }],
  };
}

const REVERSED_EVENT = { id: `evt_${"1".repeat(32)}`, type: "deposit.reversed", received_at: 1, verified: true, data: {} };

for (const status of ["reversed", "rejected"] as const) {
  test(`a ${status} deposit ${status === "reversed" ? "stops" : "continues"} timeline polling`, async ({ page }) => {
    await page.clock.install();
    await page.route("**/api/quotes/*", async (route) => {
      await route.fulfill({
        // The reversal's webhook has reached the product: nothing is left to follow.
        json: depositTimeline(status, status === "reversed" ? [REVERSED_EVENT] : []),
        headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
      });
    });
    await page.goto(env("SITE_URL"));
    const product = page.getByRole("region", { name: "Customer view" });
    await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
    await expect(page.getByRole("list", { name: "Payment timeline" })).toBeVisible();
    await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
    expect((await refetchTimeline(page)).status()).toBe(200);
    if (status === "reversed") {
      await expectNoTimelineRequests(page, 3 * TIMELINE_INTERVAL_MS);
    } else {
      const response = page.waitForResponse("**/api/quotes/*");
      await page.clock.runFor(TIMELINE_INTERVAL_MS);
      expect((await response).status()).toBe(200);
    }
  });
}

test("a reversed deposit keeps polling while the reversal's webhook has not arrived", async ({ page }) => {
  let delivered = false;
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    await route.fulfill({
      json: depositTimeline("reversed", delivered ? [REVERSED_EVENT] : []),
      headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
    });
  });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  await expect(page.getByRole("list", { name: "Payment timeline" })).toBeVisible();
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  expect((await refetchTimeline(page)).status()).toBe(200);
  // The service says reversed; the product has not heard yet: the timeline keeps following.
  const waiting = page.waitForResponse("**/api/quotes/*");
  await page.clock.runFor(TIMELINE_INTERVAL_MS);
  const stillWaiting = await waiting;
  expect(stillWaiting.status()).toBe(200);
  await stillWaiting.finished();
  delivered = true;
  const response = page.waitForResponse("**/api/quotes/*");
  await page.clock.runFor(TIMELINE_INTERVAL_MS);
  const update = await response;
  expect(update.status()).toBe(200);
  await update.finished();
  await expectNoTimelineRequests(page, 3 * TIMELINE_INTERVAL_MS);
});

test("a quote: locked price, metadata, the merchant's sweep, and refunds that succeed, fail, or are canceled", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(300_000);
  const problems = await watchConsole(page);
  await installWallet(page);
  const response = await page.goto(env("SITE_URL"));
  expect(response?.headers()["content-security-policy"]).toContain("default-src 'none'");
  expect(response?.headers()["content-security-policy"]).not.toContain("'unsafe-inline'");

  // The headline, its call to deploy, the product (marked as a testnet demo) beside its backend
  // (the attestation in the backend's Trust tab), and a fresh demo account.
  const headline = "Crypto payments, without a custodian";
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(headline);
  const hero = page.getByRole("region", { name: headline });
  await expect(hero.getByRole("link", { name: "Start a testnet instance" })).toHaveAttribute(
    "href",
    "/docs/self-hosting#one-command-deploy",
  );
  // The one-command deploy is a release asset: the site only redirects to it (public/_redirects).
  const releases = "https://github.com/Phala-Network/phala-pay/releases";
  for (const [path, location] of [
    ["deploy.sh", `${releases}/latest/download/deploy.sh`],
    ["deploy/v0.3.2.sh", `${releases}/download/v0.3.2/deploy.sh`],
  ] as const) {
    const redirect = await page.request.get(new URL(path, env("SITE_URL")).href, { maxRedirects: 0 });
    expect(redirect.status(), path).toBe(302);
    expect(redirect.headers()["location"], path).toBe(location);
  }
  await expect(hero.getByRole("link", { name: "Read the docs" })).toHaveAttribute(
    "href",
    "/docs",
  );
  await expect(page.getByRole("banner").getByRole("link", { name: "Self-host", exact: true })).toHaveAttribute(
    "href",
    "/docs/self-hosting",
  );
  await expectMetadata(page);
  const product = page.getByRole("region", { name: "Customer view" });
  await expect(product.getByTestId("testnet-notice")).toHaveText("Testnet");
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const preview = scenes.getByRole("list", { name: "The steps of a payment" });
  await expect(preview).toBeVisible();
  // The credit's expected time is the chain's typical_credit_seconds, from GET /v1/config.
  await expect(step(preview, "credited")).toContainText("usually ~30\u00a0s");
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  await expectAccessible(page, "home and the demo's initial state");
  const trust = await openTab(scenes, "Trust");
  await expect(trust).toContainText("Attestation verified");
  await expect(trust).toContainText("Verified");
  await expect(trust).toContainText("e2e0000000000000000000000000000000000001");
  const [cookie] = await context.cookies();
  expect(cookie).toMatchObject({ name: "demo_account", path: "/", httpOnly: true, sameSite: "Lax" });

  // The payer mints test PHA from the wallet, as the helper below the product offers.
  const testTokens = page.getByRole("note", { name: "Test tokens" });
  await testTokens.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
  await expect(testTokens).toContainText("Minted:");
  expect(await tokenBalance(env("PAYER_ADDRESS"))).toBe(parseEther("1000"));

  // The customer picks the amount, the network, then the token: the product's one network,
  // Sepolia, and its one token, test PHA, shown and chosen.
  await expectPaymentOptions(product);

  // $20 at the fake service's 0.25 USD per PHA: exactly 80 PHA, with an order id in its metadata.
  // The quote is for the chosen network and token.
  await product.getByText("$20", { exact: true }).click();
  const quoteRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  expect((await quoteRequest).postDataJSON()).toEqual({ amount: 2000, chain_id: sepolia.id, asset: "pha" });
  // The checkout's summary states the quote: 80 PHA for $20.00 (its rate is in the backend's
  // timeline); the SDK's status line holds the one countdown.
  const summary = product.locator(".pp-summary");
  await expect(summary).toContainText("80 PHA");
  await expect(summary).toContainText("$20.00");
  await expect(product.getByLabel("Time left to pay")).toHaveText(/^1[45]:\d\d$/);
  await expect(product.getByText(/\d+:\d\d$/)).toHaveCount(1);
  await expectAccessible(page, "a quote awaiting payment");
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  await expect(step(timeline, "quote_created")).toHaveAttribute("data-state", "complete");
  await expect(step(timeline, "sent")).toHaveAttribute("data-state", "current");
  const created = await openStep(timeline, "quote_created");
  await expect(created).toContainText("1 PHA = $0.25");
  await expect(created).toContainText("80 PHA");
  const order = (await scenes.getByTestId("meta-order").locator("[data-value]").getAttribute("data-value"))?.match(/^order_[0-9a-f]{12}$/)?.[0];
  expect(order).toBeDefined();
  // Nothing of the backend shows in the product.
  await expect(product).not.toContainText("order_");
  await page.screenshot({ path: testInfo.outputPath("checkout.png"), fullPage: true });

  await product.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(product.getByText(/^Transaction sent:/)).toBeVisible();
  await expectComplete(timeline, ["sent", "received", "credited", "webhook_received"]);
  // One confirmation: the credit, the demo merchant's bonus, the total, and the transaction.
  const confirmation = product.getByTestId("payment-credited");
  await expect(confirmation).toContainText("Payment credited");
  await expect(confirmation).toContainText("$20.00");
  // Nothing pending once credited: the checkout is gone with its countdown.
  await expect(summary).toHaveCount(0);
  // The demo merchant's +10% PHA bonus, a line of its own: $20.00 and $2.00.
  await expect(product.getByTestId("bonus-credited")).toContainText("+$2.00", { timeout: 10_000 });
  await expect(confirmation).toContainText("Total$22.00");
  await expect(confirmation.getByRole("link")).toHaveAttribute("href", /\/tx\/0x[0-9a-f]{64}$/);
  await expect(product.getByTestId("balance")).toHaveText("$22.00", { timeout: 10_000 });
  // Real times: the block's, then each step's, with the elapsed time since sending.
  const credited = await openStep(timeline, "credited");
  await expect(credited).toContainText("after sending");
  await expect(credited).toContainText("the quote's locked price");
  // Each step opens to its data. The order id arrives in the verified deposit.credited's
  // data.object.metadata.
  const webhook = await openStep(timeline, "webhook_received");
  await expect(webhook.getByText("data.object.metadata")).toBeVisible();
  await expect(webhook).toContainText("verified");
  await expect(webhook).toContainText(`"order_id": "${order ?? ""}"`);
  await expect(webhook).toContainText("+$20.00");
  await openTab(scenes, "API");
  await expect(scenes.getByTestId("webhook-event").first()).toContainText("deposit.credited");
  // Refunds wait for finality.
  await openTab(scenes, "Refunds");
  await expect(scenes.getByTestId("refund-unavailable")).toContainText("deposit_not_final");
  await expectComplete(timeline, ["final"]);
  await expect(step(timeline, "swept")).toHaveAttribute("data-state", "current");

  // The merchant sweeps: the SDK's flush, signed from a wallet (anyone may send it; the funds can
  // only reach the treasury), indexed by the service once final.
  const sweepsPanel = await openTab(scenes, "Sweeps");
  const sweeps = sweepsPanel.getByRole("region", { name: "PHA on Sepolia testnet" });
  await expect(sweeps.getByTestId("unswept")).toContainText("80 PHA in 1 forwarder", { timeout: 30_000 });
  await sweeps.getByRole("button", { name: "Sweep from wallet" }).click();
  await expect(sweeps.getByTestId("flush-status")).toContainText("Flush sent: 0x");
  await expectComplete(timeline, ["swept"]);
  expect(await tokenBalance(env("TREASURY"))).toBe(parseEther("80"));
  await expect((await openStep(timeline, "swept")).locator("a").first()).toHaveAttribute(
    "href",
    /^https:\/\/sepolia\.etherscan\.io\/tx\/0x[0-9a-f]{64}$/,
  );
  await sweepsPanel.getByRole("button", { name: /^Show finalized sweeps/ }).click({ timeout: 30_000 });
  await expect(sweepsPanel.getByTestId("sweep").first()).toContainText("80 PHA", { timeout: 30_000 });

  // A refund paid from the treasury succeeds: 20 of 80 PHA takes back a quarter of the credit.
  const paid = await declareRefund(scenes, "20");
  await expect(paid.getByTestId("refund-transfer")).toContainText(env("TREASURY"));
  const hash = await payFromTreasury(env("PAYER_ADDRESS"), parseEther("20"));
  await paid.getByLabel("Transaction hash of the payment").fill(hash);
  await paid.getByRole("button", { name: "Mark paid" }).click();
  await expect(paid).toContainText("Marked paid");
  await expect(paid).toHaveAttribute("data-status", "succeeded", { timeout: 60_000 });
  await expect(paid).toContainText("deposit.refunded");
  await expect(scenes.getByTestId("nets-to")).toHaveText("$15.00");
  // The bonus follows the credit down: 10% of $15.00.
  await expect(product.getByTestId("balance")).toHaveText("$16.50", { timeout: 10_000 });
  await expect(scenes.getByTestId("console-net")).toContainText("−$5.00 by deposit.refunded");
  await expect(scenes.getByTestId("console-bonus")).toContainText("+$1.50");

  // A refund paid from another wallet fails verification: the service checks the sender.
  const wrong = await declareRefund(scenes, "20");
  await wrong.getByRole("button", { name: "Pay it from my wallet instead" }).click();
  await expect(wrong.getByLabel("Transaction hash of the payment")).toHaveValue(/^0x[0-9a-f]{64}$/);
  await wrong.getByRole("button", { name: "Mark paid" }).click();
  await expect(wrong).toHaveAttribute("data-status", "failed", { timeout: 60_000 });
  await expect(wrong).toContainText("sender_mismatch");
  await expect(product.getByTestId("balance")).toHaveText("$16.50");

  // A declared refund without a payment can be canceled.
  const canceled = await declareRefund(scenes, "20");
  await canceled.getByRole("button", { name: "Cancel refund" }).click();
  await expect(canceled).toHaveAttribute("data-status", "canceled");
  await expectAccessible(page, "refunds that succeeded, failed, and were canceled");
  // The tab shows the latest events and requests; the rest open in the page.
  const apiPanel = await openTab(scenes, "API");
  await showAll(apiPanel);
  const events = scenes.getByTestId("webhook-event");
  for (const type of ["refund.created", "refund.updated", "refund.failed", "deposit.refunded"]) {
    await expect(events.filter({ hasText: type }).first()).toBeVisible();
  }

  // The developer view shows the requests, never the API key or a client secret.
  const api = scenes.getByRole("region", { name: /^API requests/ });
  await expect(api).toContainText("GET /v1/deposits");
  await expect(api).toContainText("GET /v1/refunds");
  await api.locator("details").first().click();
  await expect(api).toContainText("Bearer ppay_rk_test_…");
  await expect(api).not.toContainText("AAAAAAAA");

  // The history row and the ledger lines behind the balance, all of them.
  await showAll(await openTab(scenes, "Credits"));
  const row = scenes.getByTestId("payment").first();
  await expect(row).toContainText("Quote");
  await expect(row).toContainText("80 PHA");
  await expect(row).toContainText("at $0.25 / PHA");
  await expect(row).toContainText("$15.00");
  await expect(row).toContainText("+$1.50 bonus");
  // How the balance adds up: the credit and its refund, and the bonus and its claw-back, apart.
  const credits = scenes.locator('[data-testid="ledger-line"][data-kind="credit"]');
  const bonuses = scenes.locator('[data-testid="ledger-line"][data-kind="bonus"]');
  await expect(credits.filter({ hasText: "deposit.credited" })).toContainText("+$20.00");
  await expect(credits.filter({ hasText: "deposit.refunded" })).toContainText("−$5.00");
  await expect(bonuses.filter({ hasText: "PHA bonus +10%" })).toContainText("+$2.00");
  await expect(bonuses.filter({ hasText: "deposit.refunded" })).toContainText("−$0.50");

  await page.screenshot({ path: testInfo.outputPath("refunds-light.png"), fullPage: true });
  await page.getByRole("button", { name: "Dark theme" }).click();
  await page.screenshot({ path: testInfo.outputPath("refunds-dark.png"), fullPage: true });
  await page.setViewportSize({ width: 420, height: 900 });
  await page.screenshot({ path: testInfo.outputPath("mobile-dark.png"), fullPage: true });

  // A quote that ends unpaid, expired or canceled, says so; another top-up starts from the button.
  await page.setViewportSize({ width: 1360, height: 1000 });
  // The quote the backend follows, its id in full in the header's Quote.
  const followed = scenes.getByTestId("meta-selected").locator('[data-value^="qt_"]');
  for (const [end, message] of [
    ["expire", "Quote expired"],
    ["cancel", "Quote canceled"],
  ] as const) {
    const before = await followed.getAttribute("data-value");
    await product.getByRole("button", { name: "Start a new top-up" }).click();
    await product.getByText("$5", { exact: true }).click();
    await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
    await expect(followed).not.toHaveAttribute("data-value", before ?? "");
    await expect(summary).toContainText("20 PHA");
    const quote = (await followed.getAttribute("data-value")) ?? "";
    const ended = await fetch(`${env("SERVICE_URL")}/_test/quotes/${quote}/${end}`, { method: "POST" });
    expect(ended.status).toBe(200);
    await expect(product.getByRole("status").first()).toContainText(message, { timeout: 10_000 });
    const accountStatus = message.replace("Quote ", "").replace(/^./, (letter) => letter.toUpperCase());
    await expect(scenes.getByTestId("payment").filter({ hasText: accountStatus }).first()).toBeVisible({ timeout: 10_000 });
  }
  expect(problems).toEqual([]);
});

test("a deposit address: one verified address, any amount credited at spot, then reversed", async ({
  page,
}, testInfo) => {
  test.setTimeout(180_000);
  const problems = await watchConsole(page);
  await installWallet(page);
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");

  // The tabs follow the keyboard.
  await page.getByRole("tab", { name: "Exact amount" }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "Deposit address" })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tab", { name: "Deposit address" })).toBeFocused();

  await expectPaymentOptions(product);
  await product.getByRole("button", { name: "Show my deposit address" }).click();
  // The backend sees the address checked against the product's pins.
  await expect(scenes.getByTestId("deposit-address-verified")).toContainText("Verified");
  // Its networks and tokens, named as the product's selectors name them.
  // One address on both networks (the treasury is the same), each with its tokens.
  const tokensOnNetwork = scenes.getByTestId("deposit-address-network");
  await expect(tokensOnNetwork).toHaveText(["Test PHA, Test USDC", "Test PHA, Test USDT"]);
  await expect(tokensOnNetwork.first().locator("xpath=..")).toContainText("Sepolia testnet");
  await expect(tokensOnNetwork.last().locator("xpath=..")).toContainText("Base Sepolia testnet");
  const address = (await scenes.getByTestId("deposit-address").getByRole("link").getAttribute("href"))?.split("/").at(-1) ?? "";
  expect(address).toMatch(/^0x[0-9a-fA-F]{40}$/);
  // The SDK's <DepositAddress> shows the customer the same address to copy, and the networks'
  // typical credit time from the address's public view.
  await expect(product.getByText(address).first()).toBeVisible();
  await expect(product.getByText(/on arrival, usually in about 30 seconds\./)).toBeVisible();

  // Any amount, sent from a wallet as from an exchange.
  const testTokens = page.getByRole("note", { name: "Test tokens" });
  await testTokens.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
  await expect(testTokens).toContainText("Minted:");
  const form = product.getByRole("form", { name: "Pay to the deposit address from a browser wallet" });
  await form.getByLabel(/^Send from your browser wallet/).fill("25");
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  const mining = async (state: "pause" | "resume") => {
    expect((await fetch(`${env("SERVICE_URL")}/_test/mining/${state}`, { method: "POST" })).status).toBe(200);
  };
  // Keep reversal deterministic: the service refuses it once the payment reaches finality.
  await mining("pause");
  try {
    await form.getByRole("button", { name: "Send" }).click();
    await expect(form).toContainText("Sent: 0x");
    const chain = createTestClient({ mode: "anvil", chain: sepolia, transport: http(env("ANVIL_URL")) });
    await chain.mine({ blocks: 1 });
    await product.getByRole("tab", { name: "Exact amount", exact: true }).click();
    await expect(form).not.toBeVisible();

    // The mounted address SDK keeps the backend following payments while its tab is inactive.
    const payment = scenes.getByTestId("address-payment").first();
    await expect(payment).toContainText("25 PHA");
    await expect(payment).toHaveAttribute("aria-current", "true");
    await expect(payment).toContainText("Viewing");
    await expectComplete(timeline, ["sent", "received", "credited", "webhook_received"]);

    // Before it is final, the service's finality watch proves the transaction dropped (here, the
    // stand-in's test hook): deposit.reversed takes the credit back.
    const deposit = await page.evaluate(async (api) => {
      const response = await fetch(`${api}/api/deposit_address`, { credentials: "include" });
      return ((await response.json()) as { deposit_address: { payments: { deposit: string }[] } }).deposit_address
        .payments[0]?.deposit;
    }, env("API_URL"));
    const reversed = await fetch(`${env("SERVICE_URL")}/_test/deposits/${deposit ?? ""}/reverse`, { method: "POST" });
    expect(reversed.status).toBe(200);
  } finally {
    await mining("resume");
  }

  // 25 PHA at 0.25 USD, credited at spot; the address's metadata arrived with the deposit.
  // Two steps open at once, each with its own details.
  const creditedDetails = await openStep(timeline, "credited");
  const webhookDetails = await openStep(timeline, "webhook_received");
  await expect(creditedDetails).toContainText("spot");
  await expect(creditedDetails).toContainText("$6.25");
  await expect(webhookDetails).toContainText('"workspace": "demo-');
  await expect(webhookDetails).toContainText("+$6.25");
  await expect(step(timeline, "reversed")).toHaveAttribute("data-state", "failed", { timeout: 30_000 });
  // The service reverses first; the balance follows once the reversal's webhook reaches the
  // product's ledger, which the step shows by the event's name (its hint names it too, in a sentence).
  const reversedDetails = await openStep(timeline, "reversed");
  await expect(reversedDetails.getByText("deposit.reversed", { exact: true })).toBeVisible({ timeout: 30_000 });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  await openTab(scenes, "Refunds");
  await expect(scenes.getByTestId("nets-to")).toHaveText("$0.00");
  await expect(scenes.getByTestId("refund-unavailable")).toContainText("reversed");
  await openTab(scenes, "API");
  await expect(scenes.getByTestId("webhook-event").filter({ hasText: "deposit.reversed" })).toBeVisible();
  await product.getByRole("tab", { name: "Deposit address", exact: true }).click();
  await expect(product.locator(".pp-payments")).toContainText("25 PHA");
  // The customer sees each payment at the rate it was credited at.
  await expect(product.getByTestId("credit").first()).toContainText("25 Test PHA");
  await expect(product.getByTestId("credit").first()).toContainText("Credited at $0.25 / PHA, then reversed");
  await showAll(await openTab(scenes, "Credits"));
  const lines = scenes.getByTestId("ledger-line");
  await expect(lines.filter({ hasText: "deposit.credited" })).toContainText("+$6.25");
  // A reversal takes the whole bonus back with the credit.
  await expect(lines.filter({ hasText: "PHA bonus +10%" })).toContainText("+$0.62");
  const reversals = lines.filter({ hasText: "deposit.reversed" });
  await expect(reversals.and(scenes.locator('[data-kind="credit"]'))).toContainText("−$6.25");
  await expect(reversals.and(scenes.locator('[data-kind="bonus"]'))).toContainText("−$0.62");
  await page.screenshot({ path: testInfo.outputPath("deposit-address.png"), fullPage: true });
  expect(problems).toEqual([]);
});

test("networks and tokens: USDC and USDT at $1.00 without a bonus, and PHA on Base Sepolia with one; the faucets follow", async ({
  page,
}, testInfo) => {
  test.setTimeout(180_000);
  const problems = await watchConsole(page);
  await installWallet(page);
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const helper = page.getByRole("note", { name: "Test tokens" });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  await expectPaymentOptions(product);

  // Test PHA mints from the wallet; gas comes from ethereum.org's list of Sepolia faucets.
  await expect(helper.getByRole("button", { name: "Mint 1,000 test PHA" })).toBeVisible();
  await expect(helper.getByRole("link", { name: /^Sepolia ETH faucets/ })).toHaveAttribute(
    "href",
    "https://ethereum.org/en/developers/docs/networks/#sepolia",
  );

  // Test USDC: Circle's faucet, on the network chosen there; no bonus, at $1.00. The helper stays
  // the same whatever token is chosen.
  const circle = helper.getByRole("link", { name: /^Circle USDC faucet/ });
  await expect(circle).toHaveAttribute("href", "https://faucet.circle.com");
  await expect(circle).toHaveAttribute("title", "On the faucet, pick Sepolia as the network.");
  await product.getByRole("radio", { name: "Test USDC", exact: true }).check({ force: true });
  await expect(helper.getByRole("button", { name: "Mint 1,000 test PHA" })).toBeVisible();
  await expect(circle).toBeVisible();
  await mintUsdc(env("PAYER_ADDRESS"), parseUnits("100", 6));
  await product.getByText("$5", { exact: true }).click();
  const usdcRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  expect((await usdcRequest).postDataJSON()).toEqual({ amount: 500, chain_id: sepolia.id, asset: "usdc" });
  await expect(product.locator(".pp-summary")).toContainText("5 USDC");
  await expect(await openStep(scenes.getByRole("list", { name: "Payment timeline" }), "quote_created")).toContainText(
    "1 USDC = $1.00",
  );
  await expect(product.getByRole("tabpanel", { name: "Exact amount", exact: true }).getByText(/bonus/)).toHaveCount(0);
  await page.setViewportSize({ width: 1440, height: 1100 });
  await page.screenshot({ path: testInfo.outputPath("usdc-quote.png") });
  await product.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$5.00", { timeout: 60_000 });
  await expect(product.getByTestId("balance")).toHaveText("$5.00", { timeout: 10_000 });
  await expect(product.getByTestId("bonus-credited")).toHaveCount(0);
  expect(await tokenBalance(env("PAYER_ADDRESS"), { token: env("USDC_ADDRESS") })).toBe(parseUnits("95", 6));
  await openTab(scenes, "Credits");
  await expect(scenes.getByTestId("payment").first()).toContainText("5 USDC");
  await expect(scenes.getByTestId("payment").first()).toContainText("at $1.00 / USDC");

  // Base Sepolia: its own token list and faucets; test PHA mints there, from the wallet on that
  // network, and a PHA quote there earns the bonus, at staging's rate, formatted.
  await product.getByRole("button", { name: "Start a new top-up" }).click();
  await chooseNetwork(product, "Base Sepolia testnet");
  const tokens = product.getByRole("radiogroup", { name: "Token" });
  await expect(tokens.getByRole("radio")).toHaveCount(2);
  await expect(tokens.getByRole("radio", { name: "Test PHA", exact: true })).toBeChecked();
  await expect(circle).toHaveCount(0);
  // Test USDT mints from the wallet too, through its faucet contract, as Aave's does.
  await expect(helper.getByRole("button", { name: "Mint 1,000 test USDT" })).toBeVisible();
  await expect(helper.getByRole("link", { name: /^Base Sepolia ETH faucets/ })).toHaveAttribute(
    "href",
    "https://docs.base.org/get-started/get-funds#testnet-base-sepolia",
  );
  await helper.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
  await expect(helper).toContainText("Minted:");
  const base = { rpc: env("BASE_ANVIL_URL"), token: env("BASE_TOKEN_ADDRESS") };
  expect(await tokenBalance(env("PAYER_ADDRESS"), base)).toBe(parseEther("1000"));
  await expect(helper.locator("p", { hasText: "Minted:" }).getByRole("link")).toHaveAttribute(
    "href",
    /^https:\/\/sepolia\.basescan\.org\/tx\//,
  );
  const baseRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  expect((await baseRequest).postDataJSON()).toEqual({ amount: 2000, chain_id: baseSepolia.id, asset: "pha" });
  await expect(product.locator(".pp-summary")).toContainText("$20.00");
  await expect(await openStep(scenes.getByRole("list", { name: "Payment timeline" }), "quote_created")).toContainText(
    "1 PHA = $0.06041",
  );
  await expect(product.getByTestId("testnet-notice")).toBeVisible();
  await product.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$20.00", { timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toContainText("+$2.00", { timeout: 10_000 });
  await expect(product.getByTestId("balance")).toHaveText("$27.00", { timeout: 10_000 });
  await page.getByRole("button", { name: "Dark theme" }).click();
  await page.screenshot({ path: testInfo.outputPath("base-bonus-dark.png") });
  await page.getByRole("button", { name: "Dark theme" }).click();

  // $5 in test USDT on Base Sepolia, minted through the faucet, at $1.00 with no bonus. Its
  // `transfer` returns nothing, as Tether's does on Ethereum, and the checkout pays it all the same.
  const usdt = { rpc: env("BASE_ANVIL_URL"), token: env("BASE_USDT_ADDRESS") };
  await helper.getByRole("button", { name: "Mint 1,000 test USDT" }).click();
  await expect.poll(() => tokenBalance(env("PAYER_ADDRESS"), usdt)).toBe(parseUnits("1000", 6));
  await product.getByRole("button", { name: "Start a new top-up" }).click();
  await product.getByRole("radio", { name: "Test USDT", exact: true }).check({ force: true });
  await product.getByText("$5", { exact: true }).click();
  const usdtRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  expect((await usdtRequest).postDataJSON()).toEqual({ amount: 500, chain_id: baseSepolia.id, asset: "usdt" });
  await expect(product.locator(".pp-summary")).toContainText("5 USDT");
  await expect(await openStep(scenes.getByRole("list", { name: "Payment timeline" }), "quote_created")).toContainText(
    "1 USDT = $1.00",
  );
  await product.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$5.00", { timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toHaveCount(0);
  await expect(product.getByTestId("balance")).toHaveText("$32.00", { timeout: 10_000 });
  expect(await tokenBalance(env("PAYER_ADDRESS"), usdt)).toBe(parseUnits("995", 6));
  expect(problems).toEqual([]);
});

test("a quote a wallet cannot cover sends nothing; the mint beside it funds that wallet, not the first", async ({
  page,
}, testInfo) => {
  test.setTimeout(180_000);
  const problems = await watchConsole(page);
  // Two browser wallets: the payer's, announced first, and an empty one, which pays.
  const empty = await emptyAccount(1);
  await installWallet(page, [
    { name: "Test Wallet", account: env("PAYER_ADDRESS") },
    { name: "Empty Wallet", account: empty },
  ]);
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  const helper = page.getByRole("note", { name: "Test tokens" });
  const pay = product.getByRole("button", { name: "Pay with crypto (Empty Wallet)" });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");

  // $20 in test USDC from a wallet holding a millionth less, in USDC's 6 decimals: the checkout
  // sends nothing, and points to Circle's faucet.
  await mintUsdc(empty, 19_999_999n);
  await product.getByRole("radio", { name: "Test USDC", exact: true }).check({ force: true });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  await pay.click();
  await expect(
    product.getByText("Your wallet holds 19.999999 USDC, less than the 20 USDC to pay. Nothing was sent."),
  ).toBeVisible();
  const usdc = product.getByTestId("fund-wallet");
  await expect(usdc).toContainText("Not enough Test USDC in your wallet");
  await expect(usdc.getByRole("link", { name: /^Circle USDC faucet/ })).toHaveAttribute("href", "https://faucet.circle.com");
  await expect(helper.getByRole("button", { name: "Mint 1,000 test PHA" })).toBeVisible();
  expect(await sentCount(empty)).toBe(0);

  // $500 in test PHA at 0.25 USD per PHA: 2,000 PHA, more than the default mint, which grows to
  // cover the quote; paying first is refused, and the mint beside the refusal mints to the wallet
  // that paid, not the first one announced.
  await product.getByRole("button", { name: "Start a new top-up" }).click();
  await product.getByRole("radio", { name: "Test PHA", exact: true }).check({ force: true });
  await product.getByText("Custom", { exact: true }).click();
  await product.getByLabel("Custom amount (USD)").fill("500");
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  await expect(product.locator(".pp-summary")).toContainText("2,000 PHA");
  await expect(helper.getByRole("button", { name: "Mint 2,000 test PHA" })).toBeVisible();
  await pay.click();
  await expect(product.getByText("Your wallet holds 0 PHA, less than the 2,000 PHA to pay. Nothing was sent.")).toBeVisible();
  const pha = product.getByTestId("fund-wallet");
  await expect(pha).toContainText("Not enough Test PHA in your wallet");
  expect(await sentCount(empty)).toBe(0);
  await page.screenshot({ path: testInfo.outputPath("underfunded.png"), fullPage: true });
  const payerHeld = await tokenBalance(env("PAYER_ADDRESS"));
  await pha.getByRole("button", { name: "Mint 2,000 test PHA" }).click();
  await expect(pha).toContainText("Minted:");
  expect(await tokenBalance(empty)).toBe(parseEther("2000"));
  expect(await tokenBalance(env("PAYER_ADDRESS"))).toBe(payerHeld);
  await pay.click();
  await expect(product.getByTestId("payment-credited")).toContainText("$500.00", { timeout: 60_000 });
  await expect(pha).toHaveCount(0);
  expect(await tokenBalance(empty)).toBe(0n);
  expect(problems).toEqual([]);
});

test("the deposit address: a canceled mint mints nothing; the amount typed sizes the mint, and too much sends nothing", async ({
  page,
}) => {
  test.setTimeout(180_000);
  const problems = await watchConsole(page);
  const account = await emptyAccount(2);
  const { forwarded, holdSends } = await installWallet(page, [{ name: "Test Wallet", account }]);
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  const helper = page.getByRole("note", { name: "Test tokens" });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");

  // The wallet cancels the mint (a 0 ETH transfer to itself with its nonce) before it is mined: the
  // page says nothing was minted. No block is mined (neither Anvil's automine nor the stand-in
  // service's block a second) until the cancel replaces the mint.
  const chain = createTestClient({ mode: "anvil", chain: sepolia, transport: http(env("ANVIL_URL")) })
    .extend(walletActions)
    .extend(publicActions);
  const mining = async (state: "pause" | "resume") => {
    expect((await fetch(`${env("SERVICE_URL")}/_test/mining/${state}`, { method: "POST" })).status).toBe(200);
  };
  await mining("pause");
  await chain.setAutomine(false);
  try {
    const nonce = await sentCount(account);
    await helper.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
    // The page has read its pending mint, so it can tell what replaced it.
    await expect
      .poll(() => {
        const read = forwarded.indexOf("eth_getTransactionByHash");
        return read >= 0 && forwarded.indexOf("eth_getTransactionReceipt", read) > read;
      })
      .toBe(true);
    await chain.sendTransaction({
      account,
      to: account,
      value: 0n,
      nonce,
      maxFeePerGas: parseGwei("1000"),
      maxPriorityFeePerGas: parseGwei("500"),
    });
    await chain.mine({ blocks: 1 });
  } finally {
    await chain.setAutomine(true);
    await mining("resume");
  }
  await expect(helper).toContainText("The mint was canceled in the wallet; nothing was minted.", { timeout: 30_000 });
  await expect(helper).not.toContainText("Minted:");
  expect(await tokenBalance(account)).toBe(0n);

  await page.getByRole("tab", { name: "Deposit address" }).click();
  await product.getByRole("button", { name: "Show my deposit address" }).click();
  const form = product.getByRole("form", { name: "Pay to the deposit address from a browser wallet" });
  const amount = form.getByLabel(/^Send from your browser wallet/);
  const send = form.getByRole("button", { name: "Send" });
  const fund = form.getByTestId("fund-wallet");
  const sent = await sentCount(account);
  await send.click();
  await expect(form).toContainText("Your wallet holds 0 PHA, less than the 25 PHA to send. Nothing was sent.");
  await expect(fund.getByRole("button", { name: "Mint 1,000 test PHA" })).toBeVisible();

  // Another amount clears that refusal and its mint; 1,234.5 PHA rounds the mint up to 1,300.
  await amount.fill("1234.5");
  await expect(fund).toHaveCount(0);
  await expect(helper.getByRole("button", { name: "Mint 1,300 test PHA" })).toBeVisible();
  await send.click();
  await expect(form).toContainText("Your wallet holds 0 PHA, less than the 1,234.5 PHA to send. Nothing was sent.");
  expect(await sentCount(account)).toBe(sent);
  await fund.getByRole("button", { name: "Mint 1,300 test PHA" }).click();
  await expect(fund).toContainText("Minted:");
  expect(await tokenBalance(account)).toBe(parseEther("1300"));
  // While the wallet confirms, the amount and Send stay fixed: another amount cannot be sent
  // beside the pending one.
  const release = holdSends();
  await send.click();
  await expect(form.getByRole("button", { name: "Confirm in your wallet…" })).toBeDisabled();
  await expect(amount).toBeDisabled();
  release();
  await expect(form).toContainText("Sent: 0x");
  await expect(amount).toBeEnabled();
  expect(await tokenBalance(account)).toBe(parseEther("65.5"));
  expect(problems).toEqual([]);
});

test("refuses another browser's payments and refunds, and rate-limits quote creation", async ({ browser }) => {
  const first = await browser.newContext();
  const page = await first.newPage();
  await page.goto(env("SITE_URL"));
  await expect(page.getByTestId("balance")).toHaveText("$0.00");
  const created = await page.evaluate(async (api) => {
    const statuses: number[] = [];
    let quote = "";
    for (let i = 0; i < 4; i += 1) {
      const response = await fetch(`${api}/api/quotes`, {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ amount: 500, chain_id: 11155111, asset: "pha" }),
      });
      statuses.push(response.status);
      const body = (await response.json()) as { quote?: string };
      quote ||= body.quote ?? "";
    }
    return { statuses, quote };
  }, env("API_URL"));
  expect(created.statuses).toEqual([200, 200, 200, 429]);
  // The API's origin serves only the API: no page.
  for (const path of ["", "index.html"]) {
    expect((await fetch(`${env("API_URL")}/${path}`)).status).toBe(404);
  }
  // Another origin's page cannot read the API, even with the visitor's cookie.
  const elsewhere = await first.newPage();
  await elsewhere.goto(`${env("SERVICE_URL")}/evidences/quote.json`);
  const refused = await elsewhere.evaluate(async (api) => {
    try {
      await fetch(`${api}/api/account`, { credentials: "include" });
      return "read";
    } catch {
      return "refused";
    }
  }, env("API_URL"));
  expect(refused).toBe("refused");

  const second = await browser.newContext();
  const other = await second.newPage();
  await other.goto(env("SITE_URL"));
  await expect(other.getByTestId("balance")).toHaveText("$0.00");
  const statuses = await other.evaluate(
    async ({ api, quote }) => {
      const get = (path: string) => fetch(`${api}/api/${path}`, { credentials: "include" });
      const post = (path: string, body: unknown) =>
        fetch(`${api}/api/${path}`, {
          method: "POST",
          credentials: "include",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        });
      return [
        (await get(`quotes/${quote}`)).status,
        (await get(`deposits/dep_${"0".repeat(32)}`)).status,
        (await post(`refunds/re_${"0".repeat(32)}/cancel`, {})).status,
        (await post(`refunds/re_${"0".repeat(32)}/mark_paid`, { transaction_hash: `0x${"ab".repeat(32)}` })).status,
      ];
    },
    { api: env("API_URL"), quote: created.quote },
  );
  expect(statuses).toEqual([404, 404, 404, 404]);
  await first.close();
  await second.close();
});


test("prerendered marketing works without JavaScript; comparison chrome stays interactive", async ({ browser, page }) => {
  const staticContext = await browser.newContext({ javaScriptEnabled: false, viewport: { width: 390, height: 844 } });
  try {
    const staticPage = await staticContext.newPage();
    const home = await staticPage.goto(env("SITE_URL"));
    expect(home?.status()).toBe(200);
    await expect(staticPage.getByRole("heading", { level: 1 })).toHaveText("Crypto payments, without a custodian");
    await expect(staticPage.getByRole("heading", { level: 2 })).toHaveCount(5);
    // The answers fold natively, without script.
    const question = staticPage.locator("summary", { hasText: "Which chains and tokens are supported?" });
    const answer = staticPage.getByText("The live demo uses test tokens on Sepolia and Base Sepolia.", { exact: false });
    await expect(answer).toBeHidden();
    await question.click();
    await expect(answer).toBeVisible();
    await expectMetadata(staticPage);
    await expect(staticPage.getByRole("button", { name: "Menu", exact: true })).toHaveCount(0);
    // The theme button cannot know the visitor's theme, so it states none; and the demo, which never
    // arrives without script, reserves no space: only its note shows.
    await expect(staticPage.getByRole("button", { name: "Dark theme" })).not.toHaveAttribute("aria-pressed");
    await expect(staticPage.locator("#demo-root p")).toHaveText("The demo needs JavaScript.");
    expect((await staticPage.locator("#demo-root").boundingBox())?.height).toBeLessThan(48);
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Compare", exact: true })).toBeVisible();
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Demo", exact: true })).toHaveAttribute("href", "/#demo");
    const homeHtml = await home?.text();
    expect(homeHtml).not.toContain('style="');
    const compare = await staticPage.goto(new URL("compare", env("SITE_URL")).href);
    expect(compare?.status()).toBe(200);
    await expect(staticPage.getByRole("heading", { level: 1 })).toHaveText("How Phala Pay compares");
    // Below xl (a phone, a tablet, a small laptop), Phala Pay beside one provider, chosen above the
    // table (natively, without script): two columns, ten dimensions. From xl, the full table, all
    // six vendors in the page's width: nothing scrolls inside it, and no cell overflows.
    const full = staticPage.getByRole("table", { name: /five crypto payment services/ });
    const versus = staticPage.getByRole("table", { name: /provider chosen above/ });
    await expect(full).toBeHidden();
    await expect(versus.getByRole("columnheader", { name: "Stripe stablecoin payments" })).toBeVisible();
    await staticPage.getByRole("group", { name: "Compare Phala Pay with" }).getByText("BTCPay Server", { exact: true }).click();
    await expect(versus.getByRole("columnheader", { name: "BTCPay Server" })).toBeVisible();
    await expect(versus.getByRole("columnheader", { name: "Stripe stablecoin payments" })).toBeHidden();
    await expect(versus.getByRole("rowgroup").filter({ has: staticPage.getByRole("cell") })).toHaveCount(10);
    for (const width of [768, 1024]) {
      await staticPage.setViewportSize({ width, height: 900 });
      await expect(full, `${width}px`).toBeHidden();
      await expect(versus, `${width}px`).toBeVisible();
    }
    for (const width of [1280, 1440]) {
      await staticPage.setViewportSize({ width, height: 900 });
      await expect(full).toBeVisible();
      await expect(versus).toBeHidden();
      const overflow = await full.evaluate((table) => {
        const parent = table.parentElement?.getBoundingClientRect().right ?? 0;
        const cells = [...table.querySelectorAll("th, td")].filter((cell) => cell.scrollWidth > cell.clientWidth + 1);
        return { beyond: Math.max(0, Math.round(table.getBoundingClientRect().right - parent)), cells: cells.length };
      });
      expect(overflow, `the full table at ${width}px`).toEqual({ beyond: 0, cells: 0 });
    }
    await staticPage.setViewportSize({ width: 390, height: 844 });
    await expect(staticPage.getByRole("button", { name: "Menu", exact: true })).toHaveCount(0);
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Compare", exact: true })).toBeVisible();
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Demo", exact: true })).toHaveAttribute("href", "/#demo");
    await expect(staticPage.getByText("Partially stated by the vendor; see source.", { exact: false })).toBeVisible();
    const alias = await page.request.get(new URL("compare.html", env("SITE_URL")).href, { maxRedirects: 0 });
    expect(alias.status()).toBe(307);
    expect(alias.headers()["location"]).toBe("/compare");
  } finally {
    await staticContext.close();
  }
  await page.goto(new URL("compare", env("SITE_URL")).href);
  await page.getByRole("button", { name: "Dark theme" }).click();
  await expect(page.locator("html")).toHaveClass(/dark/);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: "Menu", exact: true }).click();
  await expect(page.getByRole("navigation", { name: "Menu" }).getByRole("link", { name: "Demo" })).toHaveAttribute("href", "/#demo");
  await page.keyboard.press("Escape");
  await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  await expect.poll(async () => (await page.getByRole("banner").boundingBox())?.y).toBe(0);
});


test("loading home islands preserves the original prerendered hero", async ({ page }) => {
  let releaseEntry: (() => void) | undefined;
  const entryReady = new Promise<void>((resolve) => { releaseEntry = resolve; });
  await page.route("**/assets/home-*.js", async (route) => {
    await entryReady;
    await route.continue();
  });
  try {
    await page.goto(env("SITE_URL"), { waitUntil: "commit" });
    const hero = page.locator("#hero-title");
    await expect(hero).toBeVisible();
    const original = await hero.elementHandle();
    const originalHeader = await page.getByRole("banner").elementHandle();
    const originalFooter = await page.getByRole("contentinfo").elementHandle();
    releaseEntry?.();
    await expect(page.getByRole("region", { name: "Customer view" })).toBeVisible();
    expect(await original.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalHeader.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalFooter.evaluate((node) => node.isConnected)).toBe(true);
    await page.getByRole("button", { name: "Dark theme" }).click();
    await expect(page.locator("html")).toHaveClass(/dark/);
    await expect(page.getByRole("region", { name: "Customer view" })).toBeVisible();
  } finally {
    releaseEntry?.();
  }
});


test("forced colors keep the chosen amount, the active tab, and keyboard focus visible", async ({ page }) => {
  await page.emulateMedia({ forcedColors: "active" });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  const style = (locator: Locator, property: "backgroundColor" | "outlineStyle") =>
    locator.evaluate((element, name) => getComputedStyle(element)[name], property);

  // The chosen option and the active tab stand out from their track (which the system paints as
  // its canvas, like the others) in the system's highlight.
  const amounts = product.getByRole("radiogroup", { name: "Amount" });
  const chosen = amounts.getByRole("radio", { name: "$20", exact: true });
  await expect(chosen).toBeChecked();
  expect(await style(chosen, "backgroundColor")).not.toBe(await style(amounts, "backgroundColor"));
  const methods = product.getByRole("tablist", { name: "Payment method" });
  const active = methods.getByRole("tab", { name: "Exact amount" });
  await expect(active).toHaveAttribute("aria-selected", "true");
  expect(await style(active, "backgroundColor")).not.toBe(await style(methods, "backgroundColor"));

  // Keyboard focus on a select and a text field keeps an outline, which the system colours.
  await chosen.focus();
  await page.keyboard.press("Tab");
  const network = product.getByRole("combobox", { name: "Network" });
  await expect(network).toBeFocused();
  expect(await style(network, "outlineStyle")).not.toBe("none");
  await amounts.getByRole("radio", { name: "Custom", exact: true }).click();
  const custom = product.getByLabel("Custom amount (USD)");
  await custom.focus();
  expect(await style(custom, "outlineStyle")).not.toBe("none");
});

test("home and comparison hydrate in either theme without CSP violations or React errors", async ({ browser }) => {
  for (const colorScheme of ["light", "dark"] as const) {
    for (const path of ["", "compare"]) {
      const context = await browser.newContext({ colorScheme });
      try {
        const page = await context.newPage();
        const problems = await watchConsole(page);
        await page.goto(new URL(path, env("SITE_URL")).href);
        await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
        if (path === "") await expect(page.getByRole("region", { name: "Customer view" })).toBeVisible();
        const next = colorScheme === "dark" ? "light" : "dark";
        const theme = page.getByRole("button", { name: "Dark theme" });
        await expect(theme).toHaveAttribute("aria-pressed", String(colorScheme === "dark"));
        await theme.click();
        await expect(page.locator("html")).toHaveClass(next === "dark" ? /dark/ : /^$/);
        await expect(theme).toHaveAttribute("aria-pressed", String(next === "dark"));
        // Nothing scrolls sideways at the narrowest phone width.
        await page.setViewportSize({ width: 320, height: 640 });
        expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(320);
        await page.setViewportSize({ width: 390, height: 844 });
        const toggle = page.getByRole("button", { name: "Menu", exact: true });
        await expect(toggle).toHaveAttribute("aria-expanded", "false");
        await toggle.click();
        await expect(toggle).toHaveAttribute("aria-expanded", "true");
        const menu = page.getByRole("navigation", { name: "Menu" });
        await expect(menu).toBeVisible();
        const panelId = await toggle.getAttribute("aria-controls");
        expect(await menu.evaluate((node) => node.parentElement?.id)).toBe(panelId);
        await expect(page.locator("body")).not.toHaveAttribute("data-scroll-locked");
        await menu.getByRole("link", { name: "Demo", exact: true }).focus();
        await page.keyboard.press("Escape");
        await expect(menu).toBeHidden();
        await expect(toggle).toHaveAttribute("aria-expanded", "false");
        await expect(toggle).toBeFocused();
        // A press outside the header closes it too.
        await toggle.click();
        await page.getByRole("heading", { level: 2 }).first().click();
        await expect(menu).toBeHidden();
        await toggle.click();
        await menu.getByRole("link", { name: path === "" ? "Demo" : "Compare", exact: true }).click();
        await expect(toggle).toHaveAttribute("aria-expanded", "false");
        await expect(menu).toBeHidden();
        expect(problems).toEqual([]);
      } finally {
        await context.close();
      }
    }
  }
});

test("sweeps from the last good data show their balances and age, without sweep actions; an unavailable group says so", async ({ page }) => {
  const problems = await watchConsole(page);
  const asOf = Math.floor(Date.now() / 1000) - 12;
  // The product serves its last good data when a refresh fails: stale, with no flush to sign.
  await page.route(`${env("API_URL")}/api/sweeps`, async (route) => {
    const response = await route.fetch();
    const body = (await response.json()) as { groups?: Record<string, unknown>[] };
    // Only a successful listing is rewritten (one before the demo account exists is refused).
    if (!response.ok() || body.groups === undefined) return route.fulfill({ response });
    const [first, second, ...rest] = body.groups;
    expect(first).toBeDefined();
    expect(second).toBeDefined();
    const groups = [
      { ...first, unswept_atomic: "80000000000000000000", final_unswept_atomic: "80000000000000000000", sweepable_forwarders: 1, stale: true, as_of: asOf, flush: [], safe_batch: null },
      { ...second, unavailable: true, stale: false },
      ...rest,
    ];
    await route.fulfill({ response, json: { ...body, groups } });
  });
  await page.goto(env("SITE_URL"));
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const panel = await openTab(scenes, "Sweeps");
  const [stale, unavailable] = [panel.getByTestId("sweep-group").nth(0), panel.getByTestId("sweep-group").nth(1)];
  // The listing is read once the demo account exists.
  await expect(stale).toHaveAttribute("data-stale", "true", { timeout: 15_000 });
  await expect(stale.getByTestId("unswept")).toContainText("80");
  await expect(stale.getByTestId("sweep-stale")).toHaveText(/^Updated \d+\u00a0s ago; refreshing$/);
  await expect(stale.getByRole("button", { name: "Sweep from wallet" })).toHaveCount(0);
  await expect(stale.getByRole("button", { name: "Safe batch" })).toHaveCount(0);
  await expect(unavailable.getByTestId("sweep-unavailable")).toHaveText("Temporarily unavailable; retrying");
  await expect(unavailable.getByTestId("unswept")).toHaveCount(0);
  // The treasury is shown in full, grouped in fours, never shortened.
  const treasury = panel.getByTestId("treasury");
  await expect(treasury.locator("[data-value]")).toHaveAttribute("data-value", new RegExp(`^${env("TREASURY")}$`, "i"));
  expect((await treasury.innerText()).replace(/\s/g, "").toLowerCase()).toContain(env("TREASURY").toLowerCase());
  await expectAccessible(page, "sweeps from the last good data");
  expect(problems).toEqual([]);
});

test("every page fits every width in either theme, with no serious accessibility violation", async ({ browser }) => {
  // Each page is checked for sideways scrolling at every width, and with axe at the widths its
  // layout changes the content: every width for the marketing pages, a desktop and a phone for the
  // long docs, and the desktop for the API reference, whose markup is the same at every width (axe
  // takes most of a minute over its 15,000 elements).
  test.setTimeout(600_000);
  const all = [1440, 1280, 1024, 768, 390];
  const pages: [string, number[]][] = [
    ["", all], ["compare", all], ["no-such-page/deeper", all], ["docs", all], ["docs/sdk/react", all],
    ["docs/integration", [1440, 390]], ["reference", [1440]],
  ];
  for (const colorScheme of ["light", "dark"] as const) {
    const context = await browser.newContext({ colorScheme });
    try {
      const page = await context.newPage();
      for (const [path, axeWidths] of pages) {
        const response = await page.goto(new URL(path, env("SITE_URL")).href);
        expect(response?.status(), path).toBe(path.startsWith("no-such-page") ? 404 : 200);
        await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
        if (path === "") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
        for (const width of all) {
          await page.setViewportSize({ width, height: 900 });
          const [scrollWidth, clientWidth] = await page.evaluate(() => [document.documentElement.scrollWidth, document.documentElement.clientWidth]);
          expect(scrollWidth, `/${path} at ${width}px, ${colorScheme}`).toBe(clientWidth);
          if (!axeWidths.includes(width)) continue;
          const { violations } = await new AxeBuilder({ page }).analyze();
          const serious = violations
            .filter((violation) => violation.impact === "serious" || violation.impact === "critical")
            .map((violation) => `${violation.id}: ${violation.nodes.map((node) => node.target.join(" ")).join(", ")}`);
          expect(serious, `/${path} at ${width}px, ${colorScheme}`).toEqual([]);
        }
      }
    } finally {
      await context.close();
    }
  }
});

/**
 * Nothing in the demo is cut off sideways: no tab panel, and nothing that hides its overflow,
 * holds content wider than itself (scrolling regions, which show theirs, are exempt).
 */
const DEMO_TABS = ["Credits", "Refunds", "Sweeps", "API", "Trust"];

async function expectNothingClipped(page: Page, state: string): Promise<void> {
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  for (const tab of DEMO_TABS) {
    await openTab(scenes, tab);
    const clipped = await page.locator("#demo-root").evaluate((root) => [...root.querySelectorAll<HTMLElement>("*")]
      .filter((element) => {
        const style = getComputedStyle(element);
        const hides = style.overflowX === "hidden" || style.overflowX === "clip" || element.getAttribute("role") === "tabpanel";
        // Visually hidden text (a 1px box for screen readers) is clipped on purpose.
        return hides && element.clientWidth > 1 && element.checkVisibility() && element.scrollWidth > element.clientWidth + 1;
      })
      .map((element) => `${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")} (${element.scrollWidth} > ${element.clientWidth})`));
    expect(clipped, `${state}, ${tab} tab`).toEqual([]);
    // Nothing scrolls inside the demo: every element shows its overflow, but for code blocks, which
    // may scroll sideways on a phone and never scroll down. Native controls, images, and visually
    // hidden text keep their own.
    const scrolling = await page.locator("#demo-root").evaluate((root) => [...root.querySelectorAll<HTMLElement>("*")]
      .filter((element) => {
        if (element.matches("input, textarea, select, img, svg, svg *") || element.clientWidth <= 1) return false;
        const style = getComputedStyle(element);
        if (element.tagName === "PRE") return element.scrollHeight > element.clientHeight + 1;
        return style.overflowX !== "visible" || style.overflowY !== "visible";
      })
      .map((element) => `${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")}`));
    expect(scrolling, `${state}, ${tab} tab: inner scrolling`).toEqual([]);
  }
  await openTab(scenes, "Credits");
}

test("every tab of the demo fits one screen at 1440×900 and 1280×800, in each state, and clips nothing", async ({ page }) => {
  test.setTimeout(420_000);
  await installWallet(page);
  const product = page.getByRole("region", { name: "Customer view" });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  // The section, scrolled to as the nav's Demo link lands, is no taller than the screen under the
  // 64px header, whichever tab is open, before anything in it is expanded.
  const fits = async (state: string) => {
    for (const [width, height] of [[1440, 900], [1280, 800]] as const) {
      await page.setViewportSize({ width, height });
      for (const tab of DEMO_TABS) {
        await openTab(scenes, tab);
        const demo = await page.locator("#demo").evaluate((section) => section.getBoundingClientRect().height);
        expect(demo, `${state}, ${tab} tab, at ${width}×${height}`).toBeLessThanOrEqual(height - 64);
        // Its text at least 14px too, in every state and on every tab.
        expect(await smallText(page, "#demo"), `${state}, ${tab} tab: text under 14px`).toEqual([]);
      }
      await expectNothingClipped(page, `${state} at ${width}×${height}`);
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await expectNothingClipped(page, `${state} at 390px`);
    await page.setViewportSize({ width: 1440, height: 900 });
  };
  await page.goto(env("SITE_URL"));
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  await fits("idle");
  const testTokens = page.getByRole("note", { name: "Test tokens" });
  await testTokens.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
  await expect(testTokens).toContainText("Minted:");
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  await expect(product.locator(".pp-summary")).toContainText("80 PHA");
  await fits("paying");
  await product.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(product.getByTestId("payment-credited")).toBeVisible({ timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toBeVisible({ timeout: 10_000 });
  await fits("credited");
  // After the merchant's actions: the payment swept, and a refund requested (its row closed).
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  await expectComplete(timeline, ["final"]);
  // The merchant is every visitor's: other payments may wait in its forwarders too, and the sweeps
  // view is the product's cached one, which can predate this payment. Swept once the view shows the
  // service's final unswept balance, which counts this payment now that it is final: that view's
  // flush names this forwarder, with any others.
  const sweepsPanel = await openTab(scenes, "Sweeps");
  const sweeps = sweepsPanel.getByRole("region", { name: "PHA on Sepolia testnet" });
  // The stand-in service takes any key of a restricted key's form, as the product sends.
  const response = await fetch(`${env("SERVICE_URL")}/v1/balance`, { headers: { authorization: `Bearer ppay_rk_test_${"A".repeat(43)}000000` } });
  expect(response.ok, "the service's balance").toBe(true);
  const balance = (await response.json()) as {
    unswept: { chain_id: number; token: string; final_amount_atomic: string }[];
  };
  const pha = balance.unswept.find(({ chain_id, token }) => chain_id === sepolia.id && token.toLowerCase() === env("TOKEN_ADDRESS").toLowerCase());
  expect(BigInt(pha?.final_amount_atomic ?? "0")).toBeGreaterThanOrEqual(parseEther("80"));
  const sweepable = tokens(pha?.final_amount_atomic ?? "0", "PHA").replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  await expect(sweeps.getByTestId("unswept")).toContainText(new RegExp(`^${sweepable} in `), { timeout: 30_000 });
  await sweeps.getByRole("button", { name: "Sweep from wallet" }).click();
  await expectComplete(timeline, ["swept"]);
  await expect(sweepsPanel.getByRole("button", { name: /^Show finalized sweeps/ })).toBeVisible({ timeout: 30_000 });
  await declareRefund(scenes, "20", false);
  // Measured once the request's own webhook has arrived, the API tab's tallest case.
  await openTab(scenes, "API");
  await expect(scenes.getByTestId("webhook-event").filter({ hasText: "refund.created" })).toBeVisible({ timeout: 30_000 });
  await fits("after a sweep and a refund request");
});

test("the demo arrives without shifting what is in view, from the top and at /#demo", async ({ page }) => {
  // Cumulative layout shift, as the browser reports it, while the page loads and the demo renders:
  // under 0.05 at a desktop and a phone ("good" is under 0.1), opened at the top and at the demo.
  // The demo's code arrives a second late, as on a slow network, after the page has painted.
  await page.route("**/assets/Demo-*.js", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1000));
    await route.continue();
  });
  for (const [width, height] of [[1440, 900], [390, 844]] as const) {
    for (const path of ["", "#demo"]) {
      await page.setViewportSize({ width, height });
      await page.addInitScript(() => {
        window.layoutShifts = [];
        new PerformanceObserver((list) => {
          for (const entry of list.getEntries()) {
            if ("value" in entry && typeof entry.value === "number" && "hadRecentInput" in entry && entry.hadRecentInput === false) {
              window.layoutShifts.push(entry.value);
            }
          }
        }).observe({ type: "layout-shift", buffered: true });
      });
      await page.goto(new URL(path, env("SITE_URL")).href);
      await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
      await page.waitForTimeout(500);
      const shift = await page.evaluate(() => window.layoutShifts.reduce((sum, value) => sum + value, 0));
      expect(shift, `/${path} at ${width}px`).toBeLessThan(0.05);
    }
  }
});

test("the hero's code fits its window at every width, a phone's too: no line is cut", async ({ page }) => {
  await page.goto(env("SITE_URL"));
  for (const width of [1440, 1280, 1024, 768, 390]) {
    await page.setViewportSize({ width, height: 900 });
    // Both snippets, the shown and the other (laid out in the same cell).
    const wide = await page.locator("#hero-code pre").evaluateAll((blocks) =>
      blocks.filter((block) => block.scrollWidth > block.clientWidth).map((block) => block.getAttribute("aria-label")));
    expect(wide, `${width}px`).toEqual([]);
  }
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(390);
});

/**
 * Every two-column part of the page (src/layout.ts marks its columns), measured by what it shows:
 * each right column's left edge, and in each part where the left and right columns' first lines of
 * text sit (their baselines), or, where the part centres its columns on each other
 * (`data-align="center"`: the hero, the close), the columns' middles.
 */
async function columnLayout(page: Page) {
  return page.evaluate(() => {
    // The baseline of a column's first shown line: an empty inline-block's bottom sits on it.
    const baseline = (column: Element): number | null => {
      const walker = document.createTreeWalker(column, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
        const parent = node.parentElement;
        if ((node.textContent ?? "").trim() === "" || parent === null || parent.closest(".sr-only") !== null || !parent.checkVisibility()) continue;
        const probe = document.createElement("span");
        probe.style.display = "inline-block";
        parent.insertBefore(probe, node);
        const y = probe.getBoundingClientRect().bottom;
        probe.remove();
        return y;
      }
      return null;
    };
    return [...document.querySelectorAll('[data-column="right"]')].filter((right) => right.checkVisibility()).map((right) => {
      const grid = right.parentElement;
      const left = [...(grid?.children ?? [])].find((child) => child.getAttribute("data-column") === "left");
      const name = right.closest("[aria-labelledby]")?.getAttribute("aria-labelledby") ?? right.closest("footer, main")?.tagName.toLowerCase() ?? "?";
      const centred = grid?.getAttribute("data-align") === "center";
      const layout = grid?.getAttribute("data-layout") ?? "split";
      const middle = (element: Element) => { const box = element.getBoundingClientRect(); return box.top + box.height / 2; };
      return {
        name,
        layout,
        x: right.getBoundingClientRect().left,
        how: centred ? "middles" : "baselines",
        left: left === undefined ? null : centred ? middle(left) : baseline(left),
        right: centred ? middle(right) : baseline(right),
      };
    });
  });
}

test("one layout grid: every right column starts on one line, and each part's columns line up", async ({ page }) => {
  // On the home page: the hero, each section's header, the demo's cards, the spec list, the FAQ,
  // the close, and the footer; on /compare, its header, its sources, and the footer; on the docs
  // and the reference, the navigation beside the page, each operation's and object's fields beside
  // their example, and the footer. Within a page, every right column of one layout (the 6/6 split,
  // the docs' sidebar layout, the reference's parts) starts on one line; the sidebar layout's page
  // starts on one line across the docs and the reference.
  const pages = [["", 9], ["compare", 3], ["docs", 2], ["docs/integration", 2], ["docs/configuration", 2], ["docs/sdk/react", 2], ["reference", 40]] as const;
  for (const [width, height] of [[1440, 900], [1280, 800]] as const) {
    const sidebar = new Set<number>();
    for (const [path, least] of pages) {
      await page.setViewportSize({ width, height });
      await page.goto(new URL(path, env("SITE_URL")).href);
      if (path === "") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
      const parts = await columnLayout(page);
      const where = `/${path} at ${width}px`;
      expect(parts.length, `${where}: two-column parts`).toBeGreaterThanOrEqual(least);
      for (const layout of new Set(parts.map((part) => part.layout))) {
        const group = parts.filter((part) => part.layout === layout);
        const line = group[0]?.x ?? 0;
        expect(group.filter(({ x }) => Math.abs(x - line) > 1).map(({ name, x }) => `${name} starts at ${x.toFixed(1)}, not ${line.toFixed(1)}`), `${where}, ${layout}`).toEqual([]);
        if (layout === "sidebar") sidebar.add(Math.round(line));
      }
      // A right column with no left one beside it (the FAQ's questions, under its header) is
      // measured by its left edge only.
      expect(parts.filter(({ left, right }) => left !== null && (right === null || Math.abs(left - right) > 1))
        .map(({ name, how, left, right }) => `${name}: ${how} at ${left?.toFixed(1) ?? "none"} and ${right?.toFixed(1) ?? "none"}`), where).toEqual([]);
    }
    expect([...sidebar], `the docs' and the reference's page column at ${width}px`).toHaveLength(1);
  }
});


/**
 * The text in `scope` smaller than 14px, save what may be: code and ids (monospace), footnote
 * markers, text not shown, and uppercase, tracked labels.
 */
async function smallText(page: Page, scope = "body"): Promise<string[]> {
  return page.locator(scope).first().evaluate((root) => {
    const found: string[] = [];
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
      const text = node.textContent?.trim() ?? "";
      const element = node.parentElement;
      if (text === "" || element === null || element.closest("pre, code, sup, .sr-only") !== null || !element.checkVisibility()) continue;
      const style = getComputedStyle(element);
      if (style.fontFamily.includes("Mono")) continue;
      const label = style.textTransform === "uppercase" && style.letterSpacing !== "normal";
      if (parseFloat(style.fontSize) < 14 && !label) found.push(`${element.tagName.toLowerCase()} "${text.slice(0, 40)}" ${style.fontSize}`);
    }
    return found;
  });
}

test("body text is at least 14px; only uppercase, tracked labels are smaller", async ({ page }) => {
  test.setTimeout(240_000);
  for (const path of ["", "compare", "docs", "docs/integration", "docs/configuration", "docs/sdk/react", "reference"]) {
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(new URL(path, env("SITE_URL")).href);
      if (path === "") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
      expect(await smallText(page), `/${path} at ${width}px`).toEqual([]);
    }
  }
});

/**
 * The owner's rule for the docs and the reference: the left navigation may stick and scroll on its
 * own; nothing else scrolls inside the page, save a code block sideways.
 */
async function expectOnlyNavScrolls(page: Page, label: string): Promise<void> {
  const scrollers = await page.evaluate(() => {
    // An axis scrolls when the element lets it (auto or scroll; CSS computes the other axis of
    // `overflow-y: auto` as auto too) and its content is larger than its box on it.
    const scrolls = (value: string) => value === "auto" || value === "scroll";
    const found: string[] = [];
    for (const element of document.body.querySelectorAll("*")) {
      const style = getComputedStyle(element);
      const x = scrolls(style.overflowX) && element.scrollWidth > element.clientWidth + 1;
      const y = scrolls(style.overflowY) && element.scrollHeight > element.clientHeight + 1;
      if (!x && !y) continue;
      const navigation = element.closest("aside") !== null && element.querySelector("nav") !== null;
      if (navigation && !x) continue;
      if (element.tagName === "PRE" && !y) continue;
      found.push(`${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")} (${style.overflowX} ${style.overflowY})`);
    }
    return found;
  });
  expect(scrollers, `${label}: elements that scroll inside the page`).toEqual([]);
}

/**
 * A diagram's drawing at its natural size (1:1, as the docs' column shows it on a desktop): what
 * overlaps or overflows. No two labels overlap; no sequence number's disc (the marker at its
 * message's start) touches a label; every note, actor, and node contains its text.
 */
async function diagramProblems(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const svg = document.querySelector("svg");
    if (svg === null) return ["no svg"];
    svg.style.maxWidth = "none";
    svg.setAttribute("width", String(svg.viewBox.baseVal.width));
    svg.setAttribute("height", String(svg.viewBox.baseVal.height));
    const box = (element: Element) => element.getBoundingClientRect();
    const meet = (a: { left: number; right: number; top: number; bottom: number }, b: DOMRect) =>
      a.left < b.right - 0.5 && b.left < a.right - 0.5 && a.top < b.bottom - 0.5 && b.top < a.bottom - 0.5;
    const outside = (inner: DOMRect, outer: DOMRect) =>
      inner.left < outer.left - 0.5 || inner.right > outer.right + 0.5 || inner.top < outer.top - 0.5 || inner.bottom > outer.bottom + 0.5;
    const label = (element: SVGElement) => `"${element.textContent.trim().slice(0, 32)}"`;
    const texts = [...svg.querySelectorAll("text")].filter((text) => text.textContent.trim() !== "" && box(text).width > 0);
    const problems: string[] = [];
    texts.forEach((a, index) => {
      for (const b of texts.slice(index + 1)) {
        if (!a.contains(b) && !b.contains(a) && meet(box(a), box(b))) problems.push(`${label(a)} overlaps ${label(b)}`);
      }
    });
    const ctm = svg.getScreenCTM();
    for (const line of svg.querySelectorAll('line[marker-start*="sequencenumber"]')) {
      const id = /#([^)]+)\)/.exec(line.getAttribute("marker-start") ?? "")?.[1] ?? "";
      const radius = Number(document.getElementById(id)?.querySelector("circle")?.getAttribute("r") ?? 0) *
        (parseFloat(getComputedStyle(line).strokeWidth) || 1) * (ctm === null ? 1 : ctm.a);
      const centre = new DOMPoint(Number(line.getAttribute("x1")), Number(line.getAttribute("y1"))).matrixTransform(ctm === null ? undefined : ctm);
      const disc = { left: centre.x - radius, right: centre.x + radius, top: centre.y - radius, bottom: centre.y + radius };
      for (const text of texts) {
        if (!text.classList.contains("sequenceNumber") && meet(disc, box(text))) problems.push(`a number's marker touches ${label(text)}`);
      }
    }
    for (const shape of svg.querySelectorAll("rect.note, rect.actor")) {
      for (const text of shape.parentElement?.querySelectorAll("text") ?? []) {
        if (box(text).width > 0 && outside(box(text), box(shape))) problems.push(`${label(text)} overflows its ${shape.getAttribute("class") ?? "box"}`);
      }
    }
    // No edge crosses a subgraph's title: points every 2px along each edge, against each title.
    for (const title of svg.querySelectorAll<SVGGraphicsElement>("g.cluster-label")) {
      const area = box(title);
      if (area.width === 0) continue;
      for (const edge of svg.querySelectorAll<SVGPathElement>("path.flowchart-link")) {
        const matrix = edge.getScreenCTM();
        for (let at = 0; at <= edge.getTotalLength(); at += 2) {
          const point = edge.getPointAtLength(at).matrixTransform(matrix ?? undefined);
          if (point.x > area.left && point.x < area.right && point.y > area.top && point.y < area.bottom) {
            problems.push(`an edge crosses the title ${label(title)}`);
            break;
          }
        }
      }
    }
    for (const node of svg.querySelectorAll("g.node")) {
      const shape = node.querySelector("rect, path, polygon, circle, ellipse");
      const text = node.querySelector<SVGElement>(".label, text");
      if (shape !== null && text !== null && outside(box(text), box(shape))) problems.push(`${label(text)} overflows its node`);
    }
    return problems;
  });
}

test("the docs' diagrams are legible: at least 12px text on a desktop, nothing overlapping", async ({ page }) => {
  const names = readdirSync(new URL("../public/diagrams/", import.meta.url)).filter((name) => name.endsWith(".svg"));
  expect(names.length).toBeGreaterThan(0);
  for (const name of names) {
    // The served SVG, drawn as an <img> draws it: on a document of its own, without the page's
    // CSP (which, opened directly, refuses the SVG's own <style>; an image is not subject to it).
    const served = await page.request.get(new URL(`diagrams/${name}`, env("SITE_URL")).href);
    expect(served.status(), name).toBe(200);
    const body = await served.body();
    await page.route("https://diagram.test/*", (route) => route.fulfill({ body, contentType: "image/svg+xml" }));
    await page.goto(`https://diagram.test/${name}`);
    await page.unroute("https://diagram.test/*");
    await page.evaluate(() => document.fonts.ready);
    expect(await diagramProblems(page), name).toEqual([]);
  }
  // Drawn with 14px text, a diagram shown at its column's width keeps at least 12px of it.
  for (const path of ["docs/overview", "docs/integration"]) {
    for (const [width, height] of [[1440, 900], [1280, 800]] as const) {
      await page.setViewportSize({ width, height });
      await page.goto(new URL(path, env("SITE_URL")).href);
      const image = page.locator(".docs-prose figure.diagram img:visible");
      await image.scrollIntoViewIfNeeded();
      const [shown, natural] = await image.evaluate(async (element: HTMLImageElement) => {
        await element.decode();
        return [element.getBoundingClientRect().width, element.naturalWidth];
      });
      expect((shown / natural) * 14, `${path} at ${width}px: the diagram's text size`).toBeGreaterThanOrEqual(12);
    }
  }
});

test("the docs and the API reference are rendered from the repository, linked within the site, and work without script", async ({ browser, page }) => {
  const problems = await watchConsole(page);
  // Every doc, prerendered from its markdown: its own title and canonical URL, in the sitemap.
  const sitemap = await (await page.request.get(new URL("sitemap.xml", env("SITE_URL")).href)).text();
  const docs = [
    ["docs", "Phala Pay documentation"],
    ["docs/overview", "How Phala Pay works"],
    ["docs/integration", "Integration guide"],
    ["docs/self-hosting", "Self-hosting Phala Pay"],
    ["docs/sdk/react", "@phala/pay-react"],
  ] as const;
  for (const [path, title] of docs) {
    const response = await page.goto(new URL(path, env("SITE_URL")).href);
    expect(response?.status(), path).toBe(200);
    expect(await response?.text(), path).not.toContain('style="');
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(title);
    await expect(page.locator('link[rel="canonical"]')).toHaveAttribute("href", `https://pay.phala.com/${path}`);
    expect(sitemap).toContain(`<loc>https://pay.phala.com/${path}</loc>`);
  }
  // Only the left navigation scrolls on its own, on every doc and the reference, wide and narrow.
  for (const path of [...DOCS.map(({ slug }) => docPath(slug)), "/reference"]) {
    await page.goto(new URL(path, env("SITE_URL")).href);
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 });
      await expectOnlyNavScrolls(page, `${path} at ${width}px`);
    }
  }
  await page.setViewportSize({ width: 1440, height: 900 });
  // Links between docs stay on the site with GitHub's anchors; other repository files open on
  // GitHub; the old API reference's address is /reference.
  await page.goto(new URL("docs/integration", env("SITE_URL")).href);
  const prose = page.locator(".docs-prose");
  await expect(prose.locator('a[href^="/docs/self-hosting"]').first()).toBeVisible();
  await expect(prose.locator('a[href*=".md"]:not([href^="https://github.com/"])')).toHaveCount(0);
  await expect(prose.locator('a[href^="https://phala-network.github.io"]')).toHaveCount(0);
  // A code block is highlighted at build time, and its copy button works once the page hydrates.
  const block = prose.locator(".code-block").first();
  await expect(block.locator("pre .line").first()).toBeVisible();
  await expect(block.getByRole("button", { name: "Copy" })).toBeVisible();
  // The reference: Redoc's anchors (each error's doc_url, each operation) resolve.
  await page.goto(new URL("reference#section/Errors/deposit_not_final", env("SITE_URL")).href);
  await expect(page.locator('[id="section/Errors/deposit_not_final"]')).toBeInViewport();
  await expect(page.locator('[id="tag/quotes/operation/create_quote"]')).toContainText("POST");
  await expect(page.locator('[id="schema/Quote"]')).toContainText("client_secret");
  expect(problems).toEqual([]);
  // A Mermaid diagram is its committed SVG for the page's theme (`npm run diagrams`), described by
  // what it draws, and opens at full size.
  await page.goto(new URL("docs/overview", env("SITE_URL")).href);
  const diagram = page.locator(".docs-prose figure.diagram img:visible");
  await expect(diagram).toHaveCount(1);
  await expect(diagram).toHaveAttribute("src", /^\/diagrams\/docs-overview-1-(light|dark)\.svg$/);
  await expect(diagram).toHaveAttribute("alt", /^Flowchart\. .*Payer to Deposit address: pays/);
  expect(await diagram.evaluate((image: HTMLImageElement) => image.decode().then(() => image.naturalWidth))).toBeGreaterThan(0);
  await expect(page.locator(".docs-prose figure.diagram figcaption a:visible")).toHaveText("Open the diagram full size");
  // The header marks the part of the site a page is in.
  for (const [path, current] of [["docs/overview", "Docs"], ["reference", "API reference"], ["compare", "Compare"], ["", null]] as const) {
    await page.goto(new URL(path, env("SITE_URL")).href);
    const marked = page.getByRole("navigation", { name: "Site" }).locator('[aria-current="page"]');
    if (current === null) await expect(marked).toHaveCount(0);
    else await expect(marked).toHaveText(current);
  }
  // Without script, the docs' menu still opens: it is a native disclosure.
  const staticContext = await browser.newContext({ javaScriptEnabled: false, viewport: { width: 390, height: 844 } });
  try {
    const staticPage = await staticContext.newPage();
    await staticPage.goto(new URL("docs/overview", env("SITE_URL")).href);
    await staticPage.getByText("Documentation menu").click();
    await expect(staticPage.getByRole("navigation", { name: "Documentation" }).getByRole("link", { name: "Integration guide" })).toBeVisible();
    // The copy button, which needs script, stays hidden.
    await expect(staticPage.getByRole("button", { name: "Copy" })).toHaveCount(0);
  } finally {
    await staticContext.close();
  }
});

test("every link in the site resolves: its pages, their anchors, and the repository files it opens on GitHub", async ({ page }) => {
  test.setTimeout(300_000);
  const origin = new URL(env("SITE_URL")).origin;
  const repo = "https://github.com/Phala-Network/phala-pay/";
  const root = new URL("../../../../", import.meta.url);
  const pages = ["/", "/compare", "/reference", ...DOCS.map(({ slug }) => docPath(slug))];
  // Every page's links, as written (the prerendered page: the docs and the reference need no script).
  const links = new Map<string, string>();
  for (const path of pages) {
    await page.goto(new URL(path, origin).href);
    for (const href of await page.locator("a[href]").evaluateAll((anchors) => anchors.map((anchor) => anchor.getAttribute("href") ?? ""))) {
      links.set(new URL(href, new URL(path, origin)).href, path);
    }
  }
  const ids = new Map<string, Set<string>>();
  const idsOf = async (path: string) => {
    const known = ids.get(path);
    if (known !== undefined) return known;
    const response = await page.request.get(new URL(path, origin).href);
    expect(response.status(), path).toBe(200);
    const found = new Set([...(await response.text()).matchAll(/\sid="([^"]+)"/g)].map(([, id = ""]) => id.replaceAll("&amp;", "&")));
    ids.set(path, found);
    return found;
  };
  const broken: string[] = [];
  for (const [url, from] of links) {
    const target = new URL(url);
    if (target.origin === origin) {
      const found = await idsOf(target.pathname);
      const id = decodeURIComponent(target.hash.slice(1));
      if (id !== "" && !found.has(id)) broken.push(`${from}: ${target.pathname}${target.hash} (no such anchor)`);
    } else if (url.startsWith(repo)) {
      // A repository file the docs link to: it exists in this checkout (its anchors are checked
      // in the markdown, by CI's lychee).
      const file = /^(?:blob|tree)\/main\/([^#?]+)/.exec(url.slice(repo.length))?.[1];
      if (file !== undefined && !existsSync(new URL(decodeURIComponent(file), root))) broken.push(`${from}: ${url} (no such file)`);
    }
  }
  expect(broken).toEqual([]);
});
