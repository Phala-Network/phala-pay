import process from "node:process";
// Evaluate before a merchant client can read credentials, including fromEnv defaults.
import { ConfigurationError } from "./errors.js";
if (
  typeof process === "undefined" ||
  !process.versions.node ||
  "bun" in process.versions ||
  "Deno" in globalThis ||
  typeof window !== "undefined"
) {
  throw new ConfigurationError(
    "@phala/pay/server requires Node.js; use @phala/pay/server/helpers for offline helpers",
  );
}
export { PhalaPay, type PhalaPayOptions, type Event } from "./client.js";
export * from "./types.js";
export * from "./errors.js";
export { parsePins, encodePins, type Pins } from "./pins.js";
export { balanceDelta, depositNetAmount, type LedgerSnapshot } from "./ledger.js";
export type { RequestOptions } from "./transport.js";
export type { CheckoutParams } from "../checkout-params.js";
// Legacy offline exports remain available to Node callers.
export { constructEvent, type ConstructEventOptions, type WebhookEvent } from "./webhook.js";
export {
  depositAddress,
  depositAddressSalt,
  forwarderAddress,
  quoteSalt,
  quoteAddress,
  verifyDepositAddress,
  verifyQuoteAddress,
  type AddressPins,
  type Forwarder,
} from "./addresses.js";
export * from "./sweeps.js";
export type { Forwarder as ForwarderResponse } from "./types.js";
