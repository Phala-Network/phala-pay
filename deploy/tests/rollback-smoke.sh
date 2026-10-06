#!/usr/bin/env bash
# Current binary migrates; the actual immutable N-1 image migrates and serves the resulting DB.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
current=${1:?current topup binary required}
previous=${2:?previous release image digest required}
previous_config=${3:?previous release config from its verified deploy kit required}
[[ -s "$previous_config" ]] || { echo 'previous release config is missing' >&2; exit 64; }
[[ "$previous" =~ @sha256:[0-9a-f]{64}$ ]] || { echo 'previous image must be immutable' >&2; exit 64; }
pg=postgres:18.6-trixie@sha256:86c951e05bf56c93d95d397747fb8820ac76cc3bedb78f43abd83eedbe3666ae
python=python:3.14-slim-trixie@sha256:caaf356f40667c496d405780745b9ac25771c189a51dfcc42430d531ea09f8a2
name=rollback-smoke-$$
tmp=$(mktemp -d "$root/.rollback-smoke.XXXXXX")
images=()
cleanup() {
    local status=$?
    if ((status != 0)); then
        docker logs "$name-api" >&2 2>/dev/null || true
        docker logs "$name-kms" >&2 2>/dev/null || true
    fi
    # PostgreSQL declares an anonymous data volume; remove it with its test container.
    docker rm -fv "$name-api" "$name-kms" "$name-db" >/dev/null 2>&1 || true
    docker network rm "$name" >/dev/null 2>&1 || true
    for image in "${images[@]}"; do docker image rm "$image" >/dev/null 2>&1 || true; done
    rm -rf "$tmp"
}
trap cleanup EXIT INT TERM
for image in "$pg" "$python" "$previous"; do
    if ! docker image inspect "$image" >/dev/null 2>&1; then
        images+=("$image")
        timeout --kill-after=2 180 docker pull "$image" >/dev/null
    fi
done
docker network create "$name" >/dev/null
docker run -d --name "$name-db" --network "$name" --network-alias db \
    -p 127.0.0.1::5432 -e POSTGRES_PASSWORD=smoke -e POSTGRES_DB=topup "$pg" >/dev/null
source "$root/deploy/deadline.sh"
stage_start rollback-db 60
until docker exec "$name-db" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; do
    stage_remaining || { stage_expired; exit 1; }
    stage_sleep 1
done
port=$(docker port "$name-db" 5432/tcp | cut -d: -f2)
# N-1 keeps its own verified route syntax, which need not be backwards compatible.
# Shipped examples before 0.9.1 were not production-valid; explicitly rehearse N-1
# noncommercially, without changing the verified kit's config.
sed 's/^environment: production.*/environment: local/' \
    "$previous_config" >"$tmp/previous.yaml"
stage_start current-migrate 180
DATABASE_URL="postgres://postgres:smoke@127.0.0.1:$port/topup" \
    stage_call 180 "$current" migrate
# Reject an unmarked future migration, then allow the same exact checksum at this binary's floor.
psql_owner() { docker exec "$name-db" psql -U postgres -d topup -X -v ON_ERROR_STOP=1 "$@"; }
stage_start compatibility-checks 180
schema=$(psql_owner -Atqc 'SELECT max(version) FROM _sqlx_migrations')
psql_owner -c "INSERT INTO _sqlx_migrations VALUES (20990101000000,'smoke future',now(),true,decode('abcd','hex'),0)" >/dev/null
if DATABASE_URL="postgres://postgres:smoke@127.0.0.1:$port/topup" stage_call 60 "$current" migrate >"$tmp/unmarked.log" 2>&1; then
    echo 'unmarked future migration was accepted' >&2; exit 1
fi
grep -q 'migration 20990101000000 was previously applied but is missing' "$tmp/unmarked.log"
psql_owner -c "INSERT INTO topup_migration_compatibility VALUES (20990101000000,decode('abcd','hex'),$schema)" >/dev/null
DATABASE_URL="postgres://postgres:smoke@127.0.0.1:$port/topup" stage_call 60 "$current" migrate >/dev/null
for corruption in "checksum=decode('ffff','hex')" "compatibility_floor=$((schema + 1))"; do
    psql_owner -c "UPDATE topup_migration_compatibility SET $corruption WHERE version=20990101000000" >/dev/null
    if DATABASE_URL="postgres://postgres:smoke@127.0.0.1:$port/topup" stage_call 60 "$current" migrate >"$tmp/corrupt.log" 2>&1; then
        echo "incompatible migration accepted: $corruption" >&2; exit 1
    fi
    grep -q 'migration 20990101000000 was previously applied but is missing' "$tmp/corrupt.log"
    psql_owner -c "UPDATE topup_migration_compatibility SET checksum=decode('abcd','hex'),compatibility_floor=$schema WHERE version=20990101000000" >/dev/null
