# Changelog

The releases of `@phala/pay` before v0.5.0, when it was versioned on its own and tagged
`sdk-js-v<version>`. From v0.5.0 it shares the service's version, and its changes are recorded in
the top-level [CHANGELOG.md](../../CHANGELOG.md), under "JS SDK". This file is no longer updated.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-10-01

### Added

- `payWithWallet` reads the account's token balance before sending and, when it is below the
  quote's amount, sends nothing and throws `WalletError` with the new code
  `insufficient_balance`: the transfer would revert and still cost gas. `<Checkout>` shows the
  error's message (what the wallet holds and what the quote needs). A wallet that cannot read the
  balance still pays.
- `<Checkout onWalletError>`: called with the `WalletError` and the wallet it tried (the chosen
  browser wallet's provider, or `walletClient`) when the wallet tab could not send the payment, for
  example to offer your own way to fund that wallet on `insufficient_balance`.
- `ClientQuote.amount_credited`, what a credited payment credited in cents (it differs from
  `amount` for a payment of another amount or a late one), and `ClientQuote.typical_credit_seconds`,
  the typical credit time at the quote's confirmation.

### Changed (breaking)

- `parseClientQuote`, and so `<Checkout>` and `createCheckout`, require `amount_credited` (an
  integer or `null`) and `typical_credit_seconds`, which the service sends from v0.3.4: upgrade
  the service before the SDK.

### Fixed

- `<Checkout>` showed the quoted amount as credited ("Payment credited: $25.00" for a $25 quote
  paid with $10). It now shows what was credited and, when that differs from the quote, says so
  ("Payment credited: $10.00 of $25.00"); `onSuccess(quote)` carries it as `amount_credited`.
- `<Checkout>` told a payer "crediting in about 30 seconds" on every chain. It now states the
  quote's typical wait: about 5 minutes at Base's `safe` block, 15 minutes under a `finalized`
  policy.

## [0.3.0] - 2026-10-01

### Changed (breaking)

- `constructEvent` takes webhook public keys only in Standard Webhooks' `whpk_` form (`whpk_` and
  the standard base64 of the key), as `GET /v1/attestation` lists them; a hex or bare base64 key
  is refused.
- `parseClientQuote` requires `confirmations`, an integer or `null`, as the service always sends
  it.
- `constructEvent` requires the event's `actor` and `request` (`null` or the causing request), as
  the service always sends them, and refuses an envelope without them; `WebhookEvent.actor` and
  `request` are no longer optional.

### Fixed

- `constructEvent` returns the event's `request` (the causing API request's `id` and
  `idempotency_key`, or `null`), as `WebhookEvent` declares; it was dropped. A malformed `request`
  is refused like any malformed event.

## [0.2.0] - 2026-09-29

### Changed (breaking)

- `quoteAddress(forwarder, quote, treasury, account)` takes the treasury explicitly, like
  `depositAddress`; it no longer reads the quote's own `treasury`, which a compromised service
  could set to an attacker's with that treasury's valid address.

- `quoteSalt(account, clientReferenceId, quoteId)` replaces `lockSalt`: a quote's address salt is
  tagged `"quote"` (design D3; it was `"lock"`), so `quoteAddress` recomputes the service's
  addresses again.

- `<Checkout expectedAddress>` is required, as are `expectedAddress` in `createCheckout`,
  `useCheckout`, `PhalaPay.checkout(clientSecret, { expectedAddress })`, and the second argument of
  `retrieveQuote`/`PhalaPay.retrieveQuote`: the address your backend recomputed. A quote naming
  another address fails closed with `CheckoutError` `address_mismatch`; the component shows
  nothing to pay.
- `ClientQuote` has `livemode` (the checkout shows "Test mode") and `payment_status: "reversed"`;
  `CheckoutStatus` gains `reversed`, with "reversed" copy.

### Added

- `verifyQuoteAddress(pins, quote)` and `verifyDepositAddress(pins, address)` in
  `@phala/pay/server` recompute an address from `AddressPins` you configure (`account`, `factory`,
  `implementation`, `treasuries` per chain), never from the response's treasury, and throw
  `AddressMismatchError`; in live mode a chain without a pinned treasury fails closed, and test mode
  falls back to the response's treasury with a console warning.

