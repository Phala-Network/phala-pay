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
