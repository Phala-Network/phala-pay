# Price outage

**Trigger:** `price-outage` Sentry events, zero healthy price sources, repeated failover,
`valuation_stuck_seconds` beyond the route's `alerts.stuck_after_s.detected`, or sequencer down/grace.

**Impact:** affected quotes and spot credit halt. Funds remain on chain. Never credit by hand.

## First steps

Pause new quotes and inspect the route's resolved ordered sources and licensing verdicts:

```sh
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["quotes"]}'
topup config check "$CONFIG"
curl --fail --max-time 8 -sS 'https://api.kraken.com/0/public/Ticker?pair=PHAUSD,USDCUSD,USDTUSD'
curl --fail --max-time 8 -sS 'https://data-api.binance.vision/api/v3/ticker/price?symbol=PHAUSDT'
```

Read each configured Chainlink proxy through **both observation groups**, using the approved RPC
probe workflow in [RPC operations](../RPC.md). Never print expanded keyed URLs. Check
`decimals()`, `latestRoundData()` round/value agreement, `answeredInRound >= roundId`, positive
answer, and `updatedAt <= now`. Age must be at most the pinned heartbeat plus 60 seconds.
Ethereum USDC uses 82,800 seconds; Ethereum USDT and Base stablecoin feeds use 86,400 seconds.
Testnet tokens deliberately observe Ethereum mainnet; verify `observation_chain_id` and the
configured `mainnet-a`/`mainnet-b` pair, not a nonexistent testnet price feed.

For Base and Base Sepolia, also inspect the Base mainnet sequencer proxy through
`base-mainnet-a`/`base-mainnet-b`: zero means up, one means down. A zero or future recovery
start, inconsistent round, or the first 3,600 seconds after recovery keeps valuation halted.

## Decide

- Timeout, stale, malformed or unavailable volatile source: the role advances in configured order.
  Repair the failing endpoint or quota; inspect each role's evidence and failover metrics.
- Kraken PHA/USD or Binance PHAUSDT unavailable with no configured independent replacement: PHA
  stays paused. Never accept a single company, a second endpoint of that company, or a price override.
- Any fresh stablecoin source outside the peg band: halt even if every other source is at one dollar.
  Escalate the depeg; stale observations cannot authorize or veto credit. No fresh sources also halts.
- Disagreement or FX depeg: investigate market/feed integrity. Ordered failover must not hide it.
- Licensing failure: obtain Legal approval and an attested registry/config change. Staging opt-in
  never authorizes production.

## Fix

Probe each feed/exchange, correct reviewed configuration or restore provider availability, then
observe **two complete policy windows** (the relevant feed heartbeat plus margin or exchange age,
including sequencer recovery grace). Source replacement requires a Legal-approved, company-disjoint
route config PR and Deploy `upgrade`. See [provider disagreement](provider-disagreement.md).

## Done when

Fresh sources satisfy every role, both RPC groups agree, sequencer grace has expired, deposits
advance with audited valuation evidence, and quotes are resumed:
`admin POST "/v1/admin/routes/$ROUTE/resume" '{"scopes":["quotes"]}'`.
