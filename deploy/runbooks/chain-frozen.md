# Chain frozen

**Trigger:** `TopupContractCodeMismatch`, `TopupFinalizedCheckpointConflict`,
`TopupUnverifiedEvidenceMismatch`, or `TopupReconciliationMismatch` with `check:address_derivation` or
`check:custody_balance`; merchants report
`400 chain_frozen` from address issuance or quote creation; `topup-scanner-<chain_id>` misses
its check-ins because the frozen chain's scanner has paused.

**Impact:** the service can no longer vouch for its ledger on the chain: either the factory's
`addressOf(treasury, salt)` disagrees with a stored address (`address_derivation`), so it cannot
prove where deposits go, or a forwarder's finalized balance is not its final deposits minus its
finalized `Flushed` amounts (`custody_balance`), so a transfer or sweep is missing from, or wrong
in, the ledger. Until the freeze is lifted, the chain's deposits wait (nothing is credited), its
scanner (fast discovery and dual finalized coverage) stops, and address issuance and quote creation answer `400 chain_frozen`. Other chains
keep running; credited facts are never rolled back.

## First steps

1. Read `check` and `subjects` from the Sentry event: for `address_derivation`, `chain_id`,
   `address_id`, `salt`, and `treasury`, with `expected.address` (the factory's) and
   `observed.address` (the stored one); for `custody_balance`, `address_id`, `token`, and `block`,
   with `expected` (`deposits_atomic`, `flushed_atomic`) and `observed.balance_atomic`.
2. For `address_derivation`, ask the factory through both providers and check the contracts
   against the attested route:

   ```sh
   cast call "$FACTORY" 'addressOf(address,bytes32)(address)' "$TREASURY" "$SALT" --rpc-url "$RPC_PROVIDER_A_URL"
   cast call "$FACTORY" 'addressOf(address,bytes32)(address)' "$TREASURY" "$SALT" --rpc-url "$RPC_PROVIDER_B_URL"
   cast call "$FACTORY" 'implementation()(address)' --rpc-url "$RPC_PROVIDER_A_URL"
   ```

   For `custody_balance`, read the forwarder's balance at the finding's block through both
   providers, and list the factory's events for it (`Flushed` is the second topic set below):

   ```sh
   cast call "$TOKEN" 'balanceOf(address)(uint256)' "$FORWARDER_ADDRESS" --block "$BLOCK" --rpc-url "$RPC_PROVIDER_A_URL"
   cast call "$TOKEN" 'balanceOf(address)(uint256)' "$FORWARDER_ADDRESS" --block "$BLOCK" --rpc-url "$RPC_PROVIDER_B_URL"
   cast logs --address "$FACTORY" --to-block "$BLOCK" 'Flushed(bytes32 indexed,address indexed,address indexed,address,uint256)' "" "$FORWARDER_ADDRESS" --rpc-url "$RPC_PROVIDER_A_URL"
   ```

3. Tell the merchants with routes on the chain, through their recorded contacts, that deposits on
   the chain are unavailable and their checkouts must show no address
   ([incident communication](incident-communication.md)).

## Decide

- `contract_code_mismatch` / `TopupContractCodeMismatch`: both endpoints agree factory,
  implementation or Multicall3 code differs from the reviewed build. Preserve both hashes and
  the attested route/build. Treat as a configuration or deployment incident; engage Security.
  Restore the reviewed code/configuration. Disagreement or unavailability alone is not a freeze:
  it keeps the chain not-ready and retries. The audited lift refuses until a fresh dual check passes.
- `finalized_checkpoint_conflict` / `TopupFinalizedCheckpointConflict`: both endpoints must
  re-read the previous checkpoint hash before advancing. Preserve the old hash, both current
  answers and heights; investigate a finalized fork or provider fault with both providers and
  Security. Never replace the stored checkpoint manually.
- `unverified_evidence_mismatch` / `TopupUnverifiedEvidenceMismatch`: an existing deposit past
  `detected` differs from dual verified canonical decision fields. Preserve its transition
  evidence and receipt/transaction/header, including time and nonce. Escalate to Engineering,
  Security and Finance; reconcile delivered effects before an audited lift. Never edit the
  deposit to match one endpoint.

- Providers disagree about `addressOf` or the balance: [provider disagreement](provider-disagreement.md) first.
- Factory or implementation differs from the route: configuration or deployment incident; engage
  Security and Finance.
- `custody_balance`: compare the chain's `Transfer` logs to the forwarder and the factory's
  `Flushed` logs for it with the deposit view (`admin GET /v1/admin/deposits/{id}`) of each of its
  deposits. Dual coverage records an omitted transfer after both sources agree; a
  fee-on-transfer or rebasing token, or a ledger row that disagrees with the chain, is an
  Engineering and Finance incident.
- Contracts match but the stored address differs: database corruption or tampering; preserve the
  Sentry event and escalate to Security.
- Funds already reached a stored address the factory does not derive: open a Security incident
  and tell the account that address belongs to, through its recorded contact; the funds are the
  merchant's, and only it can recover what its treasury controls.

## Fix

Fix the cause first: wrong contracts are corrected with a new route version; wrong stored rows by
a restore to a point before the corruption ([RESTORE.md](../RESTORE.md)). Then, with Security's
sign-off, lift the freeze; the daily report lists it as `chain:<chain_id>`, and the `reason` goes
into the audit record:

```sh
admin GET /v1/admin/reports/daily | jq '.reconciliation_blocks'
admin POST "/v1/admin/reconciliation_blocks/chain:$CHAIN_ID/lift" '{"reason":"INC-123: factory and stored addresses agree, Security sign-off"}'
```

The chain resumes on the next iteration of each component, without a restart. The lift first runs a fresh dual-source factory, implementation and Multicall3 code check. It
refuses unavailable, disagreeing or mismatched evidence. Only a passing check permits the
audited lift. A contract mismatch agreed by both endpoints freezes only its chain; disagreement
or one unavailable endpoint keeps that chain not-ready without a freeze row. Correct code alone
does not lift an existing block. If another finding still reproduces, reconciliation freezes
the chain again.

## Done when

A reconciliation round raises no new `address_derivation` or `custody_balance` finding,
`topup-fast-scanner-<chain_id>` and `topup-coverage-scanner-<chain_id>` check in again, and address issuance answers normally.
