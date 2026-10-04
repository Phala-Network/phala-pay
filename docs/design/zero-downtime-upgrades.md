# Zero-downtime planned upgrades

Status: Proposed

## Decision and scope

Recommend **A: temporary blue-green instances under one app ID**, with archive-fed PostgreSQL
standby recovery and a cooperative, verified handoff. Keep one CVM in steady state; create the
second only for an upgrade and delete the old one afterwards. No permanent standby, HA service,
automatic failover, or implementation is authorized by this record.

Today [compose.yaml](../../deploy/compose.yaml) puts PostgreSQL 18, dstack-ingress, smokescreen,
workers, heartbeat and backups in one CVM. An attested compose upgrade restarts it. Staging
[run 37215125136][run] spans 9:10:30–9:13:13 AM PDT on 2026-10-04
(16:10:30–16:13:13 UTC), **163 seconds**, approximately 2 min 40 s. These deployment timestamps
are the reported restart envelope, not a continuous request-level outage measurement.
The separate maintenance-503/SDK-tolerance PR handles that envelope gracefully; it cannot remove it.

The target is continuous reachable ingress/safe reads and **5–15 seconds of write admission
pause**, with zero lost committed writes. This is not literal uninterrupted write execution:
requests need bounded buffering or the SDK retry contract during handoff. Advertise zero downtime
only after drills prove successful client requests across that pause. Stock deployment cannot do
this today, and slow external calls or platform routing can exceed the target.

## Platform findings (primary sources checked 2026-10-04)

| Question | Evidence and conclusion |
|---|---|
| Can one app run old and new compose simultaneously? | **Yes.** Cloud's [replication guide][replicas] explicitly describes multiple live compose hashes. [instances add][add] accepts `--app-id` and `--compose-file`; [deploy/phala](../../deploy/phala) pins CLI 1.1.22. Its [published package][cli-package], `dist/index.js` instances/add handler, POSTs `docker_compose_file` to `/apps/{app_id}/instances`. A new UUID/instance ID has independent disk and billing. |
| Does replication deploy the new release? | `cvms replicate` copies the selected source compose and encrypted env; it does not copy volumes or replace compose. Use `instances add` with the reviewed new artifact, or replicate then upgrade **only the new UUID** while passive. Never upgrade by ambiguous app ID. |
| Does the gateway route to several instances? | In [dstack v0.5.9 gateway source][gateway], `select_top_n_hosts` resolves an instance ID directly or an app ID to candidate instances. [TLS passthrough][passthrough] races TCP connects and takes the first success. This is connection selection, not HTTP readiness or primary election. |
| What does the custom domain do today? | [Ingress 2.6 tlsalpn.sh][tlsalpn] pins `_dstack-app-address.DOMAIN` to `<instance_id>:443`: only that instance receives traffic. Its [README][ingress] explains that `dns-01` can instead bind `<app_id>:443` and use multiple instances. Multiple TXT records are not weighted routing: the gateway reads the first record. |
| How fast is a switch? | No documented atomic switch API or latency SLA was found. TXT/CNAME changes depend on recursive DNS caches; existing TCP connections stay on their selected instance. [Gateway defaults][gateway-config] include 5 s DNS/connect timeouts and a 30 s candidate cache; these are not a cutover guarantee. Ingress adds a 30 s initial DNS settle wait. Cloud's [domain guide][domain] warns of 2–5 min DNS propagation. |
| What needs Cloud confirmation? | Cloud's [scaling guide][scaling] says it provides no load balancing, whereas the pinned gateway/ingress source supports app-level selection. Confirm the deployed gateway version, app routing and port-policy behavior on the actual nodes; source capability is not a hosted-service promise. |

Routing must never send payment traffic to a passive standby just because its TCP listener is healthy.

### Key continuity and attestation

