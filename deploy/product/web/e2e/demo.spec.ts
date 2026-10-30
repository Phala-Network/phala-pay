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
import type { CreatedQuote, Timeline } from "../src/api.js";
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

// The demo's submit: "Pay $20.00 in USDC".
const PAY = /^Pay \$[\d,]+\.\d\d in [A-Z]+$/;

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
async function mintUsdc(to: string, amount: bigint, onBase = false): Promise<void> {
  const chain = createTestClient({
    mode: "anvil",
    chain: onBase ? baseSepolia : sepolia,
    transport: http(env(onBase ? "BASE_ANVIL_URL" : "ANVIL_URL")),
  })
    .extend(walletActions)
    .extend(publicActions);
  const payer = getAddress(env("PAYER_ADDRESS"));
  await chain.waitForTransactionReceipt({
    hash: await chain.sendTransaction({
      account: payer,
      to: getAddress(env(onBase ? "BASE_USDC_ADDRESS" : "USDC_ADDRESS")),
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
 * The token, then the network, chosen by default: test PHA, which earns the demo merchant's bonus,
 * on Sepolia. Every token is offered, and the networks that take the chosen one: PHA and USDT only
 * Sepolia, USDC both. Test USDC is a stablecoin.
 */
async function expectPaymentOptions(product: Locator) {
  await expect(product.getByRole("group", { name: "Pay with" })).toBeVisible();
  await expectChosen(product, "PHA · Sepolia");
  const price = product.getByTestId("token-price").filter({ visible: true });
  const bonus = product.getByTestId("token-bonus").filter({ visible: true });
  // A spot token is at the market rate, which a quote locks; a stablecoin is $1.00.
  await expect(price).toHaveText("Market rate");
  await expect(bonus).toHaveText("+10% bonus");
  await expectOffered(product, "Token", ["PHA", "USDC", "USDT"]);
  await expectOffered(product, "Network", ["Sepolia"]);
  await chooseOption(product, "USDC · Sepolia");
  await expectOffered(product, "Network", ["Sepolia", "Base Sepolia"]);
  await expect(price).toHaveText("$1.00 per USDC");
  await expect(bonus).toHaveCount(0);
  await chooseOption(product, "PHA · Sepolia");
}

/** The items `field` ("Token" or "Network") offers, opened and closed again. */
async function expectOffered(product: Locator, field: string, names: string[]) {
  const combobox = product.getByRole("combobox", { name: field, exact: true });
  await combobox.click();
  await expect(product.getByRole("listbox", { name: field, exact: true }).getByRole("option")).toHaveText(names);
  await combobox.press("Escape");
  await expect(combobox).toHaveAttribute("aria-expanded", "false");
}

/** The token and network chosen, as `USDC · Sepolia`. */
async function expectChosen(product: Locator, name: string) {
  const [token = "", network = ""] = name.split(" · ");
  await expect(product.getByRole("combobox", { name: "Token", exact: true }).locator(".pp-select__value")).toHaveText(token);
  await expect(product.getByRole("combobox", { name: "Network", exact: true }).locator(".pp-select__value")).toHaveText(network);
}

/** Chooses a token, then its network, as `USDC · Sepolia`, in the visible payment options. */
async function chooseOption(product: Locator, name: string) {
  const [token = "", network = ""] = name.split(" · ");
  for (const [field, value] of [["Token", token], ["Network", network]] as const) {
    const combobox = product.getByRole("combobox", { name: field, exact: true });
    if ((await combobox.locator(".pp-select__value").textContent()) !== value) {
      await combobox.click();
      await product.getByRole("listbox", { name: field, exact: true }).getByRole("option", { name: value, exact: true }).click();
    }
  }
  await expectChosen(product, name);
}

/**
 * Leaves the quote for the payment options, whatever its state: the page's own button once
 * credited, else the payment element's way back.
 */
async function startOver(product: Locator) {
  await product.getByRole("button", { name: /^(Start a new top-up|Start a new payment|Back)$/ }).click();
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
  await expect(product.getByRole("combobox", { name: "Token", exact: true })).not.toHaveAttribute("aria-disabled");
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
  await product.getByRole("button", { name: PAY }).click();
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
  await product.getByRole("button", { name: PAY }).click();
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
    await product.getByRole("button", { name: PAY }).click();
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
  await product.getByRole("button", { name: PAY }).click();
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
    await product.getByRole("button", { name: PAY }).click();
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
  await product.getByRole("button", { name: PAY }).click();
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
  // The submit names the amount and the token, as the payment element's own would.
  await expect(product.getByRole("button", { name: PAY })).toHaveText("Pay $20.00 in PHA");
  await product.getByRole("button", { name: PAY }).click();
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

  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByText(/^Transaction sent:/)).toBeVisible();
  await expectComplete(timeline, ["sent", "received", "credited", "webhook_received"]);
  // One confirmation: the credit, the demo merchant's bonus, the total, and the transaction.
  const confirmation = product.getByTestId("payment-credited");
  await expect(confirmation).toContainText("Payment credited");
  await expect(confirmation).toContainText("$20.00");
  // Nothing pending once credited: the checkout is gone with its countdown.
  await expect(summary).toBeHidden();
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
    await startOver(product);
    await product.getByText("$5", { exact: true }).click();
    await product.getByRole("button", { name: PAY }).click();
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

/** Mine wallet payments strictly after the server's quote-creation second. */
async function advanceQuoteWindow(page: Page, quote: CreatedQuote): Promise<void> {
  const response = await page.request.get(`${env("SERVICE_URL")}/v1/quotes/${quote.quote}?client_secret=${quote.client_secret}`);
  expect(response.status()).toBe(200);
  const view = await response.json() as { window_start: number };
  const chain = createTestClient({ mode: "anvil", chain: quote.chain_id === baseSepolia.id ? baseSepolia : sepolia,
    transport: http(env(quote.chain_id === baseSepolia.id ? "BASE_ANVIL_URL" : "ANVIL_URL")) }).extend(publicActions);
  await chain.setNextBlockTimestamp({ timestamp: BigInt(Math.max(view.window_start + 1, Number((await chain.getBlock()).timestamp) + 1)) });
}

test("fixed-address quotes credit once at lock, then spot, and a new quote closes the old window", async ({ page }) => {
  await installWallet(page);
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  await product.getByText("$20", { exact: true }).click();
  const created = page.waitForResponse((r) => r.request().method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: PAY }).click();
  const first = await (await created).json() as CreatedQuote;
  const publicQuote = async (quote: CreatedQuote) => {
    const response = await page.request.get(`${env("SERVICE_URL")}/v1/quotes/${quote.quote}?client_secret=${quote.client_secret}`);
    expect(response.status()).toBe(200);
    return await response.json() as { address: string; window_start: number; expires_at: number; replaced_by: string | null };
  };
  const initial = await publicQuote(first);
  const chain = createTestClient({ mode: "anvil", chain: sepolia, transport: http(env("ANVIL_URL")) })
    .extend(publicActions).extend(walletActions);
  // Fund the actual browser wallet, then pay through Checkout's wallet button.
  await chain.writeContract({ account: getAddress(env("PAYER_ADDRESS")), address: getAddress(env("TOKEN_ADDRESS")),
    abi: parseAbi(["function mint(address,uint256)"]), functionName: "mint", args: [getAddress(env("PAYER_ADDRESS")), parseEther("1000")] });
  await chain.setNextBlockTimestamp({ timestamp: BigInt(Math.max(initial.window_start + 1, Number((await chain.getBlock()).timestamp) + 1)) });
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByTestId("payment-credited")).toBeVisible();
  const timeline = await page.request.get(`${env("API_URL")}/api/quotes/${first.quote}`);
  const paid = await timeline.json() as Timeline;
  expect(paid.deposit?.price_source).toBe("quote");
  const hash = await chain.writeContract({ account: getAddress(env("PAYER_ADDRESS")), address: getAddress(env("TOKEN_ADDRESS")),
    abi: erc20Abi, functionName: "transfer", args: [getAddress(initial.address), BigInt(first.amount_atomic)] });
  await chain.mine({ blocks: 2 });
  const secondDeposit = async () => {
    const response = await page.request.get(`${env("SERVICE_URL")}/v1/deposits?tx_hash=${hash}`, { headers: { authorization: `Bearer ppay_rk_test_${"A".repeat(43)}000000` } });
    expect(response.status()).toBe(200);
    const deposits = await response.json() as { data: { quote: string | null; price_source: string; tx_hash: string }[] };
    return deposits.data.find((deposit) => deposit.tx_hash === hash);
  };
  await expect.poll(async () => (await secondDeposit())?.price_source).toBe("spot");
  expect((await secondDeposit())?.quote).toBeNull();
  const replacementResponse = await page.request.post(`${env("API_URL")}/api/quotes`, {
    data: { amount: 2000, chain_id: sepolia.id, asset: "pha" },
  });
  expect(replacementResponse.status()).toBe(200);
  const replacement = await replacementResponse.json() as CreatedQuote;
  const current = await publicQuote(replacement);
  expect(current.address).toBe(initial.address);
  const previous = await publicQuote(first);
  expect(previous.replaced_by).toBe(replacement.quote);
  expect(previous.expires_at).toBe(current.window_start);
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
  await expect(tokensOnNetwork).toHaveText(["Test PHA, Test USDC, Test USDT", "Test USDC"]);
  await expect(tokensOnNetwork.first().locator("xpath=..")).toContainText("Sepolia testnet");
  await expect(tokensOnNetwork.last().locator("xpath=..")).toContainText("Base Sepolia testnet");
  const address = (await scenes.getByTestId("deposit-address").getByRole("link").getAttribute("href"))?.split("/").at(-1) ?? "";
  expect(address).toMatch(/^0x[0-9a-fA-F]{40}$/);
  // The SDK's <DepositAddress> shows the same address to copy without a discovery-time promise:
  // the public view does not identify whether this address belongs to the fast hot set.
  await expect(product.getByText(address).first()).toBeVisible();
  await expect(product.getByText(/Credited at the market rate after confirmation\./)).toBeVisible();

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

test("networks and tokens: Base Sepolia offers only USDC, Sepolia keeps PHA and USDT; the faucets follow", async ({
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
  await chooseOption(product, "USDC · Sepolia");
  await expect(helper.getByRole("button", { name: "Mint 1,000 test PHA" })).toBeVisible();
  await expect(circle).toBeVisible();
  await mintUsdc(env("PAYER_ADDRESS"), parseUnits("100", 6));
  await product.getByText("$5", { exact: true }).click();
  const usdcCreated = page.waitForResponse((r) => r.request().method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  const usdcRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: PAY }).click();
  expect((await usdcRequest).postDataJSON()).toEqual({ amount: 500, chain_id: sepolia.id, asset: "usdc" });
  await expect(product.locator(".pp-summary")).toContainText("5 USDC");
  await expect(await openStep(scenes.getByRole("list", { name: "Payment timeline" }), "quote_created")).toContainText(
    "1 USDC = $1.00",
  );
  await expect(product.getByRole("tabpanel", { name: "Exact amount", exact: true }).getByText(/bonus/)).toHaveCount(0);
  await page.setViewportSize({ width: 1440, height: 1100 });
  await page.screenshot({ path: testInfo.outputPath("usdc-quote.png") });
  await advanceQuoteWindow(page, await (await usdcCreated).json() as CreatedQuote);
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$5.00", { timeout: 60_000 });
  await expect(product.getByTestId("balance")).toHaveText("$5.00", { timeout: 10_000 });
  await expect(product.getByTestId("bonus-credited")).toHaveCount(0);
  expect(await tokenBalance(env("PAYER_ADDRESS"), { token: env("USDC_ADDRESS") })).toBe(parseUnits("95", 6));
  await openTab(scenes, "Credits");
  await expect(scenes.getByTestId("payment").first()).toContainText("5 USDC");
  await expect(scenes.getByTestId("payment").first()).toContainText("at $1.00 / USDC");

  // Base Sepolia offers only USDC and Circle's faucet; no token can mint from the helper.
  await startOver(product);
  await chooseOption(product, "USDC · Base Sepolia");
  await expectOffered(product, "Network", ["Sepolia", "Base Sepolia"]);
  await expect(circle).toHaveAttribute("title", "On the faucet, pick Base Sepolia as the network.");
  await expect(helper.getByRole("button", { name: /Mint/ })).toHaveCount(0);
  await expect(helper.getByRole("link", { name: /^Base Sepolia ETH faucets/ })).toHaveAttribute(
    "href",
    "https://docs.base.org/get-started/get-funds#testnet-base-sepolia",
  );
  await mintUsdc(env("PAYER_ADDRESS"), parseUnits("100", 6), true);
  const baseCreated = page.waitForResponse((r) => r.request().method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  const baseRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: PAY }).click();
  expect((await baseRequest).postDataJSON()).toEqual({ amount: 2000, chain_id: baseSepolia.id, asset: "usdc" });
  await expect(product.locator(".pp-summary")).toContainText("20 USDC");
  await expect(await openStep(scenes.getByRole("list", { name: "Payment timeline" }), "quote_created")).toContainText(
    "1 USDC = $1.00",
  );
  await advanceQuoteWindow(page, await (await baseCreated).json() as CreatedQuote);
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$20.00", { timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toHaveCount(0);
  await expect(product.getByTestId("balance")).toHaveText("$25.00", { timeout: 10_000 });
  expect(await tokenBalance(env("PAYER_ADDRESS"), {
    rpc: env("BASE_ANVIL_URL"), token: env("BASE_USDC_ADDRESS"),
  })).toBe(parseUnits("80", 6));
  await page.getByRole("button", { name: "Dark theme" }).click();
  await page.screenshot({ path: testInfo.outputPath("base-usdc-dark.png") });
  await page.getByRole("button", { name: "Dark theme" }).click();

  // Sepolia retains USDT and its faucet. `transfer` returns nothing, as Tether's does on
  // Ethereum, and the checkout pays it all the same.
  await startOver(product);
  await chooseOption(product, "USDT · Sepolia");
  const usdt = { token: env("USDT_ADDRESS") };
  await helper.getByRole("button", { name: "Mint 1,000 test USDT" }).click();
  await expect.poll(() => tokenBalance(env("PAYER_ADDRESS"), usdt)).toBe(parseUnits("1000", 6));

  await product.getByText("$5", { exact: true }).click();
  const usdtCreated = page.waitForResponse((r) => r.request().method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  const usdtRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: PAY }).click();
  expect((await usdtRequest).postDataJSON()).toEqual({ amount: 500, chain_id: sepolia.id, asset: "usdt" });
  await expect(product.locator(".pp-summary")).toContainText("5 USDT");
  await expect(await openStep(scenes.getByRole("list", { name: "Payment timeline" }), "quote_created")).toContainText(
    "1 USDT = $1.00",
  );
  await advanceQuoteWindow(page, await (await usdtCreated).json() as CreatedQuote);
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$5.00", { timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toHaveCount(0);
  await expect(product.getByTestId("balance")).toHaveText("$30.00", { timeout: 10_000 });
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
  const pay = product.getByRole("button", { name: "Pay with Empty Wallet" });
  await expect(product.getByTestId("balance")).toHaveText("$0.00");

  // $20 in test USDC from a wallet holding a millionth less, in USDC's 6 decimals: the checkout
  // sends nothing, and points to Circle's faucet.
  await mintUsdc(empty, 19_999_999n);
  await chooseOption(product, "USDC · Sepolia");
  await product.getByRole("button", { name: PAY }).click();
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
  await startOver(product);
  await chooseOption(product, "PHA · Sepolia");
  await product.getByText("Custom", { exact: true }).click();
  await product.getByLabel("Custom amount (USD)").fill("500");
  await product.getByRole("button", { name: PAY }).click();
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
    // The demo, where the money goes, what it is for, the comparison, the questions, and the close.
    await expect(staticPage.getByRole("heading", { level: 2 })).toHaveCount(6);
    // The answers fold natively, without script.
    const question = staticPage.locator("summary", { hasText: "Which chains and tokens are supported?" });
    const answer = staticPage.getByText("The live demo uses test PHA, USDC and USDT on Sepolia, and only test USDC on Base Sepolia.", { exact: false });
    await expect(answer).toBeHidden();
    await question.click();
    await expect(answer).toBeVisible();
    await expectMetadata(staticPage);
    await expect(staticPage.getByRole("button", { name: "Menu", exact: true })).toHaveCount(0);
    // The theme button cannot know the visitor's theme, so it states none; and the demo, prerendered
    // but unable to work without script, is not shown: only its note.
    await expect(staticPage.getByRole("button", { name: "Dark theme" })).not.toHaveAttribute("aria-pressed");
    const note = staticPage.locator(`${DEMO_SECTION} noscript > p`);
    await expect(note).toHaveText("The demo needs JavaScript.");
    await expect(note).toBeVisible();
    await expect(staticPage.locator("#demo-root")).toBeHidden();
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Compare", exact: true })).toBeVisible();
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Demo", exact: true })).toHaveAttribute("href", "/#demo");
    // The page arrives complete: the demo's two panels are in its HTML, not added by script. (Their
    // style attributes, Radix's, are each permitted by the CSP's hashes: scripts/prerender.ts.)
    const homeHtml = await home?.text();
    expect(homeHtml).toContain('aria-label="Your backend"');
    expect(homeHtml).toContain('aria-labelledby="product-title"');
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


test("loading home islands preserves the original prerendered hero and demo", async ({ page }) => {
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
    // The demo's panels are prerendered too: in view before any script, and the same elements once
    // the demo has hydrated and loaded its data.
    const product = page.getByRole("region", { name: "Customer view" });
    await expect(product).toBeVisible();
    const originalProduct = await product.elementHandle();
    const originalBackend = await page.getByRole("complementary", { name: "Your backend" }).elementHandle();
    releaseEntry?.();
    await expect(product.getByTestId("balance")).toHaveText("$0.00");
    expect(await original.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalHeader.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalFooter.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalProduct.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalBackend.evaluate((node) => node.isConnected)).toBe(true);
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

  // Keyboard focus on the token field and a text field keeps an outline, which the system colours.
  await chosen.focus();
  await page.keyboard.press("Tab");
  const token = product.getByRole("combobox", { name: "Token", exact: true });
  await expect(token).toBeFocused();
  expect(await style(token, "outlineStyle")).not.toBe("none");
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

test("the checkout takes the site's theme in either theme, over the SDK's defaults", async ({ browser }) => {
  // The SDK's stylesheet comes before the site's (src/index.css), whose `.pp-root[data-theme]` sets
  // the components' tokens from the page's: the checkout's surface and primary are the site's muted
  // fill and primary, not the SDK's own (its dark surface is #18181b).
  for (const colorScheme of ["light", "dark"] as const) {
    const context = await browser.newContext({ colorScheme });
    try {
      const page = await context.newPage();
      await page.goto(env("SITE_URL"));
      const product = page.getByRole("region", { name: "Customer view" });
      await expect(product.getByTestId("balance")).toHaveText("$0.00");
      await product.getByRole("button", { name: PAY }).click();
      const checkout = product.locator(".pp-root").first();
      await expect(checkout).toHaveAttribute("data-theme", colorScheme);
      const tokens = await checkout.evaluate((root) => {
        const style = getComputedStyle(root);
        const value = (name: string) => style.getPropertyValue(name).trim();
        return {
          surface: [value("--pp-color-surface"), value("--muted")],
          primary: [value("--pp-color-primary"), value("--primary")],
        };
      });
      for (const [name, [checkoutValue, siteValue]] of Object.entries(tokens)) {
        expect(siteValue, `${colorScheme}: the site's ${name}`).not.toBe("");
        expect(checkoutValue, `${colorScheme}: the checkout's ${name}`).toBe(siteValue);
      }
    } finally {
      await context.close();
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
  test.setTimeout(900_000);
  const all = [2560, 1920, 1440, 1280, 1024, 768, 390];
  const pages: [string, number[]][] = [
    ["", all], ["compare", all], ["no-such-page/deeper", all], ["docs", all], ["docs/sdk/react", all],
    ["docs/integration", [1920, 1440, 390]], ["reference", [1440]],
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
/** The demo's section (its anchor, #demo, is its introduction, which a link to it opens at). */
const DEMO_SECTION = 'section[aria-labelledby="demo-title"]';

async function expectNothingClipped(page: Page, state: string): Promise<void> {
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  for (const tab of DEMO_TABS) {
    await openTab(scenes, tab);
    const clipped = await page.locator("#demo-root").evaluate((root) => [...root.querySelectorAll<HTMLElement>("*")]
      .filter((element) => {
        const style = getComputedStyle(element);
        const hides = style.overflowX === "hidden" || style.overflowX === "clip" || element.getAttribute("role") === "tabpanel";
        // Visually hidden text (a 1px box for screen readers) is clipped on purpose, and so is a
        // request's one-line summary, its id ending in an ellipsis (the whole id in its title, and
        // in the opened request).
        const summary = style.textOverflow === "ellipsis" && element.hasAttribute("title");
        return hides && !summary && element.clientWidth > 1 && element.checkVisibility() && element.scrollWidth > element.clientWidth + 1;
      })
      .map((element) => `${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")} (${element.scrollWidth} > ${element.clientWidth})`));
    expect(clipped, `${state}, ${tab} tab`).toEqual([]);
    // Nothing scrolls inside the demo: every element shows its overflow, but for code blocks, which
    // may scroll sideways on a phone and never scroll down, and the backend's row of tabs, which
    // scrolls sideways where its five tabs do not fit (under 360px). Native controls, images,
    // visually hidden text, and a summary's ellipsis (above) keep their own.
    const scrolling = await page.locator("#demo-root").evaluate((root) => [...root.querySelectorAll<HTMLElement>("*")]
      .filter((element) => {
        if (element.matches("input, textarea, select, img, svg, svg *, [role=tablist][aria-label=Backend]") || element.clientWidth <= 1) return false;
        const style = getComputedStyle(element);
        if (style.textOverflow === "ellipsis" && element.hasAttribute("title")) return false;
        if (element.tagName === "PRE") return element.scrollHeight > element.clientHeight + 1;
        return style.overflowX !== "visible" || style.overflowY !== "visible";
      })
      .map((element) => `${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")}`));
    expect(scrolling, `${state}, ${tab} tab: inner scrolling`).toEqual([]);
  }
  await openTab(scenes, "Credits");
}

test("the demo is a section like the others: their padding, label, heading, and introduction; a link to it opens at its heading", async ({ page }) => {
  await page.goto(env("SITE_URL"));
  for (const [width, height] of [[1920, 1080], [1440, 900], [1280, 800], [1024, 768], [390, 844]] as const) {
    await page.setViewportSize({ width, height });
    const [demo, features] = await page.evaluate(() => ["demo-title", "features-title"].map((id) => {
      const section = document.querySelector(`section[aria-labelledby="${id}"]`);
      const container = section?.firstElementChild;
      const heading = document.getElementById(id);
      const eyebrow = heading?.previousElementSibling;
      const lead = heading?.nextElementSibling;
      if (container == null || heading === null || eyebrow == null || lead == null) return null;
      const style = getComputedStyle(container);
      return {
        padding: [style.paddingTop, style.paddingBottom],
        label: getComputedStyle(eyebrow).fontSize,
        heading: getComputedStyle(heading).fontSize,
        lead: getComputedStyle(lead).fontSize,
        gaps: [heading.getBoundingClientRect().top - eyebrow.getBoundingClientRect().bottom, lead.getBoundingClientRect().top - heading.getBoundingClientRect().bottom],
        eyebrow: eyebrow.textContent,
      };
    }));
    expect(demo, `the demo's introduction at ${width}px`).not.toBeNull();
    expect({ ...demo, eyebrow: undefined }, `at ${width}px`).toEqual({ ...features, eyebrow: undefined });
    expect(demo?.eyebrow).toBe("Live demo");
  }
  // The header's Demo link lands on the demo's label and heading, under the header.
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "Demo" }).click();
  await expect(page).toHaveURL(/#demo$/);
  await expect.poll(() => page.locator("#demo").evaluate((intro) => intro.getBoundingClientRect().top)).toBeCloseTo(96, 0);
  await expect(page.getByRole("heading", { level: 2, name: "Try it on testnet" })).toBeInViewport();
});

test("a credit whose bonus the ledger has not applied yet shows the bonus pending, never as $0.00", async ({ page }) => {
  test.setTimeout(180_000);
  await installWallet(page);
  // The race: the checkout reports the quote credited while the account, polled a moment before
  // the ledger applied the webhook, still lists its deposit with no bonus (`bonus: null`). Served
  // so until released.
  let held = true;
  await page.route(`${env("API_URL")}/api/account`, async (route) => {
    const response = await route.fetch();
    const body = (await response.json()) as { payments: { id: string; bonus: number | null }[] };
    if (held) body.payments = body.payments.map((row) => (row.id.startsWith("dep_") ? { ...row, bonus: null } : row));
    await route.fulfill({ response, json: body });
  });
  const product = page.getByRole("region", { name: "Customer view" });
  await page.goto(env("SITE_URL"));
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  const testTokens = page.getByRole("note", { name: "Test tokens" });
  await testTokens.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
  await expect(testTokens).toContainText("Minted:");
  // Every text the confirmation shows, from its first frame.
  await product.evaluate((card) => {
    const seen = new Set<string>();
    Object.assign(window, { confirmations: seen });
    new MutationObserver(() => {
      const confirmation = card.querySelector("[data-testid=payment-credited]");
      if (confirmation !== null) seen.add(confirmation.textContent);
    }).observe(card, { subtree: true, childList: true, characterData: true });
  });
  await product.getByRole("button", { name: PAY }).click();
  await expect(product.locator(".pp-summary")).toContainText("80 PHA");
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  const confirmation = product.getByTestId("payment-credited");
  await expect(confirmation).toBeVisible({ timeout: 60_000 });
  // The deposit's row, credited but without its bonus, has arrived: the bonus and the total wait
  // in their places.
  await page.waitForResponse(async (response) => response.url() === `${env("API_URL")}/api/account` &&
    ((await response.json()) as { payments: { id: string }[] }).payments.some((row) => row.id.startsWith("dep_")));
  await expect(confirmation).toContainText("PHA bonus +10%");
  await expect(product.getByTestId("bonus-credited")).toHaveCount(0);
  // Released, the next poll brings the bonus: $2.00 on the $20.00 credit.
  held = false;
  await expect(product.getByTestId("bonus-credited")).toContainText("+$2.00", { timeout: 30_000 });
  await expect(confirmation).toContainText("Total$22.00");
  const shown = await page.evaluate(() => [...(window as unknown as { confirmations: Set<string> }).confirmations]);
  expect(shown.length, "the confirmation's texts").toBeGreaterThan(0);
  expect(shown.filter((text) => text.includes("+$0.00") || /Total\$(?!22\.00)/.test(text)), "a bonus of $0.00 or a wrong total").toEqual([]);
});

test("the demo's states move nothing on the page: one height in each state and on each tab, no layout shift, nothing clipped", async ({ page }) => {
  test.setTimeout(600_000);
  await installWallet(page);
  // The fixture matches staging: Sepolia has three tokens, two mints and two faucets.
  // Every layout shift as the browser reports it, but one within half a second of a click (the
  // click's), by what moved: anything but an entry of the backend's live logs (its open tab, its
  // stepper), where an entry arriving moves the entries after it, as a log grows.
  await page.addInitScript(() => {
    const live = '[aria-label="Your backend"] [role="tabpanel"], [aria-label="Your backend"] [aria-label="Payment timeline"]';
    Object.assign(window, { shifted: [] });
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        if (!("hadRecentInput" in entry) || entry.hadRecentInput === true || !("sources" in entry) || !Array.isArray(entry.sources)) continue;
        for (const { node, previousRect, currentRect } of entry.sources as { node?: Node | null; previousRect: DOMRectReadOnly; currentRect: DOMRectReadOnly }[]) {
          const element = node instanceof Element ? node : node?.parentElement;
          if (element?.closest(live) != null) continue;
          (window as unknown as { shifted: string[] }).shifted.push(element == null ? "?" : `${element.tagName.toLowerCase()}.${[...element.classList].slice(0, 4).join(".")}: ${element.textContent.slice(0, 40)} ${JSON.stringify([previousRect.x, previousRect.y, previousRect.width, currentRect.x, currentRect.y, currentRect.width])}`);
        }
      }
    }).observe({ type: "layout-shift", buffered: true });
  });
  const product = page.getByRole("region", { name: "Customer view" });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const section = page.locator(DEMO_SECTION);
  // From lg, where the two panels sit side by side, the section is one height in every state and
  // on every tab (its first measure at each width), so nothing under it moves; where they stack on
  // a tablet, the customer's view, which changes with the payment, is one height in every state the
  // payment brings, and no taller than the form it starts from (src/index.css,
  // --spacing-demo-view-sm). On a phone the view is as tall as its content.
  const fixed = new Map<string, number>();
  const idle = new Map<string, number>();
  const same = (key: string, value: number, where: string) => {
    if (!fixed.has(key)) fixed.set(key, value);
    expect(Math.abs(value - (fixed.get(key) ?? value)), `${where}: ${value} against ${fixed.get(key)}`).toBeLessThanOrEqual(1);
  };
  // Each layout's narrowest width, where its content is tallest (2xl, xl, lg, sm, a phone), and two wider.
  const sizes = [[1920, 1080], [1536, 864], [1440, 900], [1280, 800], [1024, 768], [640, 900], [390, 844]] as const;
  const settle = () => page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const steady = async (state: string) => {
    // Nothing shifted since the last state, at 1440×900 with the panels in view, while this state
    // arrived (a shift within half a second of a click is the click's, and not counted).
    await settle();
    expect(await page.evaluate(() => (window as unknown as { shifted: string[] }).shifted), `${state}: what moved as it arrived`).toEqual([]);
    for (const [width, height] of sizes) {
      await page.setViewportSize({ width, height });
      for (const tab of DEMO_TABS) {
        await openTab(scenes, tab);
        const where = `${state}, ${tab} tab, at ${width}×${height}`;
        if (width >= 1024) same(`${width}`, await section.evaluate((element) => element.getBoundingClientRect().height), where);
        else if (width >= 640) {
          const view = await product.evaluate((element) => element.getBoundingClientRect().height);
          if (state === "idle") idle.set(`${width}`, view);
          else {
            same(`${width}`, view, where);
            expect(view, `${where}: against the form's ${idle.get(`${width}`)}`).toBeLessThanOrEqual((idle.get(`${width}`) ?? 0) + 1);
          }
        }
        // Its text at least 14px too, in every state and on every tab.
        expect(await smallText(page, DEMO_SECTION), `${where}: text under 14px`).toEqual([]);
      }
      await expectNothingClipped(page, `${state} at ${width}×${height}`);
    }
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.locator("#demo-root").evaluate((root) => root.scrollIntoView({ block: "start" }));
    await settle();
    await page.evaluate(() => Object.assign(window, { shifted: [] }));
  };
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(env("SITE_URL"));
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
  // From here on, with the panels in view: the page's arrival is the next test's.
  await page.locator("#demo-root").evaluate((root) => root.scrollIntoView({ block: "start" }));
  await settle();
  await page.evaluate(() => Object.assign(window, { shifted: [] }));
  const testTokens = page.getByRole("note", { name: "Test tokens" });
  await expect(product.getByRole("combobox")).toHaveCount(2);
  await expect(testTokens.getByRole("button", { name: /^Mint 1,000 test / })).toHaveCount(2);
  await expect(testTokens.getByRole("link", { name: /^Circle USDC faucet/ })).toBeVisible();
  await expect(testTokens.getByRole("link", { name: /ETH faucets/ })).toBeVisible();
  await steady("idle");
  await testTokens.getByRole("button", { name: "Mint 1,000 test PHA" }).click();
  await expect(testTokens).toContainText("Minted:");
  await product.getByRole("button", { name: PAY }).click();
  await expect(product.locator(".pp-summary")).toContainText("80 PHA");
  await steady("paying");
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByTestId("payment-credited")).toBeVisible({ timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toBeVisible({ timeout: 10_000 });
  await steady("credited");
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
  await steady("after a sweep and a refund request");
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
  // On the home page: the hero, the demo's header and its two cards (the split), and the questions
  // and the footer (the aside); on /compare, its header and its sources (the split) and the footer;
  // on the docs and the reference, the navigation beside the page, each operation's and object's
  // fields beside their example, and the footer. Within a page, every right column of one layout
  // (the 6/6 split, the 4/8 aside, the docs' sidebar layout, the reference's parts) starts on one
  // line; the sidebar layout's page starts on one line across the docs and the reference. A part
  // is named by its section's heading, else by its landmark.
  const docs = ["main", "footer"];
  const pages: [string, string[] | number][] = [
    ["", ["hero-title", "demo-title", "faq-title", "closing-title", "footer"]],
    ["compare", ["main", "sources-title", "footer"]],
    ["docs", docs], ["docs/integration", docs], ["docs/configuration", docs], ["docs/sdk/react", docs],
    ["reference", 40],
  ];
  for (const [width, height] of [[1440, 900], [1280, 800]] as const) {
    const sidebar = new Set<number>();
    for (const [path, expected] of pages) {
      await page.setViewportSize({ width, height });
      await page.goto(new URL(path, env("SITE_URL")).href);
      if (path === "") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
      const parts = await columnLayout(page);
      const where = `/${path} at ${width}px`;
      if (typeof expected === "number") expect(parts.length, `${where}: two-column parts`).toBeGreaterThanOrEqual(expected);
      else expect(parts.map(({ name }) => name), `${where}: two-column parts`).toEqual(expected);
      for (const layout of new Set(parts.map((part) => part.layout))) {
        const group = parts.filter((part) => part.layout === layout);
        const line = group[0]?.x ?? 0;
        expect(group.filter(({ x }) => Math.abs(x - line) > 1).map(({ name, x }) => `${name} starts at ${x.toFixed(1)}, not ${line.toFixed(1)}`), `${where}, ${layout}`).toEqual([]);
        if (layout === "sidebar") sidebar.add(Math.round(line));
      }
      // A right column with no left one beside it (an object without an example in the reference)
      // is measured by its left edge only.
      expect(parts.filter(({ left, right }) => left !== null && (right === null || Math.abs(left - right) > 1))
        .map(({ name, how, left, right }) => `${name}: ${how} at ${left?.toFixed(1) ?? "none"} and ${right?.toFixed(1) ?? "none"}`), where).toEqual([]);
    }
    expect([...sidebar], `the docs' and the reference's page column at ${width}px`).toHaveLength(1);
  }
});


test("the page's content is the viewport less its gutters, up to 1600px and centred; backgrounds span the viewport", async ({ page }) => {
  // Every page's header, sections, and footer hold their content in one box: the viewport less its
  // gutters (32px at 1280, 40px at 1440), and on a wider screen 1600px, centred. Measured by that
  // box (each container's content box), and by what it holds: the header's mark and footer's brand
  // at its left edge, the header's last action, the footer's links, and the rightmost content (the
  // hero's code window, the full comparison, the docs' outline, the reference's examples) at its
  // right, and every section of the home page and the comparison from one edge to the other. The
  // sections' backgrounds and rules still span the viewport. Only running text (leads, answers, the
  // docs' paragraphs) stops at its measure, 36em.
  // Each page's rightmost content, its containers between the header's and the footer's, and the
  // parts with a background or a rule besides the header and the footer.
  const pages: Record<string, { rightmost: string; containers: string; bleeds?: string }> = {
    "": { rightmost: "#hero-code", containers: "main > section > div:first-child", bleeds: "main > section" },
    compare: { rightmost: "main table", containers: "main" },
    "docs/integration": { rightmost: 'aside nav[aria-label="On this page"]', containers: '[data-layout="sidebar"]' },
    reference: { rightmost: 'main [data-layout="operation"] [data-column="right"]', containers: '[data-layout="sidebar"]' },
  };
  // The content box's distance from each edge of the viewport.
  const insets: [number, number][] = [[1280, 32], [1440, 40], [1920, 160], [2560, 480]];
  // A page's height at 1920: the same at 2560, where its content is as wide.
  const heights = new Map<string, number>();
  // Still: the custody rail's moving segment would otherwise be measured mid-run.
  await page.emulateMedia({ reducedMotion: "reduce" });
  for (const [width, inset] of insets) {
    for (const [path, { rightmost: content, containers, bleeds }] of Object.entries(pages)) {
      await page.setViewportSize({ width, height: 1000 });
      await page.goto(new URL(path, env("SITE_URL")).href);
      if (path === "") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
      const where = `/${path} at ${width}px`;
      const measured = await page.evaluate(({ content, containers, bleeds }) => {
        const viewport = document.documentElement.clientWidth;
        const shown = (selector: string) => [...document.querySelectorAll(selector)].find((element) => element.checkVisibility());
        const box = (selector: string) => shown(selector)?.getBoundingClientRect();
        // Each container's content box: the header's, the page's (each section's on the home page),
        // and the footer's.
        const boxes = [...document.querySelectorAll<HTMLElement>(`#site-header > header > div:first-child, ${containers}, #site-footer > footer > div`)].map((element) => {
          const { left, right } = element.getBoundingClientRect();
          const style = getComputedStyle(element);
          const name = element.closest("[aria-labelledby]")?.getAttribute("aria-labelledby") ?? element.parentElement?.tagName.toLowerCase() ?? "?";
          return { name, left: left + parseFloat(style.paddingLeft), right: viewport - (right - parseFloat(style.paddingRight)) };
        });
        // The parts with a background or a rule: from one edge of the viewport to the other.
        const edgeToEdge = [...document.querySelectorAll<HTMLElement>(["#site-header > header", bleeds, "#site-footer > footer"].filter(Boolean).join(", "))].map((element) => {
          const { left, right } = element.getBoundingClientRect();
          return { name: element.getAttribute("aria-labelledby") ?? element.tagName.toLowerCase(), left, right: viewport - right };
        });
        return {
          viewport,
          height: document.documentElement.scrollHeight,
          containers: boxes,
          bleeds: edgeToEdge,
          edges: {
            "header's mark": box('header a[aria-label="Phala Pay home"]')?.left ?? 0,
            "footer's brand": box('footer [data-column="left"]')?.left ?? 0,
            "header's last action": viewport - (box('header a[href="/docs/self-hosting"]')?.right ?? 0),
            "footer's links": viewport - (box('footer [data-column="right"]')?.right ?? 0),
            [content]: viewport - (box(content)?.right ?? 0),
          },
        };
      }, { content, containers, bleeds });
      expect(measured.viewport, where).toBe(width);
      expect(measured.containers.length, `${where}: containers`).toBeGreaterThanOrEqual(3);
      for (const { name, left, right } of measured.containers) {
        expect(Math.abs(left - inset), `${where}, ${name}'s content: left edge`).toBeLessThanOrEqual(1);
        expect(Math.abs(right - inset), `${where}, ${name}'s content: right edge`).toBeLessThanOrEqual(1);
      }
      for (const { name, left, right } of measured.bleeds) {
        expect([left, right], `${where}, ${name}: from edge to edge`).toEqual([0, 0]);
      }
      for (const [name, edge] of Object.entries(measured.edges)) expect(Math.abs(edge - inset), `${where}: ${name}`).toBeLessThanOrEqual(1);
      if (width === 1920) heights.set(path, measured.height);
      if (width === 2560) expect(measured.height, `${where}: the page's height, as at 1920`).toBe(heights.get(path));
      // Every section of the home page and the comparison fills its content box: its content
      // (anything shown, short of a container or a full-bleed box) starts at one edge and ends at
      // the other.
      if (path === "" || path === "compare") {
        const sections = await page.locator("main").evaluate((main, inset) => {
          const viewport = document.documentElement.clientWidth;
          return [...main.querySelectorAll<HTMLElement>(":scope > section, :scope section[id]")].map((section) => {
            const boxes = [...section.querySelectorAll<HTMLElement>("*")]
              // Shown: not transparent (a form control Radix keeps for the form) nor hidden.
              .filter((element) => element.checkVisibility({ opacityProperty: true, visibilityProperty: true }) && element.closest(".sr-only") === null)
              .map((element) => element.getBoundingClientRect())
              .filter((box) => box.width > 0 && box.width <= viewport - 2 * inset + 1);
            return {
              name: section.getAttribute("aria-labelledby") ?? section.id,
              left: Math.min(...boxes.map((box) => box.left)),
              right: viewport - Math.max(...boxes.map((box) => box.right)),
            };
          });
        }, inset);
        expect(sections.length, `${where}: sections`).toBeGreaterThan(2);
        for (const { name, left, right } of sections) {
          expect(Math.abs(left - inset), `${where}, ${name}: left edge`).toBeLessThanOrEqual(1);
          expect(Math.abs(right - inset), `${where}, ${name}: right edge`).toBeLessThanOrEqual(1);
        }
      }
      // Running text: no paragraph of more than a line or two wider than its measure.
      const measures = await page.locator("main").evaluate((main) => [...main.querySelectorAll<HTMLElement>(".docs-prose > p, section p, details p")]
        .filter((element) => element.checkVisibility() && element.textContent.length > 120)
        .map((element) => element.getBoundingClientRect().width / parseFloat(getComputedStyle(element).fontSize)));
      expect(measures.length, `${where}: paragraphs`).toBeGreaterThan(0);
      expect(Math.max(...measures), `${where}: the widest paragraph, in em`).toBeLessThanOrEqual(36.01);
    }
  }
});

/**
 * The site's pages in a visitor's order, each reached by its link (a full page load, as the site
 * serves them), with what shows it has hydrated: the header's theme button states its state once
 * it has, and the home page's demo shows the balance once its data is in.
 */
const JOURNEY: { path: string; link: (page: Page) => Locator }[] = [
  { path: "/compare", link: (page) => page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "Compare" }) },
  { path: "/docs", link: (page) => page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "Docs" }) },
  { path: "/docs/integration", link: (page) => page.getByRole("navigation", { name: "Documentation" }).getByRole("link", { name: "Integration guide" }) },
  { path: "/reference", link: (page) => page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "API reference" }) },
  { path: "/#demo", link: (page) => page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "Demo" }) },
  { path: "/", link: (page) => page.getByRole("link", { name: "Phala Pay home" }) },
];

test("moving between pages keeps their server HTML: no hydration error, and nothing shifts", async ({ browser }) => {
  // Each page is complete as it arrives, and hydrating it changes nothing in view: React reports no
  // hydration error (a mismatch logs one and replaces the server's HTML), each island's server-
  // rendered element is still the one in the page once hydrated, and the browser records no layout
  // shift, in either theme, at a desktop and a phone.
  test.setTimeout(240_000);
  for (const colorScheme of ["light", "dark"] as const) {
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      const context = await browser.newContext({ colorScheme, viewport });
      try {
        await context.addInitScript(() => {
          window.layoutShifts = [];
          new PerformanceObserver((list) => {
            for (const entry of list.getEntries()) {
              if ("value" in entry && typeof entry.value === "number" && "hadRecentInput" in entry && entry.hadRecentInput === false) window.layoutShifts.push(entry.value);
            }
          }).observe({ type: "layout-shift", buffered: true });
          // Each island's first element as the server rendered it, before any script of the page's ran.
          document.addEventListener("readystatechange", () => {
            if (document.readyState !== "interactive") return;
            const islands = [...document.querySelectorAll("#site-header, #site-footer, #hero-code, #deploy-command, #demo-root")];
            Object.assign(window, { serverIslands: islands.map((island) => island.firstElementChild) });
          });
        });
        const page = await context.newPage();
        const problems = await watchConsole(page);
        const hydrated = async (path: string) => {
          await expect(page.getByRole("button", { name: "Dark theme" })).toHaveAttribute("aria-pressed", String(colorScheme === "dark"));
          if (path.startsWith("/#") || path === "/") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
          await page.evaluate(() => document.fonts.ready);
          const { shifts, replaced } = await page.evaluate(() => ({
            shifts: window.layoutShifts,
            replaced: ((window as unknown as { serverIslands?: (Element | null)[] }).serverIslands ?? [null]).filter((node) => node === null || !node.isConnected).length,
          }));
          const where = `${path}, ${colorScheme}, ${viewport.width}px`;
          expect(replaced, `${where}: islands replaced on hydration`).toBe(0);
          expect(shifts, `${where}: layout shifts`).toEqual([]);
        };
        await page.goto(env("SITE_URL"));
        await hydrated("/");
        for (const { path, link } of JOURNEY) {
          // On a phone the site's links are in the menu, and the docs' in the page's own.
          if (viewport.width < 768) {
            await page.goto(new URL(path, env("SITE_URL")).href);
          } else {
            await Promise.all([page.waitForURL(new URL(path, env("SITE_URL")).href), link(page).click()]);
          }
          await hydrated(path);
        }
        expect(problems, `${colorScheme}, ${viewport.width}px`).toEqual([]);
      } finally {
        await context.close();
      }
    }
  }
});

