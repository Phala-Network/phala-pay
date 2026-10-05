import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes } from "node:crypto";
import { mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createTestClient, http, publicActions, walletActions, type Address, type Hash, type Hex } from "viem";
import { baseSepolia, sepolia } from "viem/chains";
import { createBuilder, preview, type PreviewServer } from "vite";

const web = resolve(import.meta.dirname, "..");
const root = resolve(web, "../../..");
// cf's Build Output, where the Cloudflare Vite plugin (vite.config.ts) builds the page.
const buildOutput = join(web, ".cloudflare/output");
// The deterministic deployment (deploy/CONTRACTS.md) and staging's treasury Safe, the same on every
// chain, as the product pins them.
const FACTORY = "0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747";
const IMPLEMENTATION = "0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9";
const TREASURY = "0x26430107887d4a691B340BdB887096B83E7a5844";
const ACCOUNT = `acct_${"e2e0".repeat(8)}`;
const DETERMINISTIC_PROXY = "0x4e59b44847b379578588920cA78FbF26c0B4956C";

/**
 * Runs the whole demo locally: two Anvils, as Sepolia (test PHA and a 6-decimal test USDC) and as
 * Base Sepolia (test PHA, and a 6-decimal test USDT whose `transfer` returns nothing, minted through
 * a faucet contract as Aave's is), each with the real forwarder factory, deployed as deploy/CONTRACTS.md deploys it (through the deterministic
 * deployment proxy with the committed salt), so it lands at its pinned address on both; the fake top-up service
 * (e2e/fake_service.py), the reference product serving the demo's API, pinned to the fake
 * service's webhook key, and the page, built against that API and served from its own origin (as
 * Cloudflare serves pay.phala.com), under the CSP of public/_headers with the local origins: the
 * page calls the API cross-origin, with CORS and the demo account cookie. Tests read SITE_URL,
 * API_URL, SERVICE_URL, ANVIL_URL and BASE_ANVIL_URL, PAYER_ADDRESS, TOKEN_ADDRESS (Sepolia's test PHA),
 * BASE_TOKEN_ADDRESS, USDC_ADDRESS, BASE_USDT_ADDRESS, and TREASURY (which the tests control on
 * Anvil, as the merchant's finance team controls its treasury). Service logs go to test-results/services.
 */
