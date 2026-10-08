# Deposit reversed or pending after a reorg

**Trigger:** `TopupDepositReversed` (a deposit's transaction left the chain before finality and
the deposit is now `reversed`), or `TopupDepositPendingAfterReorg` (a deposit's transaction has
been in no block for an hour, without positive replacement evidence). Both carry `chain_id` and
`state` tags and the `deposit_id` and `tx_hash` fields.

**Impact:** the service credits at the route's confirmation (two blocks on Ethereum) and watches
each deposit to finality ([architecture §7](../../docs/architecture.md#7-states-and-pump)). A
reversal already sent `deposit.reversed` when the merchant had been told of the deposit
(`credited` or `rejected`); its snapshot's `amount_reversed` takes the credit back in the
merchant's ledger, and a quote the deposit completed always reopened with its reservation; dual coverage later expires or cancels it. Nothing needs
undoing in the service. What an account can lose this way is bounded by its cap on credit before
finality (`max_unfinalized_credit`, $1 000 per mode by default). A reversal
is a chain-health signal: depth-2 reorgs were not observed on post-Merge Ethereum, so more than a
rare one means the chain, or a provider, is misbehaving.

## First steps

1. Read the deposit's timeline and events (the admin deposit view):

   ```sh
   admin GET "/v1/admin/deposits/$DEPOSIT_ID" | jq "del(.admin), .admin.transitions, .admin.events"
   ```

   In `admin.transitions`, the transition to `reversed` has `evidence.result`
   `known_finalized_replacement` (exactly one other transaction already known to the service with
   the same chain, sender and nonce, independently agreed at or below the checkpoint),
   `transfer_absent_at_finality` (with the block both providers showed), or
   `transfer_changed_at_finality`: another transfer is final at the deposit's receipt position (a
   contract-mediated payment re-executed against other state), and `successor_deposit_id`, when
   present, is the new deposit recorded for it, which the pump credits like any other. The
   watch's records (`evidence.stage` `finality`, `result` `followed`) show where the transaction
   was followed; `admin.final_at` stays `null` (and `final` false) on a reversed deposit.
2. Read the transaction on both providers:

   ```sh
   cast rpc --rpc-url "$RPC_PROVIDER_A_URL" eth_getTransactionReceipt "$TX_HASH" | jq .blockHash
   cast rpc --rpc-url "$RPC_PROVIDER_B_URL" eth_getTransactionReceipt "$TX_HASH" | jq .blockHash
   ```

## Decide

- `TopupDepositReversed`, one deposit, both providers positively prove a finalized replacement
  under the single-candidate rule or a transfer missing from its final receipt: a real reorg or
  a replaced transaction. Missing receipts alone do not prove reversal. Confirm the
  merchant received `deposit.reversed` (the view's `events` shows `delivered_at`); if the payer
  still wants to top up, they pay a new quote. With `transfer_changed_at_finality` and a
  `successor_deposit_id`, the payer's payment is the successor instead (the reversed deposit's
  `replaced_by` names it within the same account and mode): confirm it is credited
  (`admin GET "/v1/admin/deposits/$SUCCESSOR_DEPOSIT_ID"`, the evidence's id), and nothing more is
  needed.
- Several reversals on one chain in a short time: treat as a chain or provider incident. Pause
  settlement on the chain's routes so no further credit is made before finality, and escalate:

  ```sh
  admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["settlement"]}'
  ```

- Reversals concentrated on one account: set its cap on credit before finality to 0, so its
  deposits are credited only once final, while its other deposits keep being credited:

  ```sh
  admin POST "/v1/admin/accounts/$ACCOUNT" '{"max_unfinalized_credit":0,"reason":"reversals under review"}'
  ```

- `TopupDepositPendingAfterReorg`: the transaction is out of every block and may still be mined
  (for example stuck in a mempool at a low fee). Wait for positive evidence. A changed account nonce, including an EIP-7702 authorization,
  does not prove replacement. `TopupDepositReversalUnproven` records this wait. If the providers disagree about the receipt, follow
  [Provider disagreement](provider-disagreement.md).

## Replacement-candidate anomaly

When both providers return no receipt for the original transaction, replacement lookup considers
only service-known transactions with the same chain, sender and nonce and a different hash.
K=1 bounds candidate reads:

- Exactly one candidate: independently read its evidence on both endpoints. Reverse only when
  both agree it is finalized at or below the published checkpoint.
- More than one candidate: read none of the candidates and raise an anomaly alert. Make no
  reversal; the deposit stays unresolved, retains its reservations and counts toward stock S
  until an operator resolves the anomaly.
- No candidates: make no candidate reads and wait for positive evidence.

Do not choose among multiple candidates, infer replacement from an account nonce, delete
candidate history or release reservations to bypass the gate. Preserve the deposit timeline,
original and candidate hashes, and existing canonical evidence for operator review. Resolve
the anomaly through reviewed, audited action backed by independent canonical evidence; this
runbook does not authorize a forced reversal. Manual investigation consumes the
[extra-operation reserve](../RPC.md#worst-case-pilot-budget).

## Unresolved finality stock and recovery

At its first unresolved check, a deposit enters S and persists `first_unresolved_at`. This
includes a `detected` deposit whose transfer both endpoints agree is absent during confirmation;
do not wait for the checkpoint. The pump and finality watcher share one persisted backoff
anchored to `first_unresolved_at`: every 60 seconds until ten minutes, every ten minutes until
six hours, then hourly. Exactly one reader runs per due time. The first day budgets
`10 + 34 + 18 = 62` rechecks; each later day budgets 24. The existing one-hour
`TopupDepositPendingAfterReorg` alert remains, including while a replacement anomaly waits
for operator resolution.

Count a deposit in S from that first unresolved check, not after one hour. Monitor S=1 as the
sum across all payment chains per environment, two combined.
Also allow at most one new unresolved entry per environment in any rolling 24 hours; resolved
entries still count in that window. Each day's budget includes first-day cost for one arrival
plus later-day cost for one carried deposit in each environment. A recheck
costs at most four methods per endpoint: `max(3, 1 + 3×K) = 4` at K=1. See the
[complete stock/turnover arithmetic](../RPC.md#worst-case-pilot-budget).

Use `topup_finality_unresolved` (per-chain gauge) for current stock and
`topup_finality_unresolved_entries_24h` (per-chain DB-derived gauge) for new entries. The latter
counts deposits whose persisted `first_unresolved_at` is within the last 24 hours, including
resolved ones. Evaluate the alerts separately for each environment and sum across its chains:

- Current stock: `sum(topup_finality_unresolved) > 1`.
- New entries: `sum(topup_finality_unresolved_entries_24h) > 1`; use this exact DB count
  directly. Resolution does not remove deposits from the rolling count; they leave when their
  `first_unresolved_at` falls outside the last 24 hours. Rechecks do not add entries.
- Age: retain the separate one-hour `TopupDepositPendingAfterReorg` alert.

Restrict selectors to one environment using deployment scrape labels when both environments
share a Prometheus. Above either S or the rolling-24-hour entry allowance, pause new quotes on
all routes of the affected chain and escalate:

```sh
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["quotes"]}'
```

The one-hour age alert is not permission to delay the stock stop action. Verification, existing
credit and reservations continue. Never force a verdict to reduce the stock. Resume new quotes
only after the incident has been reviewed, positive evidence has resolved the affected deposits,
and pending inventory and the next budget window fit the operating limits. Keep the timeline
and resolution audit intact.

## Fix

A reversal needs no repair. If the evidence contradicts the chain (a receipt at or below
`finalized` with the transfer, on both providers), escalate to Engineering with the timeline.
Raising a route's confirmation (for example to `finalized`) is a route config change and Deploy
`upgrade`.

## Done when

The merchant confirms it applied `amount_reversed` for every reversed credit, and no further
reversals arrive (or
settlement is resumed after the incident:
`admin POST "/v1/admin/routes/$ROUTE/resume" '{"scopes":["settlement"]}'`).
