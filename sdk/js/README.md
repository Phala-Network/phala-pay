# @phala/pay

Framework-free browser core for Phala Pay checkout sessions, retrieval helpers, wallets, payment URIs, icons, formatting, and public types. React components are published separately as [`@phala/pay-react`](../js-react).

It is the browser half of the flow; your backend creates the quote with the Node or Python merchant SDK and fulfils from the signed `deposit.credited` webhook.

## Install

```sh
npm install @phala/pay viem
```

For React components, install `@phala/pay-react` alongside `react` and `react-dom`; this package
itself has no React peer dependency.

`@phala/pay` shares the service's version: pin the one equal to your operator's service version
([compatibility](../../docs/integration.md#compatibility)).

Peer dependency: `viem` 2. Browser wallets are
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
   import { Checkout } from "@phala/pay-react";
   import "@phala/pay-react/styles.css";

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

The component reads `GET /v1/quotes/{id}?client_secret=…` about every three seconds. That endpoint is
public (`Access-Control-Allow-Origin: *`) and shows only what the payer needs: the amount, the
token, the chain, the deposit address, the EIP-681 payment request, the expiry, and the payment
status. Keep the client secret out of logs and URLs you share; anyone holding it can see that view.
`expectedAddress` is required and fails closed: when the quote the service returns names another
address, the checkout shows "This payment address could not be verified" and nothing to pay (a
compromised service cannot make the page show its own address). A test-mode quote says "Test
mode".

Checkout and React deposit-address polling apply uniform ±20% jitter to normal intervals and
failure backoff, while respecting `Retry-After`. Hidden tabs pause polling and read immediately
when visible again. Jittered delays are capped at 30 seconds; a longer `Retry-After` remains
a minimum. The checkout core also works without `document`. `pollInterval` sets the
normal polling interval. Explicit `checkout.refresh()` still reads immediately while hidden,
including the post-broadcast refresh used by `<Checkout>`.
`pollInterval` is in milliseconds (default 3000). Other than 408/429, non-terminal 4xx responses
stop checkout polling after three consecutive occurrences and surface the existing `error`
state. Success or another kind of failure resets that count. An unknown client secret (404)
still stops immediately.

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
| `error` | Invalid client secret, address mismatch, or three consecutive non-retryable 4xx; nothing to pay |

`onChange(state)` is called once per status change with `{ status, quote, error }`, like Stripe
Elements' `onChange`, for example to hide your own "new payment" control while a payment is
`seen` or `confirming`.

`CheckoutError` exposes `code`, `message`, and optional readonly fields `status` (the HTTP status
when the error came from a response), `requestId` (the response's `Request-Id`), and `retryAfter`
(seconds to wait before the next read). `status` is `undefined` for network errors and errors
created without a response.

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

Checkout opens the QR code tab when no browser wallet is discovered; the Wallet tab remains
available.

### Your own wallet connection

By default the wallet tab lists the browser's wallets. If your page already connects one, pass its
viem `WalletClient` as `walletClient`: the wallet tab then pays with that client and its account
(switching its chain to the quote's) and lists nothing else. With wagmi, including RainbowKit,
ConnectKit, and AppKit, that is `useWalletClient()`:

```tsx
"use client";
import { Checkout } from "@phala/pay-react";
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
import { DepositAddress } from "@phala/pay-react";

<DepositAddress
  depositAddress={{ address, networks }}
  clientSecret={clientSecret} // the same response's client_secret: shows payments as they arrive
  apiBase="https://pay.example.com"
  chainId={84532}
  asset="usdc"
/>;
```

With `clientSecret` and `apiBase` it reads the address's public view about every three seconds and lists
its payments of the last 24 hours: "1.5 PHA received on Sepolia, 1 confirmation" within about a
block of the transfer, then "credited" (or rejected or reversed). Display only. It also states each
network's typical credit time from the view's `typical_credit_seconds` ("usually in about 30 seconds
on Sepolia and about 7 seconds on Base Sepolia"); until the view is loaded it names no time.

After ten minutes without a change in the public view, `<DepositAddress>` uses a base interval
of `max(pollInterval, 15000)` milliseconds, with the same jitter and failure backoff, so a longer
configured interval stays unchanged. Any view change or visibility regain resets
the idle window and restores `pollInterval`. Optional `onChange(state)` receives the existing
`ClientDepositAddress` public view after the first successful read and then once per content change,
including payment confirmations and network credit times. Unlike `<Checkout onChange>`, it does
not report loading. Unchanged polls, callback replacements,
and reconnecting alone do not trigger it. Use it to update your page without additional polling;
fulfil from the `deposit.credited` webhook.

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

The default theme is neutral, like a form native to your page: a near-black (dark theme:
near-white) primary color, zinc greys, and your page's font. The components draw no frame or
background and fill their container's width: place them in your own card or dialog, and pick
the theme (or map the colors) that suits its background.

| CSS custom property             | Light default     | Dark default | Use                                                 |
| ------------------------------- | ----------------- | ------------ | --------------------------------------------------- |
| `--pp-color-surface`            | `#f4f4f5`         | `#18181b`    | Status row, loading placeholder, copy button hover  |
| `--pp-color-text`               | `#18181b`         | `#fafafa`    | Text                                                |
| `--pp-color-text-muted`         | `#52525b`         | `#a1a1aa`    | Labels, hints, in-progress status icon              |
| `--pp-color-border`             | `#e4e4e7`         | `#27272a`    | Borders and dividers                                |
| `--pp-color-primary`            | `#171717`         | `#fafafa`    | Pay button, selected tab and choice                 |
| `--pp-color-primary-foreground` | `#fafafa`         | `#18181b`    | Text on the primary color                           |
| `--pp-color-focus`              | `#2563eb`         | `#60a5fa`    | Keyboard focus ring                                 |
| `--pp-color-success`            | `#15803d`         | `#4ade80`    | Credited icon                                       |
| `--pp-color-warning`            | `#b45309`         | `#fbbf24`    | "Test mode"                                         |
| `--pp-color-danger`             | `#dc2626`         | `#f87171`    | Failure icon, wallet errors                         |
| `--pp-font-family`              | inherited         | inherited    | Font                                                |
| `--pp-font-family-mono`         | `ui-monospace, …` | same         | Addresses, amounts to copy, hashes                  |
| `--pp-font-size`                | `14px`            | `14px`       | Root text size; other text sizes are relative to it |
| `--pp-radius`                   | `8px`             | `8px`        | Corner radius of controls and the status row        |
| `--pp-control-height`           | `44px`            | `44px`       | Minimum height of the pay button, tabs, and choices |

Import the static stylesheet once in your application entry point:

```ts
import "@phala/pay-react/styles.css";
```

Set these CSS custom properties in your own stylesheet, after the SDK stylesheet, on
`.pp-root[data-theme]`; they are the supported way to theme the components. Pair a light primary
color with a dark `--pp-color-primary-foreground`. Set `--pp-font-size: 1em` to follow your
page's text size. Keep `--pp-color-focus` distinct from `--pp-color-primary`, with at least 3:1
contrast to the background: it is how a keyboard user tells the focused choice from the selected
one.

```css
.pp-root[data-theme] {
  --pp-color-primary: #cdfa50;
  --pp-color-primary-foreground: #161616;
  --pp-color-focus: #2563eb;
  --pp-radius: 12px;
}
```

To map your own light/dark tokens, omit `appearance.theme` and load your mapping after the SDK
stylesheet. Passing `theme: "dark"` sets `data-theme="dark"`, whose SDK selector
`.pp-root[data-theme="dark"]` overrides a plain `.pp-root` mapping, so target
`.pp-root[data-theme]` in your mapping. Without `theme`, defaults suit light backgrounds only; on
dark hosts map every color variable: one left unmapped keeps its light default (a light status
row on a dark card, for example). Mappings written for earlier releases use renamed variables, which
are silently ignored: `--pp-color-text-secondary` is now `--pp-color-text-muted`; see the CHANGELOG
for the others.

The components draw no frame or background: give the card or dialog around them inner padding of
at least 4px, so that the 2px focus outline (offset by 2px) is not clipped by `overflow: hidden`.

The components show a payer's receive address and token contract in full, in groups of four
characters after `0x` (`0x5290 8400 0985 …`), never shortened, so that a payer can compare it
group by group with what their wallet shows; selecting and copying it gives the address without
spaces. The QR code keeps the standard quiet zone of four modules inside 8px of white, so it scans
on a dark page too. The status row's text stays the body color on a neutral surface; its icon
carries the tone. Transitions and the loading placeholder's pulse stop under
`prefers-reduced-motion: reduce`, and the selected choice and tab keep a system color in
forced-colors mode.

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

## React components

Install `@phala/pay-react` for `Checkout`, `DepositAddress`, hooks, icons, and styles:

```sh
npm install @phala/pay-react react react-dom @phala/pay viem
```

Import components from `@phala/pay-react` and styles from `@phala/pay-react/styles.css`. Import
the server client and API types from `@phala/pay-server`, and offline helpers from
`@phala/pay-server/helpers`; see its [README](../js-server/README.md).

The checkout session retains its last known status and quote during temporary network/5xx outages
and exposes `reconnecting: true` with no permanent error. Polling resumes with bounded backoff and
Retry-After; a successful refresh clears the reconnecting state. Invalid-secret and address-mismatch
handling remains unchanged. See [planned upgrades](../../docs/integration.md#planned-upgrades-and-reconnecting).