test("a link to a part of a page opens the page at that part from its first frame", async ({ browser }) => {
  // The header's Demo link (/#demo) and the hero's call to deploy (a docs section): the new page's
  // first frame is already at the part, not at the page's top for a frame before it. The browser
  // paints as it parses, and scrolls to the part only once the page's scripts have run. Measured
  // where that shows: without motion (no cross-fade covers a frame), and on a slow device (the CPU
  // throttled sixfold, as the CI runner's is slow), where the parser yields to the renderer before
  // the page is parsed. The first frame is the page's first animation frame: rendering runs none
  // before it.
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: "reduce" });
  try {
    await context.addInitScript(() => {
      requestAnimationFrame(() => Object.assign(window, { firstFrameScroll: scrollY }));
    });
    const page = await context.newPage();
    const cdp = await context.newCDPSession(page);
    const cases: [string, string, Locator, string][] = [
      ["/compare", "/#demo", page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "Demo" }), "#demo"],
      ["/", "/docs/self-hosting#one-command-deploy", page.getByRole("link", { name: "Start a testnet instance" }).first(), "#one-command-deploy"],
    ];
    for (const [from, to, link, target] of cases) {
      await cdp.send("Emulation.setCPUThrottlingRate", { rate: 1 });
      await page.goto(new URL(from, env("SITE_URL")).href);
      await cdp.send("Emulation.setCPUThrottlingRate", { rate: 6 });
      await Promise.all([page.waitForURL(new URL(to, env("SITE_URL")).href), link.click()]);
      await expect(page.locator(target)).toBeInViewport();
      const { first, now, top } = await page.evaluate((selector) => ({
        first: (window as unknown as { firstFrameScroll: number }).firstFrameScroll,
        now: scrollY,
        top: document.querySelector(selector)?.getBoundingClientRect().top ?? -1,
      }), target);
      expect(first, `${to}: the first frame's scroll`).toBeGreaterThan(0);
      expect(Math.abs(first - now), `${to}: the first frame at the part (${first}, then ${now})`).toBeLessThanOrEqual(1);
      expect(top, `${to}: the part under the header`).toBeGreaterThanOrEqual(0);
    }
    await cdp.send("Emulation.setCPUThrottlingRate", { rate: 1 });
  } finally {
    await context.close();
  }
});

