import { ConfigurationError } from "./errors.js";
import { isRecord, parseJson } from "./json.js";
export interface Pins {
  readonly api_base: string;
  readonly account: string;
  readonly livemode: boolean;
  readonly factory: string;
  readonly implementation: string;
  readonly treasuries: Readonly<Record<string, string>>;
  readonly webhook_keys: readonly Readonly<{ version: number; public_key: string }>[];
}
const invalid = (): never => {
  throw new ConfigurationError("Invalid pins");
};
export function normalizeOrigin(input: string, allowHttpLoopback = false): string {
  if (typeof input !== "string") throw new ConfigurationError("Invalid API origin");
  let url: URL;
  try {
    url = new URL(input);
  } catch {
    throw new ConfigurationError("Invalid API origin");
  }
  const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (
    (url.protocol !== "https:" && !(allowHttpLoopback && loopback && url.protocol === "http:")) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== "/"
  )
    throw new ConfigurationError("Invalid API origin");
  // Reject even empty query/fragment delimiters and credential syntax.
  if (/[?#]/.test(input) || input.includes("@")) throw new ConfigurationError("Invalid API origin");
  return url.origin;
}
function fields(value: Record<string, unknown>, expected: string[]): void {
  if (
    Object.keys(value).length !== expected.length ||
    expected.some((key) => !Object.hasOwn(value, key))
  )
    invalid();
}
function address(value: unknown): string {
  if (typeof value !== "string" || !/^0x[0-9a-fA-F]{40}$/.test(value) || /^0x0{40}$/i.test(value))
    return invalid();
  return value.toLowerCase();
}
function base64(bytes: Uint8Array): string {
  return btoa(Array.from(bytes, (byte) => String.fromCharCode(byte)).join(""));
}
function base64url(bytes: Uint8Array): string {
  return base64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
export function parsePins(encoded: string): Pins {
  try {
    if (!encoded.startsWith("ppay_pins_v1.")) invalid();
    const content = encoded.slice(13);
    if (!/^[A-Za-z0-9_-]+$/.test(content) || content.length > 21846) invalid();
    const bytes = Uint8Array.from(atob(content.replace(/-/g, "+").replace(/_/g, "/")), (char) =>
      char.charCodeAt(0),
    );
    if (bytes.length > 16384 || base64url(bytes) !== content) invalid();
    return validatePins(parseJson(new TextDecoder("utf-8", { fatal: true }).decode(bytes)));
  } catch {
    return invalid();
  }
}
export function validatePins(value: unknown): Pins {
  if (!isRecord(value)) return invalid();
  fields(value, [
    "api_base",
    "account",
    "livemode",
    "factory",
    "implementation",
    "treasuries",
    "webhook_keys",
  ]);
  const { api_base, account, livemode, treasuries, webhook_keys } = value;
  if (
    typeof api_base !== "string" ||
    typeof account !== "string" ||
    !/^acct_[0-9a-f]{32}$/.test(account) ||
    typeof livemode !== "boolean" ||
    !isRecord(treasuries) ||
    !Object.keys(treasuries).length ||
    !Array.isArray(webhook_keys) ||
    !webhook_keys.length
  )
    return invalid();
  const normalized: Record<string, string> = {};
  for (const [chain, treasury] of Object.entries(treasuries)) {
    if (!/^[1-9]\d*$/.test(chain) || !Number.isSafeInteger(Number(chain))) invalid();
    normalized[chain] = address(treasury);
  }
  const versions = new Set<number>();
  const keys = new Set<string>();
  const parsedKeys = webhook_keys
    .map((entry: unknown) => {
      if (!isRecord(entry)) return invalid();
      fields(entry, ["version", "public_key"]);
      const { version, public_key } = entry;
      if (
        typeof version !== "number" ||
        !Number.isInteger(version) ||
        version < 1 ||
        version > 0xffffffff ||
        versions.has(version) ||
        typeof public_key !== "string" ||
        !/^whpk_[A-Za-z0-9+/]{43}=$/.test(public_key) ||
        keys.has(public_key)
      )
        return invalid();
      const raw = Uint8Array.from(atob(public_key.slice(5)), (char) => char.charCodeAt(0));
      if (raw.length !== 32 || base64(raw) !== public_key.slice(5)) return invalid();
      versions.add(version);
      keys.add(public_key);
      return Object.freeze({ version, public_key });
    })
    .sort((a, b) => a.version - b.version);
  return Object.freeze({
    api_base: normalizeOrigin(api_base, !livemode),
    account,
    livemode,
    factory: address(value["factory"]),
    implementation: address(value["implementation"]),
    treasuries: Object.freeze(normalized),
    webhook_keys: Object.freeze(parsedKeys),
  });
}
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (isRecord(value))
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`)
      .join(",")}}`;
  return JSON.stringify(value);
}
export function encodePins(pins: Pins): string {
  const json = canonical(validatePins(pins));
  const bytes = new TextEncoder().encode(json);
  if (bytes.length > 16384) return invalid();
  return `ppay_pins_v1.${base64url(bytes)}`;
}
/** Service key format: CRC-32/ISO-HDLC, six base62 digits, after 43 random digits. */
export function keyLivemode(key: string): boolean {
  if (typeof key !== "string") throw new ConfigurationError("Invalid API key");
  const match = /^ppay_(?:sk|rk)_(test|live)_[A-Za-z0-9]{49}$/.exec(key);
  if (!match) throw new ConfigurationError("Invalid API key");
  let crc = 0xffffffff;
  for (const byte of new TextEncoder().encode(key.slice(0, -6))) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  let number = (crc ^ 0xffffffff) >>> 0;
  let checksum = "";
  const alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  for (let index = 0; index < 6; index++) {
    checksum = (alphabet[number % 62] ?? "") + checksum;
    number = Math.floor(number / 62);
  }
  if (checksum !== key.slice(-6)) throw new ConfigurationError("Invalid API key");
  return match[1] === "live";
}
