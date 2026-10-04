<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="deploy/product/web/brand/lockup-dark.svg">
    <img alt="Phala Pay" src="deploy/product/web/brand/lockup-light.svg" height="48">
  </picture>
</p>

# Phala Pay

Self-hosted, non-custodial crypto payments API for merchants: quotes, deposit addresses, signed
webhooks, and refunds, running in an attested confidential VM.

[![CI](https://github.com/Phala-Network/phala-pay/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Phala-Network/phala-pay/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Phala-Network/phala-pay?filter=v*)](https://github.com/Phala-Network/phala-pay/releases)
[![License](https://img.shields.io/github/license/Phala-Network/phala-pay)](LICENSE)
[![npm](https://img.shields.io/npm/v/@phala/pay?label=%40phala%2Fpay)](https://www.npmjs.com/package/@phala/pay)
[![PyPI](https://img.shields.io/pypi/v/phala-pay?label=phala-pay)](https://pypi.org/project/phala-pay/)

## What it is

- **An API-only payments service in Stripe's shape.** Merchants create quotes and deposit
  addresses with API keys, and credit their customers from signed `deposit.credited` webhooks.
- **Self-hosted.** Each operator runs its own instance in a [dstack](https://github.com/Dstack-TEE/dstack)
  confidential VM on Phala Cloud, for the merchant accounts it onboards.
- **Non-custodial.** Payments land in CREATE2 forwarder contracts that can only pay the
  merchant's own treasury. The service holds no funds and sends no transactions.

## What it isn't

- **Not a hosted service.** Phala runs an instance only for Phala Cloud; everyone else runs
  their own ([self-hosting](docs/self-hosting.md)).
- **Not a wallet, exchange, or dashboard.** There is no custody, trading, fiat conversion,
  signup, or merchant UI; merchants use the API and the SDKs.
- **Not a compliance service.** Beyond direct sanctions screening, compliance (KYC, KYT, the
  Travel Rule) is each operator's and merchant's responsibility.

## Quickstart

Pick the path that matches your role:

- **Merchants** integrating with an operator's instance: the
  [integration quickstart](docs/integration.md#quickstart).
- **Operators** running their own instance: the
  [one-command deploy](docs/self-hosting.md#one-command-deploy) starts a verified release on
  Phala Cloud; the [self-hosting guide](docs/self-hosting.md) covers the environment repository,
  merchant onboarding, and upgrades.
- **Evaluators and contributors**: the [local sandbox](deploy/sandbox/README.md#run-everything-locally)
  builds and runs the stack, then pays and credits a quote end to end.

To see it running, [pay.phala.com](https://pay.phala.com/) has a live demo: a cloud console's
billing page that takes the test tokens of Phala's [staging routes](deploy/phala.md#staging-routes)
on Sepolia and Base Sepolia. The site is static, on Cloudflare Workers; its demo calls an API-only
reference merchant backend at `pay-demo-api.phala.com`, an ordinary merchant account of Phala's
staging instance ([staging reference product](deploy/phala.md#staging-reference-product)).

## Features

- **Quotes**: a locked price, an exact token amount, and a single-use address to pay within a
  window. Late, partial, or extra payments are still credited, at spot.
- **Deposit addresses**: one persistent, rotatable address per customer for every supported token
  on every chain, credited at spot for any amount.
- **Fast credit, watched to finality**: a deposit is credited at the route's confirmation (about
  30 seconds after paying on Ethereum, about 7 seconds on Base), confirmed by a second RPC
  provider, and watched to finality: reversed with `deposit.reversed` if a reorganization proves
  the payment replaced; one whose transaction leaves the chain with its nonce unspent stays
  credited and not final, within the account's cap on such credit, and raises an operator alert.
- **Signed webhooks**: Standard Webhooks with ed25519 keys per account and mode, derived in the
  CVM and pinned by merchants from TDX attestation.
- **Merchant sweeps and refunds**: merchants sweep forwarders and pay refunds from their own
  wallet or Safe; the service verifies refunds at finality.
- **Stripe-style API**: test and live modes, secret and restricted keys, idempotency keys, events,
  cursor pagination, and Stripe's error object.
- **Screening and pricing**: direct sanctions screening with the Chainalysis oracle, and Coin
  Metrics reference-rate prices with a deviation check.
- **Operable in a CVM**: reproducible images, an attested compose, encrypted WAL-G backups,
  restore mode, Sentry alerts linked to [runbooks](deploy/runbooks/README.md).

## How it works

A merchant's backend creates a quote or a customer's deposit address with its API key; the payer
pays a CREATE2 forwarder that can only pay the merchant's treasury; the service, in an attested
CVM, watches the chain with two RPC providers and sends a signed `deposit.credited` webhook; the
merchant sweeps forwarders to its treasury with its own wallet or Safe.
[How Phala Pay works](docs/overview.md) has the diagram, the payment lifecycle, and who owns what.

## Documentation

| I want to… | Read |
|---|---|
| Understand the model | [How Phala Pay works](docs/overview.md) |
| Integrate as a merchant | [Integration guide](docs/integration.md), [API reference](https://phala-network.github.io/phala-pay/) |
| Run my own instance | [Self-hosting guide](docs/self-hosting.md), [releases](https://github.com/Phala-Network/phala-pay/releases), [deployment reference](deploy/README.md), [runbooks](deploy/runbooks/README.md) |
| Configure the service | [Service configuration](docs/configuration.md) |
| Read the specification | [Architecture](docs/architecture.md), [design record](docs/design/multi-tenant.md) |

The [documentation index](docs/README.md) lists every document.

## SDKs

| Package | Install | For |
|---|---|---|
| [`@phala/pay`](sdk/js) | `npm install @phala/pay viem` | The browser checkout (`<Checkout>`, `<DepositAddress>`) and server helpers for Node |
| [`phala-pay`](sdk/python) | `pip install phala-pay` | The Python backend client, webhook verification, and address pinning |

The SDKs share the service's version: pin the one equal to your operator's service version
([compatibility](docs/integration.md#compatibility)). The Python SDK's low-level client is generated
from [crates/topup/openapi.json](crates/topup/openapi.json), the API's OpenAPI document.

## Status

Phala Pay is pre-1.0 and has had no third-party security audit. Phala's production instance is
not deployed yet; its staging instance (`https://pay-api-staging.phala.com`) serves test routes
on Sepolia and Base Sepolia. [The plan to production](docs/plan.md) lists what remains.

## Contributing

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the development setup,
tests, and pull request conventions, and everyone taking part follows the
[code of conduct](CODE_OF_CONDUCT.md).

## Security

Report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md). Do not open public
issues for them.

## License

[Apache-2.0](LICENSE)