test("the head's theme bootstrap alone sets the visitor's theme, with every module script refused", async ({ browser }) => {
  // Only the head's inline theme script may run: every module script is refused, so nothing else of
  // the page's can set the theme. The page's class and background are then the visitor's theme: the
  // system's dark preference, or a choice stored earlier over it.
  const dark = "oklch(0.145 0 0)";
  const light = "oklch(1 0 0)";
  const cases = [
    { colorScheme: "dark" as const, stored: null, background: dark },
    { colorScheme: "light" as const, stored: "dark", background: dark },
    { colorScheme: "dark" as const, stored: "light", background: light },
  ];
  for (const { colorScheme, stored, background } of cases) {
    const context = await browser.newContext({ colorScheme });
    try {
      await context.route("**/assets/*.js", (route) => route.abort());
      if (stored !== null) await context.addInitScript((theme) => localStorage.setItem("demo-theme", theme), stored);
      const page = await context.newPage();
      for (const path of ["", "compare", "docs/integration", "reference"]) {
        await page.goto(new URL(path, env("SITE_URL")).href);
        const painted = await page.evaluate(() => ({
          dark: document.documentElement.classList.contains("dark"),
          background: getComputedStyle(document.body).backgroundColor,
        }));
        expect(painted, `/${path}, ${colorScheme} system, ${stored ?? "nothing"} stored`).toEqual({ dark: background === dark, background });
      }
    } finally {
      await context.close();
    }
  }
});

