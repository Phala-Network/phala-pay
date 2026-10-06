# @phala/pay-react

React components for Phala Pay. The framework-free checkout core is [`@phala/pay`](../js); the merchant backend is [`@phala/pay-server`](../js-server).

```sh
npm install @phala/pay-react @phala/pay react react-dom viem
```

Import `Checkout`, `DepositAddress`, `NetworkIcon`, `AssetIcon`, and hooks from `@phala/pay-react`, and include `@phala/pay-react/styles.css` once. React 18 and 19 are supported; React is a peer dependency.

`Checkout` and `DepositAddress` retain their last view and payer selection during temporary
network/5xx outages, show a neutral reconnecting message, and resume polling without creating a
new payment. Invalid secrets and mismatched addresses retain their normal failure handling.
See [planned upgrades](../../docs/integration.md#planned-upgrades-and-reconnecting).

Both components poll about every three seconds (`pollInterval`, in milliseconds), with uniform
±20% jitter on intervals and failure backoff, respecting `Retry-After`. Polling pauses in hidden
tabs and reads immediately when they become visible. Checkout stops after three consecutive
non-terminal 4xx responses other than 408/429, using its existing error state. Success or another
kind of failure resets the count; 404 still stops immediately as `invalid_client_secret`.

After ten minutes without an observed public-view change, `DepositAddress` uses a base interval
of `max(pollInterval, 15000)` milliseconds, so a longer configured interval stays unchanged.
Any change or visibility regain restores the normal interval and resets the idle window.
Its optional `onChange?: (state: ClientDepositAddress) => void` follows Checkout's callback naming:
it receives the full public view after the first successful read and then once per content change,
including payment confirmations and network credit times. Unlike `<Checkout onChange>`, it does
not report loading. Unchanged polls, reconnecting alone, and replacing the
callback do not notify or restart polling. This callback is for your UI; fulfil from your
`deposit.credited` webhook.

```tsx
<DepositAddress
  depositAddress={{ address, networks }}
  clientSecret={clientSecret}
  apiBase="https://pay.example.com"
  pollInterval={3000}
  onChange={(state) => setPayments(state.payments)}
/>
```

See the [component guide](../js/README.md#a-customers-deposit-address) for the other props.
