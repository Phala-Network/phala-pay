# @phala/pay

Browser checkout for Phala Pay: a framework-agnostic client for a quote's public
view, and a React component that lets the payer pay from a browser wallet, by QR code, or manually,
with live status. It is the browser half of the flow; your backend creates the quote with the
Python SDK (`phala-pay`) and fulfils from the signed `deposit.credited` webhook.

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
| `reversed` | A reorganization proved the credited payment replaced before finality |
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

## Server helpers

`@phala/pay/server` is for your backend and takes no API key (call the API itself with a
restricted key, `ppay_rk_…`, from the backend only, never the browser):

```ts
import { constructEvent, flushTransactions, safeBatch, verifyQuoteAddress } from "@phala/pay/server";

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