test("the hero's facts wrap alike", async ({ page }) => {
  // The owner's regression: one fact's meaning on two lines beside its siblings' one. At every
  // width the three sit side by side, their meanings take as many lines each.
  await page.goto(env("SITE_URL"));
  for (const width of [1440, 1280, 1024, 768]) {
    await page.setViewportSize({ width, height: 900 });
    const lines = await page.locator("[data-facts] dt").evaluateAll((terms) => terms.map((term) => {
      const range = document.createRange();
      range.selectNodeContents(term);
      return new Set([...range.getClientRects()].map((rect) => Math.round(rect.top))).size;
    }));
    expect(lines, `${width}px`).toHaveLength(3);
    expect(new Set(lines).size, `${width}px: lines per fact ${lines.join(", ")}`).toBe(1);
  }
});

/**
 * How each shown element matching `selector` sets its text. Its words run across inline elements (an
 * id's `qt_` and the hex after it are one word); a run kept whole (`white-space: nowrap`) counts as
 * one. Returned: its lines; its words broken across two lines, save where a word may break (after a
 * `/`; at a `<wbr>`, where an identifier divides (scripts/markdown.ts, src/lib/typesetting.ts) or an
 * id's groups of four meet (src/components/ui/hash.tsx), but never between an id's prefix and its
 * hex; at the hyphen of a word wider than the whole line; anywhere in a hash or a base64 key past
 * its prefix); its runs that read as one (a number and its unit, a rate, a date, the product's name)
 * split across lines; and
 * whether its last line is a lone short word that the word before it would have fitted beside,
 * without making two balanced lines less even.
 */
