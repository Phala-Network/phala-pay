import { expect, test, type Page } from "@playwright/test";
import jsqr from "jsqr";
import {
  createPublicClient,
  erc20Abi,
  getAddress,
  http,
  parseEther,
  type Address,
} from "viem";
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts";
import { sepolia } from "viem/chains";
import type { ClientQuote } from "../src/index.js";

// jsqr is CommonJS; its function is `module.exports.default`.
const jsQR = jsqr.default;
const API_BASE = "http://topup.test";

declare global {
  interface Window {
    anvilRequest(
      method: string,
      params: unknown,
    ): Promise<{ result: unknown } | { error: { code: number; message: string } }>;
    walletCalls: string[];
    testWallet: unknown;
  }
}

function env(name: string): string {
  const value = process.env[name];
  if (value === undefined) {
    throw new Error(`${name} is not set; global setup did not run`);
  }
  return value;
}

const chain = () => createPublicClient({ chain: sepolia, transport: http(env("ANVIL_URL")) });

/** A fresh quote for a new deposit address, payable in the test token. */
function newQuote(overrides: Partial<ClientQuote> = {}): { quote: ClientQuote; secret: string } {
  const id = `qt_${crypto.randomUUID().replaceAll("-", "")}`;
  const address = privateKeyToAccount(generatePrivateKey()).address;
  const amount = parseEther("12.345678901234567891").toString();
  const token = env("TOKEN_ADDRESS");
  return {
    secret: `${id}_secret_${"5e".repeat(24)}`,
    quote: {
      id,
      object: "quote",
      livemode: false,
      status: "open",
      amount: 2500,
      currency: "usd",
      asset: "pha",
      decimals: 18,
      chain_id: sepolia.id,
      amount_atomic: amount,
      address,
      payment_uri: `ethereum:${token}@${sepolia.id}/transfer?address=${address}&uint256=${amount}`,
      expires_at: Math.floor(Date.now() / 1000) + 900,
      payment_status: "none",
      confirmations: null,
      amount_credited: null,
      typical_credit_seconds: 30,
      ...overrides,
    },
  };
}

/**
 * Serves the quote's public view the way the service does: 404 without the client secret, and the
 * payment status read from the chain (`seen` on the first read after the transfer, then `credited`).
 */
async function serveQuote(page: Page, quote: ClientQuote, secret: string) {
  const requests: { method: string; headers: string[] }[] = [];
  let reads = 0;
  await page.route(`${API_BASE}/v1/quotes/**`, async (route) => {
    const request = route.request();
    requests.push({ method: request.method(), headers: Object.keys(request.headers()) });
    const url = new URL(request.url());
    const cors = { "access-control-allow-origin": "*" };
    if (url.pathname !== `/v1/quotes/${quote.id}` || url.searchParams.get("client_secret") !== secret) {
      await route.fulfill({ status: 404, headers: cors, json: { error: { type: "invalid_request_error", code: "resource_missing" } } });
      return;
    }
    const balance = await chain().readContract({
      address: getAddress(env("TOKEN_ADDRESS")),
      abi: erc20Abi,
      functionName: "balanceOf",
      args: [quote.address as Address],
    });
    const paid = balance === BigInt(quote.amount_atomic);
    const body: ClientQuote = !paid
      ? quote
      : reads++ === 0
        ? { ...quote, payment_status: "seen", confirmations: 1 }
        : { ...quote, status: "complete", payment_status: "credited", amount_credited: quote.amount };
    await route.fulfill({ status: 200, headers: cors, json: body });
  });
  return requests;
}

/**
 * An EIP-6963 wallet backed by Anvil: it holds the payer account, starts on Ethereum mainnet, does
 * not know Sepolia until asked to add it, and forwards every other call to the node.
 */
