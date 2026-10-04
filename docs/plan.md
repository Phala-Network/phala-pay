# Plan to production

This page tracks Phala's own instance, not the software: what remains before it takes live
payments for Phala Cloud's account on Ethereum Mainnet, and who owns each item. Phala's instance
serves only Phala Cloud; other operators run their own ([self-hosting](self-hosting.md)). The
decisions are the [design](design/multi-tenant.md) (§16 is its PR plan), and the specification is
[architecture.md](architecture.md). Completed work is in git history and the changelogs.

## Where things stand

- **Service**: the launch set (design §16, PRs 1–12) is merged and released; the latest release
  is on the [releases page](https://github.com/Phala-Network/phala-pay/releases).
- **Contracts**: the permissionless factory is deterministic: `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747`,
  implementation `0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9`, on every chain
  ([deploy/CONTRACTS.md](../deploy/CONTRACTS.md)). Deployed and verified on Sepolia and Base
  Sepolia; not yet on mainnet. The staging finance Safe has its `CompatibilityFallbackHandler` set
  and passes `verify-safe.sh`.
- **Staging** (`https://pay-api-staging.phala.com`) runs the release that
  [deploy-phala.yml](../.github/workflows/deploy-phala.yml) pins, serves the test routes of
  [deploy/phala.md, "Staging routes"](../deploy/phala.md#staging-routes), and runs the reference
  product behind the demo on [pay.phala.com](https://pay.phala.com/). The staging paths passed on
  Sepolia and Base Sepolia: quote, underpayment, late payment, persistent deposit address,
  unsupported token, refund success and failure, sweep, and each test token.
- **SDKs**: [`@phala/pay`](https://www.npmjs.com/package/@phala/pay), [`@phala/pay-react`](https://www.npmjs.com/package/@phala/pay-react), and [`@phala/pay-server`](https://www.npmjs.com/package/@phala/pay-server) on npm, and
  [`phala-pay`](https://pypi.org/project/phala-pay/) on PyPI, published with trusted publishing.

## Remaining work

### Phala Cloud

- [ ] The operator onboards Phala Cloud's account: `POST /v1/admin/accounts` with its due diligence
      record (Phala's own) and contact, and hands the first keys to its engineers, who roll them;
      `charges_enabled` once mainnet is ready. Owner: operator.
- [ ] Phala Cloud integrates like any merchant (design §16, "Phala Cloud"), in the monorepo draft
      PR: restricted key, pins (account, factory and implementation, its treasuries), webhook
      endpoint and pinned webhook keys, `client_reference_id` (team id), every `deposit.*` event
      by the balance rule, `<Checkout expectedAddress>`; its finance sets Phala's Safe as treasury
      per chain through the API (Safe message), sweeps with the SDK's `safe_batch`, and pays
      refunds from the Safe with `mark_paid`. Owner: engineering; review: Phala Cloud team.

### Production inputs

| Input | Owner | Status |
|---|---|---|
| Mainnet PHA contract (proposed `0x6c5bA91642F10282b576d91922Ae6448C9d52f4E`) | Finance | to confirm |
| Phala Cloud's treasury Safe per chain (owners, threshold), set by Phala Cloud through the API; not a route input | Finance | open |
| Two independent mainnet RPC groups (public gateways rate-limit), named by `chain.rpc_groups` and configured in `topup.yaml` with `rpc_groups`, `rpc_companies`, and `rpc_budgets`; keyed members name sealed `TOPUP_RPC_*_KEY` variables ([RPC configuration and acceptance](../deploy/RPC.md#configuration-and-acceptance)) | Ops | open |
| Production R2 bucket and keys for WAL-G | Ops | open |
| Production Phala Cloud workspace and API key for the CVM (`production` Environment) | Ops | open |
| Production admin key (the operator's RFC 9421 key) | Operator | open |
| Sentry quota for production | Ops | open |
| DNS for Phala's production domain, `pay-api.phala.com` (CNAME and `_dstack-app-address` TXT) | Ops | open |
| Route defaults in architecture §14 (minimum deposit 0, minimum credit $1, 4 quote decimals, deposit bounds, open exposure caps) and Phala Cloud's `max_unfinalized_credit` (default $1 000) | Finance | to confirm |

### Before mainnet

- [ ] Deploy the factory on mainnet at the same deterministic address (HUMAN-ONLY).
- [ ] Production deploy (`provision`) with the first live route,
      [examples/phala-cloud-pha.yaml](../examples/phala-cloud-pha.yaml), then a small mainnet
      deposit, sweep, and refund end to end on Phala Cloud's account.
- [ ] Restore drill against production backups, including the freeze and reconciliation.

### Later

- [ ] Design PR 13, account closure.
- [ ] A Phala Cloud template for the one-click testnet quick start, once Phala Cloud publishes it
      ([self-hosting, "The Phala Cloud template"](self-hosting.md#the-phala-cloud-template)).