[KMS GetAppKey][kms] authorizes the attested boot before deriving the application secp256k1 key
from the KMS root and `app_id`; [guest GetKey][getkey] derives from that application key and path.
Neither compose hash nor instance ID enters **GetKey** derivation. Therefore two authorized
instances on the **same app and KMS root**, with unchanged paths/algorithm, derive identical:

| Use | Existing path in [signer.rs](../../crates/core/src/signer.rs) |
|---|---|
| DB owner/application credentials | `db/owner/v1`, `db/app/v1` |
| Quote/deposit-address client-secret authentication | `client-secret/v1` |
| Per-account/mode/version webhook signing | `settlement/{account}/{live\|test}/v{n}` |
| WAL-G base-backup/WAL encryption | `backup/v1` |

A compose upgrade changes measurements and **authorization**, not these secrets. Cloud KMS must
authorize both revisions; Onchain KMS also requires allowed compose hashes/devices ([replicas][replicas]).
Changing app identity, KMS root, derivation version or the legacy derivation API can change keys.
The disk-encryption key is different: KMS includes **instance ID**, so copying an encrypted CVM
disk is not the recovery mechanism. Restore through WAL-G instead.

Verify both full app-compose artifacts/quotes, TCB, key public identities, old client secrets and
actual backup decryption before draining. Compare secret-derived challenge proofs inside the TEEs;
never export/log raw keys. Keep both approved compose hashes in merchant verification policy
during overlap, then retire the old revision. Key continuity does not make new code trusted.

## Option A: preparation and routing

Preparation remains outside the write pause, while the old release serves normally:

1. Serialize the upgrade; record old/new UUIDs, instance IDs, app/KMS identity, release digests,
   PostgreSQL system identifier, timeline and operation ID. Pre-authorize the new measurements.