async function installWallet(page: Page) {
  const rpc = env("ANVIL_URL");
  await page.exposeFunction("anvilRequest", async (method: string, params: unknown) => {
    const response = await fetch(rpc, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params: params ?? [] }),
    });
    const body = (await response.json()) as { result?: unknown; error?: { code: number; message: string } };
    return body.error === undefined ? { result: body.result } : { error: body.error };
  });
  await page.addInitScript(
    ({ account, chainId }) => {
      window.walletCalls = [];
      let current = 1;
      const known = new Set([1]);
      const fail = (code: number, message: string) => Object.assign(new Error(message), { code });
      const provider = {
        async request({ method, params }: { method: string; params?: unknown }): Promise<unknown> {
          window.walletCalls.push(method);
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
          if (current !== chainId) {
            throw fail(4901, "wallet is on another chain");
          }
          const answer = await window.anvilRequest(method, params);
          if ("error" in answer) {
            throw fail(answer.error.code, answer.error.message);
          }
          return answer.result;
        },
        on: () => undefined,
        removeListener: () => undefined,
      };
      window.testWallet = provider;
      const info = {
        uuid: "7f2b7a2c-2f5a-4d7e-9a0e-5b5c1a7d3e10",
        name: "Test Wallet",
        icon: "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'/%3E",
        rdns: "test.wallet",
      };
      const announce = () =>
        window.dispatchEvent(
          new CustomEvent("eip6963:announceProvider", { detail: Object.freeze({ info, provider }) }),
        );
      window.addEventListener("eip6963:requestProvider", announce);
      announce();
    },
    { account: env("PAYER_ADDRESS"), chainId: sepolia.id },
  );
}

test("retains checkout through a three-minute gateway outage and reconnects", async ({ page }) => {
  const { quote, secret } = newQuote();
  let online = true;
  await page.route(`${API_BASE}/v1/quotes/**`, (route) => route.fulfill({
    status: online ? 200 : 502,
    headers: { "access-control-allow-origin": "*" },
    ...(online ? { json: quote } : { body: "Bad Gateway" }),
  }));
  await page.clock.install();
  await page.goto(`/?client_secret=${secret}&expected_address=${quote.address}&api_base=${API_BASE}`);
  await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
  await page.getByRole("tab", { name: "Manual transfer" }).click();
  const details = await page.locator(".pp-fields").textContent();
  await page.clock.pauseAt(new Date(Date.now() + 1000));
  online = false;
  await page.clock.runFor(180000);
  await expect(page.getByRole("status")).toContainText("reconnecting…");
  await expect(page.locator(".pp-status")).toHaveAttribute("data-tone", "neutral");
  await expect(page.locator(".pp-fields")).toHaveText(details ?? "");
  await expect(page.getByTestId("events")).toBeEmpty();
  online = true;
  await page.clock.fastForward(30000);
  await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
});

test("pays a quote from a browser wallet, end to end on Anvil", async ({ page }) => {
  const { quote, secret } = newQuote();
  const requests = await serveQuote(page, quote, secret);
  await installWallet(page);
  const before = await chain().readContract({
    address: getAddress(env("TOKEN_ADDRESS")),
    abi: erc20Abi,
    functionName: "balanceOf",
    args: [getAddress(env("PAYER_ADDRESS"))],
  });

  await page.goto(`/?client_secret=${secret}&expected_address=${quote.address}&api_base=${API_BASE}`);
  await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
  await expect(page.getByText("12.345678901234567891 PHA").first()).toBeVisible();

  await page.getByRole("button", { name: "Pay with crypto (Test Wallet)" }).click();
  await expect(page.getByText(/^Transaction sent:/)).toBeVisible();
  await expect(page.getByRole("status")).toHaveText("Payment credited: $25.00");
  await expect(page.getByTestId("events")).toHaveText("success");

  const token = { address: getAddress(env("TOKEN_ADDRESS")), abi: erc20Abi, functionName: "balanceOf" } as const;
  expect(await chain().readContract({ ...token, args: [quote.address as Address] })).toBe(
    BigInt(quote.amount_atomic),
  );
  expect(await chain().readContract({ ...token, args: [getAddress(env("PAYER_ADDRESS"))] })).toBe(
    before - BigInt(quote.amount_atomic),
  );

  const calls = await page.evaluate(() => window.walletCalls);
  expect(calls).toContain("wallet_addEthereumChain");
  expect(calls.filter((m) => m === "eth_sendTransaction")).toHaveLength(1);
  // Only simple GETs: no custom header that would need a CORS preflight.
  const safelisted = /^(accept|accept-language|referer|user-agent|origin|sec-.*)$/;
  expect(requests.every((r) => r.method === "GET" && r.headers.every((h) => safelisted.test(h)))).toBe(true);
});

