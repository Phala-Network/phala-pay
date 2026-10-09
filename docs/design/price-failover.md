# Price failover

Status: Accepted (owner-delegated, 2026-10-04); Chainlink and Uniswap on-chain consumption are
Allowed. PHA production quotes and spot credit remain disabled because no second Allowed source
is available.
Uniswap V2 TWAP is implemented with persisted service observations.

## Decision

Remove Coin Metrics. Production stablecoin valuation reads Chainlink Data Feeds through the price
chain's read and verify endpoints. Public exchange adapters remain available for explicit noncommercial staging rehearsal,
not production defaults. Volatile prices require two fresh observations from independent
companies to agree; an outage or disagreement
pauses the affected quote/credit path. On-chain evidence follows [chain reads](chain-reads.md).

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
| Kraken public market data | [API guide](https://docs-legacy.kraken.com/api/docs/guides/global-intro): “You must seek our prior permission for certain uses of the Kraken API's. This includes, but is not limited to, any non-personal commercial use of data from publicly accessible endpoints, such as market data … contacting `marketdata@kraken.com`”. | **PermissionRequired**; staging-only. |
| Binance, including `data-api.binance.vision` | [Terms](https://data.binance.vision/terms-of-use.html) §3.1: **CC BY-NC-SA 4.0**; §3.4: “any commercial utilization requires a separate, written enterprise data license agreement executed with Binance”. | **Prohibited** for commercial use without an enterprise licence. |
| Coinbase market data | [Terms](https://www.coinbase.com/legal/market_data): “exclusively for you or your entity's personal or research purposes and may not be used to build an application intended for use by end users…”; redistribution and derived works are also prohibited. | **Prohibited**. No adapter. |
| Coin Metrics Community | [Package terms](https://docs.coinmetrics.io/packages/coin-metrics-community-data): “non-commercial use only”; [licence](https://coinmetrics.io/?p=16175): **CC BY-NC 4.0**. | **Prohibited**; removed from runtime and defaults. |
| Uniswap V2 on-chain TWAP | [PHA/WETH pair contract](https://etherscan.io/address/0x8867f20c1c63baccec7617626254a060eeb0e61e): public contract state through our own RPC; no API account or terms. | **Allowed**; source `uniswap_v2_twap`, company `uniswap-v2-onchain`. |

Staging PHA uses on-chain TWAP primary and explicitly opted-in Kraken check. Stablecoin defaults use only
Chainlink and do not need a staging licensing opt-in. PHA production quotes/spot credit remain
unavailable until the Kraken check is Allowed after written permission. Source counts and
company disjointness are never weakened to enable production.

## Valuation rules

### Stablecoins

USDC and USDT are the only RedPill assets. The source set is Chainlink USDC/USD and USDT/USD on the route chain when a feed exists
(Ethereum and Base), plus Ethereum-mainnet Chainlink as a configured fallback. Defaults contain Chainlink only;
exchange USDC/USD and USDT/USD adapters require explicit noncommercial staging opt-in. Sepolia and Base Sepolia deliberately use mainnet feeds through a
configured mainnet read/verify pair and/or exchange tickers: test tokens have no market, and config marks
this cross-network observation explicitly. A Base route must also read the [sequencer uptime feed](https://docs.chain.link/data-feeds/l2-sequencer-feeds):
when `answer == 1`, or the grace period after recovery has not elapsed, halt.

For every observation, `updatedAt <= now`, the round is complete (`answeredInRound >= roundId`;
reject zero answers), and read and verify return the same bytes at a canonical pin. Chainlink freshness is
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
and `check` company sets are disjoint, with independent read/verify companies in [chain reads](chain-reads.md).
FX is independently checked and is required for USDT-quoted markets.

| Role | Ordered sources | Rule |
|---|---|---|
| primary | **min(Uniswap V2 PHA/WETH TWAP, current spot) × Chainlink ETH/USD** | public on-chain state, Allowed; agreement uses current spot × ETH/USD |
| check | **Kraken PHA/USD order-book mid (`PHAUSD`)** | compare against Uniswap current spot; spread guard at `max_deviation_bps`; PermissionRequired and staging-only |
| fx | Chainlink USDT/USD | independently peg-checked by the existing volatile policy; the USD check is not multiplied by USDT |

Production rejects the route because Kraken is PermissionRequired and no second Allowed source is
available. The two companies remain disjoint. Do not substitute one company or an unreviewed
endpoint.
Legacy exchange adapters remain available for explicit noncommercial rehearsal.

## PHA on-chain follow-up

The orchestrator found no Chainlink PHA feed in the Ethereum, Base, BSC, Polygon or Arbitrum
feed directories (over 1,800 feeds checked), and no Pyth PHA feed. DIA's free API is CC BY-NC-SA;
CoinGecko paid is dropped. Do not add these as defaults or implement a CoinGecko adapter.

The follow-up to #331 implements persisted Uniswap V2 PHA/WETH TWAP and current spot, multiplied by
Chainlink ETH/USD, as the staging primary; the Kraken PHA/USD order-book mid check remains an
explicit staging-only test source. Kraken permission and a sponsored Chainlink PHA/USD feed are out of
scope, so PHA is production-ineligible until a separately reviewed Allowed source exists.

The implementation reads the pair's `price0CumulativeLast`/`price1CumulativeLast` and reserves
through independent Ethereum read and verify endpoints, verifies token ordering and requires agreement,
using counterfactual cumulative accumulation as defined by Uniswap V2. Pair:
`0x8867f20c1c63baccec7617626254a060eeb0e61e`; PHA:
`0x6c5bA91642F10282b576d91922Ae6448C9d52f4E`. Service-recorded cumulative snapshots are persisted in
DB with a **minimum 30-minute window**. Restart retains history and fails closed if it
is insufficient. On-chain token getters confirm PHA is token0 and WETH is token1; the reader
validates both tokens and handles either ordering. ETH/USD is a pinned Chainlink feed with the same A/B, round and freshness checks.

Merchant valuation is **min(TWAP, current Uniswap spot) × Chainlink ETH/USD**, using the same
pinned block for the spot, TWAP endpoint and ETH/USD round. This follows lending-protocol style
conservative pricing: falling markets immediately lower collateral/payment value instead of
letting a payer receive credit at a lagging, higher average. Pumping spot above TWAP cannot raise
valuation above TWAP; depressing spot reduces the credit the payer receives. This limits the
spot-only manipulation incentive, but does not replace the independent market check or exposure
caps. Quote and deposit-credit workers use the same rule.

Independent agreement compares **current Uniswap spot × ETH/USD against Kraken PHA/USD order-book
mid**, calculated as `(bid + ask) / 2` and rounded down at the service price scale, at the route's
`max_deviation_bps` (default 100, or 1%). A Kraken check whose spread relative to mid exceeds that
same bound is unavailable (`wide_spread`), permitting ordered failover or failing closed with
`source_failure`; it is not a price disagreement. Missing, invalid, zero or crossed books are
malformed. Kraken audit evidence records bid, ask and last trade; the last trade does not price
the observation. The adapter also uses book mid for USDT/USD and USDC/USD. It does not compare the
thirty-minute average or the conservative valuation against a live ticker. A normal 2% move can pass
when the current arbitraged markets agree, while an isolated 2% Uniswap pump fails the Kraken
check even though valuation remains capped at TWAP. The TWAP is the manipulation guard: a
spot/TWAP difference **above 3%** pauses valuation by default in either direction, so fast markets
fail closed. Accepted audit evidence records TWAP, spot, agreement and the chosen valuation.

Every pair getter, reserve and Chainlink round/decimals call uses the canonical snapshot pin
specified by [chain reads](chain-reads.md#24-money-evidence-both-endpoints): read supplies the latest block, state calls
use its hash with EIP-1898 `requireCanonical`, and verify independently re-pins after read.
Both endpoints must agree on number, hash and timestamp. Stored endpoint hashes and the latest persisted sample are rechecked for reorgs.
This also pins the existing standalone Chainlink readers. Historical calls within the window
must be supported; archive access to the entire chain is not required.

Default guard rails (under source `twap`) are:

| Field | Default | Reason |
|---|---|---|
| `window_s` | 1800 s; window + sample age ≤2880 s | prevents using a spot-sized window; at least thirty minutes |
| `max_sample_age_s` | 180 s; configurable 60–900 s, at most 600 s on live routes | staging explicitly uses 900 s to tolerate two missed five-minute samples; bounds every gap and anchor slack |
| `min_weth_reserve_usd` | $100,000 | about 100× the default $1,000 unfinalized exposure cap, on the WETH side alone |
| `max_spot_deviation_bps` | 300 (3%) | pauses fast markets and rejects spikes/ramps inconsistent with the averaging window |
| `max_sample_jump_bps` | 500 (5%) | rejects abrupt sample-to-sample reserve-ratio changes before insertion |

The staging-only PHA sampler runs every **300 s** even without quote traffic. Staging explicitly
sets `max_sample_age_s: 900` and `max_sample_jump_bps: 1100`; the defaults remain 180 s and
500 bps. Quote/credit workers share PostgreSQL history and an atomic per-policy lock, append
no more than once every 300 s, and refuse storage failure. Between persisted samples, the
quote's counterfactual endpoint still uses the same
current pinned block as ETH/USD without writing an extra database row. Each policy has its own history so changing limits cannot reuse samples accepted
under weaker settings. Tightening the default deviation from 10% to 3% likewise starts a new
policy window. A restart with a gap exceeding the age limit needs a new continuous window;
it cannot reuse a thirty-minute-old endpoint across an unobserved outage. Jump refusals do not
advance the previous accepted sample. A persistent jump or a stored reorg requires investigation;
there is no automatic price override or history reset. The expand-only migration
`20261029000000_uniswap_twap` adds an immutable table only, retains it on protocol-aware compatible
binary rollback, and keeps compatibility floor `20261028000002`. The inherited 0.9.0 declaration
requires restoring the pre-upgrade backup for legacy 0.8.x; the N-1 gate validates that declaration
in `declared` mode rather than demonstrating old-image startup.

Each refusal emits `price_source_refusals_total{code=...}` plus existing source health and
`price-outage` alerts: `twap_history`, `twap_liquidity`, `twap_spot_divergence`,
`twap_stale_sample`, `twap_sample_jump`, `twap_sample_order`, `twap_token_order`, `twap_storage`,
and `twap_reorg`. A/B disagreement uses the existing `divergent` alert and disagreement counter.

The reviewed pool estimate is $312k TVL/$218k 24h volume (indicative, not runtime evidence).
An attacker must sustain a reserve-ratio skew for **at least thirty minutes** while arbitrageurs
trade against it. This requires moving inventory, paying fees and repeatedly defending the skew;
an isolated transaction cannot dominate the average. These costs depend on actual reserves,
arbitrage participation and external prices, so they are not a guaranteed dollar security bound.
A flat sustained skew can pass TWAP's internal guard rails: the independent agreeing Kraken check
is available only in staging, and `max_unfinalized_credit` (default **$1,000**) bounds our
outstanding exposure. Production remains disabled for PHA until a second Allowed source exists.
The route remains disabled in production; staging-only operation is bounded by the existing caps.

Tests cover arithmetic against recorded Ethereum blocks **26,120,450** and **26,120,610**
([raw fixture](../../crates/adapters/tests/fixtures/uniswap-v2-pha-mainnet.json)), including wrapping counters,
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
    - { source: chainlink, feed: USDC_USD, chain_id: 8453 }
    - { source: chainlink, feed: USDT_USD, chain_id: 1,
        observation_chain_id: 11155111 }
  primary: [{ source: uniswap_v2_twap,
              observation_chain_id: 11155111 }] # volatile PHA only
  check: [{ source: kraken, symbol: PHAUSD, company: kraken }] # requires written permission in production
  fx: [{ source: chainlink, feed: USDT_USD, chain_id: 1, observation_chain_id: 11155111 }]
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
