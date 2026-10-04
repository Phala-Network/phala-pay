# Price outage

**Trigger:** `price-outage` Sentry events, zero healthy price sources, repeated failover,
`valuation_stuck_seconds` beyond the route's `alerts.stuck_after_s.detected`, TWAP safety refusals, or sequencer down/grace.

**Impact:** affected quotes and spot credit halt. Funds remain on chain. Never credit by hand.

## First steps

Pause new quotes and inspect the route's resolved ordered sources and licensing verdicts:

```sh
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["quotes"]}'
topup config check "$CONFIG"
curl --fail --max-time 8 -sS 'https://api.kraken.com/0/public/Ticker?pair=PHAUSD,USDCUSD,USDTUSD'
```

Read each configured Chainlink proxy through **both observation groups**, using the approved RPC
probe workflow in [RPC operations](../RPC.md). Never print expanded keyed URLs. Check
`decimals()`, `latestRoundData()` round/value agreement, `answeredInRound >= roundId`, positive
answer, and `updatedAt <= now`. Age must be at most the pinned heartbeat plus 600 seconds.
The last eight Ethereum rounds arrived up to 36 seconds after their heartbeat in calm conditions;
the pinned margin allows publication delay under congestion while deviation-triggered updates
and agreement/peg checks remain active (see the design and registry evidence).
Ethereum USDC uses 82,800 seconds; Ethereum USDT and Base stablecoin feeds use 86,400 seconds.
Ethereum ETH/USD uses 3,600 seconds. All values are read at one A/B-agreed numeric block.
Testnet tokens deliberately observe Ethereum mainnet; verify `observation_chain_id` and the
configured `mainnet-a`/`mainnet-b` pair, not a nonexistent testnet price feed.

For Base and Base Sepolia, also inspect the Base mainnet sequencer proxy through
`base-mainnet-a`/`base-mainnet-b`: zero means up, one means down. A zero or future recovery
start, inconsistent round, or the first 3,600 seconds after recovery keeps valuation halted.

For PHA, inspect `uniswap_v2_twap` evidence and `price_source_refusals_total{code=...}`.
Read the pinned PHA/WETH pair through both mainnet groups at the **same numeric block** as ETH/USD;
compare headers, token order, both cumulatives and reserves. Check the WETH-side USD reserve floor,
TWAP/spot divergence, the persisted window's block range, and sample ages/gaps. The service sampler
must advance once/minute even without quote traffic. Do not call `sync()` or mutate the pair.

## Decide

- Timeout, stale, malformed or unavailable volatile source: the role advances in configured order.
  Repair the failing endpoint or quota; inspect each role's evidence and failover metrics.
- TWAP primary or Kraken PHA/USD check unavailable with no configured independent replacement: PHA
  stays paused. Never accept a single company, a second endpoint of that company, or a price override.
- Any fresh stablecoin source outside the peg band: halt even if every other source is at one dollar.
  Escalate the depeg; stale observations cannot authorize or veto credit. No fresh sources also halts.
- Disagreement or FX depeg: investigate market/feed integrity. Ordered failover must not hide it.
- Licensing failure: stablecoin defaults use only Allowed Chainlink on-chain data. Kraken is
  PermissionRequired; Binance (including data-api.binance.vision), Coinbase and Coin Metrics are
  Prohibited for commercial use. Keep PHA production disabled. Staging opt-in permits only
  noncommercial rehearsal, never production or a licence grant.
- `twap_history`/`twap_stale_sample`: restore continuous sampling and wait for the configured
  window (at least thirty minutes). Restart does not erase history; a long gap needs a new window.
- `twap_liquidity`, `twap_spot_divergence` or `twap_sample_jump`: investigate reserves and market
  integrity. Safety refusals cannot trigger fallback. Jump refusals preserve the previous sample;
  a lasting jump needs review, not an automatic baseline reset.
- `twap_storage`, `twap_sample_order`, `twap_token_order` or `twap_reorg`: inspect DB availability,
  concurrent reads, pinned token identities and canonical block hashes. Preserve evidence and
  escalate persistent failures. Do not delete history or override a price.
- PHA uses [TWAP × Chainlink ETH/USD primary](../../docs/design/price-failover.md#pha-on-chain-follow-up)
  and Kraken PHA/USD check. Production remains disabled until written Kraken permission and an
  attested Allowed verdict. A sponsored Chainlink PHA feed is a long-term option.

## Fix

Probe each feed/exchange, correct reviewed configuration or restore provider availability, then
observe **two complete policy windows** (the relevant feed heartbeat plus margin or exchange age,
including sequencer recovery grace, and a continuous TWAP window for PHA). Source replacement requires a Legal-approved, company-disjoint
route config PR and Deploy `upgrade`. See [provider disagreement](provider-disagreement.md).

## Done when

Fresh sources satisfy every role, both RPC groups agree, sequencer grace has expired, deposits
advance with audited valuation evidence, and quotes are resumed:
`admin POST "/v1/admin/routes/$ROUTE/resume" '{"scopes":["quotes"]}'`.
