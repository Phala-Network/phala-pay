# Price failover

Status: Accepted (owner-delegated, 2026-10-04); Chainlink on-chain consumption Allowed; PHA
production remains gated on an independent Allowed check source. DEX TWAP is a follow-up after #331.

## Decision

Remove Coin Metrics. Production stablecoin valuation reads Chainlink Data Feeds on the route's RPC
groups. Public exchange adapters remain available for explicit noncommercial staging rehearsal,
not production defaults. Volatile prices require two fresh observations from independent
companies to agree; an outage or disagreement
pauses the affected quote/credit path. This is the price counterpart to [RPC failover](rpc-failover.md).

Coin Metrics Community is unsuitable: its [package terms](https://docs.coinmetrics.io/packages/coin-metrics-community-data)
say “**non-commercial use only**” and the [announcement](https://coinmetrics.io/?p=16175) says
“**CC BY-NC 4.0**”. Phala production is commercial.

## Licensing gate (reviewed 2026-10-04)

The orchestrator reviewed the primary sources below on 2026-10-04. The attested registry records
URL, date, quoted evidence, verdict, legal owner and rate limit. Production accepts **Allowed**
only; **PermissionRequired**, **Prohibited** and **Unclear** are all refused. Explicit
`allow_unclear_sources` is retained for noncommercial staging rehearsal only, including the
existing PHA route; it is never commercial permission. Coin Metrics remains removed from runtime.

| Source | Evidence and quoted clause | Verdict / use |
|---|---|---|
| Chainlink on-chain feeds | Public on-chain feed state read via our own RPC, without an account/key. [ToS](https://chain.link/terms) is client-rendered; full text could not be retrieved. The basis is public on-chain consumption, not a fetched commercial API grant. | **Allowed** for this on-chain consumption. |
| Kraken public market data | [API guide](https://docs-legacy.kraken.com/api/docs/guides/global-intro): “You must seek our prior permission for certain uses of the Kraken API's. This includes, but is not limited to, any non-personal commercial use of data from publicly accessible endpoints, such as market data … contacting marketdata@kraken.com”. | **PermissionRequired**; obtain written permission. |
| Binance, including `data-api.binance.vision` | [Terms](https://data.binance.vision/terms-of-use.html) §3.1: **CC BY-NC-SA 4.0**; §3.4: “any commercial utilization requires a separate, written enterprise data license agreement executed with Binance”. | **Prohibited** for commercial use without an enterprise licence. |
| Coinbase market data | [Terms](https://www.coinbase.com/legal/market_data): “exclusively for you or your entity's personal or research purposes and may not be used to build an application intended for use by end users…”; redistribution and derived works are also prohibited. | **Prohibited**. No adapter. |
| Coin Metrics Community | [Package terms](https://docs.coinmetrics.io/packages/coin-metrics-community-data): “non-commercial use only”; [licence](https://coinmetrics.io/?p=16175): **CC BY-NC 4.0**. | **Prohibited**; removed from runtime and defaults. |
| Uniswap V2 on-chain TWAP (follow-up) | [PHA/WETH pair contract](https://etherscan.io/address/0x8867f20c1c63baccec7617626254a060eeb0e61e): public contract state through our own RPC; no API account or terms. | **Allowed**; company `uniswap-v2-onchain`. Adapter not shipped in #331. |

Kraken/Binance remain in the explicit staging PHA configuration. Stablecoin defaults use only
Chainlink and do not need a staging licensing opt-in. PHA production quotes/spot credit remain
unavailable until both primary and check sources are implemented and Allowed. Source counts and
company disjointness are never weakened to enable production.

## Valuation rules

### Stablecoins

USDC and USDT are the only RedPill assets. The source set is Chainlink USDC/USD and USDT/USD on the route chain when a feed exists
(Ethereum and Base), plus Ethereum-mainnet Chainlink as a configured fallback. Defaults contain Chainlink only;
exchange USDC/USD and USDT/USD adapters require explicit noncommercial staging opt-in. Sepolia and Base Sepolia deliberately use mainnet feeds through a
configured mainnet RPC group and/or exchange tickers: test tokens have no market, and config marks
this cross-network observation explicitly. A Base route must also read the [sequencer uptime feed](https://docs.chain.link/data-feeds/l2-sequencer-feeds):
when `answer == 1`, or the grace period after recovery has not elapsed, halt.

For every observation, `updatedAt <= now`, the round is complete (`answeredInRound >= roundId`;
reject zero answers), and RPC groups A and B return the same round/value. Chainlink freshness is
`now - updatedAt <= heartbeat + margin`, where heartbeat is pinned per feed and verified against
the [Chainlink feed registry](https://data.chain.link/). A deviation update remains valid until
the heartbeat bound: worst-case age is heartbeat plus margin, and movement is bounded by the
feed's deviation threshold during normal feed operation. The pinned publication margin is **600 s**.
Review of the last eight Ethereum rounds observed USDC/USD intervals of **82,812–82,836 s**
(heartbeat **82,800 s**) and USDT/USD intervals of **86,412–86,436 s** (heartbeat **86,400 s**).
Updates already arrive up to **36 s late in calm conditions**; a 60 s margin leaves too little
allowance for congestion and needlessly drops a source. The 600 s margin accommodates publication
delay while deviation-triggered updates continue to constrain movement. This assumes the feed is
operating: completeness, A/B agreement, peg/deviation checks and the heartbeat + margin cutoff
remain mandatory. The [pinned registry evidence](price-feed-registry.json) records this allowance.
Exchanges use receive-time `max_age_s`.

Credit exactly 1.00 iff at least one fresh source is within the peg band and **no fresh source is
outside it**. Any fresh source outside the band halts and alerts; no fresh source also halts.
Store every observation and decision for audit.

### Volatile assets

Each role has an ordered failover list. A role advances only on timeout, stale/malformed data or
provider outage. Every accepted valuation needs at least two fresh, agreeing sources; `primary`
and `check` company sets are disjoint, as in `rpc_companies` in [RPC failover](rpc-failover.md).
FX is independently checked and is required for USDT-quoted markets.

| Role | Ordered sources (noncommercial staging) | Rule |
|---|---|---|
| primary | **Kraken PHA/USD (`PHAUSD`)** | use first healthy source; Kraken AssetPairs confirms PHAUSD |
| check | **Binance PHAUSDT** | normalize with USDT/USD; Binance differs from Kraken |
| fx | Chainlink USDT/USD, plus explicitly opted-in Kraken USDT/USD | fresh and within FX band |

If Binance is unavailable, pause staging PHA quotes and spot credit. Production rejects the
current Kraken/Binance route: Kraken requires written permission and Binance requires an enterprise
licence. The on-chain production plan below replaces Binance rather than treating it as Allowed.
Never substitute one company or an unreviewed endpoint.

## PHA on-chain follow-up

The orchestrator found no Chainlink PHA feed in the Ethereum, Base, BSC, Polygon or Arbitrum
feed directories (over 1,800 feeds checked), and no Pyth PHA feed. DIA's free API is CC BY-NC-SA;
CoinGecko paid is dropped. Do not add these as defaults or implement a CoinGecko adapter.

Implement the DEX TWAP as a separate follow-up PR **after #331 merges**, because it adds persistent
observation history and a migration beyond the current reader/config change. Production primary
will be Uniswap V2 PHA/WETH TWAP multiplied by Chainlink ETH/USD; check will be Kraken PHA/USD
**after written permission** and an attested Allowed verdict. Until then PHA is production-ineligible.
A Phala-sponsored Chainlink PHA/USD feed is a long-term option for two on-chain sources.

The implementation must read the pair's `price0CumulativeLast`/`price1CumulativeLast` and reserves
through independent Ethereum A/B RPC groups, verify token ordering and require agreement; use
counterfactual cumulative accumulation as defined by Uniswap V2. Pair:
`0x8867f20c1c63baccec7617626254a060eeb0e61e`; PHA:
`0x6c5bA91642F10282b576d91922Ae6448C9d52f4E`. Persist service-recorded cumulative snapshots in
DB and require a **minimum 30-minute window**. Restart must retain history and fail closed if it
is insufficient. ETH/USD is a pinned Chainlink feed with the same A/B, round and freshness checks.

Guard rails include a configurable WETH-side USD reserve floor (for example **$100,000**), maximum
TWAP/spot divergence, observation freshness and minimum window. The reviewed pool estimate is
$312k TVL/$218k 24h volume (GeckoTerminal; indicative, not runtime evidence). Sustained manipulation
must move pool inventory over the observation window; this raises cost relative to spot, but is
not a guaranteed security bound. Keep exposure bounded by `max_unfinalized_credit` (default
**$1,000**), enforce the reserve floor and independent Allowed check, and halt on divergence or
insufficient history. Reassess caps against actual reserves before enabling production.

Tests must cover arithmetic against recorded cumulative values (including wrapping counters),
liquidity floor, restart with persisted history, divergence, token ordering/A/B disagreement,
and manipulation-shaped price spikes and sustained skew. Do not accept one company during
bootstrap or an outage.

## Route schema and migration

Replace today's `pricing.primary`/`pricing.check` with a `price` section; accept the old shape
only in a migration parser and emit the resolved new form.

```yaml
price:
  mode: stablecoin # or volatile
  max_age_s: 90 # exchange ticker age; Chainlink uses feed heartbeat + margin
  peg_band_bps: 100
  sources: # stablecoin mode: one quorum list, never primary/check/fx
    - { source: chainlink, feed: USDC_USD, chain_id: 8453, rpc_group: a }
    - { source: chainlink, feed: USDT_USD, chain_id: 1, rpc_group: mainnet-a,
        observation_chain_id: 11155111 }
  primary: [{ source: kraken, symbol: PHAUSD, company: kraken }] # volatile only
  check: [{ source: binance, symbol: PHAUSDT, company: binance }] # volatile only
  fx: [{ source: chainlink, feed: USDT_USD, chain_id: 1, rpc_group: mainnet-b }]
  sequencer_uptime: { feed: BASE_SEQUENCER_UPTIME, grace_s: 3600 }
```

Validation rejects unknown sources, duplicate companies, fewer than two independent volatile
sources, missing FX for USDT markets, unsupported chain/feed pairs, non-positive ages, and an
empty list. Stablecoin mode requires `sources` and forbids `primary`, `check` and `fx`; it must
cover both symbols or an explicit Ethereum-mainnet fallback. `observation_chain_id` is required
for Sepolia/Base Sepolia mainnet observations. `topup config check` resolves and prints ordered lists, feed heartbeat and testnet
behaviour (without secrets), and fails on any non-Allowed production source or migration ambiguity.
The old `primary` maps to a one-item primary list and old `check` to check/fx; volatile routes fail
validation until an independent second source is configured; production also requires Allowed verdicts.

## Operations, metrics and runbooks

Emit Sentry metrics tagged by route, asset, role, source and company: `price_source_health`,
`price_failover_total`, `price_disagreement_total`, `price_depeg_total`, and
`valuation_stuck_seconds`. Page on zero healthy sources, any disagreement, sequencer down,
repeated failover, or a valuation stuck beyond `alerts.stuck_after_s`. Include source error,
round and age, never credentials or raw API bodies.

Update [price-outage](../../deploy/runbooks/price-outage.md) to probe Chainlink RPC/feed,
sequencer status and each exchange, then wait for two policy windows; remove “wait” as the only
remedy and document ordered failover. Update [provider-disagreement](../../deploy/runbooks/provider-disagreement.md)
to show per-source evidence and require Legal-approved replacement; never override a price.

## Implementation and release

1. Add typed Chainlink RPC readers (round completeness, heartbeat/deviation, sequencer grace) and
   adapters with bounded timeouts; retain exchange adapters behind the provider registry.
2. Implement ordered role failover, company-disjoint validation, stablecoin quorum rule and
   migration parser; wire `topup config check` and resolved-config output.
3. Add fault-injection tests for each source outage/staleness, disagreement, depeg, stale round,
   incomplete round, Base sequencer down/grace, and testnet feed absence; add property tests for
   “never one source” and company disjointness.
4. Update architecture/config examples, dashboards and both runbooks; deploy staging and watch
   failover/deposit age before production.

This is a breaking route-config change released in the next minor version: old files are accepted
only for one migration window, then rejected. Rollback uses the previous image and config; no
runtime price override exists.
