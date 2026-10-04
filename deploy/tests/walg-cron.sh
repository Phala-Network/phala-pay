#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
built_image=

cleanup() {
    find "$tmp" -depth -delete
    if [ -n "$built_image" ]; then
        docker image rm "$built_image" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

# The archive_command cases run in the postgres-walg image (CI passes the one it just built);
# otherwise a per-run tag is built from this checkout.
if [ "$#" -ge 1 ]; then
    image=$1
else
    built_image="phala-pay-postgres-walg:walg-cron-$$"
    docker build -q -f "$root/deploy/Dockerfile.postgres-walg" -t "$built_image" "$root" >/dev/null
    image=$built_image
fi

touch "$tmp/alpha" "$tmp/beta"
output=$(
    cd "$tmp"
    WALG_CRON_DRY_RUN=1 \
        WALG_RETENTION_FULL=3 \
        PGDATA=/var/lib/postgresql/data \
        "$root/deploy/scripts/walg-cron" backup-push "0 3 * * *"
)

printf '%s\n' "$output"
printf '%s\n' "$output" | grep -F \
    'parsed schedule: minute=0 hour=3 day-of-month=* month=* day-of-week=*' >/dev/null
printf '%s\n' "$output" | grep -F 'next WAL-G base backup at ' >/dev/null
printf '%s\n' "$output" | grep -F \
    'dry-run: wal-g backup-push /var/lib/postgresql/data' >/dev/null
printf '%s\n' "$output" | grep -F \
    'dry-run: wal-g delete retain FULL 3 --use-sentinel-time --confirm' >/dev/null

# A restored instance pushes no base backup into the prefix it restores from.
output=$(
    WALG_CRON_DRY_RUN=1 TOPUP_RESTORE_FROM_BACKUP=on \
        "$root/deploy/scripts/walg-cron" backup-push "0 3 * * *"
)
printf '%s\n' "$output" | grep -Fx 'base backups are disabled while TOPUP_RESTORE_FROM_BACKUP=on' \
    >/dev/null
if printf '%s\n' "$output" | grep -F 'dry-run:' >/dev/null; then
    echo "walg-cron scheduled a base backup while TOPUP_RESTORE_FROM_BACKUP=on" >&2
    exit 1
fi

# archive_command: in the image, so the marker is the fixed path topup reads.
docker run --rm -i --entrypoint sh "$image" -s <<'CASES'
set -eu
marker=/run/topup-observability/last-backup-unix-seconds
mkdir -p /tmp/bin
cat >/tmp/bin/wal-g <<'FAKE'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>/tmp/wal-g.call
if [ "$1" = wal-push ] && [ -n "${WALG_TEST_FAIL_PUSH:-}" ]; then
    exit 1
fi
FAKE
chmod +x /tmp/bin/wal-g
touch /tmp/segment
export PATH="/tmp/bin:$PATH"

# wal-push, then the marker is refreshed.
AWS_ACCESS_KEY_ID=test walg-cron wal-push /tmp/segment
grep -Fx "wal-push /tmp/segment" /tmp/wal-g.call >/dev/null
grep -E '^[0-9]+$' "$marker" >/dev/null
[ "$(stat -c %a "$marker")" = 644 ]

# An old backlog upload keeps its data timestamp instead of making health fresh.
touch -d '10 minutes ago' /tmp/segment
expected=$(stat -c %Y /tmp/segment)
AWS_ACCESS_KEY_ID=test walg-cron wal-push /tmp/segment
[ "$(cat "$marker")" -eq "$expected" ]
[ ! -e /run/topup-observability/last-base-backup-unix-seconds ]

# A failed upload must leave the marker untouched.
rm "$marker"
if WALG_TEST_FAIL_PUSH=1 AWS_ACCESS_KEY_ID=test walg-cron wal-push /tmp/segment 2>/dev/null; then
    echo "failed WAL upload unexpectedly succeeded" >&2
    exit 1
fi
[ ! -e "$marker" ]

# Unsealed (no S3 key): archive_command fails at once, without calling WAL-G.
: >/tmp/wal-g.call
if AWS_ACCESS_KEY_ID='' walg-cron wal-push /tmp/segment 2>/dev/null; then
    echo "WAL archiving without S3 credentials unexpectedly succeeded" >&2
    exit 1
fi
[ ! -s /tmp/wal-g.call ] && [ ! -e "$marker" ]

# A restored instance does not archive into the prefix it restores from.
if TOPUP_RESTORE_FROM_BACKUP=on AWS_ACCESS_KEY_ID=test \
    walg-cron wal-push /tmp/segment 2>/dev/null; then
    echo "WAL archiving while TOPUP_RESTORE_FROM_BACKUP=on unexpectedly succeeded" >&2
    exit 1
fi
[ ! -s /tmp/wal-g.call ] && [ ! -e "$marker" ]
CASES

# walg-timeline-backup: a base backup is taken at start only when none is on the current timeline.
mkdir -p "$tmp/timeline-bin"
cat >"$tmp/timeline-bin/psql" <<'FAKE'
#!/bin/sh
printf '%s\n' "$TEST_TIMELINE"
FAKE
cat >"$tmp/timeline-bin/wal-g" <<'FAKE'
#!/bin/sh
set -eu
case "$*" in
    "backup-list --json") printf '%s\n' "$TEST_BACKUP_LIST" ;;
    "backup-push "*) printf '%s\n' "$2" >>"$TEST_BASE_BACKUP_CALL" ;;
    *) exit 70 ;;
