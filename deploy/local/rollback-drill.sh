#!/usr/bin/env bash
# Published N-1 backup -> current migration -> N-1 refused -> pre-upgrade restore.
# Uses only a disposable PostgreSQL cluster and the verified published image.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
previous=${1:?verified published N-1 image digest required}
[[ "$previous" =~ @sha256:[0-9a-f]{64}$ ]] || { echo 'N-1 must be an immutable published image' >&2; exit 64; }
for tool in docker cargo; do command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }; done
tmp=$(mktemp -d "$root/.task-tmp/rollback-drill.XXXXXX")
name=topup-rollback-drill-$$
image=postgres:18.6-trixie@sha256:86c951e05bf56c93d95d397747fb8820ac76cc3bedb78f43abd83eedbe3666ae
cleanup() {
    local status=$?
    docker rm -fv "$name" >/dev/null 2>&1 || true
    rm -rf "$tmp"
    exit "$status"
}
trap cleanup EXIT INT TERM
docker image inspect "$previous" >/dev/null 2>&1 || timeout --kill-after=2 180 docker pull "$previous" >/dev/null
docker run -d --name "$name" -p 127.0.0.1::5432 -e POSTGRES_PASSWORD=drill "$image" -c max_connections=100 >/dev/null
for ((attempt=0; attempt<60; attempt++)); do
    docker exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break
    sleep 1
done
docker exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null
port=$(docker port "$name" 5432/tcp | cut -d: -f2)
cd "$root"
export CI=true SQLX_OFFLINE=true TOPUP_ROLLBACK_IMAGE="$previous"
export OWNER_DATABASE_URL="postgres://postgres:drill@127.0.0.1:$port/postgres"
export DATABASE_URL="postgres://topup_ci:topup_ci@127.0.0.1:$port/postgres"
# The declared breaking migration refuses the old image; only restoring the backup permits it.
timeout --kill-after=10 1800 cargo test --locked -p topup --features test-support --test rollback_drill \
    published_image_requires_pre_upgrade_restore -- --ignored --exact --nocapture