- `CheckoutError.requestId` (the failed response's `Request-Id`) and, for `rate_limited`,
  `retryAfter` (its `Retry-After`, in seconds), which `createCheckout` and `<DepositAddress>` wait
  before reading again.
- `WebhookEvent.request` (the request that caused the event, or `null` for the service's
  workers) and the documented `data.previous_attributes` of `*.updated` events.

- `<DepositAddress clientSecret apiBase pollInterval?>` follows the address's payments and shows
  each within about a block of arriving ("1.5 PHA received on Sepolia, 1 confirmation"), then
  credited, rejected, or reversed; `retrieveDepositAddress`, `parseClientDepositAddress`, and
  `depositAddressIdFromClientSecret` read the same public view.
- `@phala/pay/server`, which takes no secret key: `constructEvent(payload, headers, publicKeys,
  { expectedAccount, expectedLivemode })` verifies Standard Webhooks `v1a` deliveries with
  WebCrypto (Node 20+, Deno, Bun, edge runtimes), failing closed for another account or mode;
  `forwarderAddress`, `quoteSalt`, `depositAddressSalt`, `quoteAddress`, and `depositAddress`
  recompute addresses; `flushTransaction`, `flushTransactions`, `safeBatch`, and `batchChecksum`
  build sweeps offline, with the Transaction Builder's `BatchFile` type.

- `<DepositAddress depositAddress chainId? asset?>` (`@phala/pay/react`): a customer's persistent
  deposit address, one address for all supported tokens and networks. The payer picks a network
  and a token; it shows a QR code of that token's EIP-681 request, the token contract and address
  to copy (each network's own address where they differ), and "send only supported tokens; any
  amount, credited at the market rate when it arrives" copy. `depositAddressTransfer(network,
  asset)` reads and checks one token's amount-less `payment_uri`, and `parseTransferUri(uri)` is
  the EIP-681 ERC-20 transfer parser `quoteTransfer` uses.

- `<Checkout walletClient>` and `payWithWallet(walletClient, quote)`: pay with the page's own viem
  `WalletClient`, such as wagmi's `useWalletClient()` (RainbowKit, ConnectKit, AppKit). The wallet
  tab then uses its account and switches its chain, without listing the browser's wallets.

### Changed

- `<Checkout>` copy for fast credit: a seen payment reads "Payment received, N confirmations.
  Crediting in about 30 seconds"; `confirming` and `credited` are documented for crediting at the
  route's confirmation (two blocks on Ethereum) instead of finality.
- `watchWallets` discovers EIP-6963 wallets through [mipd](https://github.com/wevm/mipd), wagmi's
  discovery store, now a dependency; the `window.ethereum` fallback stays. Wallet errors are
  classified with viem's `UserRejectedRequestError` and `SwitchChainError`.

## [0.1.2] - 2026-09-27

### Added

- `CheckoutError.requestId` (the failed response's `Request-Id`) and, for `rate_limited`,
  `retryAfter` (its `Retry-After`, in seconds), which `createCheckout` and `<DepositAddress>` wait
  before reading again.
- `WebhookEvent.request` (the request that caused the event, or `null` for the service's
  workers) and the documented `data.previous_attributes` of `*.updated` events.

- `<Checkout onChange>`: called once per status change with `{ status, quote, error }`, like
  Stripe Elements' `onChange`, so the page can react to `seen` or `confirming` without its own
  polling.

### Fixed

- The transaction link in `<Checkout>` uses the text color with an underline instead of
  `colorPrimary`, so it stays readable with a light brand color.

## [0.1.1] - 2026-09-27

### Added

- `CheckoutError.requestId` (the failed response's `Request-Id`) and, for `rate_limited`,
  `retryAfter` (its `Retry-After`, in seconds), which `createCheckout` and `<DepositAddress>` wait
  before reading again.
- `WebhookEvent.request` (the request that caused the event, or `null` for the service's
  workers) and the documented `data.previous_attributes` of `*.updated` events.

- `appearance.variables.accessibleColorOnColorPrimary` (`--pp-accessible-color-on-color-primary`,
  default `#ffffff`): the color of text on a `colorPrimary` background, such as the pay button's
  label, so a light brand primary stays readable. Same name and meaning as Stripe's current
  Appearance API variable.

### Fixed

- `<Checkout>` shows the full transaction hash after a wallet payment, wrapping in monospace,
  instead of truncating it, so a payer can verify every character; a long token amount wraps
  instead of overflowing.

## [0.1.0] - 2026-09-27

### Added

- `CheckoutError.requestId` (the failed response's `Request-Id`) and, for `rate_limited`,
  `retryAfter` (its `Retry-After`, in seconds), which `createCheckout` and `<DepositAddress>` wait
  before reading again.
- `WebhookEvent.request` (the request that caused the event, or `null` for the service's
  workers) and the documented `data.previous_attributes` of `*.updated` events.

- `PhalaPay({ apiBase })`: `retrieveQuote(clientSecret)` reads a quote's public view
  (`GET /v1/quotes/{id}?client_secret=…`) and `checkout(clientSecret)` follows it, exposing the
  payer-facing status (also `createCheckout` and `retrieveQuote`).
- `watchWallets` (EIP-6963, `window.ethereum` fallback) and `payWithWallet`: pay a quote's EIP-681
  ERC-20 transfer from a browser wallet, switching or adding the chain.
- `@phala/pay/react`: `<Checkout>` with wallet ("Pay with crypto", `buttonText`), QR code, and
  manual payment, live status, and Stripe-style `appearance`; `useCheckout`.
- `formatTokenAmount(quote, locale?)` shows the exact token amount grouped for the locale and
  without trailing zeros (`1,273.9185`); `tokenAmount(quote)` is the plain decimal a wallet
  accepts, which `<Checkout>`'s "Exact amount" copies.

[0.4.0]: https://github.com/Phala-Network/phala-pay/compare/sdk-js-v0.3.0...sdk-js-v0.4.0
[0.3.0]: https://github.com/Phala-Network/phala-pay/compare/sdk-js-v0.2.0...sdk-js-v0.3.0
[0.2.0]: https://github.com/Phala-Network/phala-pay/compare/sdk-js-v0.1.2...sdk-js-v0.2.0
[0.1.2]: https://github.com/Phala-Network/phala-pay/compare/sdk-js-v0.1.1...sdk-js-v0.1.2
[0.1.1]: https://github.com/Phala-Network/phala-pay/compare/sdk-js-v0.1.0...sdk-js-v0.1.1
[0.1.0]: https://github.com/Phala-Network/phala-pay/releases/tag/sdk-js-v0.1.0
