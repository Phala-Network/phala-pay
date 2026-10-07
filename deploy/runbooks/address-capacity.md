# Address capacity

**Trigger:** `TopupAddressCapacity` (warning at 700, critical at 900 addresses on one chain),
`RpcAddressCapacityWarning` / `RpcAddressCapacityCritical`, or `422 address_capacity_reached`.

**Impact:** the pilot permits 1,000 issued addresses per chain, permanently, across all accounts
and modes. Every historical address counts, including closed quotes and retired deposit addresses.
At the cap, quotes and deposit-address issuance return the same non-retryable error. Existing
addresses remain watched. Closing, cancelling, or retiring an address does not release capacity.

## First steps

Read the admin-signed metrics snapshot and locate the affected chain:

```sh
admin GET /v1/admin/metrics | rg 'topup_issued_address(s|_cap)'
```

Compare `topup_issued_addresses{chain_id}` with `topup_issued_address_cap{chain_id}` (1,000).
The Sentry alert's component is `chain:<chain_id>` and its observation is the historical count.
Tell affected merchants to stop creating new addresses on that chain. Existing payments and
credits continue subject to the ordinary readiness and freeze gates.

## Upgrade paths

At 70%, plan and budget the next capacity release. At 90%, prioritize it before admitting new
merchants. Both paths require an approved design, load measurements, tests and an attested
configuration/release upgrade:

- Move to paid RPC providers with sufficient quotas and reviewed limits, then revise the pilot's
  address cap and request budget in that release. Buying a plan alone does not raise the code cap.
- Implement token-wide scanning or an indexer with equivalent independent verification and
  complete historical coverage, then revise the cap in that release.

Never delete historical addresses or bypass the cap. Before upgrading, run the read-only
[address capacity check](../README.md#rollback-compatibility) on a verified restore copy. It
reports all per-chain counts and refuses a database already above 1,000.

## Done when

The reviewed capacity release is deployed and verified, or merchants have an agreed capacity
plan within the pilot limit. A retry cannot resolve `address_capacity_reached` on its own.
