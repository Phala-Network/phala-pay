import { expect, test, type Locator, type Page } from "@playwright/test";
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
import { EXPIRED_QUOTE_INTERVAL_MS, QUERY_RETRY_LIMIT, TIMELINE_ACTIVE_INTERVAL_MS, TIMELINE_INTERVAL_MS } from "../src/polling.js";

declare global {
  interface Window {
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

/** Opens a step's line to its hint, time, and data (closed lines render none). */
async function openStep(timeline: Locator, key: string): Promise<Locator> {
  const line = step(timeline, key);
  const trigger = line.getByRole("button").first();
  if ((await trigger.getAttribute("aria-expanded")) !== "true") {
    await trigger.click();
  }
  await expect(trigger).toHaveAttribute("aria-expanded", "true");
  return line;
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

/** Declares a refund of `amount` PHA of the selected deposit and returns its list item. */
async function declareRefund(scenes: Locator, amount: string): Promise<Locator> {
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
  await expect(product).toContainText("Account is unavailable right now; retrying…", { timeout: 15_000 });
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const trust = await openTab(scenes, "Trust");
  await expect(trust).toContainText("Trust information is unavailable right now; retrying…");
  await expect(trust).toContainText("Networks are unavailable right now; retrying…");

  available = true;
  const recovered = page.waitForResponse((response) => response.url().endsWith("/api/account") && response.status() === 200);
  await page.evaluate(() => window.dispatchEvent(new Event("visibilitychange")));
  await recovered;
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
  let timelineReads = 0;
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    timelineReads += 1;
    await route.fulfill({
      status: 404,
      json: { code: "not_found" },
      headers: { "access-control-allow-origin": new URL(env("SITE_URL")).origin, "access-control-allow-credentials": "true" },
    });
  });
  await page.goto(env("SITE_URL"));
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  await expect(scenes).toContainText("Timeline is unavailable for this request.");
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  await expect(scenes.getByTestId("stream-status")).toHaveText("Unavailable");
  await expect(scenes.getByRole("list", { name: /^Loading/ })).toHaveCount(0);
  const reads = timelineReads;
  await page.clock.runFor(3 * TIMELINE_INTERVAL_MS);
  expect(timelineReads).toBe(reads);
});

test("a refused timeline keeps its cached data and shows paused updates", async ({ page }) => {
  let missing = false;
  let reads = 0;
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    reads += 1;
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  await expect(step(timeline, "quote_created")).toHaveAttribute("data-state", "complete");
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  missing = true;
  const response = page.waitForResponse((response) =>
    new URL(response.url()).pathname.startsWith("/api/quotes/") && response.status() === 404,
  );
  await page.clock.runFor(TIMELINE_INTERVAL_MS);
  await response;
  await expect(scenes.getByText("Updates paused.", { exact: true })).toBeVisible();
  await expect(step(timeline, "quote_created")).toHaveAttribute("data-state", "complete");
  const paused = reads;
  await page.clock.runFor(3 * TIMELINE_INTERVAL_MS);
  expect(reads).toBe(paused);
});

