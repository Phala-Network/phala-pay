# RPC providers: fee models, free tiers and compatibility

This page tells you which RPC providers fit Phala Pay's read model and what each one costs, so you
can choose a pair for each environment. [RPC operations](RPC.md) has the current pilot budget,
stop lines and alerts. [Chain reads](../docs/design/chain-reads.md) specifies the read and evidence
paths.

All prices and limits come from each provider's own pricing and documentation pages, checked on
2026-10-11. Providers change plans often, so re-check a provider's pages before you adopt it.

## What a provider must support

- Ethereum (1) and Base (8453) for production; Sepolia and Base Sepolia for staging.
- An HTTPS URL with the API key in a path segment or query value. The configuration requires
  `{key}` in every endpoint URL (see [configuration and preflight](RPC.md#configuration-and-preflight)),
  so keyless public endpoints cannot be configured.
- `eth_getLogs` over windows of up to 3,000 blocks (`max_log_blocks`, which must be lowered for a
  provider with a smaller cap).
- `eth_getBlockByNumber` with the `finalized` and `safe` tags, plus `eth_getBlockByHash`,
  `eth_getTransactionReceipt`, `eth_getTransactionByHash`, `eth_call` (including EIP-1898
  block-pinned calls), `eth_getCode`, `eth_chainId` and `eth_blockNumber`. Archive data is not
  needed beyond recent blocks.
- Production use allowed on the plan you run.

The `read` and `verify` endpoints of a chain must belong to two independent companies.
Aggregators (dRPC, Lava, Pocket/Nodies) route requests to third-party node operators, so a request
can be served by the same operator as the other endpoint. They are fine for staging; for
production, pair them only with a provider that runs its own nodes, and know that independence is
not guaranteed.

Run the preflight (`topup rpc check`, see [RPC operations](RPC.md#configuration-and-preflight))
against a new provider on all four chains before you rely on it.

## Usage model

Calls per endpoint per day, from the [worst-case pilot budget](RPC.md#worst-case-pilot-budget):

```text
calls/day per endpoint ≈ 15,050 fixed + 26.4 × deposits/day
                         (24 calls per deposit, plus 10% for retries)
calls/month per endpoint ≈ 30 × calls/day
```

| Volume | Deposits/day | Calls/day, both endpoints | Calls/month per endpoint |
|---|---:|---:|---:|
| (a) today's pilot caps (production 46 + staging 20) | 66 | 33.6k | about 0.5M |
| (b) | 2,800 | 178k | about 2.7M |
| (c) | 15,000 | 822k | about 12.3M |

At (c) the 1,000 issued-address cap and the hint and quote caps must also rise; custody then adds
about 120 calls/day per route per 1,000 addresses on each endpoint.

About 11% of calls are `eth_getLogs` and about 15% are `eth_call` or `eth_blockNumber`; the rest
are header, receipt and transaction reads. Providers that weight methods differently are
converted to a blended cost per call with this mix.

## Fee models

| Provider | Unit | Weight of our calls | Free tier | Paid price | Cost per 1M of our calls |
|---|---|---|---|---|---:|
| Ankr | API credit | 200 credits per EVM request, any method | Freemium: 200M credits/month (about 1M requests), 30 req/s, 1 project | Pay as you go: $0.10 per 1M credits ($10 minimum deposit) | $20 |
| Infura | Credit | About 80 per call, 255 per `eth_getLogs`; about 90 blended | Core: 3M credits/day, 500 credits/s, 1 API key | Developer $50/month (15M/day); Team $225/month (75M/day) | plan-based |
| BlockPI | Request unit (RU) | About 25 RU blended | 50M RU/month, 20 req/s, 400 RU/s | $0.20 per 1M RU pay as you go; packages such as $49 per 60 days | $5 |
| Chainstack | Request unit | 1 RU per call (2 for archive) | 3M RU/month | Growth $49/month for 20M RU | $2.45 within Growth |
| dRPC | Compute unit (CU) | Flat 20 CU per call | 210M CU per 30 days, public nodes only, 2 s timeout | Pay as you go | $6 |
| Alchemy | Compute unit (CU) | About 25 CU blended | 30M CU/month | Pay as you go: $0.525 per 1M CU | $13.10 |
| QuickNode | API credit | 20 credits per call | 10M-credit trial only | Plans from $49/month | $12.40 |
| GetBlock | Compute unit (CU) | About 20 CU per call | 50k CU/day (about 2,500 calls) | Plans from $49/month | plan-based |
| OnFinality | Response unit | Not published | 400k response units/day | Growth $49/month for 20M, then $6 per 1M | unknown |
| Lava | Request | 1 per request | 5M requests/month, 25 req/s | Starter $179/month for 25M | $7.16 within Starter |
| Nodies (Pocket) | Request | 1 per request | 4M requests/month, low rate limit | Starter $50/month for 10M | $5 within Starter |
| Coinbase CDP Node | Billing unit (BU) | About 55 BU blended | 10M BU/month; payment method required | $0.50 per 1M BU | $27.50 |

## Compatibility

| Provider | Our chains | `eth_getLogs` window on the free tier | On paid plans | Keyed HTTPS | Usable here |
|---|---|---|---|---|---|
| Ankr | All four | Enough for 3,000 blocks | Same | Yes | **Yes** (current read endpoint) |
| Infura | All four | Enough for 3,000 blocks | Same | Yes | **Yes** (current verify endpoint) |
| BlockPI | All four | 5,000 blocks with an address or topic filter | Same | Yes | **Yes** |
| dRPC | All four | Public nodes, 2 s timeout | Full | Yes | Staging; production only with an own-node partner |
| Lava | Not verified | Not documented | Not documented | Yes | Staging, after `rpc check` |
| Nodies (Pocket) | Not verified | Not documented | Not documented | Yes | Staging, after `rpc check` |
| OnFinality | Not verified | Not documented | Not documented | Yes | After `rpc check` |
| Coinbase CDP Node | Base only | Not documented | Not documented | Yes | Base only, after `rpc check` |
| Chainstack | All four | 100 blocks | 10,000 blocks | Yes | Paid plans only |
| Alchemy | All four | 10 blocks | Unlimited range on pay as you go | Yes | Paid plans only |
| QuickNode | All four | 5 blocks (trial) | 10,000 blocks | Yes | Paid plans only |
| GetBlock | All four | Not documented | Not documented | Yes | Free tier too small |
| Tenderly | Not verified | No free tier | Quotas not published | Yes | Not evaluated |
| PublicNode, LlamaRPC | Varies | Varies | n/a | No key | **No**: endpoints need `{key}`, and there is no service commitment |
| Blast (Bware) | n/a | n/a | n/a | n/a | **No**: shut down in October 2025 |
| Grove | n/a | n/a | n/a | n/a | **No**: wound down in July 2026 |

None of these providers' documentation restricts the `finalized` and `safe` tags on Ethereum or
Base. On aggregators, consecutive requests can reach different backend nodes, so head and tag
answers can disagree between calls; the preflight and the read model's dual checks catch this,
but it costs retries.

## Monthly cost per pair

Costs for both endpoints, from the usage model above.

| Pair (read + verify) | (a) | (b) | (c) |
|---|---:|---:|---:|
| Ankr Freemium + Infura Core (current) | **$0** | about $104 (Ankr $54 + Infura Developer $50) | about $471 (Ankr $246 + Infura Team $225) |
| BlockPI + Infura Core | $0 | about $53 (BlockPI $3 + Infura Developer $50) | about $251 to $277 |
| BlockPI + dRPC | $0 to $3 | about $19 | about $100 to $126 |
| BlockPI + Chainstack Growth | $49 | **about $52** | **about $75 to $101** |

BlockPI + Chainstack Growth is the cheapest pair in which both companies run their own nodes once
volume exceeds the free tiers. BlockPI + dRPC costs less but gives up the independence guarantee
described above.

## Free capacity

Infura's and Ankr's free plans each allow a single key per account, and their quotas are per
account. Do not register more accounts at the same provider to stack free quotas: it risks
suspension under the provider's terms, and a suspended key stops crediting.
To add free capacity, spread environments and chains across different companies instead, each with
its own account:

- Give staging its own pair, for example BlockPI + dRPC, so production no longer shares the Ankr
  and Infura quotas with staging.
- Give a production chain (for example Base) a different pair from the other chain.

Each change needs new sealed keys, the endpoints in `topup.yaml`, matching quota alerts in
`rpc-alerts.yaml`, a recomputed [pilot budget](RPC.md#worst-case-pilot-budget) and a passing
preflight.

## Sources

Checked on 2026-10-11.

- Ankr: [API credits](https://www.ankr.com/docs/rpc-service/pricing/), [service plans](https://www.ankr.com/docs/rpc-service/pricing-plans)
- Infura: [pricing](https://www.infura.io/pricing), [daily limits](https://support.metamask.io/develop/account/limits/daily-limits/), [credit costs](https://docs.metamask.io/services/get-started/pricing/credit-cost/)
- BlockPI: [packages and rate limits](https://docs.blockpi.io/pricing/pricing-and-rate-limit), [RU table](https://docs.blockpi.io/pricing/request-unit-ru/evm-ru-table), [range limits](https://docs.blockpi.io/basic-tutorials/best-practices)
- Chainstack: [pricing](https://chainstack.com/pricing/), [`eth_getLogs` limits](https://docs.chainstack.com/docs/understanding-eth-getlogs-limitations)
- dRPC: [pricing](https://drpc.org/pricing), [compute units](https://drpc.org/docs/pricing/compute-units), [free and paid](https://drpc.org/docs/pricing/requests)
- Alchemy: [pricing](https://www.alchemy.com/pricing), [compute unit costs](https://www.alchemy.com/docs/reference/compute-unit-costs), [`eth_getLogs`](https://www.alchemy.com/docs/reference/eth-getlogs)
- QuickNode: [pricing](https://www.quicknode.com/pricing), [credits](https://www.quicknode.com/api-credits/eth)
- GetBlock: [pricing](https://getblock.io/pricing/), [compute units](https://docs.getblock.io/getting-started/plans-and-limits/what-counts-as-a-cu)
- OnFinality: [pricing](https://onfinality.io/en/pricing)
- Lava: [access plans](https://docs.lavanet.xyz/rpc-api-access-plans)
- Nodies: [pricing](https://www.nodies.app/pricing)
- Coinbase CDP Node: [pricing](https://docs.cdp.coinbase.com/data/node/pricing)
- Tenderly: [Node RPC pricing](https://docs.tenderly.co/node/pricing)
- PublicNode: [terms](https://www.publicnode.com/terms)
