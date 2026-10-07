# Provider disagreement

**Trigger:** `TopupRpcDisagreement`, `TopupSanctionsHold` (held past its confirmation window), or `TopupDepositStateAgeExceeded` with `state:detected` (the confirm step: finality and
valuation) or `state:confirmed` (the sanctions screen).

**Impact:** affected deposits keep retrying in their state; nothing is rejected or credited on
one provider's word. Sanctions decisions also require agreement at the same canonical pin, at or after the payment block. A single hit waits and alerts.

## First steps

1. Take the `deposit_id` from the Sentry event and read its timeline with the admin deposit view
   (`admin GET /v1/admin/deposits/{id}`). The latest attempt's `evidence` names the failure:
   `stage: "finality"` with `error` `rpc_disagreement`, `rpc_failure`, `recipient_mismatch`, or
   `log_absent_at_finality`; `stage: "valuation"` (a price failure: [Price outage](price-outage.md));
   or, in `confirmed`, the oracle answers of `provider_a` and `provider_b`.
2. For price evidence, compare every observation's source/company/role, scaled price, age,
   round, heartbeat and decision in the deposit timeline. Chainlink read/verify disagreement halts;
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
- Sanctions: only `Clear` from both endpoints at the verified screening block permits new credit.
  Only `Sanctioned` from both rejects; page Compliance and follow
  [rejected funds at treasury](rejected-funds-at-treasury.md). Disagreement or unavailability
  holds and retries; never credit or record a sanctions hit from one answer. `TopupSanctionsHold`
  alerts after the route's confirmation window. Preserve block number/hash and both answers.
- Restore replay of an already delivered credit: both endpoints agreeing `Sanctioned` preserves
  the delivered credit, records the hit and blocks sweep. Disagreement or unavailability holds
  without a hit or any change to that credit. Follow
  [sanctioned delivered credit](restore.md#sanctioned-delivered-credit).

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