async function typeset(page: Page, selector: string): Promise<{ text: string; lines: string[][]; broken: string[]; split: string[]; lone: boolean }[]> {
  return page.locator(selector).evaluateAll((elements) => elements
    .filter((element) => element.checkVisibility() && element.getBoundingClientRect().width > 2 && element.closest(".sr-only, pre") === null)
    .map((element) => {
      const style = getComputedStyle(element);
      const content = element.getBoundingClientRect().width - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
      // The element's text, character by character (a block inside it adds a space), and where a <wbr> sits.
      const chars: (readonly [Text, number] | null)[] = [];
      const wbr = new Set<number>();
      let text = "";
      const collect = (node: Node) => {
        for (const child of node.childNodes) {
          if (child instanceof Text) {
            for (let offset = 0; offset < child.length; offset += 1) chars.push([child, offset]);
            text += child.data;
          } else if (child instanceof HTMLElement || child instanceof SVGElement) {
            if (child.tagName === "WBR") wbr.add(text.length);
            else if (child.closest(".sr-only") === null && child.checkVisibility() && child.tagName !== "svg") {
              const block = !["inline", "contents"].includes(getComputedStyle(child).display);
              if (block) { chars.push(null); text += " "; }
              collect(child);
              if (block) { chars.push(null); text += " "; }
            }
          }
        }
      };
      collect(element);
      const range = document.createRange();
      const rectsOf = (start: number, end: number) => {
        const first = chars[start];
        const last = chars[end - 1];
        if (first == null || last == null) return [];
        range.setStart(first[0], first[1]);
        range.setEnd(last[0], last[1] + 1);
        return [...range.getClientRects()].filter((rect) => rect.width > 0);
      };
      const half = parseFloat(style.fontSize) * 0.6;
      const lineCount = (rects: DOMRect[]) => {
        const mids = rects.map((rect) => (rect.top + rect.bottom) / 2).sort((a, b) => a - b);
        return mids.filter((mid, index) => index === 0 || mid - (mids[index - 1] ?? 0) >= half).length;
      };
      const unitOf = (index: number) => {
        let unit: Element | null = null;
        for (let up = chars[index]?.[0].parentElement ?? null; up !== null && up !== element.parentElement; up = up.parentElement) {
          if (getComputedStyle(up).textWrapMode === "nowrap") unit = up;
        }
        return unit;
      };
      const words: { text: string; left: number; right: number; mid: number; unit: Element | null }[] = [];
      const broken: string[] = [];
      for (const match of text.matchAll(/\S+/g)) {
        const start = match.index;
        const end = start + match[0].length;
        const rects = rectsOf(start, end);
        const first = rects[0];
        const last = rects[rects.length - 1];
        if (first === undefined || last === undefined) continue;
        if (lineCount(rects) > 1) {
          // Where it breaks: the first character below the first line's middle.
          let at = start + 1;
          while (at < end && (rectsOf(at, at + 1)[0]?.top ?? 0) < (first.top + first.bottom) / 2) at += 1;
          const before = text[at - 1] ?? "";
          const rest = text.slice(at, end);
          const width = rects.reduce((sum, rect) => sum + rect.width, 0);
          const allowed = before === "/" ||
            (wbr.has(at) && !(before === "_" && /^[0-9a-f]{8,}/i.test(rest))) ||
            (before === "-" && width > content) ||
            (/^[("'`]?(0x|[a-z]{2,8}_)?[0-9a-f]{24,}/i.test(match[0]) && /^[0-9a-f]/i.test(rest) && /[0-9a-f]$/i.test(before)) ||
            // A key in base64 (`whpk_…`, a webhook signing key), past its prefix.
            (/^[a-z]{2,8}_[A-Za-z0-9+/]{24,}={0,2}$/.test(match[0]) && !/_$/.test(before));
          if (!allowed) broken.push(`${text.slice(start, at)}|${rest}`);
        }
        const unit = unitOf(start);
        const previous = words[words.length - 1];
        if (previous !== undefined && unit !== null && previous.unit === unit) {
          previous.text += ` ${match[0]}`;
          previous.right = last.right;
          continue;
        }
        words.push({ text: match[0], left: first.left, right: last.right, mid: (last.top + last.bottom) / 2, unit });
      }
      const runs = /~?\$?\d[\d,.]*\s(?:s|ms|min|h|%|PHA|USDC|USDT|ETH)(?![\p{L}\p{N}])|\$?\d[\d,.]* \/ [A-Z]{2,5}\b|\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d{1,2}\b|Phala Pay/gu;
      const split = [...text.matchAll(runs)].filter((match) => lineCount(rectsOf(match.index, match.index + match[0].length)) > 1).map((match) => match[0]);
      // Lines: a word whose middle is below the last line's by more than half a line starts the next.
      const lines: (typeof words)[] = [];
      for (const word of words) {
        const line = lines[lines.length - 1];
        if (line !== undefined && Math.abs(word.mid - (line[0]?.mid ?? 0)) < half) line.push(word);
        else lines.push([word]);
      }
      const widthOf = (line: typeof words) => (line[line.length - 1]?.right ?? 0) - (line[0]?.left ?? 0);
      const last = lines[lines.length - 1] ?? [];
      const before = lines[lines.length - 2]?.at(-1);
      const space = parseFloat(style.fontSize) * 0.3;
      let lone = false;
      if (lines.length >= 2 && last.length === 1 && before !== undefined && widthOf(last) < 0.5 * Math.max(...lines.map(widthOf))) {
        const moved = before.right - before.left + space;
        const fits = moved + widthOf(last) <= content + 0.5;
        const firstWidth = widthOf(lines[0] ?? []);
        const evener = lines.length === 2 && style.textWrapStyle === "balance" && Math.abs(firstWidth - moved - (widthOf(last) + moved)) >= Math.abs(firstWidth - widthOf(last));
        lone = fits && !evener;
      }
      return { text: element.textContent.replace(/\s+/g, " ").trim().slice(0, 60), lines: lines.map((line) => line.map((word) => word.text)), broken, split, lone };
    }));
}

/** Elements in `scope` whose box leaves the nearest container drawn around them (an edge or a fill): a card, a pill, a cell. */
async function outsideTheirBox(page: Page, scope: string): Promise<string[]> {
  return page.locator(scope).evaluateAll((roots) => roots.flatMap((root) => [...root.querySelectorAll<HTMLElement>("*")].flatMap((element) => {
    const position = getComputedStyle(element).position;
    const box = element.getBoundingClientRect();
    if (element.closest("svg, .sr-only") !== null || position === "absolute" || position === "fixed" || box.width <= 2 || !element.checkVisibility()) return [];
    for (let up = element.parentElement; up !== null && up !== root.parentElement; up = up.parentElement) {
      const style = getComputedStyle(up);
      if (style.overflowX !== "visible") return [];
      const drawn = parseFloat(style.borderLeftWidth) > 0 || parseFloat(style.borderRightWidth) > 0 || !/rgba\(.*, 0\)$|transparent/.test(style.backgroundColor);
      if (!drawn || style.display === "inline" || style.display === "contents") continue;
      const edge = up.getBoundingClientRect();
      return box.right > edge.right + 1 || box.left < edge.left - 1 ? [`${element.tagName.toLowerCase()} "${element.textContent.trim().slice(0, 30)}" past ${up.tagName.toLowerCase()}.${[...up.classList].slice(0, 3).join(".")}`] : [];
    }
    return [];
  })));
}

/** Units and short labels: each on one line (a step's title, in a column, up to four words). */
const ONE_LINE = [
  "header nav a", "header [data-slot=button]", "main [data-slot=button]", "[data-facts] dd", "footer nav a",
  `${DEMO_SECTION} [role=tab]`, "[data-slot=segmented-control-item]", "[data-testid=token-price]", "[data-testid=token-bonus]",
  "[data-slot=status-badge]", "[data-step] button > span:nth-child(2) > span:first-child", "[data-testid=payment-credited] dt",
  ".docs-prose .unbroken", ".docs-prose th",
];
/** Headings and the like: never a lone short word on a last line that its neighbour would have fitted. */
const HEADINGS = "h1, h2, h3, h4, summary > span, [data-feature] p, figure ol p, main p[class*=text-lead]";
/** Rows of siblings that wrap alike: in each row (one top), as many lines each. */
const ROWS = ["header nav a", "[data-facts] dt", "[data-feature] h3", "footer nav > p", "figure ol p.text-heading", "#fit h3", `${DEMO_SECTION} [role=tab]`];
/** All the text there is to set, in `scope`. */
const TEXT = (scope: string) => `${scope} :is(p, li, dt, dd, td, th, h1, h2, h3, h4, h5, figcaption, summary, code, label, button, a)`;
/** The widths measured: a phone's (the narrowest included), a tablet's, the desktops'. */
const TYPE_WIDTHS = [320, 360, 390, 768, 1024, 1280, 1440, 1920];
/**
 * At 320px, what may not fit, each for its reason: a vendor's three-word name in a column of the
 * comparison half a phone wide; a step's name of four words and its menu's chevron (about 167px)
 * beside its time (162px beside "+0 s", 130px beside "since" over a time). From 360px each fits.
 */
const AT_320 = ["Stripe stablecoin payments", "Received by Phala Pay", "Swept to the treasury"];

/** Every check of the text in `scope` at the page's current width. */
async function expectTypeset(page: Page, where: string, width: number, scope = "") {
  const allowed = (text: string) => width <= 320 && AT_320.some((each) => text.includes(each));
  for (const selector of ONE_LINE) {
    const short = selector.startsWith("[data-step]");
    const wrapped = (await typeset(page, `${scope} ${selector}`.trim()))
      .filter(({ lines, text }) => lines.length > 1 && !allowed(text) && (!short || lines.flat().length <= 4))
      .map(({ lines }) => lines.map((line) => line.join(" ")).join(" / "));
    expect(wrapped, `${where}: ${selector} on more than one line`).toEqual([]);
  }
  const text = await typeset(page, scope === "" ? `${TEXT("main")}, ${TEXT("footer")}` : TEXT(scope));
  expect(text.flatMap(({ broken }) => broken), `${where}: words broken across lines`).toEqual([]);
  expect(text.flatMap(({ split }) => split), `${where}: runs split across lines`).toEqual([]);
  expect((await typeset(page, HEADINGS.split(", ").map((each) => `${scope} ${each}`.trim()).join(", "))).filter(({ lone }) => lone).map(({ text }) => text), `${where}: a lone word ending a heading`).toEqual([]);
  for (const selector of ROWS) {
    const counts = await page.locator(`${scope} ${selector}`.trim()).evaluateAll((elements) => elements.filter((element) => element.checkVisibility()).map((element) => {
      const range = document.createRange();
      range.selectNodeContents(element);
      const half = parseFloat(getComputedStyle(element).fontSize) * 0.6;
      const mids = [...range.getClientRects()].filter((rect) => rect.width > 0).map((rect) => (rect.top + rect.bottom) / 2).sort((a, b) => a - b);
      return { top: Math.round(element.getBoundingClientRect().top), text: element.textContent, lines: mids.filter((mid, index) => index === 0 || mid - (mids[index - 1] ?? 0) >= half).length };
    }));
    for (const top of new Set(counts.map(({ top }) => top))) {
      const row = counts.filter((each) => each.top === top && !allowed(each.text)).map(({ lines }) => lines);
      expect(new Set(row).size, `${where}: ${selector} in a row, lines ${row.join(", ")}`).toBeLessThanOrEqual(1);
    }
  }
  expect(await outsideTheirBox(page, DEMO_SECTION), `${where}: the demo's boxes past their containers`).toEqual([]);
}

test("type sets well: units and labels on one line, no word split, no heading ending on a lone word, rows wrap alike", async ({ page }) => {
  // The owner's regression: text that wraps where it should not. At a phone's widths (the narrowest
  // too), a tablet's, and the desktops', on the marketing pages and the docs (their identifiers and
  // tables), none scrolling sideways; the API reference, too large to set word by word, for that alone.
  test.setTimeout(600_000);
  for (const path of ["", "compare", "no-such-page/deeper", "docs/integration", "docs/sdk/server", "reference"]) {
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(new URL(path, env("SITE_URL")).href);
    if (path === "") await expect(page.getByRole("region", { name: "Customer view" }).getByTestId("balance")).toHaveText("$0.00");
    for (const width of TYPE_WIDTHS) {
      await page.setViewportSize({ width, height: 900 });
      const where = `/${path} at ${width}px`;
      expect(await page.evaluate(() => document.documentElement.scrollWidth), `${where}: scrolls sideways`).toBe(width);
      if (path !== "reference") await expectTypeset(page, where, width);
    }
  }
});

test("the demo sets its text well while it loads, while paying, once credited, and once final, at every width", async ({ browser, page }) => {
  test.setTimeout(600_000);
  // While its data loads (the API held): the placeholders keep to their boxes, and the page does not scroll sideways.
  const loading = await browser.newPage();
  try {
    await loading.route(`${env("API_URL")}/**`, () => undefined);
    await loading.goto(env("SITE_URL"));
    for (const width of TYPE_WIDTHS) {
      await loading.setViewportSize({ width, height: 900 });
      expect(await loading.evaluate(() => document.documentElement.scrollWidth), `loading at ${width}px: scrolls sideways`).toBe(width);
      expect(await outsideTheirBox(loading, DEMO_SECTION), `loading at ${width}px`).toEqual([]);
    }
  } finally {
    await loading.close();
  }
  // Credited: the receipt, the timeline, the payment's row and its API requests, on their tabs.
  await installWallet(page);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Customer view" });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  await expect(product.getByTestId("balance")).toHaveText(/^\$/);
  await page.getByRole("note", { name: "Test tokens" }).getByRole("button", { name: /^Mint 1,000 test PHA/ }).click();
  await expect(page.getByRole("note", { name: "Test tokens" })).toContainText("Minted:");
  // Each state on each of the backend's tabs, at every width: every list shown in full (the API's
  // requests, the quote's id in its path among them).
  const pass = async (state: string) => {
    for (const tab of ["Credits", "API", "Trust", "Sweeps"]) {
      await openTab(scenes, tab);
      await showAll(scenes);
      for (const width of TYPE_WIDTHS) {
        await page.setViewportSize({ width, height: 900 });
        await expectTypeset(page, `${state}, ${tab} tab, at ${width}px`, width, DEMO_SECTION);
      }
      await page.setViewportSize({ width: 1440, height: 900 });
    }
  };
  // Paying: the checkout, and the stepper waiting for the transfer.
  await product.getByRole("button", { name: PAY }).click();
  await expect(product.locator(".pp-summary")).toContainText("PHA");
  await pass("paying");
  // Credited: the receipt, the payment's row, its webhook and requests.
  await product.getByRole("button", { name: "Pay with Test Wallet" }).click();
  await expect(product.getByTestId("payment-credited")).toBeVisible({ timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toBeVisible({ timeout: 10_000 });
  await openTab(scenes, "API");
  await expect(scenes.getByTestId("webhook-event").first()).toBeVisible({ timeout: 30_000 });
  await pass("credited");
  // Final and waiting for the sweep: the stepper's last step "since" the final time, beside the
  // longest of its names ("Swept to the treasury").
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  await expectComplete(timeline, ["final"]);
  await expect(step(timeline, "swept")).toHaveAttribute("data-state", "current");
  await pass("final, awaiting the sweep");
});

test("the feature cards side by side share their rules and their titles' lines", async ({ page }) => {
  // Each card's figure over a rule, its title under it: in a row of cards, every rule and every
  // title on one line, whatever each figure's height (the cards are subgrids of the row).
  await page.goto(env("SITE_URL"));
  for (const width of [1440, 1280, 1024]) {
    await page.setViewportSize({ width, height: 900 });
    const cards = await page.locator("[data-feature]").evaluateAll((elements) => elements.map((card) => ({
      top: Math.round(card.getBoundingClientRect().top),
      rule: card.querySelector("[data-feature-well]")?.getBoundingClientRect().bottom ?? 0,
      title: card.querySelector("h3")?.getBoundingClientRect().top ?? 0,
    })));
    expect(cards, `${width}px`).toHaveLength(4);
    const tops = [...new Set(cards.map(({ top }) => top))];
    expect(tops, `${width}px: rows of cards`).toHaveLength(2);
    for (const row of tops.map((top) => cards.filter((card) => card.top === top))) {
      expect(row, `${width}px: cards in a row`).toHaveLength(2);
      const [first, second] = row;
      expect(Math.abs((first?.rule ?? 0) - (second?.rule ?? 0)), `${width}px: rules`).toBeLessThanOrEqual(1);
      expect(Math.abs((first?.title ?? 0) - (second?.title ?? 0)), `${width}px: titles`).toBeLessThanOrEqual(1);
    }
  }
});

test("the custody rail moves only where motion is welcome", async ({ browser }) => {
  for (const reducedMotion of ["no-preference", "reduce"] as const) {
    const context = await browser.newContext({ reducedMotion, viewport: { width: 1440, height: 900 } });
    try {
      const page = await context.newPage();
      await page.goto(env("SITE_URL"));
      const running = await page.locator("[data-rail-segment]").evaluateAll((segments) =>
        segments.map((segment) => segment.getAnimations().length));
      expect(running, reducedMotion).toEqual(reducedMotion === "reduce" ? [0, 0] : [1, 1]);
    } finally {
      await context.close();
    }
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
  // A doc title that already names the product is the page title as is; others get the suffix.
  const titles = { "docs": "Phala Pay documentation", "docs/integration": "Integration guide | Phala Pay docs" } as const;
  for (const [path, title] of docs) {
    const response = await page.goto(new URL(path, env("SITE_URL")).href);
    expect(response?.status(), path).toBe(200);
    expect(await response?.text(), path).not.toContain('style="');
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(title);
    if (path in titles) await expect(page).toHaveTitle(titles[path as keyof typeof titles]);
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
