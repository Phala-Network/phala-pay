# Recovery across failure domains

Status: **Declined (owner, 2026-10-04)**: no permanent second CVM. Recovery stays the single-CVM
restore from backup ([RESTORE.md](../../deploy/RESTORE.md)); planned upgrades are addressed by
zero-downtime upgrade work instead. Kept as the reference if the decision is revisited.

This proposal implements nothing. Today one CVM contains PostgreSQL, ingress, egress and workers.
Its replacement requires Phala Cloud/KMS, object storage, DNS and an operator. A second CVM
reduces host/region recovery time; it does not remove shared control-plane dependencies.

## Placement and replication

- Keep a warm PostgreSQL standby in a different physical host and region/provider failure domain.
  Confirm actual placement with Phala Cloud; two CVM IDs on one host do not qualify.
- Restore an encrypted base backup, keep `standby.signal`, and continuously fetch/replay archived
  WAL using WAL-G `restore_command`. A missing segment causes retry, never promotion past a gap.
  This requires a future standby-specific entrypoint; the current restore drill is not a standby.
- Primary retains the one-minute heartbeat and archive timeout. Measure the standby's replay LSN,
  last restored heartbeat timestamp, missing segment age and storage headroom externally.
- Only the primary archives under its writer epoch/prefix. Promotion creates a new timeline and
  backup epoch; the old primary never rejoins as a writer. Rebuild it from the promoted primary.
- Pre-pull immutable release images and prepare ingress, egress and worker configuration, but
  keep all standby workers, heartbeat, API writes and archiving disabled before promotion.
- Keep all financial/application records for seven years. Short base-backup rotation is not a
  seven-year audit retention strategy: retained records must remain recoverable in retained
  current backups and a documented seven-year archival/export policy. Capacity must be funded.

## Single-writer invariant and fencing

DNS, advisory locks in two separate databases, operator memory and an expiring lease alone are
not fencing. A partitioned old primary can still submit transactions or deliver webhooks.

Use an independent strongly consistent witness for an increasing writer epoch and a hardware/
provider fence. Promotion MUST wait for verifiable power-off/deletion or upstream network fencing
of the old CVM, including every RPC and webhook egress path. A successful API request to stop a
CVM is insufficient; require completed operation and independently verified enforcement.

An alternative future automated design is witness-leased write capability enforced by an
independent egress gateway, with maximum request duration, lease expiry and clock-skew safety
margin. Every transaction submission and webhook delivery must pass that gate. Application
self-fencing must also stop DB writes before lease expiry. This is a separate implementation and
security review, not an assumption about current workers.

Until such gates exist, use manual, positively confirmed provider power fencing. If the provider
cannot prove the fence (especially during a control-plane outage), **do not promote**. Availability
is sacrificed to prevent two writers. Witness unavailability also blocks promotion. A database
standby is read-only until the fence and epoch acquisition are complete.

## Promotion procedure (future runbook)

1. Declare an incident and freeze routing/acceptance. Record last primary and standby heartbeats,
   archived/replayed LSN and timeline, measured lag, and suspected failure domain.
2. Acquire incident serialization at the independent witness. Fence the old CVM and confirm the
   fence out-of-band. Record evidence and new writer epoch; never proceed on an ambiguous fence.
3. Drain the archive to the latest contiguous recoverable LSN. Quantify the possible missing
   interval and obtain the incident owner's acceptance of that loss. Do not invent missing WAL.
4. Promote PostgreSQL once, record its timeline, configure the new archive epoch and take a base
   backup. Preserve the old backup lineage for investigation and reconciliation.
5. Run the existing restore-check/reconciliation gate before starting workers. Reconcile
   external chain receipts and potentially delivered webhooks; a lost WAL tail can contain
   already-executed external effects. Keep writes frozen until checks and owner acceptance pass.
6. Start ingress/egress/API and workers only on the new writer. Verify DB ownership, `/healthz`,
   attested compose, TLS evidence, scanner progress and actual webhook delivery.
7. Switch the domain's Phala gateway CNAME and TXT app binding to the standby. Preconfigure a
   60-second TTL and certificate provisioning; measure resolver/client caching in drills.
   An independently hosted proxy can switch faster but adds cost and a trust boundary.
   The old fenced endpoint must remain unavailable even to clients caching old DNS.
8. Watch replay/reconciliation, undelivered events, archiving and capacity. Rebuild the former
   primary as a fresh standby; never reverse promotion or reuse divergent pgdata.

## Key and backup availability

The new CVM must derive exactly the same application-scoped DB, client-secret, webhook and backup
keys through an approved KMS policy bound to attested releases. Verify cross-host/region recovery
with test ciphertext before relying on it; a fresh app identity may derive different keys.

Pre-provision authorized standby key access and test cold restart. Keeping a running CVM with
keys in tmpfs only helps while it stays running; it does not survive reboot or eliminate KMS risk.
Consider an independently administered recovery KMS with owner-approved, encrypted escrow and
access audit for the backup key, never a plaintext key in GitHub/compose/logs. DB restore alone
cannot recover signing continuity without the other application keys. Escrow feasibility and
key-rotation/attestation policy require a separate owner security decision.

Replicate encrypted base backups and WAL into an independent object store with independent
credentials and billing/account access, and monitor replication lag. R2 in another region/account
alone does not eliminate an R2 provider outage. Confirm WAL-G store interoperability and recovery
prefixes in drills. Retention and legal deletion policy must match both stores. Keep read-only
standby storage credentials; activate new writer credentials only after fencing.

## Expected service objectives

| Scenario | Proposed RTO | Proposed RPO | Preconditions |
| --- | --- | --- | --- |
| One host/region lost | 15–30 minutes | <=2 minutes | Standby WAL lag <=120s, positive fence, keys/store/DNS available, practiced operator |
| Primary object store lost | 30–60 minutes | <=5 minutes | Independent mirror lag <=300s and keys available |
| Shared Cloud/KMS outage | Unbounded | Last recoverable WAL | No promotion without verifiable fence/key availability |
| Logical corruption | Dataset-dependent, hours possible | Chosen PITR boundary | Warm standby may replay corruption; historical backup/PITR needed |

These are targets, not current guarantees. Archive-only replication cannot offer zero RPO.
Measure p95/p99 replication and replay lag and full recovery including reconciliation/TLS/DNS in
quarterly drills; advertise targets only after successful drills under realistic seven-year data
size. Loss of already-submitted chain/webhook effects may extend reconciliation beyond the RTO.

## Incremental monthly cost estimate

Planning assumptions, not provider quotations; USD, excluding the current primary:

| Item | Assumption | Incremental monthly cost |
| --- | --- | --- |
| Warm CVM | 2–4 vCPU, 8–16 GiB RAM, independent placement | $100–250 |
| Standby disk | 500 GiB, $0.10–0.20/GiB-month | $50–100 |
| Independent encrypted backup mirror | 1 TiB, $0.015–0.025/GiB-month | $15–26 |
| WAL storage/transfer/requests | 5–20 GiB/day; $0–0.09/GiB transfer | $5–60 |
| Witness/external monitoring/DNS | Small independent service | $10–40 |
| Total at this initial size | Before labor, tax, growth and optional proxy/recovery KMS | **$180–476/month** |

Data grows for seven years: project actual DB/index/WAL growth, compression, backup multiplicity
and mirror retention before approval. Disk cost scales linearly; this initial sizing is not a
seven-year fixed budget. Provisioning quotes, placement guarantee, fencing capability, recovery
key policy and a staffed incident rota are owner decision gates. No HA implementation belongs in
this PR.
