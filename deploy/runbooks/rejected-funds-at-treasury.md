# Rejected funds at treasury

**Trigger:** `TopupUnsupportedInflows` (tag `chain_id`, field `count`: finalized transfers of an
unrouted token to forwarder addresses), a merchant seeing treasury inflow tied to rejected
deposits, or rejected holdings that disagree with the sweeps.

**Impact:** rejected deposits are never credited. Rejections of the route's token
(`below_minimum`, `out_of_bounds`, `out_of_range`, `sanctioned`) reach the
treasury with everything else when the merchant sweeps the forwarder; an unsupported token stays
in its forwarder until someone flushes that token. The case may be a reporting question, a
refundable customer case, or a custody mismatch.

## First steps

1. Read the route's `rejected_holds_atomic` in the daily report
   (`admin GET /v1/admin/reports/daily`); `$TREASURY` below is the treasury the deposit's forwarder
   pays (its `treasury` in the merchant's `GET /v1/forwarders`), which may differ from the
   account's current one.
2. Find the deposits: the merchant lists them with `GET /v1/deposits?tx_hash=…` or
   `?status=rejected`; the admin deposit view (`admin GET /v1/admin/deposits/{id}`) shows each
   one's `rejection_reason` and timeline.
3. Check the chain:

   ```sh
   cast call "$TOKEN" 'balanceOf(address)(uint256)' "$TREASURY" --rpc-url "$RPC_PROVIDER_A_URL"
   cast receipt "$FLUSH_TX_HASH" --json --rpc-url "$RPC_PROVIDER_A_URL" | jq '(.data // .) | {status,blockNumber,logs}'
   ```

## Decide

- Expected rejection and a matching flush: custody is correct; the merchant decides refund
  eligibility (architecture §15).
- A route-token rejection with no matching flush: the forwarder has not been swept yet, or its
  flush failed (`FlushFailed`); the merchant sweeps ([deploy/README.md, "Sweeping"](../README.md#sweeping)).
- Treasury inflow differs from the `Flushed` events: critical reconciliation incident; see
  [chain frozen](chain-frozen.md) for `custody_balance`.
- Sanctioned funds: the merchant's compliance matter (architecture §15, "Compliance"); the
  operator's compliance owner records its review, and the merchant refunds nothing until its own
  disposition is recorded.

## Fix

The merchant refunds eligible deposits itself: it creates the refund, pays it from the treasury
of the deposit's address, and attaches the transaction with `mark_paid`
([integration guide, §3](../../docs/integration.md#3-refunds)). Returning an unsupported
token first needs a flush of that token (the factory's `flush(treasury, salts, token)`, which
anyone may call), as in [wrong-network deposit](wrong-network-deposit.md) step 3.

## Pending refund transaction alert

`TopupRefundProgressAge` can mean neither endpoint has ever returned a refund's attached payout
transaction for 24 hours. The refund stays pending and its amount stays reserved; missing data
is not proof that it cannot later pay. Ask the merchant to stop replacement refunds and preserve
its signed transaction, broadcast history, sender, nonce, treasury, token, destination and amount.
Read the refund and its `transaction_hash` with `GET /v1/refunds/{id}` through the merchant.

1. Ask both configured independent RPC endpoints for the recorded transaction and receipt by
   hash. If both finalized receipts agree, let normal verification resolve it. If either endpoint
   reports pending/included evidence, or responses disagree or fail, keep the reservation and
   follow [provider disagreement](provider-disagreement.md); do not infer failure.
2. If the original receipt is missing, establish the paying transaction's sender and nonce from
   the original signed transaction, checked against its hash, or matching prior observations from
   both endpoints. A receipt disappearing or the sender's transaction count advancing alone does
   not prove a replacement.
3. Locate the different transaction with the original sender and nonce, then independently
   retrieve its
   transaction, finalized receipt and canonical block on both endpoints. Verify identical sender,
   nonce, replacement hash and canonical inclusion, and inspect all transfers to ensure the
   replacement did not itself pay this refund. For a Safe, use the actual outer payout transaction's
   sender/nonce; the Safe's internal nonce is insufficient. If any required evidence is missing,
   treat the payout as unresolved. A merely dropped mempool transaction can still be broadcast.
4. Preserve the decoded evidence from each endpoint and the merchant's confirmation in an incident
   record. Record the refund/deposit ids, original and replacement hashes, sender/nonce, agreed
   finalized block number/hash, transfer assessment, verifier identity and time. Keep provider
   credentials, full keyed URLs and signed raw transactions out of logs.
5. Through the merchant, repeat `POST /v1/refunds/{id}/mark_paid` with the original
   `transaction_hash` and `replacement_transaction_hash`. One candidate is stored at a time;
   repeating it is idempotent and consumes no additional attachment quota. Each candidate has
   eight persisted recovery passes per epoch on the A/C cadence (checkpoint waiting spends
   none), at most 40 additional logical methods per endpoint, or 120 transport attempts with
   retries. An invalid or never-proven candidate stops consuming RPC after exhaustion. Before
   exhaustion or 24 hours, identical re-POSTs are idempotent no-ops. After both, an explicit POST
   with a fresh `Idempotency-Key` can renew the same candidate (including one that finalized
   late) or supply another; a stored timestamp allows only one renewal per rolling 24 hours per
   attachment. The worker never renews it. Reserve up to 80 additional logical methods per
   endpoint in a 24-hour window crossing epochs (240 transport attempts), charged to the
   extra-operation reserve before submission. The original stays reserved and continues verification. The service requires
   the original's sender and nonce agreed by both endpoints, plus identical finalized replacement
   receipts and both replacement origins matching that original. It then fails the original as
   `transaction_dropped`, audits the candidate and persists proof with `refund.updated` and
   `refund.failed`. Missing original origin, nonce/sender mismatch, provider disagreement or an
   unfinalized replacement leaves the refund pending and reserved. Do not edit the database or
   bypass the reservation. Check the replacement did not already pay before any new payout.

Read receipt probes run at 0, 1, 2, 5, 10 minutes, then every ten minutes through two hours,
then hourly; verify probes hourly from hour one. After 24 hours of absence both probe every six
hours. Once inclusion is observed there are no RPC calls until the published checkpoint reaches
its block. Dual receipt/header verification then retries unresolved evidence at ten minutes,
hourly, and every six hours after day one. Reorgs return to inclusion/checkpoint waiting. Phase,
attempt count and due times survive restarts. The per-account attached-pending cap defaults to
one across modes; environment R/N stay separately configured. A rollback to N-1 can read the
rows, but restores its old cadence and cannot consume replacement candidates.

## Done when

For every reviewed deposit, the chain amount, the flush, the treasury receipt, the reason, and the
refund disposition agree, and the merchant confirms the case list through its recorded contact.
