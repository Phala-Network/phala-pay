# RPC providers

Which RPC providers can serve Phala Pay, how each one bills, and what a deployment costs at three
volumes. Use it to choose a `read` and `verify` pair for each environment.
[RPC operations](RPC.md) owns the live budget, stop lines and alerts, and
[chain reads](../docs/design/chain-reads.md) specifies the read and evidence paths.

> **Measured with real keys:** 2026-10-13 (below). Pricing-page comparison checked 2026-10-11;
> BlockPI RU and rate-limit pages rechecked 2026-10-13 (see [Sources](#sources)).
> Measured limits take precedence over advertised limits. Re-check before adopting a provider.

## Summary

| Volume | Deposits/day | Recommended pair (read + verify) | Monthly cost |
|---|---:|---|---:|
| Pilot (allocation B) | 280 production + 10 staging | Production Ankr + BlockPI; staging Ankr + Infura | **$0** |
| Growth | 2,800 | BlockPI + Chainstack Growth | **about $52** |
| Scale | 15,000 | BlockPI + Chainstack Growth | **about $75–101** |

- **Allocation B (selected):** production keeps Ankr read and verifies with BlockPI on Ethereum
  and Base. Staging keeps Ankr + Infura on its two payment chains and two mainnet price chains.
  One account per company: Ankr is shared, Infura is staging-only, and the two BlockPI keys share
  one account quota. Under the [total budget](RPC.md#worst-case-pilot-budget), shared Ankr
  uses 89.64% of its stop line at D280/H300/Q600/refunds10/10, with 12,000 historical addresses
  and at most 2,000 hot recipients per production chain. Staging uses D10/H20/Q30/refunds1/1
  at 1,000 addresses/chain. The budget includes unbatched openings and monthly recovery; see
  the authoritative formulas before changing load.
- **Allocation C (next candidate, untested):** replace staging read with Chainstack free on
  all four configured chains and retain Infura verify, freeing Ankr for production. Its
  100-block log cap multiplies discovery/coverage requests; preflight, account-wide budgeting,
  throughput and reliability must be measured before adoption. No keys or staging changes are
  required for allocation B.
- **Past the free tiers,** BlockPI + Chainstack Growth remains a paid option. The cost estimates
  below come from pricing pages and a blended request mix, not deployment measurements.

## Requirements

A provider can serve an endpoint only if it offers all of the following.

| Requirement | Detail |
|---|---|
| Chains | Ethereum (1) and Base (8453) for production; Sepolia and Base Sepolia for staging. |
| Keyed HTTPS | Every endpoint URL contains `{key}` in a path segment or query value ([configuration](RPC.md#configuration-and-preflight)). Keyless public endpoints cannot be configured. |
| Log window | `eth_getLogs` over up to 3,000 blocks. A smaller provider cap means lowering `max_log_blocks`, which multiplies log calls. |
| Methods | `eth_getBlockByNumber` with the `finalized` and `safe` tags, `eth_getBlockByHash`, `eth_getTransactionReceipt`, `eth_getTransactionByHash`, `eth_call` (including EIP-1898 block-pinned calls), `eth_getCode`, `eth_chainId`, `eth_blockNumber`. Recent blocks only; no archive history. |
| Production use | Allowed on the plan in use. |
| Independence | A chain's `read` and `verify` endpoints belong to two different companies. |

**Aggregators and independence.** dRPC, Lava and Pocket (Nodies) route requests to third-party
node operators, so a request can land on the same operator as the other endpoint. Use them for
staging; in production, pair one only with a provider that runs its own nodes, accepting that
independence is not guaranteed.

Run `topup rpc check` ([preflight](RPC.md#configuration-and-preflight)) against a new provider
on every chain assigned to it before relying on it. BlockPI free is assigned only mainnets.

## Measured with real keys (2026-10-13)

| Provider | Measured result | Allocation consequence |
|---|---|---|
| Ankr free | All four chains; ≥3,000-block logs; at most 1,000 topic alternatives per position and topic0×topic2 ≤1,024. Production preflight passed after separate Transfer-recipient and factory-address filters | Keep read at `max_log_blocks: 3000` in both environments |
| Infura free | All four chains; production preflight passed after the coverage fix; HTTP 429 bursts when shared with live staging | Keep staging verify; remove production traffic |
| BlockPI free | Ethereum and Base mainnets only; testnets require a paid $49 package. 3,000/5,000-block probes failed with `-32602`, "eth_getLogs is limited to 1024 block range". One key per chain (another chain's key errors "apikey is on another chain"), archive mode off | Use production verify at `max_log_blocks: 1000`, beneath the measured 1,024-block cap |
| dRPC free | All four chains; 101-block cap; safe/finalized and EIP-1898 worked; four-signature × 1,000-recipient filter worked. Staging preflight passed 1/5, with transport/429 and "Temporary internal error" on Base/Base Sepolia | Unreliable for the launch allocation; no dRPC adoption |

The orchestrator's production Ankr/BlockPI `topup rpc check` passed **5/5 consecutive runs**
with real free keys, including historical state, 1×1,000-recipient coverage plus factory filters,
receipt-log equality and dual contract identity. The BlockPI URLs are
`https://ethereum.blockpi.network/v1/rpc/{key}` and `https://base.blockpi.network/v1/rpc/{key}`.
Seal `TOPUP_RPC_BLOCKPI_ETHEREUM_KEY` and `TOPUP_RPC_BLOCKPI_BASE_KEY` in production only.
Measurements are from an operator host; they do not establish CVM egress behavior.

## Capacity check 2026-10-30

Read-only checks from the implementation host on October 30, 2026, 2:26–2:27 PM PDT. Both
production providers on Ethereum and Base accepted one 1,000-call `aggregate3` of canonical
USDC `balanceOf` calls at each endpoint's recent `finalized` block hash (`requireCanonical:true`).
All 1,000 subcalls succeeded with a 32-byte balance result. Each also accepted one address-less
Transfer `eth_getLogs` with 1,000 distinct synthetic recipient topics. No tokens/funds moved.

| Provider | Chain | Finalized pin | aggregate3 latency/results | Log window (blocks) | getLogs latency/result |
|---|---|---:|---|---:|---|
| Ankr | Ethereum | 26,308,176 | 1,278 ms / 1,000 successes | 3,000 | 2,052 ms / empty success |
| BlockPI | Ethereum | 26,308,176 | 345 ms / 1,000 successes | 1,000 | 1,962 ms / empty success |
| Ankr | Base | 53,302,632 | 1,431 ms / 1,000 successes | 3,000 | 2,973 ms / empty success |
| BlockPI | Base | 53,302,445 | 677 ms / 1,000 successes | 1,000 | 2,369 ms / empty success |

The log ranges contain exactly the configured per-request maximum: 3,000 Ankr/1,000 BlockPI,
ending at the recorded pin. A normal hourly Base range is about 1,800 blocks, so the production
adapter issues two BlockPI requests. This check exercises a full supported slice rather than
sending an unsupported 1,800-block BlockPI request. Transfer signature is topic0; topic1 is
unrestricted; topic2 is the 1,000-recipient OR list, without a token-address filter.
Synthetic recipients are the last 20 bytes of SHA-256 of `phala-pay-capacity-2026-10-30:<i>`
for i=0..999. Multicall3 is `0xcA11bde05977b3631167028862bE2a173976CA11`, with
`allowFailure:false`; Ethereum/Base USDC targets match the production config.

Finalized pins were read independently (Base providers reported different heights); this is
endpoint capacity evidence, not a dual-agreement or deployment preflight. Credentials were
loaded locally, with no keys/credential-bearing URLs in output. The client supplied
`User-Agent: phala-pay-rpc-check`; BlockPI rejected Python's default User-Agent at HTTP level
before the successful checks. These single samples establish payload acceptance on this host,
not sustained throughput, CVM egress behavior or a measured monthly budget. Full deployment
preflight, including independent contract identity and common pins, remains required.

## Pricing-page usage estimates

The earlier equal-window, combined-environment comparison used this approximate per-endpoint
model. It is only a pricing illustration; allocation B uses the provider-specific
[worst-case pilot budget](RPC.md#worst-case-pilot-budget), including BlockPI window splitting:

```text
calls/day   ≈ 15,050 fixed + 26.4 × deposits/day     (24 calls per deposit + 10% retries)
calls/month ≈ 30 × calls/day
```

| Volume | Deposits/day | Calls/day, both endpoints | Calls/month, per endpoint |
|---|---:|---:|---:|
| Pilot | 66 | 33.6k | 0.5M |
| Growth | 2,800 | 178k | 2.7M |
| Scale | 15,000 | 822k | 12.3M |

The method mix is about 11% `eth_getLogs`, 15% `eth_call` and `eth_blockNumber`, and 74% header,
receipt and transaction reads. Weighted billing below is blended with this mix. At Scale, the
configured issued-address, hot-set, hint and quote allowances need a new budget; six-hour
custody adds four calls/day per route per 1,000 addresses per endpoint.

## From pricing pages: fee models

| Provider | Billing unit | Our blended weight | Free tier | Paid pricing | $ per 1M of our calls |
|---|---|---|---|---|---:|
| Ankr | API credit | 200 per request | 200M credits/month (≈1M requests), 30 req/s, 1 project | $0.10 per 1M credits, pay as you go ($10 minimum) | 20.00 |
| Infura | Credit | ≈90 (80 per call, 255 per `eth_getLogs`) | 3M credits/day, 500 credits/s, 1 key | Developer $50/month (15M/day); Team $225/month (75M/day) | plan |
| BlockPI | Request unit | ≈25 | 50M RU/month, 20 req/s, 400 RU/s | $0.20 per 1M RU, or packages (for example $49 per 60 days) | 5.00 |
| Chainstack | Request unit | 1 (2 for archive) | 3M RU/month | Growth $49/month for 20M RU | 2.45 ¹ |
| dRPC | Compute unit | 20 per call | 210M CU per 30 days, public nodes, 2 s timeout | Pay as you go | 6.00 |
| Alchemy | Compute unit | ≈25 | 30M CU/month | $0.525 per 1M CU, pay as you go | 13.10 |
| QuickNode | API credit | 20 per call | 10M-credit trial | Plans from $49/month | 12.40 |
| GetBlock | Compute unit | ≈20 per call | 50k CU/day (≈2,500 calls) | Plans from $49/month | plan |
| OnFinality | Response unit | Not published | 400k units/day | Growth $49/month for 20M, then $6 per 1M | — |
| Lava | Request | 1 | 5M requests/month, 25 req/s | Starter $179/month for 25M | 7.16 ¹ |
| Nodies (Pocket) | Request | 1 | 4M requests/month, low rate limit | Starter $50/month for 10M | 5.00 ¹ |
| Coinbase CDP Node | Billing unit | ≈55 | 10M BU/month (payment method required) | $0.50 per 1M BU | 27.50 |

¹ Within the plan's included quota.

## Compatibility: measured limits and pricing-page claims

| Provider | Chains | Free-tier log window | Paid log window | Fit |
|---|---|---|---|---|
| Ankr | All four | ≥3,000 blocks | ≥3,000 blocks | **Production** (current `read`) |
| Infura | All four | ≥3,000 blocks, measured | ≥3,000 blocks | **Staging** (`verify`) |
| BlockPI | Mainnets free; testnets paid | **1,024 blocks measured**, configure 1,000 | 5,000 blocks advertised, untested | **Production** (`verify`), separate chain keys |
| Chainstack | All four | 100 blocks | 10,000 blocks | Production on paid plans |
| Alchemy | All four | 10 blocks | Unlimited (pay as you go) | Production on paid plans |
| QuickNode | All four | 5 blocks (trial) | 10,000 blocks | Production on paid plans |
| dRPC | All four | **101 blocks measured**, public nodes | Full, untested | Free preflight 1/5; unreliable for launch |
| Lava | Not verified ² | Not documented | Not documented | Staging, after preflight |
| Nodies (Pocket) | Not verified ² | Not documented | Not documented | Staging, after preflight |
| OnFinality | Not verified ² | Not documented | Not documented | After preflight |
| Coinbase CDP Node | Base only | Not documented | Not documented | Base only, after preflight |
| GetBlock | All four | Not documented | Not documented | Free tier too small |
| Tenderly | Not verified ² | No free tier | Quotas not published | Not evaluated |
| PublicNode, LlamaRPC | Varies | — | — | **Unusable:** no API key, no service commitment |
| Blast (Bware) | — | — | — | **Unusable:** shut down in October 2025 |
| Grove | — | — | — | **Unusable:** wound down in July 2026 |

² The preflight confirms chains, tags and the log window before first use.

No listed provider documents a restriction on the `finalized` and `safe` tags for Ethereum or
Base. Behind aggregators, consecutive requests can reach different nodes, so head and tag answers
may disagree between calls; the preflight and the dual checks catch this at the cost of retries.

## Monthly cost by pair

Both endpoints combined, from the usage model.

| Pair (read + verify) | Pilot | Growth | Scale |
|---|---:|---:|---:|
| Ankr Freemium + Infura Core (earlier pair) | **$0** | $104 (Ankr $54 + Infura Developer $50) | $471 (Ankr $246 + Infura Team $225) |
| BlockPI + Infura | $0 | $53 (BlockPI $3 + Infura Developer $50) | $251–277 |
| BlockPI + dRPC | $0–3 | $19 | $100–126 |
| **BlockPI + Chainstack Growth** | $49 | **$52** | **$75–101** |

BlockPI + dRPC costs less, but gives up the independence guarantee described under
[Requirements](#requirements).

## Adding free capacity

Infura's and Ankr's free plans each allow one key per account, and quotas are per account. Do not
register more accounts at the same provider to stack free quotas: it risks suspension under the
provider's terms, and a suspended key stops crediting. Spread load across companies instead,
one account each:

1. **Allocation B (this change):** production read Ankr / verify BlockPI; staging read Ankr /
   verify Infura. No staging upgrade is required. Remove production's Infura sealed mapping and
   seal both chain-specific BlockPI keys with the complete production secret set before launch.
2. **Allocation C (next, untested):** staging read Chainstack / verify Infura; production stays
   Ankr / BlockPI. Research and test Chainstack's four-chain support, free endpoint count,
   100-block splitting, per-account RU, burst limits and reliability before proposing it.

BlockPI free cannot serve staging's testnets. dRPC free's 101-block cap and 1/5 preflight record
exclude the abandoned staging move from the launch plan. Never count per-chain keys as separate
free quotas.
Each change needs:

- new sealed keys;
- the endpoints in `topup.yaml`;
- matching quota alerts in `rpc-alerts.yaml`;
- a recomputed [pilot budget](RPC.md#worst-case-pilot-budget);
- a passing preflight.

## Sources

Pricing-page comparison checked on 2026-10-11; BlockPI RU and rate-limit pages rechecked on
2026-10-13.

| Provider | Pages |
|---|---|
| Ankr | [API credits](https://www.ankr.com/docs/rpc-service/pricing/), [service plans](https://www.ankr.com/docs/rpc-service/pricing-plans) |
| Infura | [pricing](https://www.infura.io/pricing), [daily limits](https://support.metamask.io/develop/account/limits/daily-limits/), [credit costs](https://docs.metamask.io/services/get-started/pricing/credit-cost/) |
| BlockPI | [packages and rate limits](https://docs.blockpi.io/pricing/pricing-and-rate-limit), [RU table](https://docs.blockpi.io/pricing/request-unit-ru/evm-ru-table), [response-size billing](https://docs.blockpi.io/pricing/request-unit-ru), [range limits](https://docs.blockpi.io/basic-tutorials/best-practices) |
| Chainstack | [pricing](https://chainstack.com/pricing/), [`eth_getLogs` limits](https://docs.chainstack.com/docs/understanding-eth-getlogs-limitations) |
| dRPC | [pricing](https://drpc.org/pricing), [compute units](https://drpc.org/docs/pricing/compute-units), [free and paid](https://drpc.org/docs/pricing/requests) |
| Alchemy | [pricing](https://www.alchemy.com/pricing), [compute unit costs](https://www.alchemy.com/docs/reference/compute-unit-costs), [`eth_getLogs`](https://www.alchemy.com/docs/reference/eth-getlogs) |
| QuickNode | [pricing](https://www.quicknode.com/pricing), [credits](https://www.quicknode.com/api-credits/eth) |
| GetBlock | [pricing](https://getblock.io/pricing/), [compute units](https://docs.getblock.io/getting-started/plans-and-limits/what-counts-as-a-cu) |
| OnFinality | [pricing](https://onfinality.io/en/pricing) |
| Lava | [access plans](https://docs.lavanet.xyz/rpc-api-access-plans) |
| Nodies | [pricing](https://www.nodies.app/pricing) |
| Coinbase CDP Node | [pricing](https://docs.cdp.coinbase.com/data/node/pricing) |
| Tenderly | [Node RPC pricing](https://docs.tenderly.co/node/pricing) |
| PublicNode | [terms](https://www.publicnode.com/terms) |
