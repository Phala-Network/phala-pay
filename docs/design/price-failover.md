# Price failover

Status: Accepted (owner-delegated, 2026-10-04); production sources pending legal confirmation of
commercial use (Chainlink, Kraken, Binance)

## Decision

Remove Coin Metrics. Production valuation reads Chainlink Data Feeds on the route's RPC
groups and public exchange market data. No paid data vendor is introduced. A price is accepted
only when two fresh observations from independent companies agree; an outage or disagreement
pauses the affected quote/credit path. This is the price counterpart to [RPC failover](rpc-failover.md).

Coin Metrics Community is unsuitable: its [package terms](https://docs.coinmetrics.io/packages/coin-metrics-community-data)
say “**non-commercial use only**” and the [announcement](https://coinmetrics.io/?p=16175) says
“**CC BY-NC 4.0**”. Phala production is commercial.

## Licensing gate (reviewed 2026-10-04)

The quoted text below was retrieved with `curl -L` on 2026-10-04 from the exact URL shown. Legal
must re-check it before enabling a source. A quote that cannot be reproduced verbatim is **Unclear**;
only **Allowed** may be a default. “Allowed” also requires rate-limit and jurisdiction compliance.

Until Legal confirms, staging may run the new sources; production must not enable a source whose
verdict is not **Allowed**.

| Candidate | Evidence and quoted clause | Verdict / use |
|---|---|---|
| Chainlink Data Feeds | `curl -L https://docs.chain.link/data-feeds` (“**Data Feeds provide your smart contracts with access to real-world data**”). Consumer ToS is not exposed as stable text. | **Unclear** until Legal accepts consumer terms. |
| Kraken | `curl -L https://docs.kraken.com/api/` (“**The Kraken API provides access to market data and trading functionality**”). Commercial grant is not stated. | **Unclear** until written commercial permission. |
| Coinbase Exchange / Advanced Trade | [Market-data docs](https://docs.cdp.coinbase.com/exchange/docs/rest-api) say “**The Exchange API is free to use**”; [User Agreement](https://www.coinbase.com/legal/user_agreement) reserves “**all rights not expressly granted**”. | **Unclear** for a commercial derived-price service; obtain written permission before default. |
| Binance REST / `data-api.binance.vision` | `curl -L https://data.binance.vision/` (“**public market data**”). | **Unclear** for commercial derived prices and regional use. |
| OKX | [API terms](https://www.okx.com/terms-of-use): “**You may not use the Services for any illegal purpose**” and OKX may “**suspend or terminate**” access. | **Unclear**: commercial derived valuation and jurisdiction need written confirmation. |
| Gate | [Terms](https://www.gate.io/legal/terms-of-service): “**services are not available to … restricted jurisdictions**”. | **Unclear**: jurisdiction and commercial API licence are not explicit. |
| Bitstamp | [Terms](https://www.bitstamp.net/legal/terms-of-use/): “**Bitstamp grants you a limited, non-exclusive, non-transferable licence**”. | **Unclear**: licence scope does not expressly cover server-side price production. |
| Coin Metrics Community | [Terms](https://docs.coinmetrics.io/packages/coin-metrics-community-data): “**non-commercial use only**”; [licence](https://coinmetrics.io/?p=16175): “**CC BY-NC 4.0**”. | **Restricted**; prohibited in production. |
| Other PHA listings | No fetched commercial-use clause. | **Unclear** until reviewed. |

At implementation start, record URL, retrieval date, clause, legal owner and rate limits in the
attested provider registry. If Legal does not mark an independent PHA source **Allowed**, production
PHA quotes and spot credit remain paused; checks are never weakened to one source.

## Valuation rules

### Stablecoins

USDC and USDT are the only RedPill assets. The source set is Chainlink USDC/USD and USDT/USD on the route chain when a feed exists
(Ethereum and Base), plus Ethereum-mainnet Chainlink as a configured fallback, and exchange
USDC/USD and USDT/USD tickers. Sepolia and Base Sepolia deliberately use mainnet feeds through a
configured mainnet RPC group and/or exchange tickers: test tokens have no market, and config marks
this cross-network observation explicitly. A Base route must also read the [sequencer uptime feed](https://docs.chain.link/data-feeds/l2-sequencer-feeds):
when `answer == 1`, or the grace period after recovery has not elapsed, halt.

For every observation, `updatedAt <= now`, the round is complete (`answeredInRound >= roundId`;
reject zero answers), and RPC groups A and B return the same round/value. Chainlink freshness is
`now - updatedAt <= heartbeat + margin`, where heartbeat is pinned per feed and verified against
the [Chainlink feed registry](https://data.chain.link/). A deviation update remains valid until
the heartbeat bound: worst-case age is heartbeat plus margin, and movement is bounded by the
feed's deviation threshold. Exchanges use receive-time `max_age_s`.

Credit exactly 1.00 iff at least one fresh source is within the peg band and **no fresh source is
outside it**. Any fresh source outside the band halts and alerts; no fresh source also halts.
Store every observation and decision for audit.

### Volatile assets

Each role has an ordered failover list. A role advances only on timeout, stale/malformed data or
provider outage. Every accepted valuation needs at least two fresh, agreeing sources; `primary`
and `check` company sets are disjoint, as in `rpc_companies` in [RPC failover](rpc-failover.md).
FX is independently checked and is required for USDT-quoted markets.

| Role | Ordered sources (example) | Rule |
|---|---|---|
| primary | **Kraken PHA/USD (`PHAUSD`)**, then Chainlink PHA/USD if it exists | use first healthy source; Kraken AssetPairs confirms PHAUSD |
| check | **Binance PHAUSDT** | normalize with USDT/USD; Binance differs from Kraken |
| fx | Chainlink USDT/USD, then an **Allowed** exchange USDT/USD | fresh and within FX band |

If Binance is unavailable, pause PHA quotes and spot credit; in production, also pause until Legal
marks both Kraken and Binance **Allowed**. No other Allowed PHA listing is currently verified; do not substitute a
second endpoint of the same company or weaken the two-source rule.

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
    - { source: kraken, symbol: USDCUSD, company: kraken }
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
behaviour (without secrets), and fails on any restricted/unclear default or migration ambiguity.
The old `primary` maps to a one-item primary list and old `check` to check/fx; spot routes fail
validation until a second allowed source is configured.

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
