#!/usr/bin/env bash
# Run the actual rendered migration variants, including one-shot container boundaries and reboot.
# CI calls this with the images already published by the existing CVM rehearsal.
set -euo pipefail
root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
: "${TOPUP_MIGRATION_TOPUP_IMAGE:?set a pinned image containing this branch}"
: "${TOPUP_MIGRATION_POSTGRES_IMAGE:?set the pinned postgres-walg image}"
: "${TOPUP_MIGRATION_SIMULATOR_IMAGE:?set the existing pinned dstack simulator image}"
project="topup-migration-compose-$$"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/$project.XXXXXX")
compose=$("$root/deploy/pinned-compose.sh")
unset SENTRY_DSN
destination_start=""
support() { "$compose" -p "$project-support" -f "$tmp/support.json" "$@"; }
source_stack() { "$compose" -p "$project-source" -f "$tmp/source.yaml" -f "$tmp/source-tls.json" -f "$tmp/source-sockets.yaml" "$@"; }
destination_stack() { "$compose" -p "$project-destination" -f "$tmp/destination.yaml" -f "$tmp/destination-tls.json" -f "$tmp/destination-sockets.yaml" "$@"; }
cleanup() {
    if [[ -n "$destination_start" ]] && jobs -pr | rg -qx "$destination_start"; then
        kill -TERM "$destination_start" 2>/dev/null || true
        wait "$destination_start" 2>/dev/null || true
    fi
    if [[ -f "$tmp/source.yaml" ]]; then source_stack down -v --remove-orphans >/dev/null 2>&1 || true; fi
    if [[ -f "$tmp/destination.yaml" ]]; then destination_stack down -v --remove-orphans >/dev/null 2>&1 || true; fi
    if [[ -f "$tmp/support.json" ]]; then support down -v --remove-orphans >/dev/null 2>&1 || true; fi
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=migration-test-ca \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
    -keyout "$tmp/ca-key.pem" -out "$tmp/ca.pem" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj /CN=s3-tls \
    -keyout "$tmp/key.pem" -out "$tmp/cert.csr" >/dev/null 2>&1
printf '%s\n' 'basicConstraints=critical,CA:FALSE' 'keyUsage=critical,digitalSignature,keyEncipherment' \
    'extendedKeyUsage=serverAuth' 'subjectAltName=DNS:s3-tls' >"$tmp/leaf.ext"
openssl x509 -req -in "$tmp/cert.csr" -CA "$tmp/ca.pem" -CAkey "$tmp/ca-key.pem" \
    -CAcreateserial -days 1 -extfile "$tmp/leaf.ext" -out "$tmp/cert.pem" >/dev/null 2>&1
"$compose" -p "$project-support" -f "$root/deploy/local/s3.compose.yml" config --format json >"$tmp/s3-base.json"
# Read only this local fixture; ignore any ambient cloud credentials.
export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
AWS_ACCESS_KEY_ID=$(jq -r '.services.s3.environment.GARAGE_DEFAULT_ACCESS_KEY' "$tmp/s3-base.json")
AWS_SECRET_ACCESS_KEY=$(jq -r '.services.s3.environment.GARAGE_DEFAULT_SECRET_KEY' "$tmp/s3-base.json")
python3 "$root/deploy/tests/migration-compose.py" "$root" "$tmp" \
    "$TOPUP_MIGRATION_TOPUP_IMAGE" "$TOPUP_MIGRATION_POSTGRES_IMAGE" "$TOPUP_MIGRATION_SIMULATOR_IMAGE" "$project"
jq --arg object "s3://topup-backups/$project" \
    '{migration_export:{recipient,object:$object}}' "$root/deploy/tests/migration-pins.json" >"$tmp/export.json"
jq --arg object "s3://topup-backups/$project" \
    '{migration_import:{source_app_id,export_public_key,object:$object}}' "$root/deploy/tests/migration-pins.json" >"$tmp/import.json"
"$root/deploy/render.sh" --export --migration-export "$tmp/export.json" --images "$tmp/images.json" \
    --project-name "$project-source" "$tmp/source-env" >"$tmp/source.yaml"
"$root/deploy/render.sh" --migration-import "$tmp/import.json" --images "$tmp/images.json" \
    --gateway-domain gateway.localhost --project-name "$project-destination" "$tmp/destination-env" >"$tmp/destination.yaml"
support up -d --wait s3 dstack-simulator s3-tls
source_stack up -d --wait keys postgres migration-holder
# Initialize only the schema, with the same image, owner credentials and migrator as deployment.
source_stack run --rm --no-deps --entrypoint topup export migrate
source_stack exec -T postgres psql -U postgres -d topup -v ON_ERROR_STOP=1 -qc 'INSERT INTO heartbeat DEFAULT VALUES; UPDATE restore_timeline SET timeline_id = 7'
# Start the whole gate chain once. A second `up` can restart a completed one-shot import before
# verification has written its marker, creating a different challenge and correctly rejecting
# the previous manifest. Keep this Compose client waiting while the source performs its export.
"$compose" -p "$project-destination" -f "$tmp/destination.yaml" -f "$tmp/destination-tls.json" \
    -f "$tmp/destination-sockets.yaml" up -d migrate >"$tmp/destination-start.log" 2>&1 &
destination_start=$!
for attempt in $(seq 1 90); do
    if destination_stack exec -T migration-holder test -s /run/migration/recipient.json; then break; fi
    if [[ "$attempt" == 90 ]]; then echo 'import did not publish recipient evidence' >&2; exit 1; fi
    sleep 1
done
destination_stack exec -T migration-holder cat /run/migration/recipient.json >"$tmp/recipient.json"
jq -e --slurpfile pins "$root/deploy/tests/migration-pins.json" \
    '.public_key == $pins[0].recipient and (.nonce | test("^[0-9a-f]{64}$"))' "$tmp/recipient.json" >/dev/null
source_stack up -d snapshot
for attempt in $(seq 1 90); do
    if source_stack exec -T migration-holder test -s /run/migration/snapshot.id; then break; fi
    if [[ "$attempt" == 90 ]]; then echo 'export snapshot did not become ready' >&2; exit 1; fi
    sleep 1
done
# Deliberate late write: metadata and pg_dump must still agree on the earlier exported snapshot.
source_stack exec -T postgres psql -U postgres -d topup -v ON_ERROR_STOP=1 -qc 'INSERT INTO heartbeat DEFAULT VALUES'
source_stack up -d export
source_stack wait export
if ! wait "$destination_start"; then
    cat "$tmp/destination-start.log" >&2
    exit 1
fi
destination_start=""
destination_stack wait migrate
[[ "$(destination_stack exec -T postgres psql -U postgres -d topup -XAtqc 'SELECT count(*) FROM heartbeat')" == 1 ]]
[[ "$(destination_stack exec -T postgres psql -U postgres -d topup -XAtqc 'SELECT count(*) FROM topup_migration.imported')" == 1 ]]
[[ "$(destination_stack exec -T postgres psql -U postgres -d topup -XAtqc "SELECT timeline_id = ('x' || left(pg_walfile_name(pg_current_wal_insert_lsn()), 8))::bit(32)::bigint FROM restore_timeline")" == t ]]
source_stack exec -T migration-holder test ! -e /run/migration/dump.pg
destination_stack exec -T migration-holder test ! -e /run/migration/dump.pg
# Use the restore session's exact libpq options for an error whose primary text contains a
# sentinel value. Neither the client stream nor PostgreSQL's server logs may expose it.
[[ "$(destination_stack run --rm --no-deps -T --entrypoint psql restore -XAtqc 'SHOW log_min_messages')" == panic ]]
if destination_stack run --rm --no-deps -T --entrypoint bash restore -ceu \
    'psql -X -v ON_ERROR_STOP=1 2>/dev/null' >"$tmp/restore-error.log" 2>&1 <<'SQL'
SELECT CAST('migration-sensitive-sentinel' AS integer);
SQL
then
    echo 'restore error fixture unexpectedly succeeded' >&2
    exit 1
fi
destination_stack logs --no-color postgres >"$tmp/server-errors.log"
if rg -q -F 'migration-sensitive-sentinel' "$tmp/restore-error.log" "$tmp/server-errors.log"; then
    echo 'restore error logging exposed a row value' >&2
    exit 1
fi
# A reboot clears tmpfs but retains the authenticated completion record. It must neither restore
# again nor require access to the export objects, and must preserve writes made after cutover.
destination_stack exec -T postgres psql -U postgres -d topup -v ON_ERROR_STOP=1 -qc 'INSERT INTO heartbeat DEFAULT VALUES'
support stop s3-tls
destination_stack down
destination_stack up -d migrate
destination_stack wait migrate
[[ "$(destination_stack exec -T postgres psql -U postgres -d topup -XAtqc 'SELECT count(*) FROM heartbeat')" == 2 ]]
[[ "$(destination_stack exec -T postgres psql -U postgres -d topup -XAtqc 'SELECT count(*) FROM topup_migration.imported')" == 1 ]]
echo 'rendered compose export/import, snapshot consistency, private tmpfs handoffs and reboot gates passed'