esac
FAKE
chmod +x "$tmp/timeline-bin/psql" "$tmp/timeline-bin/wal-g"
timeline_backup() {
    : >"$tmp/base-backup.call"
    set +e
    PATH="$tmp/timeline-bin:$PATH" \
        WALG_BIN="$tmp/timeline-bin/wal-g" \
        WALG_OBSERVABILITY_DIR="$tmp/markers" \
        TEST_BASE_BACKUP_CALL="$tmp/base-backup.call" \
        TEST_TIMELINE="$1" \
        TEST_BACKUP_LIST="$2" \
        AWS_ACCESS_KEY_ID="$3" \
        "$root/deploy/scripts/walg-timeline-backup" /var/lib/postgresql/data >/dev/null 2>&1
    timeline_status=$?
    set -e
}
listed='[{"backup_name":"base_000000010000000000000003","time":"2026-09-22T03:00:00Z"}]'
timeline_backup 1 "$listed" test
[ "$timeline_status" -eq 3 ] && [ ! -s "$tmp/base-backup.call" ] || {
    echo "walg-timeline-backup took a backup although one covers timeline 1" >&2
    exit 1
}
timeline_backup 2 "$listed" test
if [ "$timeline_status" -ne 0 ] || ! grep -Fx /var/lib/postgresql/data "$tmp/base-backup.call" >/dev/null; then
    echo "walg-timeline-backup did not back up the uncovered timeline 2" >&2
    exit 1
fi
timeline_backup 1 '[]' test
[ "$timeline_status" -eq 0 ] && [ -s "$tmp/base-backup.call" ] || {
    echo "walg-timeline-backup did not back up a prefix without base backups" >&2
    exit 1
}
timeline_backup 2 "$listed" ''
[ "$timeline_status" -eq 1 ] && [ ! -s "$tmp/base-backup.call" ] || {
    echo "walg-timeline-backup ran without object-storage credentials" >&2
    exit 1
}

echo "walg-cron dry-run test passed"

# More than ten initial failures must still retry before the daily schedule. Accelerate only
# the sleep clock; no PostgreSQL or object store is needed for this scheduler fault injection.
cat >"$tmp/timeline-bin/wal-g" <<'FAKE'
#!/bin/sh
set -eu
case "$1" in
    backup-list)
        count=$(cat "$TEST_COUNTER" 2>/dev/null || echo 0)
        count=$((count + 1))
        echo "$count" > "$TEST_COUNTER"
        [ "$count" -gt 12 ] || exit 1
        echo '[]' ;;
    backup-push) touch "$TEST_SUCCEEDED" ;;
    *) exit 70 ;;
esac
FAKE
cat >"$tmp/timeline-bin/sleep" <<'FAKE'
#!/bin/sh
# Stop the background SQL monitor; stop the scheduler only after the first backup succeeded.
[ "$1" -ne 30 ] || exit 77
[ ! -e "$TEST_SUCCEEDED" ] || exit 77
FAKE
chmod +x "$tmp/timeline-bin/sleep"
set +e
PATH="$tmp/timeline-bin:$root/deploy/scripts:$PATH" TEST_TIMELINE=1 \
    TEST_COUNTER="$tmp/retry-count" TEST_SUCCEEDED="$tmp/succeeded" \
    AWS_ACCESS_KEY_ID=test WALG_BASE_ATTEMPTS=1 WALG_OBSERVABILITY_DIR="$tmp/markers" \
    "$root/deploy/scripts/walg-cron" backup-push '0 3 * * *' >"$tmp/retry.log" 2>&1
status=$?
set -e
[ "$status" -eq 77 ] && [ "$(cat "$tmp/retry-count")" -eq 13 ] && [ -e "$tmp/succeeded" ]
[ -s "$tmp/markers/last-base-backup-unix-seconds" ]
echo 'first base backup retries past the old budget and succeeds'

# Keep the real-client network fault injection in the existing CI regression entry point.
"$root/deploy/tests/walg-stalled-s3.sh" "$image"