2. Boot the new release with an **attested passive startup gate**. Reuse [WAL-G tooling and
   restore code](../../deploy/RESTORE.md#bootstrap-from-backup) to fetch a pinned latest base backup
   and replay contiguous WAL. Add `standby.signal`, not today's `recovery.signal`: current restore
   promotes at archive exhaustion and its health check requires recovery to have finished.
3. Add standby-specific readiness: `pg_is_in_recovery()`, system/timeline match, replay LSN and
   archive lag. Missing WAL retries without promotion; storage/decryption errors fail closed.
   Use read-only object-store credentials. No standby DB migrations, heartbeat, API writes, workers,
   RPC submissions, webhook delivery, backup uploads or retention deletion on the standby.
4. Prewarm keys, images, read-only API, TLS and bounded RPC validation. Retain compatible
   PostgreSQL major version/extensions; major-version upgrades need a separate migration design.
   Wait for replay lag <=1 s after a forced WAL switch; the current `archive_timeout=30` alone
   cannot support a seconds-scale handoff. Restore/catch-up duration depends on data size/load.

**Routing is a prerequisite, not a final DNS edit.** Keep the instance-bound custom domain,
prepare valid certs on both instances, and use a temporary relay on the old CVM to bridge DNS
convergence. Establish the relay protocol before using it for upgrades:

- Move certificate preparation to delegated `dns-01`, with a narrowly scoped challenge-zone
  credential and owner-approved CAA policy. Prepare the new certificate/evidence without changing
  live DNS or opening its payment listener. Stock ingress starts a placeholder listener early;
  a measured readiness gate is required; tls-alpn-01 issuing after cutover misses the target.
- Keep TXT/CNAME pointing at old throughout preparation. After promotion/relay activation,
  update them to new instance/gateway; cached old routes remain served by the relay. App-ID routing
  is an optional Cloud-confirmed alternative, not primary election; passive must refuse TCP on 443.
- During handoff, a separate ingress coordinator keeps sessions and buffers bounded mutations;
  route safe reads to the new read-only API once it reaches F. After promotion relay all requests
  to new through an attested, instance-pinned encrypted channel. Preserve origin, signatures and
  idempotency keys; authenticate peer app, approved compose, instance and operation ID.
  Keep direct application ports private; any handoff listener needs explicit compose-policy review.
- The relay executes no local mutations or business side effects. Existing sessions and stale
  DNS routes therefore reach the new writer. Only open new public ingress after readiness;
  stop old ingress after connection drain and measured DNS/gateway convergence. Regenerate each
  instance's certificate evidence; verification must bind both TLS termination and the new backend.

These proposed ingress changes add no third CVM or permanent proxy. Owner approval is needed
for the DNS credential/relay trust boundary. Prefer a proven Cloud drain-aware switch if available.
Without a relay and preissued certificate, today's TXT/tls-alpn-01 switch can take tens of seconds
to minutes, potentially 2–5 min; that fallback does **not** satisfy this record.
Bootstrap needs one graceful restart; later upgrades must not change old compose for the relay.

### Cooperative fencing and switchover

Both releases must already implement the protocol. A database advisory lock on two restored
databases, DNS changes, a successful stop request, or a fixed sleep is not proof of fencing.
No independent witness/HA fencing service is required for this **planned, cooperative** handoff.
If old-instance cooperation or evidence is lost, abort; do not convert this into incident failover.

1. **Drain:** old sets a durable `draining` gate before acknowledging it. Gate every API/admin
   mutation and side-effecting GET, stop dequeuing all workers, and finish bounded in-flight DB
   transactions/RPC submissions/webhook sends. Continue safe reads; queue/retry mutations with
   deadlines. Stop heartbeat, migrations/manual writer commands and backup/retention scheduling.
2. **Prove business quiescence:** persist all completed/uncertain external effects and their
   transaction nonce/hash or webhook event/delivery ID. Require zero active writer transactions,
   zero worker tasks and zero outstanding external requests. A timed-out ambiguous request is
   not a successful drain: abort or resolve it before handoff; do not wait for chain finality.
3. **Seal the old writer:** latch a persistent `fenced` gate that survives CVM/container restart;
   startup cannot automatically resume workers or writes. Terminate the business/heartbeat
   processes and disable their restart paths; block every business RPC/webhook egress path
   (RPC can bypass smokescreen). Leave only ingress relay/control/archive access. Check process
   exit, egress enforcement and absence of DB writer sessions, not just application counters.
4. **Ship the cutoff:** with only the bounded handoff controller connected, checkpoint, record
   final committed/flush LSN **F** and force `pg_switch_wal()`. Wait for verified archive success
   for every segment through F; record segment names, system ID and timeline. Stop old PostgreSQL
   and verify stopped state. Shutdown may add housekeeping WAL; no business commit may follow F.
   Old PostgreSQL and all writers remain restart-inhibited. Archive/store failure prevents promotion.
5. **Acknowledge the fence:** return authenticated evidence bound to operation ID, old instance,
   F and timeline, durable gate, process/DB stop and egress state. The controller independently
   checks that evidence; a lost ACK blocks promotion. This is cooperative proof from trusted,
   attested code, not protection from a malicious old image or host-control-plane outage.
6. **Promote:** new verifies the fence, continuous replay through F and matching lineage; require
   `pg_last_wal_replay_lsn() >= F`. Explicitly promote once, verify out of recovery/new timeline,
   activate presealed writer archive credentials and its durable writer gate. Enable API and then
   workers/heartbeat exactly once. A paused worker must not infer ownership from DB health alone.
7. **Move traffic:** release queued mutations, activate old relay, open new ingress, then update DNS;
   verify attestation, origin, writes and worker progress. Old stays fenced until deleted. Take a
   current-timeline base backup immediately using [walg-timeline-backup](../../deploy/scripts/walg-timeline-backup),
   confirm new WAL archiving/backup health, then delete old through Cloud and verify billing ends.

Prepare `archive_mode=on` on standby (inactive until promotion), not today's restore-check `off`;
prepare sealed credentials before drain and gate their use; no env-update/reboot at promotion.
Only new may archive/delete. Retain lineage; see [PostgreSQL 18 standby semantics][postgres].

### Availability budget and temporary cost

| Critical-path step | Staging target |
|---|---|
| Drain, persist outcomes and fence | 2–5 s |
| Final WAL upload and replay | 1–4 s |
| Promotion, API activation and relay release | 2–6 s |
| Total write-admission pause | **5–15 s**, proposed p95 <=10 s, p99 <=15 s |

Measure from first blocked mutation to first successful new-writer mutation, including reconnects,
retries and startup gates. Preparation, certificates, migration and base backup must not be in
this budget. Abort before fencing if drain cannot fit; after fencing preserve safety even if the
pause exceeds 15 s. Reads needing unavailable primary state may also pause; prove the actual
read contract rather than treating an old local snapshot as current.

[Cloud billing][scaling] charges the extra instance at its full rate; stopped disks still cost,
deletion releases them. Cost is compute rate × overlap hours + disk + restore transfer/requests.
Planning assumptions, **not a provider quote**:
1–2 h overlap at $0.14–0.35/h, 500 GiB disk at $0.10–0.20/GiB-month (730 h), and 20–50 GiB
restore transfer at $0–0.09/GiB give **$0.21–5.48 per upgrade**, before requests, tax and labor.
Budget $6/run at this size; four upgrades/month are about $0.84–21.92 plus those extras.
Same-region free transfer approaches the lower end. Larger retained datasets, WAL backlog and
minimum billing increments change this; obtain the workspace's actual rate and billing granularity.
Cap preparation at 2 h and clean up a failed passive instance; never delete the authoritative writer.

## Option B: simpler platform updates

Cloud's [update guide][update] offers compose/code update, not a documented no-reboot attested
rolling update. The [dstack guest runner][runner] runs Compose from app-compose at boot; this
deployment pins image digests and verifies RTMR3/compose evidence. SSH-running `docker compose up`
with different code or pulling a mutable tag bypasses that reviewed artifact: the boot quote
does not prove the replacement code. Reject non-measured hot updates. Predeclaring both releases
in one compose still needs an attested upgrade to add the next release and duplicates resources.
Accept a simpler in-place option only if Phala proves new-code measurement/key authorization,
continuous PostgreSQL/ingress and merchant verification; no such contract was found.

## Migrations, rollback and failure handling

[#324][compat-pr] merged on 2026-10-04 at 10:34 AM PDT. Its [migration protocol][migrations]
uses owner-written compatibility floors and exact applied checksums; the real N-1 release image
must pass startup against N's schema. Published legacy images cannot learn this protocol.

- Apply **expand-only** N migrations to the old primary before cloning/catch-up, with N-1 still
  serving, using an attested N migrator via the approved encrypted control tunnel to old DB;
  do not hot-load N code into old CVM. Bound lock acquisition/backfills. WAL carries schema changes
  to standby; never run migrations there. A blocking migration is outside the zero-downtime class.
  Both releases must understand data/event semantics, configuration and the handoff protocol.
- Before promotion, abort by keeping new passive; resume old only after verifying new was never
  promoted and explicitly clearing its fence. Lost promotion ACK is ambiguous: inspect new state,
  never resume old on timeout. After promotion, the new DB is authoritative even before routing.
- Roll back **N to N-1 only**, keeping upgraded schema and latest committed data; no down migrations.
  Run N-1 against the authoritative new DB under the same gates, or rebuild a temporary N-1
  instance from the new timeline and reverse the full handoff. Never switch to old stale pgdata.
  Bootstrap releases without protocol-aware N-1 or `no rollback; restore required` releases
  require the documented restore exception and cannot promise this upgrade/rollback service level.

## PR-sized implementation and acceptance gates

1. **Platform proof/drill harness:** capture actual Cloud/CLI/gateway contracts and prices; prove
   different compose hashes coexist, key continuity, cross-instance discovery and TLS preparation.
2. **Standby restore variant:** add explicit non-promoting recovery, lineage/LSN readiness and
   promotion/archiving activation gates; reuse bounded WAL-G operations and fail-closed restore.
3. **Drain/fence protocol:** cover API, all workers, heartbeat/manual writers, restart inhibition,
   egress and authenticated cutoff evidence; inject lost ACK, crash and ambiguous external calls.
4. **Ingress preparation/relay:** reviewed measured changes, delegated DNS policy, ready-only TCP
   listener, peer attestation and bounded buffering; prove old connections reach only new writer.
5. **Deploy orchestration/runbook:** UUID-scoped state machine, migration/N-1 release gates,
   deadlines, cleanup/billing verification and staging acceptance before owner-enabled production.

Staging must exercise realistic DB/WAL load and long-lived SDK connections; record p95/p99 pause,
request failures, replay/commit LSNs, worker ownership and accepted transaction/webhook outcomes.
Prove no lost/duplicated financial mutation, signing/client-secret continuity, no second submitter
or dispatcher (webhook transport remains at-least-once), and successful N-1 rollback after N writes.
Inject archive gaps/storage errors, bad keys, migration lock contention, stale DNS/gateway state,
new-instance failure, old restart and lost fence/promotion ACKs. Safety failures must block promotion.

**Owner/Phala gates:** approve the 15 s write-pause contract, overlap budget, one-time bootstrap
restart, delegated DNS/CAA/relay trust policy and merchant measurement overlap. Phala must confirm
same-app multi-revision authorization/root stability, actual gateway routing/cache convergence,
cross-node reachability, TCP drain semantics, any supported atomic switch, capacity and billing.
Until these gates and the drill pass, keep the graceful restart path; do not claim zero downtime.

[run]: https://github.com/Phala-Network/phala-pay/actions/runs/37215125136
[replicas]: https://docs.phala.com/phala-cloud/cvm/replicating-cvms
[add]: https://docs.phala.com/phala-cloud/phala-cloud-cli/instances/add
[cli-package]: https://registry.npmjs.org/phala/-/phala-1.1.22.tgz
[gateway]: https://github.com/Dstack-TEE/dstack/blob/v0.5.9/gateway/src/main_service.rs
[passthrough]: https://github.com/Dstack-TEE/dstack/blob/v0.5.9/gateway/src/proxy/tls_passthough.rs
[gateway-config]: https://github.com/Dstack-TEE/dstack/blob/v0.5.9/gateway/gateway.toml
[tlsalpn]: https://github.com/Dstack-TEE/dstack-examples/blob/dstack-ingress-v2.6/custom-domain/dstack-ingress/scripts/tlsalpn.sh
[ingress]: https://github.com/Dstack-TEE/dstack-examples/blob/dstack-ingress-v2.6/custom-domain/dstack-ingress/README.md
[domain]: https://docs.phala.com/phala-cloud/networking/setup-custom-domain
[scaling]: https://docs.phala.com/phala-cloud/cvm/multi-replica-scaling
[kms]: https://github.com/Dstack-TEE/dstack/blob/v0.5.9/kms/src/main_service.rs
[getkey]: https://github.com/Dstack-TEE/dstack/blob/v0.5.9/guest-agent/src/rpc_service.rs
[postgres]: https://www.postgresql.org/docs/18/warm-standby.html
[update]: https://docs.phala.com/phala-cloud/update/upgrade-application
[runner]: https://github.com/Dstack-TEE/dstack/blob/v0.5.9/basefiles/app-compose.sh
[compat-pr]: https://github.com/Phala-Network/phala-pay/pull/324
[migrations]: https://github.com/Phala-Network/phala-pay/blob/60350e4f97cec46e06546ef3dda2dc95b3667042/crates/topup/src/db/migrations.rs
