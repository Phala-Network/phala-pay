# Provider disagreement

**Trigger:** `TopupDepositStateAgeExceeded` with `state:detected` (the confirm step: finality and
valuation) or `state:confirmed` (the sanctions screen).

**Impact:** affected deposits keep retrying in their state; nothing is rejected or credited on
one provider's word. A sanctions hit is different: any provider answering `Sanctioned` rejects the
deposit at once.

## First steps

1. Take the `deposit_id` from the Sentry event and read its timeline with the admin deposit view
   (`admin GET /v1/admin/deposits/{id}`). The latest attempt's `evidence` names the failure:
   `stage: "finality"` with `error` `rpc_disagreement`, `rpc_failure`, `recipient_mismatch`, or
   `log_absent_at_finality`; `stage: "valuation"` (a price failure: [Price outage](price-outage.md));
   or, in `confirmed`, the oracle answers of `provider_a` and `provider_b`.
2. For price evidence, compare every observation's source/company/role, scaled price, age,
   round, heartbeat and decision in the deposit timeline. Chainlink A/B disagreement halts;
   no role may fail over to conceal a fresh conflicting answer. Any fresh stablecoin depeg
   halts even when another source agrees with one dollar. Mainnet observations on test routes
   must carry the explicit route-chain marker; check Base sequencer status and recovery grace.
3. Compare the chain providers:

   ```sh
   cast block finalized --json --rpc-url "$RPC_PROVIDER_A_URL" | jq '(.data // .) | {number,hash}'
   cast block finalized --json --rpc-url "$RPC_PROVIDER_B_URL" | jq '(.data // .) | {number,hash}'
   ```

4. For chain-evidence disagreement only, pause settlement on the route (not for a sanctions hit):

   ```sh
   admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["settlement"]}'
   ```

## Decide

- Same finalized height, different hash or log: keep settlement paused; never pick a provider's
  answer by hand.
- `log_absent_at_finality`: both providers are final past the block and neither has the log the
  scanner recorded as final. That is a finality violation or a scanner-provider fault: keep
  settlement paused, open incidents with both providers, and escalate.
- One provider behind but consistent: wait within its SLA, then replace it.
- Sanctions: `Sanctioned` from either provider → `rejected(sanctioned)`, before any pause is
  considered; page Compliance and follow [rejected funds at treasury](rejected-funds-at-treasury.md)
  (no refund until Compliance records a disposition). `Unavailable` from one and no `Sanctioned`
  → the screen retries. `Clear` from both → normal.

## Fix

Open provider incidents with the exact block and log evidence. Replacing a provider is a route
config change and Deploy `upgrade`. Price-source replacement also needs a Legal **Allowed**
verdict in the attested registry and disjoint primary/check companies. Never override a price
or substitute another endpoint of the same company. Stablecoin defaults are Chainlink-only;
Kraken requires written commercial permission and Binance/Coinbase/Coin Metrics are not
commercially eligible. PHA production uses
[Uniswap V2 TWAP](../../docs/design/price-failover.md#pha-on-chain-follow-up) but still needs an Allowed
Kraken check after written permission. The reader halts on TWAP/spot divergence, sample jumps,
staleness, insufficient persisted history or liquidity below its floor. After repair, observe two policy windows.

## Done when

Both providers agree, the deposit's attempts advance (the daily report's
`age_in_state_max_seconds` for the state falls), and settlement is resumed:
`admin POST "/v1/admin/routes/$ROUTE/resume" '{"scopes":["settlement"]}'`.
