# Backup and restore

Trigger: PostgreSQL loss or corruption, a failed database volume, or a restore drill. Targets: RPO
at most one minute, RTO at most one hour (architecture §14). While the database is unavailable
the API and processing stop for every route.

Restoring the database does not restore the business: the changes after the restore point are
lost, among them key revocations, treasury cancellations, endpoint deletions, deposit addresses
and quotes given to customers, and events merchants received. So a restored service starts
**frozen**: the admin API and `/healthz` work, every merchant request with an API key answers
`503 service_restoring` with `Retry-After`, reads included (a key revoked after the restore point
is valid in the restored database), and nothing credits, settles, or delivers an event until the
operator has sent every merchant's recorded contact the restore point, re-applied what each
reports (the key revocations first: the unfreeze is when keys authenticate again), and unfrozen it
([Reconciliation after a restore](runbooks/restore.md)).

A restore is a bootstrap from backup (the pattern of CloudNativePG's `bootstrap.recovery`): a new
instance of the same dstack app boots with an empty volume, and PostgreSQL itself fetches the
newest base backup, replays the archived WAL, and promotes. The instance boots the
[restore-check variant](#the-restore-check-variant), which verifies the result read-only; a real
restore then [resumes](#resume) by upgrading it to the service compose. No step runs inside a CVM.
The same bootstrap is why an upgrade must keep the volumes: a service that booted on an empty
`pgdata` would restore from backup and look healthy while losing the unarchived tail;
[local/cvm-rehearsal.sh](local/cvm-rehearsal.sh) checks that a configuration upgrade recreates
topup only and keeps PostgreSQL and its volume.

## Backup key

WAL-G encrypts every base backup and WAL segment with one libsodium key per prefix. `keys`
derives it from the dstack path `backup/v1` and writes it only to the `walg_key` tmpfs
([README](README.md#database-credentials)); no key appears in an image, env, command line, object
metadata, or log. A replacement instance of the same app id derives the same key and database
passwords, so it needs no secret; a failed fetch or login means the app identity is wrong.

A prefix never changes its key. Rotation is a new prefix: one upgrade changes `BACKUP_KEY_DOMAIN`
(`crates/core/src/signer.rs`, for example to `backup/v2`) and `WALG_S3_PREFIX`; `backup` finds no
base backup of the running timeline in the new prefix and takes one at once. Keep the old prefix
until the new one holds `WALG_RETENTION_FULL` (7) base backups; until then it restores with the
restore-check variant rendered with the kit and images of the last release before the change.

`wal-g backup-list` does not decrypt anything and is never a key test; only fetching a base backup
and reading its `PG_VERSION` is.

## Bootstrap from backup

On every boot of every instance, the PostgreSQL entrypoint
([postgres-walg-entrypoint.sh](scripts/postgres-walg-entrypoint.sh)) handles an empty data
directory from `WALG_S3_PREFIX`:

- If the prefix lists base backups, it fetches the newest beside the data directory, moves it into
  place only when complete, and PostgreSQL replays every archived segment and promotes.
- Only a successful, empty listing initializes a new cluster.
- Any listing error (storage unreachable, credentials not sealed yet or wrong, missing bucket)
  stops the container, which Docker restarts. It never initializes then, because a new cluster
  archiving into a prefix that holds a timeline would fork it. So a new CVM waits for its
  [sealed credentials](README.md#sealing-the-secrets).

A non-empty data directory is started as is; an interrupted fetch starts over and an interrupted
recovery resumes. A new app needs a prefix of its own: its key cannot decrypt another app's
backups. [tests/walg-archive-switch.sh](tests/walg-archive-switch.sh) covers these paths in CI.

`archive_timeout=30` bounds segment switching when WAL is written. The heartbeat must commit
every 15 seconds or faster to leave room for upload latency within the one-minute RPO. After
a successful upload, `walg-cron` records the segment data age; separate base-backup and WAL
progress evidence gates the `topup-backup` monitor ([README](README.md#sentry)).

## The restore-check variant

`deploy/render.sh --restore-check` renders the Environment's directory with
[compose.restore-check.yaml](compose.restore-check.yaml) instead of the service overlay. That
gives the verification instance its own compose hash. [compose-policy.jq](compose-policy.jq)
requires each of these differences of the merged artifact, wherever it is checked (`render.sh`,
`validate-compose.sh`, preflight, `verify-attestation.sh`):

- **Only `keys`, `postgres`, `migrate`, `topup`, and `restore-check` run.** There is no `backup`,
  `heartbeat`, `smokescreen`, or `dstack-ingress`, so no base backup is pushed or deleted and no
  webhook leaves.
- **PostgreSQL restores and never archives.** Its `TOPUP_RESTORE_FROM_BACKUP=on` requires a base
  backup (an empty prefix fails) and makes the entrypoint set `archive_mode=off` after any other
  argument. The policy forbids overriding the image's entrypoint or command.
- **Its storage credentials are its own:** `RESTORE_AWS_ACCESS_KEY_ID` and
  `RESTORE_AWS_SECRET_ACCESS_KEY`, a read-only token. The variant reads no `AWS_*` name, so an
  instance that inherits the live read-write env (created without `--env-file`) cannot list the
  prefix and never starts.
- **`topup` runs `--read-only`.** It answers only `GET`, `HEAD`, and the operator's restore
  reconciliation under `/v1/admin/restore/`; anything else answers `503 service_restoring`, and so
  does every request with a merchant API key (`restore-check` records the freeze in parallel, and
  may fail before it does). It runs no loop, takes no lease-owner lock, and reports to Sentry as
  `<environment>-restore`.
- **`topup` is published on 8081 and nothing else** ([why](#addressing-the-restore-check-instance)).
  Its origin is that gateway URL, the render's `--origin`.

After PostgreSQL promotes (its health check passes only out of recovery; the start period is the
one-hour RTO) and `migrate` confirms the schema, `restore-check` runs once: it records the restore,
which freezes the service (the report's `restore_id`; the freeze is a row of the database, so it
holds after the upgrade to the service compose), then checks migration checksums, WAL state, row
counts, and runs a full reconciliation round on the restored ledger alone: the
service's record is authoritative for its credits, so nothing asks a merchant anything and the
check does not depend on any merchant being reachable (what merchants did after the restore point
is re-applied later, in the [reconciliation](runbooks/restore.md)). The read-only `topup` serves the report
on `/healthz`:

```json
{"mode":"read-only","restore_check":{"status":"ok","failures":[],"rpo_basis":"unanchored","restored_heartbeat_at":"…","latest_migration":…,"row_counts":{…},"post_restore_reconciliation":{"status":"complete","failed_checks":[],"findings":[…]},…}}
```

`restore_check` is `null` until the check finishes. A finding left unverified makes the status
`incomplete` and is listed in `failures`; a check that could not run reports `failed`. Other
reconciliation findings are reported; every failed critical check makes the report `incomplete`
and blocks acceptance and unfreeze. See the explicit, audited administrator override in
[the restore runbook](runbooks/restore.md#7-unfreeze).

**Changes inside the RPO window.** A deposit credited in the last minute before the loss is
rebuilt from the chain by the rescan and credited again after the unfreeze, with the same deposit
id and `deposit.credited` event id. Before the unfreeze the operator imports the deliveries of the
events each merchant received after the restore point, and only those the service signed: the
rebuilt deposit then finds its event recorded and nothing is sent again with another body, and it
is valued at the credit the merchant was told, not re-valued, so its refunds and reversal reference
that credit. A deposit whose recorded transfer contradicts its delivered event is held until the
operator discards the delivered credit. A spot deposit whose delivery no merchant produces is
re-valued. A deposit reversed after the restore point because a re-included transaction put
another transfer at its receipt position is rebuilt, reversed, from its signed `deposit.reversed`,
so the rescan records that transfer under its successor's id and link, at the successor's
delivered credit; a reversed deposit that was never valued is imported the same way. The same
reconciliation revokes again the keys, cancels again the treasury changes, pauses or resumes
treasury crediting again, and deletes again the endpoints that the restore brought back; applies
again, while frozen, a treasury change that applied after the restore point, from its signed
`treasury.updated`; and re-issues the deposit addresses and quotes given out after the restore
point, identically, over the treasury in force when each was issued. A re-issued quote's payment
is credited at spot unless a signed delivery carries its credit, a client secret re-issued with one
is accepted only when the service issued it for that id to that account, and a quote no merchant
reports stays lost ([runbook](runbooks/restore.md)).

A service that booted straight from backup into the service compose (an empty volume, so the
PostgreSQL entrypoint restored it, without the restore-check variant) is frozen as well: every
promotion out of archive recovery starts a new PostgreSQL timeline, and `topup run` freezes when
the timeline is newer than the one it acknowledged.

**Sentry during a real restore.** The replacement runs no loop and reports as
`<environment>-restore`, so every Crons monitor of the environment misses its check-ins and the
Uptime monitor fails. Mute the environment's Crons monitors and disable its Uptime monitor from
creation until [Resume](#resume) shows `topup-backup` checking in `ok`. A staging drill needs no
muting: the live instance keeps checking in.

### Render it and its env file

Render with the live release (the `version` Deploy last deployed to the Environment): its
`images.json` and deploy kit, downloaded and verified as in
[Verify a release](../docs/self-hosting.md#verify-a-release), the kit extracted to `kit/`. The
environment directory is the live one, at the commit Deploy rendered: for example
`production/topup` in the operator's environment repository, and for Phala's staging
`deploy/environments/phala-network/staging/topup`; `ENV_DIR` below is that directory. Pass a
provisional origin: the instance's gateway host is known only after creation, and the variant
needs no gateway domain.

```sh
kit/deploy/render.sh --restore-check --images images.json --origin https://pending.invalid \
  "$ENV_DIR" >restore-check.yml
```

The env holds the variant's sealed names, `RESTORE_AWS_ACCESS_KEY_ID` and
`RESTORE_AWS_SECRET_ACCESS_KEY` among them. Those are storage credentials that can only list and
read the prefix: on R2, an API token with **Object Read only** on the backup bucket only. Create it
on the operator's machine, for this restore only, and shred it once the instance exists or the
restore is abandoned:

```sh
umask 077
export RESTORE_ENV_DIR="$(mktemp -d)"
# Exactly the sealed names of restore-check.yml, with a TOPUP_RPC_<ID>_KEY=<the live key> line
# for each keyed provider the environment declares (Phala's staging declares none).
printf '%s\n' 'RESTORE_AWS_ACCESS_KEY_ID=<read-only key id>' \
  'RESTORE_AWS_SECRET_ACCESS_KEY=<read-only secret>' \
  'SENTRY_DSN=<the live DSN, or empty>' >"$RESTORE_ENV_DIR/restore.env"
docker pull <restore-check.yml's phala-pay image>   # --offline checks the configuration in it
kit/deploy/preflight.sh --env "$RESTORE_ENV_DIR/restore.env" --compose restore-check.yml \
  --environment-dir "$ENV_DIR" --restore-check --offline
# after creating the instance:
shred -u "$RESTORE_ENV_DIR/restore.env" && rm -rf "$RESTORE_ENV_DIR"
```

## Addressing the restore-check instance

The dstack gateway routes `https://<app_id>-<port>.<gateway domain>` to any instance of the app
that accepts a connection on that port ([dstack usage](https://github.com/Dstack-TEE/dstack/blob/v0.5.9/docs/usage.md#access-the-app)).
Two instances listening on one port therefore share its traffic: Phala's first staging drill
(2026-09-25), when the service was still published on 8080, saw 8 of 12 live `/healthz` requests
reach its drill instance. The service now publishes only `dstack-ingress`, and the gateway sends
its [custom domain](README.md#custom-domain) to the one instance the domain's TXT record names.
The restore-check variant runs no ingress, so it never obtains a certificate for or answers on the
live domain, and publishes topup on 8081, which the service never does:

- `https://$DOMAIN`, the live [custom domain](README.md#custom-domain), reaches only the live
  instance;
- `https://<app_id>-8081.<gateway domain>` (`RESTORE_URL`) reaches only the restore-check instance.

The live isolation check detects a failure; during a staging drill it is a hard abort:

```sh
# Every one of 20 requests must reach the service: an empty 200, never the read-only JSON or an error.
live_isolated() {
  for _ in $(seq 20); do
    test "$(curl -sS -o live-healthz.body -w '%{http_code}' "$LIVE_URL/healthz")" = 200 &&
      test ! -s live-healthz.body || return 1
  done
}
```

## Restore

**HUMAN-ONLY, owner**, with `PHALA_CLOUD_API_KEY` of the Environment exported.

1. **Create the instance.** It must be a new instance of the original app (same app id and KMS),
   never a new app, created with `restore-check.yml` and `--env-file` (without it Phala Cloud
   copies an existing instance's env, with read-write credentials). `--env-file` encrypts with the
   key of an existing instance record, so in a real restore stop the failed CVM but do not delete
   it before this step. `SOURCE_CVM_ID` is the Environment's `TOPUP_CVM_ID`:

   ```sh
   kit/deploy/phala cvms get "$SOURCE_CVM_ID" --json >source.json
   export APP_ID="$(jq -er '.app_id' source.json)"
   kit/deploy/phala instances add --app-id "$APP_ID" --compose-file restore-check.yml \
     --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh \
     --env-file "$RESTORE_ENV_DIR/restore.env" --name phala-pay-restore --json >instance.json
   export RESTORE_CVM_ID="$(jq -er '.vm_uuid' instance.json)"
   ```

2. **Verify its attestation**, addressing the guest agent by the instance id from the attested
   event log. Fetch the attestation from the Phala Cloud API by `vm_uuid`: CLI 1.1.22's
   `cvms attestation` looks the CVM up and then requests `cvms/app_<app id>/attestation`, which
   the API refuses (`Multiple CVMs match this identifier`) while the app has two instances, as it
   does here and in a drill (seen in the full drill of 2026-09-30):

   ```sh
   curl -fsS -H "X-API-Key: $PHALA_CLOUD_API_KEY" \
     "https://cloud-api.phala.com/api/v1/cvms/$RESTORE_CVM_ID/attestation" >attestation.json
   kit/deploy/phala cvms get "$RESTORE_CVM_ID" --json >restore-cvm.json
   INSTANCE_ID="$(jq -er '[.tcb_info.event_log[] | select(.event == "instance-id")
     | .event_payload | ascii_downcase | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' \
     attestation.json)"
   curl -fsS "https://$INSTANCE_ID-8090.$(jq -er '.gateway.base_domain' restore-cvm.json)/prpc/Info" \
     >info.json
   kit/deploy/verify-attestation.sh attestation.json info.json "$APP_ID" restore-check.yml \
     restore-check
   ```

3. **Wait for the report** (at most the RTO) and require `.restore_check.status == "ok"`,
   `post_restore_reconciliation.status == "complete"`, the expected `latest_migration`, plausible
   `row_counts`, and `restored_heartbeat_at` at most 60 seconds older than the recorded failure instant
   recorded outside the lost database. Sampling and upload latency consume this budget:

   ```sh
   export RESTORE_URL="https://${APP_ID#0x}-8081.$(jq -er '.gateway.base_domain' restore-cvm.json)"
   curl -fsS "$RESTORE_URL/healthz" | tee healthz.json | jq -e '.mode == "read-only" and .restore_check != null'
   ```

4. **Set its own origin**, so admin-signed requests verify (the admin API checks RFC 9421
   signatures against the service's origin) and the reconciliation's steps 2 to 5 can run here.
   Render the variant again with `--origin "$RESTORE_URL"`, and upgrade this instance only (no
   `-e`, so its env stays). It restarts on its non-empty data directory, and `restore-check` runs
   again:

   ```sh
   kit/deploy/render.sh --restore-check --images images.json --origin "$RESTORE_URL" \
     "$ENV_DIR" >restore-check.yml
   kit/deploy/phala deploy --json --cvm-id "$RESTORE_CVM_ID" --compose restore-check.yml \
     --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh --no-public-logs --no-public-sysinfo --wait
   ```

   Wait until `/healthz` is `ok` again. The report keeps its `restore_id` (the restore is
   recorded once); an admin-signed `admin GET /v1/admin/restore` answering `200` at `$RESTORE_URL`
   shows the new origin is in force (it answers `401` under the provisional one).
5. **Verify the application identity** with a nonce-bound quote from `$RESTORE_URL`. Merchant
   keys are refused on this instance, so fetch it with the admin API (the
   [runbook environment](runbooks/README.md#environment) with `BASE_URL=$RESTORE_URL`), for any
   account of the Environment, and verify it exactly as `public-attestation.json` in
   [Attestation, ingress, and egress](README.md#attestation-ingress-and-egress), with the compose
   hash of the `restore-check.yml` rendered in step 4; the verified app id must be the original.
   Otherwise stop:

   ```sh
   export ACCOUNT=acct_…  # any live account of the Environment, from the operator's records
   export NONCE="$(openssl rand -hex 32)"
   admin GET "/v1/admin/attestation?account=$ACCOUNT&livemode=true&nonce=$NONCE" \
     > public-attestation.json
   ```

   An Environment with no live account yet uses a test-mode one with `livemode=false`.

   Then the admin deposit view (`admin GET /v1/admin/deposits/{id}`) must return deposits known
   from before the loss in their recorded state, and `admin GET /v1/admin/restore` must show
   `"frozen": true` with the restore point.

### Resume

Real restore only, after a human review of the report, row counts, and incident markers, and
after steps 1 to 5 of the [reconciliation](runbooks/restore.md) (they run on this instance). Delete
the failed instance, so only one instance holds the keys and archives into the prefix; merchants
were sent the restore point in the reconciliation's step 2
([incident communication](runbooks/incident-communication.md)), and their API requests answer
`503 service_restoring` until the unfreeze. Then render the service variant with the same kit
(`kit/deploy/render.sh --images images.json --gateway-domain <gateway> "$ENV_DIR"`).
Its origin is the one merchants call and admin requests are signed for. Upgrade the instance to it
(`kit/deploy/phala deploy --cvm-id "$RESTORE_CVM_ID" --compose <file> --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh`, no `-e`), and set `TOPUP_CVM_ID` to
`$RESTORE_CVM_ID`. Then seal the service's names with the read-write credentials
(`kit/deploy/phala envs update "$RESTORE_CVM_ID" -e <env file>`, with `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` in place of the `RESTORE_AWS_*` pair). The domain's TXT record still names the failed instance: set
`_dstack-app-address.$DOMAIN` to `$INSTANCE_ID:443` (step 2) so the gateway routes the
domain here and dstack-ingress, whose account and certificate volume is new, can obtain a
certificate; update a CAA record that pins the old ACME account. Require:

- `/healthz` at `https://$DOMAIN` answers `200` with an empty body, its [certificate
  evidence](README.md#custom-domain) verifies for the app id, and 8081 no longer answers;
- a `base_…` backup newer than the switch is listed and new segments of the promoted timeline
  appear under `wal_005/` (`backup` takes that base backup at once; until then the new timeline
  cannot be restored);
- a verified attestation and an `ok` check-in of `topup-backup`. Only then unmute the monitors.

The service comes up frozen: merchant requests with an API key answer `503 service_restoring`,
reads included, and the scanner rescans each chain from its restored cursor while nothing is
credited or delivered. Finish the [reconciliation](runbooks/restore.md) (steps 6 to 8): wait for
the rescan, check the delivered events against it, and unfreeze with the admin API (audited) only
once every key revoked after the restore point is revoked again; merchants' keys work again once it
is lifted. Tell every contact when the
service is back and confirm each merchant's keys, treasuries, endpoints, and deposit addresses are
as it left them.

If the restored state is wrong, keep the instance isolated: return traffic to the prior CVM only
if it is authoritative, otherwise restore again from an older verified backup and repeat the check.

## Staging restore drill

The drill restores an Environment's real backups (normally `staging`'s; an operator without a
`staging` Environment drills `production` the same way) into a throwaway instance of the same app (a
copy app cannot derive the backup key) and never goes past step 5 of [Restore](#restore). The
restore-check variant guarantees it never writes to the prefix (its promoted timeline would divert
a later real restore), never runs `backup` or the full `topup`, and never takes live traffic.

1. Issue a read-only token for the Environment's bucket, build the env file, and render the
   variant with its live images. `render.sh` and preflight apply
   [compose-policy.jq](compose-policy.jq), which requires it to publish only 8081.
2. Require `live_isolated` to pass before the instance exists, with
   `LIVE_URL=https://$DOMAIN` (Phala's staging: `https://pay-api-staging.phala.com`).
3. Record the start time (the RPO anchor), run steps 1-5 of [Restore](#restore), and record the
   report, RPO, and RTO. **Hard abort:** run `live_isolated` right after creation, before each
   step, and at least every five minutes; if it fails once, delete the instance at once and record
   the drill as aborted with the failing responses.
4. Delete the instance by its own `vm_uuid` (never by app id or name, which also match the live
   instance) and revoke the token:

   ```sh
   kit/deploy/phala cvms delete "$RESTORE_CVM_ID" --force
   ```

Never upgrade a drill instance to the service compose or seal read-write credentials into it.

## Local and CI drills

`make restore-drill` ([local/restore-drill.sh](local/restore-drill.sh)) runs two modes against
local object storage: `controlled` forces a WAL switch and requires the last marker and LSN,
then pauses the archiver on the disposable source so its later business changes are
definitely lost; `crash` retains the natural 30-second archive timer, kills PostgreSQL after an
upload and reports the observed loss. Both require that a
wrong key fails the restore command (`126`). Then they boot the whole restore-check variant on an
empty volume, with its read-only `RESTORE_AWS_*` credentials while the live read-write names stay in
its environment. They require:

- PostgreSQL using only the read-only credentials;
- promotion with archiving off;
- no heartbeat, backup, egress, or ingress running;
- an `ok` report with a complete reconciliation;
- `503` on writes;
- RPO ≤ 60 seconds from the recorded failure instant to the newest replayed committed marker;
- RTO ≤ 3600 seconds through service boot, real rescan, unfreeze, and a successful merchant API read;
- an unchanged object listing. `controlled` also runs the business-consistency scenario, with the
reference product ([Staging reference product](phala.md#staging-reference-product)) as the
account's merchant. Before the backup, the account's treasury is in force and a change to a second
treasury, T2, is pending with its time-lock over. After the last archived WAL, and before
PostgreSQL is killed (a clean shutdown would archive them), the source revokes an API key, rotates
a customer's deposit address, records a `deposit.credited`, applies T2, and rotates the address
again over T2; those writes are lost with the source. The merchant is also told of three more
deposits to the rotated address: a rejected one reversed without ever being valued, and a credited
one (D0) reversed by a reorganization and recorded again as D1. Each event's delivery (signed with
the account's webhook key as the service signs it: no service runs on the source, since it needs
a chain) reaches the product's webhook receiver, which verifies it and keeps it in its inbox; the
product's ledger also records both addresses and a quote created after the backup as the service
returns them, with their client secrets. After the restore the operator uses only what the
product's `fetch-restore-records --since` the restore point returns (the same as
`export-restore-records` from its ledger, read-only). The drill requires that the replacement is frozen
(merchant writes and reads `503 service_restoring`, `GET /v1/admin/restore` `frozen`), that
[Restore](#restore) step 5's admin attestation answers where the merchant's does not, with the
account's webhook key, that the lost key is refused like every key while frozen and is revoked
again by prefix, that the lost address is re-issued with the same address, `da_` id, and client
secret, that the merchant's latest treasury objects verify as T2 `application_lost` and the
treasury it replaced `replacement_lost`, and the address over T2 is refused until
`treasuries/apply` restores T2 from its signed `treasury.updated` (a body changed after signing,
or the replaced treasury's delivery, is refused; a repeat applies nothing), at the event's time,
after which both verify as `matches` and the address is re-issued, that the
quote is re-issued at its own address (over the treasury in force at its `created`) with its
client secret (the payer's read works again) while a secret of another quote, or of this quote
issued to another account, is refused, that the deliveries the product kept are exported byte
for byte and imported exactly as delivered with no delivery and the credit kept for the deposit,
the unvalued reversal and D0 restored `reversed` and D0's successor recorded as D1, while a body
changed after signing is refused and changes nothing, and that the unfreeze is refused while no
chain is rescanned. The [Restore drill](../.github/workflows/restore-drill.yml) workflow runs
it every Monday at 03:17 UTC and on demand; the CI `deployment` job runs the bounded WAL-G and
bootstrap tests on pull requests and pushes to `main`.

## Failure handling

- **App id, KMS, compose, or attestation mismatch:** stop and create the instance under the
  original app id again; never copy key files between CVMs.
- **`/healthz` never answers, or `restore_check` stays `null` past the RTO:** the backup could not
  be listed, fetched, or decrypted, or recovery failed (`walg-restore-command` returns `126` on
  decryption and storage errors, so PostgreSQL aborts instead of promoting). Delete the instance,
  check storage access and integrity with the owner's credentials, and retry with a new instance.
- **Backup list empty, stale, or unverifiable:** do not resume; escalate the data-loss risk.
- **`restore_check.status` is `failed` or `incomplete`:** do not resume. An unverified finding
  needs an incident repair and another complete check.
- **RTO over 3600 seconds:** escalate even if the restore then passes.
- **`live_isolated` fails during a drill:** delete the drill instance by its `vm_uuid`, confirm
  `live_isolated` passes again, and do not rerun until the rendered compose is confirmed to publish
  only 8081 and the gateway's routing is understood.

## Bounded backup and recovery operations

All production WAL-G uploads, listings, retention and fetches run through `walg-cron run`.
Each attempt has a hard deadline, followed by at most five seconds to kill a stuck process group.
Retries wait one second. Configure the PostgreSQL and backup container environments together:

| Operation | Deadline per attempt | Total attempts |
| --- | --- | --- |
| WAL push | `WALG_WAL_TIMEOUT_SECONDS=10` | `WALG_WAL_ATTEMPTS=1` (PostgreSQL retries the segment) |
| Base backup/list/retention | `WALG_BASE_TIMEOUT_SECONDS=1800` | `WALG_BASE_ATTEMPTS=2` |
| Restore list/base-fetch/WAL-fetch | `WALG_RESTORE_TIMEOUT_SECONDS=120` | `WALG_RESTORE_ATTEMPTS=3` |

Values must be decimal positive integers without leading zeros, deadlines ≤86400 seconds and
attempts ≤10. A deadline returns `124` (forced kill may return `137`);
`restore_command` maps both, and every storage/decryption/configuration failure, to `126` to abort
recovery. Only WAL-G's genuine archive-not-found `74` maps to `1`. Never treat timeout as an
absent segment. Bootstrap refuses to initialize after a listing timeout.

The first base backup on the current timeline retries indefinitely with backoff from five seconds
to five minutes. A daily backup failure preserves its old age and does not terminate the scheduler.
Backup health requires an existing current-timeline base backup no older than 48 hours, segment
data age and oldest pending archive age at most 60 seconds, a WAL LSN gap at most 16 MiB, and a
progress observation no older than 45 seconds. Successful upload of old WAL keeps the old data age.
The base timestamp is cleared on scheduler startup until a current-timeline backup is verified;
existence is revalidated every five minutes, and a failed revalidation clears the timestamp.

To guarantee the one-minute target on an otherwise idle database, run the heartbeat with
`topup heartbeat --interval-s 15` or faster: 15 seconds sampling + 30 seconds segment switching +
10 seconds upload leaves five seconds of margin. The current compose heartbeat defaults to 60
seconds and requires the batch 2 owner to set this argument. The monitor will correctly report
RPO degradation until that cadence is corrected. Overrides that increase upload retry budgets
must also budget their latency. A boot-time unanchored report does not prove RPO; compare its
newest restored commit with the externally recorded failure instant, never merely with the last
source heartbeat. `--failure-at` takes that failure instant; the report records it as `failure_at`.

The local drill starts disposable Anvil chains for every configured route, with Sepolia and Base
Sepolia chain IDs, canonical factories and Multicall3, and local token/oracle fixtures. A/B groups
use distinct domains and ports over each chain's shared state; all staging RPC members are replaced.
Its own invocation sets a 15-second heartbeat cadence. On Linux, missing Foundry tools are
extracted from the pinned Anvil image into the drill's temporary directory; other platforms
require Foundry installed locally. Cleanup removes the chains and their
project volumes together with the rest of the drill.
The local drill may switch its isolated replacement to service mode; the staging isolation drill
above remains read-only and cannot attest full merchant-service RTO.
