# @phala/pay

Browser checkout for Phala Pay: a framework-agnostic client for a quote's public
view, and a React component that lets the payer pay from a browser wallet, by QR code, or manually,
with live status. It is the browser half of the flow; your backend creates the quote with the
Node or Python merchant SDK and fulfils from the signed `deposit.credited` webhook.

## Install

```sh
npm install @phala/pay viem
```

`@phala/pay` shares the service's version: pin the one equal to your operator's service version
([compatibility](../../docs/integration.md#compatibility)).

Peer dependencies: `viem` 2, and `react` 18 or 19 for `@phala/pay/react`. Browser wallets are
found with EIP-6963 through [mipd](https://github.com/wevm/mipd), wagmi's discovery store (with a
`window.ethereum` fallback for wallets that do not announce themselves). If your page already
connects a wallet with wagmi, RainbowKit, ConnectKit, or AppKit, pass it in instead; see
[Your own wallet connection](#your-own-wallet-connection).

## Quickstart

1. Your backend creates a quote (`POST /v1/quotes` with its API key, for example
   `phala-pay`'s `pay.quotes.create`), recomputes its address from the pinned forwarder (the
   Python SDK does it on every call), and returns only its `client_secret` and that address to
   the signed-in user's browser.
2. Render the checkout with them:

   ```tsx
   "use client";
   import { Checkout } from "@phala/pay/react";
   import "@phala/pay/styles.css";

   export function TopUp(props: {
     clientSecret: string;
     expectedAddress: string;
     onPaid: () => void;
     onRetry: () => void;
   }) {
     return (
       <Checkout
         clientSecret={props.clientSecret}
         expectedAddress={props.expectedAddress}
         apiBase="https://pay.example.com"
         onSuccess={props.onPaid}
         onExpire={props.onRetry}
         buttonText="Pay with crypto"
       />
     );
   }
   ```

3. Credit the account when your webhook endpoint receives `deposit.credited`. `onSuccess` is for the
   UI only: the browser is not a trusted source of payment.

The component reads `GET /v1/quotes/{id}?client_secret=…` every three seconds. That endpoint is
public (`Access-Control-Allow-Origin: *`) and shows only what the payer needs: the amount, the
token, the chain, the deposit address, the EIP-681 payment request, the expiry, and the payment
status. Keep the client secret out of logs and URLs you share; anyone holding it can see that view.
`expectedAddress` is required and fails closed: when the quote the service returns names another
address, the checkout shows "This payment address could not be verified" and nothing to pay (a
compromised service cannot make the page show its own address). A test-mode quote says "Test
mode".

### Statuses

| `status` | Shown |
|---|---|
| `loading` | Loading payment details |
| `waiting` | Payment options, the exact amount, and the time left |
| `seen` | Payment in a block, with its confirmations and the typical wait for the credit (a reorg can still remove it) |
| `confirming` | Payment at the route's confirmation (two blocks on Ethereum), being valued and screened |
| `credited` | What was credited, typically `typical_credit_seconds` after paying; `onSuccess(quote)` is called once |
| `rejected` | Will not be credited; the payer contacts support |
| `reversed` | A reorganization before finality replaced the credited payment |
| `expired`, `canceled` | The address is hidden; `onExpire` is called once |
| `error` | Invalid client secret or address mismatch; nothing to pay |

`onChange(state)` is called once per status change with `{ status, quote, error }`, like Stripe
Elements' `onChange`, for example to hide your own "new payment" control while a payment is
`seen` or `confirming`.

Before the wallet tab sends a payment, it reads the wallet's token balance; when the wallet holds
less than the quote, it sends nothing (the transfer would revert and still cost gas) and says how
much the wallet holds. `onWalletError(error, wallet)` is called with the `WalletError` and the
wallet it tried (the chosen browser wallet's EIP-1193 provider, or your `walletClient`) whenever the
wallet tab could not send (`code` `insufficient_balance`, `rejected`, `wrong_chain`, …), for example
to offer your own way to fund that wallet.

To resume after a reload, keep the client secret in the browser (for example `localStorage`, per
signed-in account) until the status is `credited`, `expired`, `canceled`, or `error`. Tell a
payer who is waiting that the payment is credited in about the quote's `typical_credit_seconds`
(30 seconds on Ethereum, about 7 seconds on Base, 15 minutes under a `finalized` policy) and the
credit arrives on its own.

A credit is made before Ethereum finality (about 15 minutes). In the rare case that a reorg proves
the payment replaced, your backend receives `deposit.reversed` and claws the credit back as for
`deposit.refunded`; a payment gone with its nonce unspent instead stays credited and not final,
and the operator is alerted. The checkout itself only reflects the quote.

The payment options disappear once a payment is seen, and at `expires_at`. A payment of a different
amount, or after expiry, is still credited, at the market price instead of the quote's: the
checkout then says so ("Payment credited: $10.00 of $25.00"), and `onSuccess` receives the quote
with what was credited as `amount_credited`, beside the quoted `amount`.

### Your own wallet connection

By default the wallet tab lists the browser's wallets. If your page already connects one, pass its
viem `WalletClient` as `walletClient`: the wallet tab then pays with that client and its account
(switching its chain to the quote's) and lists nothing else. With wagmi, including RainbowKit,
ConnectKit, and AppKit, that is `useWalletClient()`:

```tsx
"use client";
import { Checkout } from "@phala/pay/react";
import { useWalletClient } from "wagmi";

export function TopUp(props: { clientSecret: string; expectedAddress: string }) {
  const { data: walletClient } = useWalletClient();
  return (
    <Checkout
      clientSecret={props.clientSecret}
      expectedAddress={props.expectedAddress}
      apiBase="https://pay.example.com"
      walletClient={walletClient}
    />
  );
}
```

While no wallet is connected (`walletClient` is `undefined`), the tab lists the browser's wallets
as usual. The QR code and manual tabs do not change.

### A customer's deposit address

For top-ups of any amount, your backend creates the customer's persistent deposit address with
`POST /v1/deposit_addresses` (its API key) and passes its `address` and `networks` to the page:
one address for all supported tokens and networks. `<DepositAddress>` lets the payer pick a
network and a token and shows a QR code of that token's transfer request, the token contract, and
the address to copy, with "send only supported tokens" and "credited at the market rate when it
arrives" copy; where a network's address differs (another treasury), it shows that network's own.
Credit the customer from the `deposit.credited` webhook.

```tsx
import { DepositAddress } from "@phala/pay/react";

<DepositAddress
  depositAddress={{ address, networks }}
  clientSecret={clientSecret} // the same response's client_secret: shows payments as they arrive
  apiBase="https://pay.example.com"
  chainId={84532}
  asset="usdc"
/>;
```

With `clientSecret` and `apiBase` it reads the address's public view every three seconds and lists
its payments of the last 24 hours: "1.5 PHA received on Sepolia, 1 confirmation" within about a
block of the transfer, then "credited" (or rejected or reversed). Display only. It also states each
network's typical credit time from the view's `typical_credit_seconds` ("usually in about 30 seconds
on Sepolia and about 7 seconds on Base Sepolia"); until the view is loaded it names no time.

`depositAddressTransfer(network, asset)` reads and checks one token's amount-less EIP-681
`payment_uri` without React.

## Appearance

```tsx
<Checkout
  clientSecret={clientSecret}
  expectedAddress={expectedAddress}
  apiBase={apiBase}
  appearance={{
    theme: "dark",
  }}
/>
```

| CSS custom property                      | Light default | Dark default | Use                                          |
| ---------------------------------------- | ------------- | ------------ | -------------------------------------------- |
| `--pp-color-primary`                     | `#0f62fe`     | `#78a9ff`    | Pay button, selected tab, links, focus ring  |
| `--pp-accessible-color-on-color-primary` | `#ffffff`     | `#ffffff`    | Text on the primary background (button)      |
| `--pp-color-background`                  | `#ffffff`     | `#161616`    | Background                                   |
| `--pp-color-text`                        | `#1a1a1a`     | `#f4f4f4`    | Text                                         |
| `--pp-color-text-secondary`              | `#5c5f66`     | `#a8a8a8`    | Labels and hints                             |
| `--pp-color-border`                      | `#d9dce1`     | `#393939`    | Borders                                      |
| `--pp-color-danger`                      | `#c62828`     | `#ff8389`    | Errors, expired and rejected states          |
| `--pp-color-success`                     | `#1b7f3b`     | `#42be65`    | Credited state                               |
| `--pp-font-family`                       | system UI     | system UI    | Font                                         |
| `--pp-border-radius`                     | `8px`         | `8px`        | Corner radius                                |

Import the static stylesheet once in your application entry point:

```ts
import "@phala/pay/styles.css";
```

Set these CSS custom properties in your own stylesheet, after the SDK stylesheet, using a
selector such as `.pp-root[data-theme]`. For a light primary color, set a dark
`--pp-accessible-color-on-color-primary` so the button label stays readable.

For a frameless host dialog, set `--pp-root-border: 0`, `--pp-root-padding: 0`,
`--pp-root-max-width: none`, and `--pp-root-background: transparent` on the component or its
ancestor. Defaults remain `1px solid var(--pp-color-border)`, `20px`, `440px`, and
`var(--pp-color-background)`. Checkout opens QR code when no browser wallet is discovered;
the Wallet tab remains available.

```css
.pp-root[data-theme] {
  --pp-color-primary: #cdfa50;
  --pp-accessible-color-on-color-primary: #161616;
  --pp-border-radius: 12px;
}
```

To map your own light/dark tokens on `.pp-root`, omit `appearance.theme` and load your mapping
after the SDK stylesheet. Passing `theme: "dark"` sets `data-theme="dark"`, whose SDK selector
`.pp-root[data-theme="dark"]` overrides a plain `.pp-root` mapping. If you also pass the dark
theme, target `.pp-root[data-theme="dark"]` (or `.pp-root[data-theme]`) in your mapping.

### Content Security Policy

The components inject no style elements or style attributes. Serve the bundled CSS from your
own origin with `style-src 'self'`; no `unsafe-inline` style permission is needed. Allow the
payment service origin in `connect-src` for public status reads and `img-src data:` for browser
wallet icons. Configure script and font sources for your application's own bundles.

Public SDK reads accept `signal` and `requestTimeout` (milliseconds, default 10000). Each request
combines caller cancellation with `AbortSignal.timeout` through `AbortSignal.any`, including the
response body. Use Node.js 20.3 or later, or a browser supporting these standard APIs.
`destroy()` and React unmount
abort active status reads; a request timeout permits polling to retry with the usual backoff.

## Without React

```ts
import { PhalaPay, payWithWallet, watchWallets, type Wallet } from "@phala/pay";

const pay = new PhalaPay({ apiBase: "https://pay.example.com" });
const quote = await pay.retrieveQuote(clientSecret, expectedAddress); // one read, checked
const checkout = pay.checkout(clientSecret, { expectedAddress }); // or follow it until it settles
const unsubscribe = checkout.subscribe(({ status, quote, error }) => render(status, quote, error));

const stop = watchWallets((wallets) => showWalletButtons(wallets));

async function onWalletClick(wallet: Wallet) {
  const { quote } = checkout.getState();
  if (quote !== null) {
    // Resolves with the transaction hash once the wallet broadcast the transfer.
    showTransaction(await payWithWallet(wallet.provider, quote));
    await checkout.refresh();
  }
}

// When leaving the page:
stop();
unsubscribe();
checkout.destroy();
```

`payWithWallet` takes a discovered wallet's EIP-1193 provider or a viem `WalletClient` (such as
wagmi's `useWalletClient()` or `getWalletClient(config)`; its account is used without asking to
connect again). It connects, switches the wallet to the quote's chain (adding Ethereum, Sepolia,
Base, or Base Sepolia when the wallet lacks it), and sends the ERC-20 `transfer` stated by the
quote's `payment_uri`, after checking that it pays exactly `amount_atomic` to `address`. A wallet
holding less of the token than that sends nothing: `WalletError` with `code`
`insufficient_balance`.

## Merchant server client

Import `PhalaPay` from `@phala/pay/server` on Node.js >=20.3, Bun or Deno. Server runtimes
must provide `fetch`, `AbortSignal.any` and WebCrypto (including `randomUUID`). The root
`@phala/pay` entry remains keyless and browser-safe. The merchant entry rejects browser-like
environments before reading credentials; its `browser` export resolves to a throwing module
without merchant exports, so browser bundlers reject merchant imports.

```ts
import { PhalaPay, ApiError, SignatureVerificationError } from "@phala/pay/server";
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

Per-call options are `{ idempotencyKey?, signal?, requestDeadlineMs?, upgradeTolerance? }`. Defaults are a 15-second
attempt timeout (including body reads), four total attempts, and a 60-second overall deadline
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

Keyless pins, ledger, address, signature and sweep builders live at `@phala/pay/server/helpers`, including for
WebCrypto-capable non-Node runtimes. Existing exports from `/server` remain available on supported server runtimes;
browser callers must move to `/server/helpers`:

```ts
import { constructEvent, flushTransactions, safeBatch, verifyQuoteAddress } from "@phala/pay/server/helpers";

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
import { NetworkIcon, AssetIcon } from "@phala/pay/react";

<NetworkIcon chainId={8453} size={16} />; // decorative beside visible "Base" text
<AssetIcon asset="usdc" size={20} decorative={false} />; // accessible when used alone
```

Import `@phala/pay/styles.css` for alignment. The default
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
