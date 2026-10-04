#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
cleanup() {
    find "$tmp" -depth -delete
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cat >"$tmp/wal-g" <<'EOF_FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"$FAKE_LOG"
if [ "${FAKE_STALL:-0}" = 1 ]; then sleep 60; fi
exit "$FAKE_WAL_FETCH_STATUS"
EOF_FAKE
chmod +x "$tmp/wal-g"
export WALG_BIN="$tmp/wal-g" FAKE_LOG="$tmp/wal-g.log" WALG_RESTORE_ATTEMPTS=1
wal=000000010000000000000002

# restore_command: a fetched segment succeeds, WAL-G's 74 (not archived) ends recovery with 1, and
# any other failure (storage, decryption) is 126, which aborts recovery instead of promoting.
expect() {
    set +e
    FAKE_WAL_FETCH_STATUS=$1 "$root/deploy/scripts/walg-restore-command" "$wal" "$tmp/destination"
    status=$?
    set -e
    test "$status" -eq "$2" || {
        echo "wal-fetch status $1 returned $status, expected $2" >&2
        exit 1
    }
}
expect 0 0
expect 74 1
expect 1 126
expect 2 126
grep -Fx "wal-fetch $wal $tmp/destination" "$tmp/wal-g.log" >/dev/null

echo "WAL-G restore-command tests passed"

# A connected but stalled fetch must abort recovery, never signal a missing segment.
before=$(wc -l < "$tmp/wal-g.log")
set +e
FAKE_STALL=1 WALG_RESTORE_TIMEOUT_SECONDS=1 WALG_RESTORE_ATTEMPTS=2 \
    "$root/deploy/scripts/walg-restore-command" "$wal" "$tmp/destination"
status=$?
set -e
[ "$status" -eq 126 ]
[ "$(( $(wc -l < "$tmp/wal-g.log") - before ))" -eq 2 ]
# WAL push and base operations have independent attempt budgets and timeout status 124.
for operation in wal base; do
    set +e
    FAKE_STALL=1 WALG_WAL_TIMEOUT_SECONDS=1 WALG_WAL_ATTEMPTS=1 \
        WALG_BASE_TIMEOUT_SECONDS=1 WALG_BASE_ATTEMPTS=1 \
        "$root/deploy/scripts/walg-cron" run "$operation" backup-list --json
    status=$?
    set -e
    [ "$status" -eq 124 ]
done
echo 'WAL-G bounded retry and timeout tests passed'

# GNU timeout treats zero as no deadline. Reject zero (including leading-zero forms), invalid
# durations and unbounded attempt configurations before touching the client.
for invalid in 0 00 01 -1 forever 86401; do
    set +e
    WALG_RESTORE_TIMEOUT_SECONDS="$invalid" "$root/deploy/scripts/walg-restore-command" "$wal" "$tmp/destination" >/dev/null 2>&1
    status=$?
    set -e
    [ "$status" -eq 126 ]
done
echo 'invalid deadline configuration fails closed'
