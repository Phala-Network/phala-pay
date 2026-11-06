# Onchain KMS migration preparation

The approved migration specification is the root coordination task's `onchain-kms/spec.md`
(2026-11-06), including its cutover table and root decisions. The root will replace this
placeholder with the final cutover runbook after migration tooling (PR 2) is reviewed.

Production and staging remain on Phala Cloud KMS. No cutover is authorized by this deploy-support
change. Migration uses an attested TEE-to-TEE signed, encrypted logical dump; plaintext never
leaves the TEEs. New app ids change webhook, client-secret, database and backup keys. Merchants
must verify and re-pin, and the new app starts a fresh backup prefix. Keep the old app/prefix for
rollback and old backups. Deposit addresses remain unchanged.

Before rehearsal, read the [Base provisioning/upgrade procedure](onchain-kms-upgrade.md),
[device drift procedure](device-drift.md) and [restore procedure](../RESTORE.md). Safe deployer
acceptance, target-node Base KMS availability, initial request normalization, staging restore,
certificate timing and measured downtime remain unverified until the rehearsal.
