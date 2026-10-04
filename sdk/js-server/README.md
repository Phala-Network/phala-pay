# @phala/pay-server

Merchant server SDK for Node.js >=20.3, Bun, and Deno. It contains the keyed `PhalaPay` client, transport, generated server types, bound and standalone webhook verification, pins, address helpers, ledger helpers, and sweep/Safe builders. It has no React dependency.

`viem` is the only runtime dependency because the offline EVM address derivation and Safe/sweep
builders use its ABI, hashing, and checksum primitives.

```sh
npm install @phala/pay-server
```

## Merchant server client

Import `PhalaPay` from `@phala/pay-server` on Node.js >=20.3, Bun or Deno. Server runtimes
must provide `fetch`, `AbortSignal.any` and WebCrypto (including `randomUUID`). The `@phala/pay` package remains keyless and browser-safe. The server entry rejects browser-like
environments before reading credentials; keep this package out of browser bundles.

```ts
import { PhalaPay, ApiError, SignatureVerificationError } from "@phala/pay-server";
import type { CheckoutParams } from "@phala/pay";

// Reads exactly PHALA_PAY_API_KEY and PHALA_PAY_PINS from process.env.
const pay = PhalaPay.fromEnv();
// Or: new PhalaPay({ apiKey, pins, timeoutMs: 15000, maxAttempts: 4,
//                   requestDeadlineMs: 60000, fetch });

try {
  const quote = await pay.quotes.create(
    { client_reference_id: "team-42", amount: 2500, currency: "usd", chain_id: 11155111, asset: "pha" },
    { idempotencyKey: "persisted-order-42" },
  );
  const checkout: CheckoutParams = pay.checkoutParams(quote);
  // Return checkout only to this order's authenticated browser; spread it into <Checkout>.
  // Fulfill from the verified webhook, never from browser success.
  const event = await pay.webhooks.constructEvent(originalBodyBytes, requestHeaders);
  // Atomically commit your ledger snapshot and balance delta before acknowledging delivery.
  await applyVerifiedEvent(event);
} catch (error) {
  if (error instanceof SignatureVerificationError) {
    // Return 400 for an invalid webhook, without exposing exception text.
  } else if (error instanceof ApiError) {
    // Use statusCode/code/requestId internally; return a generic payment error to the caller.
  } else {
    // Return 5xx for failures before the ledger transaction commits.
  }
} finally {
  await pay.close();
}
```

`pins` accepts the versioned `ppay_pins_v1.…` string or a `Pins` value returned by
`parsePins`. `encodePins` produces its canonical encoding. Pins are recursively frozen and bind
the normalized API origin, account, mode, forwarder contracts, chain treasuries and public webhook
keys. Both test and live clients require pins. An explicit `apiBase` must match that origin;
HTTP is permitted only for explicitly configured test loopback origins. No trust discovery occurs.
`fromEnv(env, { apiBase })` reads no other variables and loads no files or dotenv configuration.

Every open quote and active deposit-address network is checked against the pinned treasury and
recomputed address, including list pages, actions and expanded objects. Historical objects cannot
be handed to checkout. `checkoutParams` requires a create/replay quote from this client with its
original `client_secret`; retrieval cannot manufacture a checkout secret. It rechecks pins and
payability and returns `{ clientSecret, expectedAddress, apiBase }`.

### Resources and types

Wire fields remain snake_case. Params and responses, including `QuoteCreateParams`, `Quote` and
`Deposit`, are aliases of the generated OpenAPI definitions. Response fields and future enums are
retained; unsafe JSON integers are rejected before rounding.

| Resource | Methods |
| --- | --- |
| `quotes` | `create`, `retrieve`, `listPage`, `list`, `update`, `cancel` |
| `depositAddresses` | `create`, `retrieve`, `listPage`, `list`, `update`, `rotate` |
| `deposits` | `retrieve`, `listPage`, `list`, `update` |
| `refunds` | `create`, `retrieve`, `listPage`, `list`, `update`, `cancel`, `markPaid` |
| `paymentSettings` | `retrieve`, `update` |
| `config`, `balance` | `retrieve` |
| `account` | `retrieve`, `pauseQuotes`, `resumeQuotes`, `rollWebhookKey` |
| `treasuries` | `challenge`, `create`, `retrieve`, `listPage`, `list`, `cancel`, `pause`, `resume` |
| `apiKeys` | `create`, `retrieve`, `listPage`, `list`, `roll`, `revoke` |
| `webhookEndpoints` | `create`, `retrieve`, `listPage`, `list`, `update`, `delete`, `test` |
| `events` | `retrieve`, `listPage`, `list`, `resend` |
| `sweeps`, `forwarders` | `listPage`, `list` |

