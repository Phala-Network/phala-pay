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

> **BLOCKER:** Chainalysis has deprecated its on-chain sanctions oracle and says it is not
> recommended for production sanctions screening. The official notice records the last oracle
> update as March 18, 2026 ([Chainalysis oracle documentation](https://go.chainalysis.com/chainalysis-oracle-docs.html)).
> Production charges must not be enabled until the replacement screening decision is made and
> implemented.

| Input | Owner | Status |
|---|---|---|
| Mainnet PHA contract (proposed `0x6c5bA91642F10282b576d91922Ae6448C9d52f4E`) | Finance | to confirm |
| Phala Cloud's treasury Safe per chain (owners, threshold), set by Phala Cloud through the API; not a route input | Finance | open |
| Production routes (live USDC and USDT on Ethereum and live USDC on Base, deterministic factory/oracles, and Chainlink sources) | Engineering | provided by this PR; PHA remains disabled because it has no second Allowed source |
| Independent Ankr read and Infura verify endpoints for every production payment and price chain, configured with `rpc` and the sealed `TOPUP_RPC_ANKR_KEY` / `TOPUP_RPC_INFURA_KEY` ([RPC configuration and preflight](../deploy/RPC.md#configuration-and-preflight)) | Engineering / Ops | endpoint configuration provided by this PR; shared provider keys remain operator-only |
| Production R2 bucket `phala-pay-production/production-v1` and public WAL-G settings | Engineering | prefix, endpoint, region, and path-style setting provided by this PR; owner must create the bucket and supply sealed access keys |
| Production Phala Cloud workspace and API key for the CVM (`production` Environment) | Ops | open; operator-only |
| Production admin key (the operator's RFC 9421 key) | Operator | public key provided by this PR; private seed remains operator-only |
| `TOPUP_MAINTENANCE_PRIVATE_KEY_PEM` secret and `TOPUP_MAINTENANCE_KEY_ID` variable in the production Environment, matching the attested `maintenance_keys` entry; separate from the full admin key ([planned upgrades](../deploy/README.md#planned-upgrade-admission-and-downtime)) | Operator / Ops | public key provided by this PR; PEM and Environment variable remain operator-only |
| Sentry DSN and production alert/Uptime monitors | Ops | open; required Environment secret `SENTRY_DSN` remains operator-only |
| DNS for Phala's production domain, `pay-api.phala.com` (CNAME and `_dstack-app-address` TXT) | Ops | open; operator-only after provision |
| Route amounts and Phala Cloud's `max_unfinalized_credit` pilot value | Finance | hard gate before upgrade and before enabling charges; template-copied defaults are illustrative until confirmed |
| Parallel pilot caps and monitoring: production `D=46`/day, staging `D=20`/day, `H=80` hint tasks/environment/UTC day, `Q=60` fresh quote snapshots/price chain/environment/UTC day, and `ISSUED_ADDRESS_CAP=1000` per chain | Ops | [RPC budget and stop actions](../deploy/RPC.md#worst-case-pilot-budget); nine hourly custody routes (3 production, 6 staging), pending refunds, unresolved finality stock, slow lane L and proof work. L stock and rolling entries are each ≤1 per environment. D, factory and Safe counts are operational caps |
| Phala Cloud PHA production pricing | Engineering | disabled: the on-chain Uniswap TWAP has no second Allowed independent source; the Kraken check is staging-only |

### Before mainnet

- [ ] Finance signs off the route amounts and the explicit pilot `max_unfinalized_credit`, and the
      operator implements the sanctions-screening replacement decision. No production charges are
      enabled before both gates pass.
- [ ] Create the `phala-pay-production` R2 bucket with account-level credentials, issue the live
      and restore read-only tokens, and configure the protected `production` GitHub Environment
      (including `SENTRY_DSN`); sealing waits until after `provision`.
- [ ] Deploy and verify the deterministic factory on Ethereum and Base (HUMAN-ONLY), then run
      `provision` and copy the run summary's CNAME/TXT records.
- [ ] Create DNS, render and run the unsealed preflight, seal the complete secret set, run the
      Sentry-required preflight, upgrade, and verify attestation, ingress evidence, health, and
      the production monitors.
- [ ] Create a dedicated Phala-owned smoke account, prove its Ethereum USDC/USDT and Base USDC
      treasuries, pin its webhook keys, and enable charges only for that account after the gates.
      Credit live smoke payments on Ethereum USDC, Ethereum USDT, and Base USDC (Base is
      sequencer-gated), while monitoring the shared caps.
- [ ] Complete the production restore drill, including the freeze, attestation, live-isolation
      hard abort, and post-restore reconciliation. The production configuration has live USDC and
      USDT on Ethereum and live USDC on Base; [the PHA example](../examples/phala-cloud-pha.yaml)
      remains a staging/test configuration.
- [ ] Only after the smoke account and restore drill pass, onboard Phala Cloud's account, prove
      its treasuries, configure payment settings and webhook keys, and enable its charges.

### Later

- [ ] Design PR 13, account closure.
- [ ] A Phala Cloud template for the one-click testnet quick start, once Phala Cloud publishes it
      ([self-hosting, "The Phala Cloud template"](self-hosting.md#the-phala-cloud-template)).