test("pays with the page's own viem wallet client instead of discovered wallets", async ({ page }) => {
  const { quote, secret } = newQuote();
  await serveQuote(page, quote, secret);
  await installWallet(page);

  await page.goto(`/?client_secret=${secret}&expected_address=${quote.address}&api_base=${API_BASE}&wallet_client=${env("PAYER_ADDRESS")}`);
  const panel = page.getByRole("tabpanel");
  await expect(panel.getByRole("button")).toHaveCount(1);
  await panel.getByRole("button", { name: "Pay with crypto" }).click();
  await expect(page.getByRole("status")).toHaveText("Payment credited: $25.00");

  const token = { address: getAddress(env("TOKEN_ADDRESS")), abi: erc20Abi, functionName: "balanceOf" } as const;
  expect(await chain().readContract({ ...token, args: [quote.address as Address] })).toBe(
    BigInt(quote.amount_atomic),
  );
  const calls = await page.evaluate(() => window.walletCalls);
  expect(calls).not.toContain("eth_requestAccounts");
  expect(calls).toContain("wallet_switchEthereumChain");
});

test("shows the payment request as a scannable EIP-681 QR code", async ({ page }) => {
  const { quote, secret } = newQuote();
  await serveQuote(page, quote, secret);
  await page.goto(`/?client_secret=${secret}&expected_address=${quote.address}&api_base=${API_BASE}`);
  await page.getByRole("tab", { name: "QR code" }).click();

  const qr = page.getByRole("img", { name: /Payment request for 12.345678901234567891 PHA/ });
  await expect(qr).toBeVisible();
  const [viewBox, path] = await Promise.all([
    qr.getAttribute("viewBox"),
    qr.locator("path").getAttribute("d"),
  ]);
  expect(decodeQr(Number(viewBox?.split(" ")[2]), path ?? "")).toBe(quote.payment_uri);
});

test("lists the manual payment details with working copy buttons", async ({ page, context }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const { quote, secret } = newQuote();
  await serveQuote(page, quote, secret);
  await page.goto(`/?client_secret=${secret}&expected_address=${quote.address}&api_base=${API_BASE}`);
  await page.getByRole("tab", { name: "Manual transfer" }).click();

  const panel = page.getByRole("tabpanel");
  await expect(panel.getByText("Sepolia (chain ID 11155111)")).toBeVisible();
  await expect(panel.getByText(quote.address)).toBeVisible();
  await expect(page.getByLabel("Time left to pay")).toHaveText(/^\d+:\d{2}$/);

  await panel.getByRole("button", { name: "Copy Send to address" }).click();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(quote.address);
  await panel.getByRole("button", { name: "Copy Exact amount" }).click();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe("12.345678901234567891");
});

test("hides the address once the quote expires", async ({ page }) => {
  const { quote, secret } = newQuote({ expires_at: Math.floor(Date.now() / 1000) + 2 });
  await serveQuote(page, quote, secret);
  await page.goto(`/?client_secret=${secret}&expected_address=${quote.address}&api_base=${API_BASE}`);
  await expect(page.getByRole("status")).toHaveText("Waiting for your payment");
  await expect(page.getByRole("status")).toHaveText(/expired. Do not send funds/, { timeout: 10_000 });
  await expect(page.getByText(quote.address)).toHaveCount(0);
  await expect(page.getByTestId("events")).toHaveText("expire");
});

test("reports an invalid client secret", async ({ page }) => {
  const { quote } = newQuote();
  await serveQuote(page, quote, "another");
  await page.goto(`/?client_secret=${quote.id}_secret_00&expected_address=${quote.address}&api_base=${API_BASE}`);
  await expect(page.getByRole("status")).toHaveText("This payment link is not valid. Start a new top-up.");
});

test("fails closed when the quote's address is not the expected one", async ({ page }) => {
  const { quote, secret } = newQuote();
  await serveQuote(page, quote, secret);
  const other = privateKeyToAccount(generatePrivateKey()).address;
  await page.goto(`/?client_secret=${secret}&expected_address=${other}&api_base=${API_BASE}`);
  await expect(page.getByRole("status")).toHaveText(
    "This payment address could not be verified. Do not send funds; contact support.",
  );
  await expect(page.getByText(quote.address)).toHaveCount(0);
  await expect(page.getByRole("button", { name: /Pay with crypto/ })).toHaveCount(0);
});

/** Rasterizes the SVG path (one unit per module) and decodes it. */
function decodeQr(size: number, path: string): string | undefined {
  const scale = 4;
  const width = size * scale;
  const pixels = new Uint8ClampedArray(width * width * 4).fill(255);
  for (const [, x, y] of path.matchAll(/M(\d+) (\d+)/g)) {
    for (let dy = 0; dy < scale; dy += 1) {
      for (let dx = 0; dx < scale; dx += 1) {
        const offset = ((Number(y) * scale + dy) * width + Number(x) * scale + dx) * 4;
        pixels.fill(0, offset, offset + 3);
      }
    }
  }
  return jsQR(pixels, width, width)?.data;
}
