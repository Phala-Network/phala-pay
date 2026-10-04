#!/usr/bin/env bash
# Read-only capacity probe. Writes only its atomic sample in the observability volume.
set -euo pipefail
pgdata=${PGDATA:-/var/lib/postgresql/data}
report_dir=${CAPACITY_REPORT_DIR:-/run/topup-observability}
sample() {
    local pg_total pg_available obs_total obs_available bytes=0 oldest=0 size modified file now
    read -r pg_total pg_available < <(df -B1 --output=size,avail "$pgdata" | tail -1) || return 1
    read -r obs_total obs_available < <(df -B1 --output=size,avail "$report_dir" | tail -1) || return 1
    [[ -d "$pgdata/pg_wal/archive_status" ]] || return 1
    now=$(date +%s) || return 1
    find "$pgdata/pg_wal/archive_status" -maxdepth 1 -name '*.ready' -print0 >"$report_dir/capacity.pending.new" || return 1
    while IFS= read -r -d '' file; do
        [[ ${file##*/} =~ ^[0-9A-F]{24}\.ready$ ]] || continue
        # A successful archive may rename .ready during inspection; retry the next sample.
        read -r size modified < <(stat -c '%s %Y' "$pgdata/pg_wal/$(basename "$file" .ready)") || return 1
        bytes=$((bytes + size))
        ((oldest != 0 && oldest <= modified)) || oldest=$modified
    done <"$report_dir/capacity.pending.new"
    rm -f "$report_dir/capacity.pending.new" || return 1
    local age=0
    ((oldest == 0 || oldest >= now)) || age=$((now - oldest))
    printf '{"timestamp":%s,"pgdata_total":%s,"pgdata_available":%s,"observability_total":%s,"observability_available":%s,"wal_bytes":%s,"wal_age_seconds":%s}\n' \
        "$now" "$pg_total" "$pg_available" "$obs_total" "$obs_available" "$bytes" "$age" >"$report_dir/capacity.json.new" || return 1
    mv "$report_dir/capacity.json.new" "$report_dir/capacity.json" || return 1
}
trap 'rm -f "$report_dir/capacity.json.new" "$report_dir/capacity.pending.new"' EXIT
while true; do
    if ! sample; then echo 'capacity sample failed; retaining previous timestamp' >&2; fi
    [[ ${CAPACITY_ONCE:-0} != 1 ]] || exit 0
    sleep 30
done