Create/singleton-update/challenge use `(params, options?)`; resource updates and actions with
bodies use `(id, params, options?)`. Actions without bodies use `(id, options?)`. Retrieve uses
`(id, params?, options?)` (pass `undefined` for endpoints without query params); singleton retrieve
uses `(options?)`. Lists accept `(params?, options?)`. `listPage` returns `{ data, has_more, … }`;
`list` is an `AsyncIterable` that validates every page and rejects empty continuing pages or
repeated cursors. Omitting `apiKeys.create` permissions creates a secret key; supplying a list
creates a restricted key.

### Transport and errors

Per-call options are `{ idempotencyKey?, signal?, requestDeadlineMs?, upgradeTolerance? }`.
Defaults are a 15-second attempt timeout (including body reads), four total attempts, and a 60-second overall deadline
(including sleeps). POST requests freeze their body and one automatically generated UUID across
retries; persist an explicit order key to survive process restarts. Keys have a 255-character limit.
Retries cover network/timeouts, 429, 500/502/503/504, and only `409 idempotency_key_in_use`, with
bounded exponential jitter and Retry-After as a minimum. A delay that exceeds the remaining budget
returns the last error immediately. Replayed errors, DELETE, cancellation and redirects do not
retry. Redirects are refused. `close()` aborts outstanding requests without closing injected fetch.

