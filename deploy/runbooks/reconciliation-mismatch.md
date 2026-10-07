# Reconciliation mismatch

**Trigger:** `TopupReconciliationMismatch` (tagged with its `check`), or the `topup-reconciler`
Crons monitor checking in `error` (a check could not complete) or missing its check-in.

**Impact:** the reconciler runs the architecture §13 checks every 10 minutes and stores each first
observation once. Dual coverage records missing transfers; reconciliation checks the resulting
ledger. By check:

| `check` | Automatic action | Blast radius |
|---|---|---|
| `address_derivation` | freezes the chain: [Chain frozen](chain-frozen.md) | whole chain |
| `custody_balance` | freezes the chain: [Chain frozen](chain-frozen.md) | whole chain |
| `credit_recomputation` | alert only | one deposit |

In the restore check's post-restore round, a finding the round could not verify keeps the check
`incomplete` ([RESTORE.md](../RESTORE.md#the-restore-check-variant)).

## First steps

1. Read the finding's `subjects`, `expected`, and `observed` from the Sentry event.
2. For `error` check-ins, read `reconciliation.failed_checks` in the daily report
   (`admin GET /v1/admin/reports/daily`); a failed check is usually an RPC error: check both
   providers first.

## Decide

- `credit_recomputation`: compare the deposit's stored valuation (the admin deposit view,
  `admin GET /v1/admin/deposits/{id}`) with the `deposit.credited` the merchant accepted. If the
  merchant credited a different amount, tell it through its recorded contact and open an
  Engineering incident; never change the credit.

## Fix

Fix the cause, not the finding. A chain freeze is lifted as [Chain frozen](chain-frozen.md)
describes. The lift requires a fresh passing dual-source contract check before the audited
unfreeze; a persistent ledger mismatch freezes again on the next reconciliation round.

## Done when

The next round raises no new finding for the subject and `topup-reconciler` checks in `ok`.
