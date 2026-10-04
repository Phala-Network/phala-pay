// @vitest-environment node
import { afterEach, describe, expect, it, vi } from "vitest";
import { build } from "vite";
import { PhalaPay, ConfigurationError } from "../src/server/index.js";
import { apiKey, pins } from "./merchant-fixtures.js";

afterEach(() => vi.unstubAllGlobals());
const env = { PHALA_PAY_API_KEY: apiKey, PHALA_PAY_PINS: pins };
function simulated<T>(values: Record<string, unknown>, action: () => T): T {
  try {
    for (const [name, value] of Object.entries(values))
      vi.stubGlobal(name, value);
    return action();
  } finally {
    vi.unstubAllGlobals();
  }
}
function attempt(action: () => void): unknown {
  try {
    action();
  } catch (error) {
    return error;
  }
}

describe("merchant runtime capabilities", () => {
  it.each(["Bun", "Deno"])(
    "accepts %s without Node compatibility globals",
    (runtime) => {
      const reads: string[] = [];
      const server =
        runtime === "Bun"
          ? { env }
          : {
              env: {
                get(name: string) {
                  reads.push(name);
                  return env[name as keyof typeof env];
                },
              },
            };
      const pay = simulated({ process: undefined, [runtime]: server }, () =>
        PhalaPay.fromEnv(),
      );
      expect(pay.pins).toEqual(pins);
      if (runtime === "Deno")
        expect(reads).toEqual(["PHALA_PAY_API_KEY", "PHALA_PAY_PINS"]);
    },
  );
  it.each(["Bun", "Deno"])(
    "accepts %s with an older Node compatibility version",
    (runtime) => {
      const server =
        runtime === "Bun"
          ? { env }
          : {
              env: {
                get(name: string) {
                  return env[name as keyof typeof env];
                },
              },
            };
      const pay = simulated(
        { process: { env, versions: { node: "18.0.0" } }, [runtime]: server },
        () => new PhalaPay({ apiKey, pins }),
      );
      expect(pay.livemode).toBe(false);
    },
  );
  it.each(["20.3.0", "20.19.0", "22.0.0"])("accepts Node %s", (node) => {
    const pay = simulated({ process: { env, versions: { node } } }, () =>
      PhalaPay.fromEnv(),
    );
    expect(pay.livemode).toBe(false);
  });
  it.each(["18.20.0", "20.2.0"])(
    "rejects Node %s before reading credentials",
    (node) => {
      const read = vi.fn(() => apiKey);
      const failure = simulated({ process: { env, versions: { node } } }, () =>
        attempt(() => {
          new PhalaPay({
            get apiKey() {
              return read();
            },
            pins,
          });
        }),
      );
      expect(failure).toBeInstanceOf(ConfigurationError);
      expect(failure).toHaveProperty(
        "message",
        expect.stringContaining("Node.js >=20.3"),
      );
      expect(read).not.toHaveBeenCalled();
    },
  );
  it.each([
    { window: undefined },
    { document: {} },
    { process: undefined },
    { fetch: undefined },
    { AbortSignal: {} },
    { crypto: {} },
    { crypto: { randomUUID() {}, getRandomValues() {}, subtle: {} } },
  ])(
    "fails closed before credentials for missing capabilities or browser globals: %j",
    (values) => {
      const read = vi.fn(() => apiKey);
      const failure = simulated(values, () =>
        attempt(() => {
          new PhalaPay({
            get apiKey() {
              return read();
            },
            pins,
          });
        }),
      );
      expect(failure).toBeInstanceOf(ConfigurationError);
      expect(read).not.toHaveBeenCalled();
    },
  );
  it("redacts failures reading Deno environment variables", () => {
    const failure = simulated(
      {
        process: undefined,
        Deno: {
          env: {
            get() {
              throw new Error(apiKey);
            },
          },
        },
      },
      () =>
        attempt(() => {
          PhalaPay.fromEnv();
        }),
    );
    expect(failure).toBeInstanceOf(ConfigurationError);
    expect(failure).toHaveProperty(
      "message",
      "Unable to read merchant environment variables",
    );
  });
  it("rejects merchant named imports during browser bundling through the browser export", async () => {
    await expect(
      build({
        configFile: false,
        logLevel: "silent",
        plugins: [
          {
            name: "merchant-browser-test",
            resolveId(id) {
              if (id === "merchant-browser-test")
                return "\0merchant-browser-test";
            },
            load(id) {
              if (id === "\0merchant-browser-test")
                return 'import { PhalaPay } from "@phala/pay/server"; console.log(PhalaPay);';
            },
          },
        ],
        build: {
          write: false,
          rollupOptions: { input: "merchant-browser-test" },
        },
      }),
    ).rejects.toThrow('"PhalaPay" is not exported by "dist/server/browser.js"');
  });
  it("the browser export throws a clear server-only error even for side-effect imports", async () => {
    await expect(import("../src/server/browser.js")).rejects.toThrow(
      "server-only; never ship a key to the browser",
    );
  });
});
