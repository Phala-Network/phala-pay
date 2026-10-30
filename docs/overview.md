# How Phala Pay works

Phala Pay is open-source, self-hosted software for an API-only, multi-tenant crypto payments
service in Stripe's shape. This page explains the model for every audience. The
[architecture](architecture.md) is the specification, and the [design](design/multi-tenant.md)
records why each decision was made.

## Operators, merchants, and customers

Each operator runs its own instance in its own dstack confidential VM (CVM), for its own
merchants, from a verified release of Phala Pay and a repository that holds only its settings.
[Self-hosting](self-hosting.md) is the path from there to a credited deposit. Phala runs an
instance for its own products: RedPill (live) and Phala Cloud (planned), with no third-party merchants.

The operator onboards each merchant as an account (`acct_…`) through the admin API. The merchant
does everything else with its API keys and the SDKs; there is no dashboard. RedPill is an
ordinary merchant account of Phala's instance.

The service turns deposits of configured ERC-20 tokens into USD-valued credits. It tells the
merchant what to credit with signed `deposit.credited` webhooks, which the merchant fulfills once
per deposit.

It is software, not custody:

- Deposit addresses are CREATE2 forwarder contracts that can only pay the merchant's own treasury.
- The service holds no funds and sends no transactions.
- The merchant sweeps and refunds from its own wallet or Safe.
- The service runs inside a dstack CVM, and each account pins its own webhook signing key from
  attestation.

## Quotes and deposit addresses

Each customer has one persistent, rotatable **deposit address** for every supported token on
every chain (the same address wherever the treasury is the same).

A **quote** adds a locked price and exact amount to that address for `(window_start, expires_at]`.
One window is open per customer and route; a new quote closes the previous window. The first
eligible transfer in the window uses the locked price. Other transfers within the per-transfer
limits credit at spot, like the stable bank-transfer details of Stripe's customer balance.

## Routes

A route is one chain and one token, in test or live mode. Routes are settings that each operator
commits to its environment repository, so they are part of the attested deployment; further
merchants are accounts, not configuration.

Phala's committed [production routes](../deploy/phala.md#production) are live Ethereum USDC/USDT
and Base USDC; its [staging routes](../deploy/phala.md#staging-routes) are test-only on Sepolia
and Base Sepolia. Production-eligible examples use Chainlink-priced stablecoins
([USDT mainnet template](../examples/phala-cloud-usdt.yaml)). The
[PHA mainnet template](../examples/phala-cloud-pha.yaml) is staging/noncommercial only: PHA has
no second Allowed independent price source ([price sources](configuration.md#price-sources)).

## Architecture at a glance

```mermaid
flowchart TB
    payer(["Payer"])
    subgraph merchant["Merchant (e.g. Phala Cloud)"]
        ui["Web app<br/>(Checkout)"]
        backend["Backend<br/>(phala-pay SDK)"]
        wallet["Wallet or Safe"]
    end
    subgraph cvm["Phala Pay (attested)"]
        api["HTTP API"]
        worker["Chain reads,<br/>webhook outbox"]
    end
    fwd["Deposit address<br/>(on-chain forwarder)"]
    treasury[("Merchant treasury<br/>(on chain)")]
    payer -->|"pays"| fwd
    ui -->|"client_secret: status"| api
    ui <--> backend
    backend -->|"API key"| api
    worker -->|"signed webhooks"| backend
    worker -->|"reads, 2 RPC providers"| fwd
    wallet -->|"flush, pays gas"| fwd
    fwd -->|"can only pay"| treasury
```

## Payment lifecycle

1. The merchant backend creates a quote with its API key, or requests the customer's deposit
   address for transfers at spot.
2. The service issues any missing customer network as a CREATE2 forwarder over the merchant's
   treasury. A quote reuses that network and opens its price window.
3. The merchant recomputes the address from its own pins before showing it.
4. A checkout transaction hint starts immediate verification; unhinted transfers are discovered
   by the five-minute read scan for recent quote/deposit recipients, or hourly dual coverage for
   cold addresses (about 70 minutes with healthy endpoints and caught-up coverage).
5. It is recorded once its block reaches the confirmation (2 blocks on Ethereum, about 30 s after
   paying; 3 blocks on an OP-stack chain such as Base, about 7 s; or the account's stricter
   policy).
6. Both RPC providers independently agree on receipt, transaction, header and transfer fields;
   the quote is taken at that instant.
7. Sanctions screening and per-deposit bounds.
8. Credited: a signed `deposit.credited` webhook, retried until the merchant fulfills it once.
9. Independent dual checkpoints every ten minutes watch finality: a payment a reorg proves
   replaced becomes `deposit.reversed`; one gone with its nonce unspent stays credited, not
   final, within the cap, and alerts the operator.
10. The merchant (or anyone) flushes forwarders to its treasury; the service marks deposits swept
    from the finalized `Flushed` events.
11. Hourly dual log coverage and custody reconciliation per forwarder.

No payment needs an operator step, and there is no failure state: anything that cannot complete
retries with backoff and raises an alert on age. Deterministic denials are recorded with evidence
and never credited.

## Who owns what

| Party | Owns |
|---|---|
| Service | Addresses, chain evidence, finality, screening, pricing, deposit state, credits and their webhooks, the swept status it reads from the chain, and reconciliation. |
| Operator | Creating accounts, deciding live access, issuing first and recovery keys, handling incidents, and its own compliance. |
| Merchant | Its keys, treasuries, webhook endpoints, sweeps, and refunds (and their gas), and its customers' identity (KYC), balances, entitlements, and billing policy. |

Payers, treasuries and refund destinations are screened against verified OFAC SDN snapshots
and audited operator supplements. A stale or unavailable negative answer holds the decision;
positive hits still deny. This is direct address-list screening; operators own broader compliance.