Set `upgradeTolerance: true` on the client or per call for backend work that can wait through a
CVM upgrade. A maintenance 503, gateway 502/503/504 (even a non-JSON body), or network/attempt
timeout allows up to five minutes of retries from the original request start, with backoff capped
at 10 seconds. This applies only to GET and replayable POST and preserves the same body/key.
Explicit deadlines, cancellation, and replayed-error handling still take precedence. The option
defaults to false so interactive requests keep the existing timeout/attempt budget. See
[planned upgrades](../../docs/integration.md#planned-upgrades-and-reconnecting).

Errors inherit `PhalaPayError`: `ApiError`, `TransportError`, `ConfigurationError`,
`ResponseValidationError`, `AddressMismatchError`, `AttestationError`,
`SignatureVerificationError` (also exported as `WebhookSignatureError`) and `LedgerSnapshotError`.
`ApiError` exposes `statusCode`, `code`, `message`, `errorType`, `param`, `docUrl`, `requestId` and
`retryAfter` in seconds; absent optional fields are null. `TransportError.code` is `network`,
`timeout` or `cancelled`. Malformed responses expose status/request ID without raw bodies or causes.

### Bound webhooks and pure ledger helpers

`pay.webhooks.constructEvent(body, headers, { tolerance: 300 })` uses only this client's pinned
public keys, account and mode. Pass original bytes or exact UTF-8, before JSON middleware. Headers
are case-insensitive; duplicate signing headers fail. Ed25519 v1a verification, the envelope and ID
must pass. The inclusive time window is bilateral; tolerance zero requires an exact timestamp.
Known deposit events expose the typed `event.deposit`; unknown event types retain their raw object
and should be ignored before accessing it. Change overlapping webhook pins manually after
attestation; security notices never modify trust.

`depositNetAmount(deposit)` returns integer minor units. `balanceDelta(previousSnapshot, deposit)`
returns `{ snapshot, contribution, delta }` without IO or mutation. Commit the returned snapshot
and delta together in your own transaction before acknowledging a webhook. Duplicate and reordered
snapshots converge through monotone statuses and cumulative deductions. Refund events retain
`status: "credited"`; unknown statuses, identity/valuation conflicts and impossible deductions
raise `LedgerSnapshotError`. These helpers provide no SQL adapter or authorization policy.

## Server helpers

Keyless pins, ledger, address, signature and sweep builders live at `@phala/pay-server/helpers`, including for
WebCrypto-capable non-Node runtimes. Existing exports from `/server` remain available on supported server runtimes;
browser callers must move to `/server/helpers`:

```ts
import { constructEvent, flushTransactions, safeBatch, verifyQuoteAddress } from "@phala/pay-server/helpers";

// Your pins, configured on your server and never read from the service. The address is derived
// from your treasury, not the quote's: a mismatch throws AddressMismatchError, and in live mode a
// chain without a pinned treasury fails closed.
const pins = { account: "acct_…", factory: FACTORY, implementation: IMPLEMENTATION, treasuries: { 1: TREASURY } };
const expectedAddress = verifyQuoteAddress(pins, quote); // pass it to <Checkout expectedAddress>

// Standard Webhooks v1a (ed25519, WebCrypto: Node 20.3+, Deno, Bun, edge runtimes). Fails closed
// unless the signature verifies with a pinned key and the event is your account's in this mode.
// WEBHOOK_PUBLIC_KEYS: your account's keys in this mode, `whpk_…`, pinned from GET /v1/attestation.
const event = await constructEvent(rawBody, request.headers, WEBHOOK_PUBLIC_KEYS, {
  expectedAccount: "acct_…",
  expectedLivemode: false,
});

// Sweeping, offline: forwarders from GET /v1/forwarders?sweepable=<token>.
const calls = flushTransactions(forwarders, PHA);
const batchFile = safeBatch(1, TREASURY_SAFE, calls); // Safe Transaction Builder JSON
```

`verifyDepositAddress(pins, address)` checks every network of a deposit address the same way.
`quoteAddress`, `depositAddress`, `forwarderAddress`, `quoteSalt`, and `depositAddressSalt`
recompute an address offline from a treasury you pass, as the Python SDK does.

## Chain and token icons

`Checkout` and `DepositAddress` include compact inline icons for Ethereum, Base, USDC, USDT,
PHA, and ETH. Testnets reuse their mainnet family artwork. Unknown chains
and assets show a neutral first-letter monogram. Symbols are case-insensitive.

```tsx
import { NetworkIcon, AssetIcon } from "@phala/pay-react";

<NetworkIcon chainId={8453} size={16} />; // decorative beside visible "Base" text
<AssetIcon asset="usdc" size={20} decorative={false} />; // accessible when used alone
```

Import `@phala/pay-react/styles.css` for alignment. The default
size is 18px. Icons use SVG width/height attributes and React elements, so they need no inline
styles, data URI allowance, or extra CSP directives, including with `style-src 'self'`.

Framework-free hosts can call `networkIcon(chainId)` or `assetIcon(symbol)` from `@phala/pay`
for decorative SVG markup. Keep the
name visible alongside it; when used alone, supply an accessible label on the host wrapper.

Only six branded SVGs from `@web3icons/core@4.0.57` are vendored, under the MIT license in
`THIRD_PARTY_NOTICES`. Regenerate both the markup and JSX modules with
`npx -y pnpm@12.6.0 run icons:generate`; the script packs the pinned package into a temporary
directory, rejects unexpected SVG tags/attributes, and cleans up. No web3icons runtime package
is installed.

## Development

```sh
pnpm install
pnpm run check   # typecheck, lint, unit tests, build
pnpm run e2e     # Playwright against Anvil; needs Foundry (anvil, forge) on PATH
pnpm run e2e:docker  # the same, with Chromium from Playwright's image (as CI runs it)
```

The end-to-end tests start Anvil as Sepolia, deploy a test token, and pay a quote from a mocked
EIP-6963 wallet backed by the node (and again through a viem `WalletClient` passed as
`walletClient`), then decode the QR code and exercise the manual details.

## Releases

`@phala/pay` is released with the service: the `v<version>` tag's Release workflow publishes it to
npm (environment `npm`) with trusted publishing and provenance
([CONTRIBUTING.md, "Releasing"](../../CONTRIBUTING.md#releasing)). Its changes are in the top-level
[CHANGELOG.md](https://github.com/Phala-Network/phala-pay/blob/main/CHANGELOG.md), under "JS SDK";
[its releases before v0.5.0](https://github.com/Phala-Network/phala-pay/blob/main/sdk/js/CHANGELOG.md),
versioned on their own, stay in `sdk/js`.