for (const sent of [false, true]) {
  test(`a quote past its deadline polls ${sent ? "in-flight transfers" : "slowly for late payments"}`, async ({ page }) => {
    let reads = 0;
    await page.clock.install();
    await page.route("**/api/quotes/*", async (route) => {
      reads += 1;
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
    const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
    await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
    await expect(page.getByRole("list", { name: "Payment timeline" })).toBeVisible();
    await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
    const initial = reads;
    const request = page.waitForRequest("**/api/quotes/*");
    const response = page.waitForResponse(async (response) => response.request() === await request);
    if (sent) {
      await page.clock.runFor(TIMELINE_ACTIVE_INTERVAL_MS);
    } else {
      await page.clock.runFor(TIMELINE_INTERVAL_MS);
      expect(reads).toBe(initial);
      await page.clock.runFor(EXPIRED_QUOTE_INTERVAL_MS - TIMELINE_INTERVAL_MS);
    }
    await response;
    expect(reads).toBeGreaterThan(initial);
  });
}

test("a swept payment keeps polling until its webhook arrives", async ({ page }) => {
  let delivered = false;
  let reads = 0;
  await page.clock.install();
  await page.route("**/api/quotes/*", async (route) => {
    reads += 1;
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  await expect(step(scenes, "webhook_received")).toHaveAttribute("data-state", "current");
  await expect(scenes.getByTestId("stream-status")).toHaveText("Live");
  await expect(product.getByRole("tab", { name: "QR code", exact: true })).toBeVisible();
  delivered = true;
  const request = page.waitForRequest("**/api/quotes/*");
  const response = page.waitForResponse(async (response) => response.request() === await request);
  await page.clock.runFor(TIMELINE_INTERVAL_MS);
  await response;
  await expect(step(scenes, "webhook_received")).toHaveAttribute("data-state", "complete");
  await expect(scenes.getByTestId("stream-status")).toHaveText("Done");
  const stopped = reads;
  await page.clock.runFor(3 * TIMELINE_INTERVAL_MS);
  expect(reads).toBe(stopped);
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
    "https://github.com/Phala-Network/phala-pay/blob/main/docs/self-hosting.md#one-command-deploy",
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
    "https://github.com/Phala-Network/phala-pay#documentation",
  );
  await expect(page.getByRole("navigation", { name: "Site" }).getByRole("link", { name: "Self-hosting" })).toHaveAttribute(
    "href",
    "https://github.com/Phala-Network/phala-pay/blob/main/docs/self-hosting.md",
  );
  await expectMetadata(page);
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
  await expect(product.getByTestId("testnet-badge")).toHaveText("Testnet");
  const scenes = page.getByRole("complementary", { name: "Your backend" });
  const preview = scenes.getByRole("list", { name: "The steps of a payment" });
  await expect(preview).toBeVisible();
  // The credit's expected time is the chain's typical_credit_seconds, from GET /v1/config.
  await expect(step(preview, "credited")).toContainText("usually ~30\u00a0s");
  const trust = await openTab(scenes, "Trust");
  await expect(trust).toContainText("Attestation verified");
  await expect(trust).toContainText("Verified");
  await expect(trust).toContainText("e2e0000000000000000000000000000000000001");
  await expect(product.getByTestId("balance")).toHaveText("$0.00");
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
  // The quote's locked rate; the SDK's status line holds the one countdown.
  const rate = product.getByTestId("locked-rate");
  await expect(rate).toContainText("Locked rate · Test PHA");
  await expect(rate).toContainText("1 PHA = $0.25");
  await expect(product.getByLabel("Time left to pay")).toHaveText(/^1[45]:\d\d$/);
  await expect(product.getByText(/\d+:\d\d$/)).toHaveCount(1);
  const timeline = scenes.getByRole("list", { name: "Payment timeline" });
  await expect(step(timeline, "quote_created")).toHaveAttribute("data-state", "complete");
  await expect(step(timeline, "sent")).toHaveAttribute("data-state", "current");
  const created = await openStep(timeline, "quote_created");
  await expect(created).toContainText("1 PHA = $0.25");
  await expect(created).toContainText("80 PHA");
  const order = (await scenes.getByTestId("meta-order").getAttribute("title"))?.match(/^order_[0-9a-f]{12}$/)?.[0];
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
  // Nothing pending once credited: the locked rate is gone with the countdown.
  await expect(rate).toHaveCount(0);
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
  await openStep(timeline, "webhook_received");
  await expect(step(timeline, "webhook_received").getByText("data.object.metadata")).toBeVisible();
  await expect(step(timeline, "webhook_received")).toContainText("verified");
  await expect(step(timeline, "webhook_received")).toContainText(`"order_id": "${order ?? ""}"`);
  await expect(step(timeline, "webhook_received")).toContainText("+$20.00");
  await openTab(scenes, "API");
  await expect(scenes.getByTestId("webhook-event").first()).toContainText("deposit.credited");
  // Refunds wait for finality.
  await openTab(scenes, "Refunds");
  await expect(scenes.getByTestId("refund-unavailable")).toContainText("deposit_not_final");
  await expectComplete(timeline, ["final"]);
  await expect(step(timeline, "swept")).toHaveAttribute("data-state", "current");

  // The merchant sweeps: the SDK's flush, signed from a wallet (anyone may send it; the funds can
  // only reach the treasury), indexed by the service once final.
  const sweeps = (await openTab(scenes, "Sweeps")).getByRole("region", { name: "PHA on Sepolia testnet" });
  await expect(sweeps.getByTestId("unswept")).toContainText("80 PHA in 1 forwarder", { timeout: 30_000 });
  await sweeps.getByRole("button", { name: "Sweep to treasury from my wallet" }).click();
  await expect(sweeps.getByTestId("flush-status")).toContainText("Flush sent: 0x");
  await expectComplete(timeline, ["swept"]);
  expect(await tokenBalance(env("TREASURY"))).toBe(parseEther("80"));
  await expect((await openStep(timeline, "swept")).locator("a").first()).toHaveAttribute(
    "href",
    /^https:\/\/sepolia\.etherscan\.io\/tx\/0x[0-9a-f]{64}$/,
  );
  await expect(sweeps.getByTestId("sweep")).toContainText("80 PHA", { timeout: 30_000 });

  // A refund paid from the treasury succeeds: 20 of 80 PHA takes back a quarter of the credit.
  const paid = await declareRefund(scenes, "20");
  await expect(paid.getByTestId("refund-transfer")).toContainText(env("TREASURY").toLowerCase());
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
  await openTab(scenes, "API");
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

  // The history row and the ledger lines behind the balance.
  await openTab(scenes, "Credits");
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
  await page.getByRole("button", { name: "Switch to dark theme" }).click();
  await page.screenshot({ path: testInfo.outputPath("refunds-dark.png"), fullPage: true });
  await page.setViewportSize({ width: 420, height: 900 });
  await page.screenshot({ path: testInfo.outputPath("mobile-dark.png"), fullPage: true });

  // A quote that ends unpaid, expired or canceled, drops its locked rate too.
  await page.setViewportSize({ width: 1360, height: 1000 });
  const followed = scenes.locator('[title^="qt_"]');
  for (const [end, message] of [
    ["expire", "Quote expired"],
    ["cancel", "Quote canceled"],
  ] as const) {
    const before = await followed.getAttribute("title");
    await product.getByRole("button", { name: "Add more credits" }).click();
    await product.getByText("$5", { exact: true }).click();
    await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
    await expect(rate).toContainText("1 PHA = $0.25");
    await expect(followed).not.toHaveAttribute("title", before ?? "");
    const quote = (await followed.getAttribute("title")) ?? "";
    const ended = await fetch(`${env("SERVICE_URL")}/_test/quotes/${quote}/${end}`, { method: "POST" });
    expect(ended.status).toBe(200);
    await expect(product.getByRole("status").first()).toContainText(message, { timeout: 10_000 });
    await expect(rate).toHaveCount(0);
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
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
  const address = (await scenes.getByTestId("deposit-address").textContent()) ?? "";
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
    await expect(payment.getByRole("button", { name: /^View/ })).toHaveAttribute("aria-pressed", "true");
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
  await openStep(timeline, "credited");
  await openStep(timeline, "webhook_received");
  await expect(step(timeline, "credited")).toContainText("spot");
  await expect(step(timeline, "credited")).toContainText("$6.25");
  await expect(step(timeline, "webhook_received")).toContainText('"workspace": "demo-');
  await expect(step(timeline, "webhook_received")).toContainText("+$6.25");
  await expect(step(timeline, "reversed")).toHaveAttribute("data-state", "failed", { timeout: 30_000 });
  await expect(product.getByTestId("balance")).toHaveText("$0.00", { timeout: 10_000 });
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
  await openTab(scenes, "Credits");
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
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
  await expect(product.getByTestId("locked-rate")).toContainText("1 USDC = $1.00");
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
  await product.getByRole("button", { name: "Add more credits" }).click();
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
  await expect(product.getByTestId("locked-rate")).toContainText("1 PHA = $0.06041");
  await expect(product.getByTestId("testnet-badge")).toBeVisible();
  await product.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(product.getByTestId("payment-credited")).toContainText("$20.00", { timeout: 60_000 });
  await expect(product.getByTestId("bonus-credited")).toContainText("+$2.00", { timeout: 10_000 });
  await expect(product.getByTestId("balance")).toHaveText("$27.00", { timeout: 10_000 });
  await page.getByRole("button", { name: "Switch to dark theme" }).click();
  await page.screenshot({ path: testInfo.outputPath("base-bonus-dark.png") });
  await page.getByRole("button", { name: "Switch to light theme" }).click();

  // $5 in test USDT on Base Sepolia, minted through the faucet, at $1.00 with no bonus. Its
  // `transfer` returns nothing, as Tether's does on Ethereum, and the checkout pays it all the same.
  const usdt = { rpc: env("BASE_ANVIL_URL"), token: env("BASE_USDT_ADDRESS") };
  await helper.getByRole("button", { name: "Mint 1,000 test USDT" }).click();
  await expect.poll(() => tokenBalance(env("PAYER_ADDRESS"), usdt)).toBe(parseUnits("1000", 6));
  await product.getByRole("button", { name: "Add more credits" }).click();
  await product.getByRole("radio", { name: "Test USDT", exact: true }).check({ force: true });
  await product.getByText("$5", { exact: true }).click();
  const usdtRequest = page.waitForRequest((r) => r.method() === "POST" && r.url() === `${env("API_URL")}/api/quotes`);
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  expect((await usdtRequest).postDataJSON()).toEqual({ amount: 500, chain_id: baseSepolia.id, asset: "usdt" });
  await expect(product.getByTestId("locked-rate")).toContainText("1 USDT = $1.00");
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
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
  await product.getByRole("button", { name: "Add more credits" }).click();
  await product.getByRole("radio", { name: "Test PHA", exact: true }).check({ force: true });
  await product.getByText("Custom", { exact: true }).click();
  await product.getByLabel("Custom amount (USD)").fill("500");
  await product.getByRole("button", { name: "Pay with crypto", exact: true }).click();
  await expect(product.getByTestId("locked-rate")).toContainText("1 PHA = $0.25");
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
  const product = page.getByRole("region", { name: "Acme Cloud · Billing" });
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
    await expect(staticPage.getByRole("heading", { level: 2 })).toHaveCount(6);
    await expect(staticPage.getByRole("heading", { name: "Which chains and tokens are supported?" })).toBeVisible();
    await expectMetadata(staticPage);
    await expect(staticPage.getByRole("button", { name: "Menu", exact: true })).toHaveCount(0);
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Compare", exact: true })).toBeVisible();
    await expect(staticPage.getByRole("contentinfo").getByRole("link", { name: "Demo", exact: true })).toHaveAttribute("href", "/#demo");
    const homeHtml = await home?.text();
    expect(homeHtml).not.toContain('style="');
    const compare = await staticPage.goto(new URL("compare", env("SITE_URL")).href);
    expect(compare?.status()).toBe(200);
    await expect(staticPage.getByRole("heading", { level: 1 })).toHaveText("How Phala Pay compares");
    await expect(staticPage.getByRole("table")).toBeVisible();
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
  await page.getByRole("button", { name: "Switch to dark theme" }).click();
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
    await expect(page.getByRole("region", { name: "Acme Cloud · Billing" })).toBeVisible();
    expect(await original.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalHeader.evaluate((node) => node.isConnected)).toBe(true);
    expect(await originalFooter.evaluate((node) => node.isConnected)).toBe(true);
    await page.getByRole("button", { name: "Switch to dark theme" }).click();
    await expect(page.locator("html")).toHaveClass(/dark/);
    await expect(page.getByRole("region", { name: "Acme Cloud · Billing" })).toBeVisible();
  } finally {
    releaseEntry?.();
  }
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
        if (path === "") await expect(page.getByRole("region", { name: "Acme Cloud · Billing" })).toBeVisible();
        const next = colorScheme === "dark" ? "light" : "dark";
        await page.getByRole("button", { name: `Switch to ${next} theme` }).click();
        await expect(page.locator("html")).toHaveClass(next === "dark" ? /dark/ : /^$/);
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
