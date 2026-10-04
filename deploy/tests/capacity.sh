#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.capacity-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/pg/pg_wal/archive_status" "$tmp/obs"
ready=000000010000000000000001
done=000000010000000000000002
truncate -s 16777216 "$tmp/pg/pg_wal/$ready"
truncate -s 33554432 "$tmp/pg/pg_wal/$done"
touch -d '5 minutes ago' "$tmp/pg/pg_wal/$ready"
touch "$tmp/pg/pg_wal/archive_status/$ready.ready" "$tmp/pg/pg_wal/archive_status/$done.done"
PGDATA="$tmp/pg" CAPACITY_REPORT_DIR="$tmp/obs" CAPACITY_ONCE=1 bash "$root/deploy/capacity.sh"
jq -e '.wal_bytes == 16777216 and .wal_age_seconds >= 300 and .pgdata_total > 0 and .observability_total > 0' \
    "$tmp/obs/capacity.json" >/dev/null
mv "$tmp/pg/pg_wal/archive_status/$ready.ready" "$tmp/pg/pg_wal/archive_status/$ready.done"
PGDATA="$tmp/pg" CAPACITY_REPORT_DIR="$tmp/obs" CAPACITY_ONCE=1 bash "$root/deploy/capacity.sh"
jq -e '.wal_bytes == 0 and .wal_age_seconds == 0' "$tmp/obs/capacity.json" >/dev/null
echo 'capacity pending WAL size/age and archived exclusion passed'
