#!/usr/bin/env bash
# Isolated logical migration rehearsal: two PostgreSQL clusters and the pinned dstack simulator.
# S3 transport is replaced by object_store's in-memory backend; the signed/encrypted payload and
# all database/attestation operations use the production implementation. No live credentials.
set -euo pipefail
root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
project="topup-migration-$(date +%s)-$$"
image=${TOPUP_MIGRATION_SIMULATOR_IMAGE:-phala-pay-migration-simulator:local}
postgres=postgres:18.6-trixie@sha256:5a5a84b19854a9ffaa54082c166ff4ec27473a361e496e5ea167f298f2da9722
cleanup() {
    docker rm -f "$project-source" "$project-destination" "$project-simulator" >/dev/null 2>&1 || true
}
trap cleanup EXIT
for name in source destination; do
    docker run -d --name "$project-$name" --label "phala-pay-test=$project" \
        -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=topup \
        -p 127.0.0.1::5432 "$postgres" >/dev/null
done
docker image inspect "$image" >/dev/null || {
    echo "build the pinned simulator first: docker build -t $image -f deploy/local/Dockerfile.dstack-simulator ." >&2
    exit 1
}
# Expose only the simulator's internal API on an ephemeral loopback port.
docker run -d --name "$project-simulator" --label "phala-pay-test=$project" \
    -p 127.0.0.1::8090 "$image" sh -ceu \
    'sed -i '\''/\[internal\]/,/\[external\]/{s|address = "unix:/var/run/dstack.sock"|address = "0.0.0.0"\nport = 8090|;}'\'' dstack.toml; exec dstack-simulator --config dstack.toml' >/dev/null
for name in source destination; do
    for attempt in $(seq 1 60); do
        if docker exec "$project-$name" pg_isready -U postgres -d topup >/dev/null; then break; fi
        if [[ "$attempt" == 60 ]]; then echo "local PostgreSQL did not become ready" >&2; exit 1; fi
        sleep 1
    done
done
source_port=$(docker port "$project-source" 5432/tcp | cut -d: -f2)
destination_port=$(docker port "$project-destination" 5432/tcp | cut -d: -f2)
simulator_port=$(docker port "$project-simulator" 8090/tcp | cut -d: -f2)
export MIGRATION_DSTACK_ENDPOINT="http://127.0.0.1:$simulator_port"
for attempt in $(seq 1 60); do
    if curl --silent --fail --max-time 2 --header Content-Type:application/json --data '{}' "$MIGRATION_DSTACK_ENDPOINT/Info" >/dev/null; then break; fi
    if [[ "$attempt" == 60 ]]; then echo "local dstack simulator did not become ready" >&2; exit 1; fi
    sleep 1
done
export MIGRATION_SOURCE_CONTAINER="$project-source" MIGRATION_DESTINATION_CONTAINER="$project-destination"
export MIGRATION_SOURCE_URL="postgres://postgres@127.0.0.1:$source_port/topup"
export MIGRATION_DESTINATION_URL="postgres://postgres@127.0.0.1:$destination_port/topup"
cd "$root"
SQLX_OFFLINE=true CARGO_BUILD_JOBS=4 timeout 1200 cargo test -p topup --lib --locked \
    migration::tests::simulator_two_postgres_roundtrip_and_row_count_mismatch -- --nocapture
