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
   `known_finalized_replacement` (another transaction already known to the service with the same
   sender and nonce, independently agreed at or below the checkpoint),
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

- `TopupDepositReversed`, one deposit, both providers agree the transaction is gone (or the
  transfer is missing from its final receipt): a real reorg or a replaced transaction. Confirm the
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