export default async function globalSetup(): Promise<() => Promise<void>> {
  const work = mkdtempSync(join(tmpdir(), "demo-e2e-"));
  const logs = join(web, "test-results", "services");
  mkdirSync(logs, { recursive: true });
  const children: ChildProcess[] = [];
  let site: PreviewServer | undefined;
  const teardown = async () => {
    await site?.close();
    await Promise.all(children.map(stop));
    rmSync(work, { recursive: true, force: true });
    rmSync(buildOutput, { recursive: true, force: true });
  };
  try {
    execFileSync(
      process.env["FORGE"] ?? "forge",
      [
        ...["build", "e2e/TestPha.sol"],
        ...["--root", ".", "--contracts", "e2e", "--no-lint"],
        ...["--out", join(work, "out"), "--cache-path", join(work, "cache")],
      ],
      { cwd: web, stdio: ["ignore", "ignore", "inherit"] },
    );
    const bytecode = (file: string, name: string) =>
      (JSON.parse(readFileSync(join(work, "out", file, `${name}.json`), "utf8")) as { bytecode: { object: Hex } })
        .bytecode.object;
    // The factory's creation code, built with the contracts' own pinned compiler profile.
    const factoryBuild = execFileSync(
      process.env["FORGE"] ?? "forge",
      [
        ...["inspect", "ForwarderFactory", "bytecode", "--root", join(root, "contracts")],
        ...["--out", join(work, "contracts-out"), "--cache-path", join(work, "contracts-cache")],
      ],
      { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] },
    )
      .trim()
      .split("\n")
      .at(-1);
    if (factoryBuild === undefined || !/^0x[0-9a-f]+$/.test(factoryBuild)) {
      throw new Error("forge inspect printed no ForwarderFactory bytecode");
    }
    const factoryInitCode: Hex = `0x${factoryBuild.slice(2)}`;
    const { factory_salt: factorySalt } = JSON.parse(
      readFileSync(join(root, "deploy/contracts/expected-codehashes.json"), "utf8"),
    ) as { factory_salt: Hex };

    const [sepoliaPort, basePort, servicePort, productPort, sitePort] = [
      await freePort(),
      await freePort(),
      await freePort(),
      await freePort(),
      await freePort(),
    ];
    // One Anvil per chain, with its test contracts and the factory at its pinned address.
    const startChain = async (chainId: number, port: number, contracts: [string, string][]) => {
      children.push(
        spawn(process.env["ANVIL"] ?? "anvil", ["--port", String(port), "--chain-id", String(chainId), "--silent"], {
          stdio: "ignore",
        }),
      );
      const url = `http://127.0.0.1:${port}`;
      const chain = createTestClient({
        mode: "anvil",
        chain: chainId === sepolia.id ? sepolia : baseSepolia,
        transport: http(url),
      })
        .extend(publicActions)
        .extend(walletActions);
      await waitFor(() => chain.getChainId());
      const [payer] = await chain.getAddresses();
      if (payer === undefined) {
        throw new Error("anvil has no dev account");
      }
      const deployed = async (hash: Hash): Promise<Address> => {
        const { contractAddress } = await chain.waitForTransactionReceipt({ hash });
        if (contractAddress == null) {
          throw new Error("contract deployment failed");
        }
        return contractAddress;
      };
      const tokens: Address[] = [];
      for (const [file, name] of contracts) {
        tokens.push(await deployed(await chain.deployContract({ account: payer, abi: [], bytecode: bytecode(file, name) })));
      }
      // Anvil carries the deterministic deployment proxy; its calldata is the salt, then the init
      // code. The factory's constructor creates the implementation (its first CREATE).
      await chain.waitForTransactionReceipt({
        hash: await chain.sendTransaction({
          account: payer,
          to: DETERMINISTIC_PROXY,
          data: `${factorySalt}${factoryInitCode.slice(2)}`,
        }),
      });
      const implementation = await chain.readContract({
        address: FACTORY,
        abi: [
          { type: "function", name: "implementation", inputs: [], outputs: [{ type: "address" }], stateMutability: "view" },
        ],
        functionName: "implementation",
      });
      if (implementation.toLowerCase() !== IMPLEMENTATION.toLowerCase()) {
        throw new Error(`the factory's implementation is ${implementation}, not the pinned ${IMPLEMENTATION}`);
      }
      return { url, payer, tokens };
    };
    const onSepolia = await startChain(sepolia.id, sepoliaPort, [
      ["TestPha.sol", "TestPha"],
      ["TestPha.sol", "TestUsdc"],
    ]);
    const onBase = await startChain(baseSepolia.id, basePort, [
      ["TestPha.sol", "TestPha"],
      ["TestPha.sol", "TestUsdt"],
      ["TestPha.sol", "TestFaucet"],
    ]);
    const [token, usdc] = onSepolia.tokens;
    const [baseToken, baseUsdt, baseFaucet] = onBase.tokens;
    if (
      token === undefined ||
      usdc === undefined ||
      baseToken === undefined ||
      baseUsdt === undefined ||
      baseFaucet === undefined
    ) {
      throw new Error("the test tokens were not deployed");
    }
    const anvil = onSepolia.url;
    const payer = onSepolia.payer;

    const webhookSeed = randomBytes(32);
    // The stand-in service does not check the key; the product runs with a restricted key's form.
    writeFileSync(join(work, "product.key"), `ppay_rk_test_${"A".repeat(43)}000000`, { mode: 0o600 });
    const product = `http://127.0.0.1:${productPort}`;
    const service = `http://127.0.0.1:${servicePort}`;
    const origin = `http://127.0.0.1:${sitePort}`;
    const uv = ["run", "--locked", "--project", join(root, "sdk/python"), "python"];

    children.push(
      spawn(
        "uv",
        [
          ...uv,
          join(web, "e2e/fake_service.py"),
          "--port",
          String(servicePort),
          "--chains",
          JSON.stringify([
            {
              chain_id: sepolia.id,
              rpc: anvil,
              tokens: [
                { asset: "pha", contract: token, decimals: 18, price: "0.25000000", pricing: "spot" },
                { asset: "usdc", contract: usdc, decimals: 6, price: "1.00000000", pricing: "stablecoin" },
              ],
            },
            {
              // Staging's own PHA rate, to show it formatted: 1 PHA = $0.06041.
              chain_id: baseSepolia.id,
              rpc: onBase.url,
              tokens: [
                { asset: "pha", contract: baseToken, decimals: 18, price: "0.06041314", pricing: "spot" },
                { asset: "usdt", contract: baseUsdt, decimals: 6, price: "1.00000000", pricing: "stablecoin" },
              ],
            },
          ]),
          ...["--product-webhook", `${product}/webhooks`],
          ...["--webhook-seed", webhookSeed.toString("hex")],
          ...["--factory", FACTORY, "--implementation", IMPLEMENTATION],
          ...["--account", ACCOUNT, "--treasury", TREASURY],
        ],
        { stdio: ["ignore", openSync(join(logs, "fake_service.log"), "w"), "inherit"] },
      ),
    );
    const config = {
      service_url: service,
      account: ACCOUNT,
      api_key_file: join(work, "product.key"),
      factory: FACTORY,
      implementation: IMPLEMENTATION,
      chains: [
        {
          chain_id: sepolia.id,
          name: "Sepolia",
          rpc_url: anvil,
          treasury: TREASURY,
          test_tokens: [{ symbol: "PHA", address: token }],
        },
        {
          chain_id: baseSepolia.id,
          name: "Base Sepolia",
          rpc_url: onBase.url,
          treasury: TREASURY,
          test_tokens: [
            { symbol: "PHA", address: baseToken },
            { symbol: "USDT", address: baseUsdt, minter: baseFaucet },
          ],
        },
      ],
      bonus_bps: { pha: 1000 },
      public_url: product,
      listen_host: "127.0.0.1",
      listen_port: productPort,
      ledger_path: join(work, "ledger.sqlite3"),
      driver_public_key: rawPublicKey(generateKeyPairSync("ed25519").publicKey.export({ format: "der", type: "spki" })),
      webhook_public_keys: [`whpk_${rawPublicKey(publicKeyOf(webhookSeed))}`],
      web_origin: origin,
    };
    writeFileSync(join(work, "product.json"), JSON.stringify(config));
    children.push(
      spawn("uv", [...uv, "-m", "reference_product", "serve", "--config", join(work, "product.json")], {
        env: { ...process.env, PYTHONPATH: join(root, "deploy/product") },
        stdio: ["ignore", openSync(join(logs, "product.log"), "w"), openSync(join(logs, "product.err"), "w")],
      }),
    );
    await waitFor(() => fetchOk(`${service}/evidences/quote.json`), 600);
    await waitFor(() => fetchOk(`${product}/healthz`), 600);

    // The page as `build:cloudflare` builds it, with the local API's origin instead of staging's,
    // into the Build Output (.cloudflare/output, removed at teardown so it is never deployed), and
    // served by the Workers runtime as Cloudflare serves it, with the headers of its _headers, whose
    // CSP connects to the local origins instead.
    process.env["VITE_DEMO_API_ORIGIN"] = product;
    await (await createBuilder({ root: web, logLevel: "warn" })).buildApp();
    await (await createBuilder({ root: web, configFile: join(web, "vite.ssr.config.ts"), logLevel: "warn" })).buildApp();
    execFileSync(process.execPath, [join(web, "scripts/prerender.ts")], { cwd: web, stdio: ["ignore", "ignore", "inherit"] });
    const stagingApi = "https://pay-demo-api.phala.com";
    const stagingService = "https://pay-api-staging.phala.com";
    const headersFile = join(buildOutput, "v0/workers/default/assets/_headers");
    const headers = readFileSync(headersFile, "utf8");
    if (!/^\s+Content-Security-Policy: .+$/m.exec(headers)?.[0].includes(` ${stagingApi} ${stagingService};`)) {
      throw new Error(`public/_headers has no CSP connecting to ${stagingApi} and ${stagingService}`);
    }
    writeFileSync(headersFile, headers.replace(` ${stagingApi} ${stagingService};`, ` ${product} ${service};`));
    site = await preview({ root: web, logLevel: "warn", preview: { host: "127.0.0.1", port: sitePort, strictPort: true } });

    Object.assign(process.env, {
      SITE_URL: `${origin}/`,
      API_URL: product,
      SERVICE_URL: service,
      ANVIL_URL: anvil,
      BASE_ANVIL_URL: onBase.url,
      PAYER_ADDRESS: payer,
      TOKEN_ADDRESS: token,
      BASE_TOKEN_ADDRESS: baseToken,
      USDC_ADDRESS: usdc,
      BASE_USDT_ADDRESS: baseUsdt,
      TREASURY,
    });
  } catch (error) {
    await teardown();
    throw error;
  }
  return teardown;
}

function publicKeyOf(seed: Buffer): Buffer {
  // PKCS#8 wrapping of a raw ed25519 seed (RFC 8410).
  const der = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]);
  const key = createPrivateKey({ key: der, format: "der", type: "pkcs8" });
  return createPublicKey(key).export({ format: "der", type: "spki" });
}

/** The raw ed25519 key of an SPKI encoding, as standard base64. */
function rawPublicKey(spki: Buffer): string {
  return spki.subarray(-32).toString("base64");
}

async function fetchOk(url: string): Promise<void> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url} answered ${response.status}`);
  }
}

function stop(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  const exited = new Promise<void>((done) => child.once("exit", () => done()));
  child.kill();
  return exited;
}

function freePort(): Promise<number> {
  return new Promise((done, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => {
        if (typeof address === "object" && address !== null) {
          done(address.port);
        } else {
          reject(new Error("no port"));
        }
      });
    });
  });
}

async function waitFor(probe: () => Promise<unknown>, attempts = 100): Promise<void> {
  for (let attempt = 0; ; attempt += 1) {
    try {
      await probe();
      return;
    } catch (error) {
      if (attempt >= attempts) {
        throw error;
      }
      await new Promise((done) => setTimeout(done, 100));
    }
  }
}
