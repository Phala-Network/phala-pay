#!/usr/bin/env bash
# Exercise deploy/instance-pause.sh against a local server that first answers transiently and then
# succeeds, and verify that client errors are returned without replaying a signed request.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
server_pid=
trap '[[ -z "${server_pid:-}" ]] || kill "$server_pid" 2>/dev/null || true; rm -rf "$tmp"' EXIT INT TERM

openssl genpkey -algorithm Ed25519 -out "$tmp/key.pem" 2>/dev/null

start_server() {
    local mode=$1
    local port_file="$tmp/$mode.port" count_file="$tmp/$mode.count" signature_file="$tmp/$mode.signatures"
    : >"$port_file"
    : >"$count_file"
    : >"$signature_file"
    python3 - "$port_file" "$count_file" "$signature_file" "$mode" >"$tmp/$mode.log" 2>&1 <<'PY' &
import http.server
import pathlib
import sys

port_file = pathlib.Path(sys.argv[1])
count_file = pathlib.Path(sys.argv[2])
signature_file = pathlib.Path(sys.argv[3])
mode = sys.argv[4]


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        count = int(count_file.read_text() or "0") + 1
        count_file.write_text(str(count))
        with signature_file.open("a") as signatures:
            signatures.write(self.headers.get("Signature-Input", "") + "\n")
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        if mode == "retry" and count <= 2:
            self.send_response(503)
            self.send_header("Retry-After", "1")
            body = b'{"error":"warming up"}\n'
        elif mode == "400":
            self.send_response(400)
            body = b'{"error":"bad request"}\n'
        elif mode == "401":
            self.send_response(401)
            body = b'{"error":"unauthorized"}\n'
        else:
            self.send_response(200)
            body = b'{"paused_scopes":[],"owner":"test-owner","expires_at":0}\n'
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


class Server(http.server.ThreadingHTTPServer):
    allow_reuse_address = True


server = Server(("127.0.0.1", 0), Handler)
port_file.write_text(str(server.server_port))
server.serve_forever()
PY
    server_pid=$!
    for _ in {1..50}; do
        [[ -s "$port_file" ]] && return 0
        sleep 0.1
    done
    echo "instance-pause test server did not start" >&2
    exit 1
}

stop_server() {
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=
}

run_pause() {
    local mode=$1 output_file=$2 error_file=$3
    local port
    port=$(<"$tmp/$mode.port")
    TOPUP_MAINTENANCE_PRIVATE_KEY_PEM=$(<"$tmp/key.pem") \
    TOPUP_MAINTENANCE_KEY_ID=maintenance/test-v1 \
    INSTANCE_PAUSE_DEADLINE_SECONDS=20 \
    "$root/deploy/instance-pause.sh" resume "http://127.0.0.1:$port" test-owner \
        >"$output_file" 2>"$error_file"
}

start_server retry
run_pause retry "$tmp/retry.out" "$tmp/retry.err"
[[ "$(jq -r '.paused_scopes | join(",")' "$tmp/retry.out")" == "" ]]
[[ "$(<"$tmp/retry.count")" == 3 ]]
[[ "$(wc -l <"$tmp/retry.signatures" | tr -d ' ')" == 3 ]]
[[ "$(sed -n 's/.*created=\([0-9][0-9]*\).*/\1/p' "$tmp/retry.signatures" | sort -u | wc -l | tr -d ' ')" == 3 ]]
grep -q 'retrying resume after HTTP 503 (attempt 1' "$tmp/retry.err"
grep -q 'retrying resume after HTTP 503 (attempt 2' "$tmp/retry.err"
stop_server

for mode in 400 401; do
    start_server "$mode"
    if run_pause "$mode" "$tmp/$mode.out" "$tmp/$mode.err"; then
        echo "instance-pause retried HTTP $mode or accepted it" >&2
        exit 1
    else
        status=$?
    fi
    [[ "$status" == 22 ]]
    [[ "$(<"$tmp/$mode.count")" == 1 ]]
    [[ "$(wc -l <"$tmp/$mode.signatures" | tr -d ' ')" == 1 ]]
    stop_server
done

echo 'instance-pause transient retry and client error tests passed'