done
psql_owner -c "CREATE ROLE smoke_app LOGIN IN ROLE topup_app" >/dev/null
if docker exec "$name-db" psql -U smoke_app -d topup -X -v ON_ERROR_STOP=1 -c 'DELETE FROM topup_migration_compatibility' >/dev/null 2>&1; then
    echo 'application role can alter compatibility ledger' >&2; exit 1
fi
psql_owner -c 'DELETE FROM topup_migration_compatibility WHERE version=20990101000000; DELETE FROM _sqlx_migrations WHERE version=20990101000000' >/dev/null
checksum=$(psql_owner -Atqc "SELECT encode(checksum,'hex') FROM _sqlx_migrations WHERE version=$schema")
psql_owner -c "UPDATE _sqlx_migrations SET checksum=decode('ffff','hex') WHERE version=$schema" >/dev/null
if DATABASE_URL="postgres://postgres:smoke@127.0.0.1:$port/topup" stage_call 60 "$current" migrate >"$tmp/known.log" 2>&1; then
    echo 'known migration checksum mismatch was accepted' >&2; exit 1
fi
grep -q "migration $schema was previously applied but has been modified" "$tmp/known.log"
psql_owner -c "UPDATE _sqlx_migrations SET checksum=decode('$checksum','hex') WHERE version=$schema" >/dev/null
# This fixture is removed before the real N-1 smoke; it must not mask a real new migration.
if [[ ${NO_ROLLBACK:-0} == 1 ]]; then
    if stage_call 60 docker run --rm --network "$name" -e DATABASE_URL=postgres://postgres:smoke@db:5432/topup \
        -v "$tmp/previous.yaml:/etc/topup.yaml:ro" \
        "$previous" topup migrate --config /etc/topup.yaml; then
        echo 'restore-only release did not reject N-1 migration startup' >&2; exit 1
    fi
    echo 'Explicit CHANGELOG exception: no rollback; restore required. N-1 failed closed.'
    exit 0
fi
stage_call 60 docker run --rm --network "$name" -e DATABASE_URL=postgres://postgres:smoke@db:5432/topup \
    -v "$tmp/previous.yaml:/etc/topup.yaml:ro" \
    "$previous" topup migrate --config /etc/topup.yaml
# Test-only KMS boundary: the official SDK GetKey contract, with a fixed non-production key.
cat >"$tmp/kms.py" <<'PY'
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path != "/GetKey":
            self.send_error(404)
            return
        body = json.dumps({"key": "01" * 32, "signature_chain": []}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

HTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
PY
chmod 755 "$tmp"
docker run -d --name "$name-kms" --network "$name" --network-alias kms \
    -v "$tmp/kms.py:/kms.py:ro" "$python" python /kms.py >/dev/null
until stage_call 5 docker exec "$name-kms" python -c \
    'import urllib.request; urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:8080/GetKey", data=b"{}", headers={"Content-Type":"application/json"})).read()' >/dev/null 2>&1; do
    stage_remaining || { stage_expired; exit 1; }
    stage_sleep 1
done
# A read-only startup smoke avoids external chains and webhook effects while loading real N-1 SQL/API.
docker run -d --name "$name-api" --network "$name" -p 127.0.0.1::8080 \
    -e DATABASE_URL=postgres://postgres:smoke@db:5432/topup \
    -e TOPUP_RPC_ALCHEMY_SEPOLIA_KEY=smoke-placeholder \
    -e DSTACK_SIMULATOR_ENDPOINT=http://kms:8080 \
    -v "$tmp/previous.yaml:/etc/topup.yaml:ro" \
    "$previous" topup run --config /etc/topup.yaml --read-only >/dev/null
port=$(docker port "$name-api" 8080/tcp | cut -d: -f2)
stage_start previous-api 60
until stage_call 5 curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null; do
    if ! stage_remaining; then docker logs "$name-api" >&2; stage_expired; exit 1; fi
    stage_sleep 1
done
stage_call 5 curl -fsS "http://127.0.0.1:$port/openapi.json" >"$tmp/openapi.json"
jq -e '.openapi and .paths' "$tmp/openapi.json" >/dev/null
echo "rollback smoke passed: current migration -> $previous migration + API health/OpenAPI"
