#!/bin/sh
# Fault injection against real WAL-G: S3 accepts the TCP request and never sends a response.
set -eu
image=${1:?usage: walg-stalled-s3.sh POSTGRES_WALG_IMAGE}
prefix="topup-walg-stall-$$"
cleanup() {
    docker rm -f "$prefix-client" "$prefix-store" >/dev/null 2>&1 || true
    docker network rm "$prefix" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker network create "$prefix" >/dev/null
docker run -d --name "$prefix-store" --network "$prefix" --network-alias stalled-s3 \
    python:3.13-slim python3 -u -c '
import socketserver
import threading

class StalledRequest(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.recv(65536)
        print("S3 request accepted", flush=True)
        threading.Event().wait()

with socketserver.ThreadingTCPServer(("0.0.0.0", 8080), StalledRequest) as server:
    print("ready", flush=True)
    server.serve_forever()
' >/dev/null
attempts=30
until docker logs "$prefix-store" 2>&1 | grep -Fx ready >/dev/null; do
    attempts=$((attempts - 1))
    [ "$attempts" -gt 0 ] || exit 1
    sleep 1
done
docker run --rm -i --name "$prefix-client" --network "$prefix" --entrypoint sh \
    -e WALG_S3_PREFIX=s3://backups/postgres -e AWS_ENDPOINT=http://stalled-s3:8080 \
    -e AWS_REGION=us-east-1 -e AWS_S3_FORCE_PATH_STYLE=true \
    -e AWS_ACCESS_KEY_ID=test -e AWS_SECRET_ACCESS_KEY=test \
    -e WALG_WAL_TIMEOUT_SECONDS=1 -e WALG_WAL_ATTEMPTS=1 \
    -e WALG_BASE_TIMEOUT_SECONDS=1 -e WALG_BASE_ATTEMPTS=1 \
    -e WALG_RESTORE_TIMEOUT_SECONDS=1 -e WALG_RESTORE_ATTEMPTS=1 \
    "$image" -s <<'CASES'
set -eu
expect() {
    expected=$1
    shift
    status=0
    "$@" || status=$?
    [ "$status" -eq "$expected" ] || {
        echo "expected $expected, got $status: $*" >&2
        exit 1
    }
}
# An upload, an S3 list, a base fetch, and a restore WAL fetch each reach a hung connection.
mkdir -p /tmp/archive_status
dd if=/dev/zero of=/tmp/000000010000000000000001 bs=1048576 count=16 2>/dev/null
expect 124 walg-cron wal-push /tmp/000000010000000000000001
expect 124 walg-cron run base backup-list --json
expect 124 walg-cron run restore backup-fetch /tmp/base base_000000010000000000000001
expect 126 walg-restore-command 000000010000000000000001 /tmp/restored-wal
[ ! -e /run/topup-observability/last-backup-unix-seconds ]
CASES
[ "$(docker logs "$prefix-store" 2>&1 | grep -c 'S3 request accepted')" -ge 4 ]
echo 'stalled S3 upload/list/base-fetch/WAL-fetch deadlines passed; restore failed closed'
