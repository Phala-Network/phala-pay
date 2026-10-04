import { ConfigurationError } from "./errors.js";

const serverOnly =
  "@phala/pay-server is server-only; never ship a key to the browser";
type ServerGlobals = {
  process?: {
    env?: Readonly<Record<string, string | undefined>>;
    versions?: { node?: string };
  };
  fetch?: typeof fetch;
  AbortSignal?: { any?: typeof AbortSignal.any };
  crypto?: Partial<Omit<Crypto, "subtle">> & { subtle?: Partial<SubtleCrypto> };
  Bun?: { env?: Readonly<Record<string, string | undefined>> };
  Deno?: { env?: { get(name: string): string | undefined } };
};
const globals: ServerGlobals = globalThis;

/** Check capabilities before touching credentials; runtime names do not grant trust. */
export function requireServer(): void {
  const nativeEnv =
    typeof globals.Deno?.env?.get === "function" ||
    typeof globals.Bun?.env === "object";
  if (
    Reflect.has(globals, "window") ||
    Reflect.has(globals, "document") ||
    !(typeof globals.process?.env === "object" || nativeEnv)
  )
    throw new ConfigurationError(serverOnly);
  // Native environment APIs may coexist with a Node compatibility shim.
  const nodeVersion = nativeEnv ? undefined : globals.process?.versions?.node;
  if (nodeVersion !== undefined) {
    const [major, minor] = nodeVersion.split(".").map(Number);
    if (
      major === undefined ||
      minor === undefined ||
      !Number.isInteger(major) ||
      !Number.isInteger(minor) ||
      major < 20 ||
      (major === 20 && minor < 3)
    )
      throw new ConfigurationError(
        "@phala/pay-server requires Node.js >=20.3 when running on Node",
      );
  }
  if (
    typeof globals.fetch !== "function" ||
    typeof globals.AbortSignal?.any !== "function" ||
    typeof globals.crypto?.randomUUID !== "function" ||
    typeof globals.crypto.getRandomValues !== "function" ||
    typeof globals.crypto.subtle?.importKey !== "function" ||
    typeof globals.crypto.subtle.verify !== "function"
  )
    throw new ConfigurationError(
      "@phala/pay-server requires fetch, AbortSignal.any and WebCrypto (including randomUUID)",
    );
}

export function serverEnv(): Readonly<Record<string, string | undefined>> {
  if (globals.process?.env) return globals.process.env;
  if (globals.Bun?.env) return globals.Bun.env;
  return new Proxy(
    {},
    {
      get(_target, name: string) {
        try {
          return globals.Deno?.env?.get(name);
        } catch {
          throw new ConfigurationError(
            "Unable to read merchant environment variables",
          );
        }
      },
    },
  );
}
