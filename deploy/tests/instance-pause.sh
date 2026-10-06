#!/usr/bin/env bash
# Exercise deploy/instance-pause.sh against a local server that first answers transiently and then
# succeeds, and verify that client errors are returned without replaying a signed request.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
server_pid=

cleanup() {
    if [[ -n "${server_pid:-}" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

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
import socket
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
        if mode == "connection-reset" and count == 1:
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if mode == "retry-after-zero" and count == 1:
            self.send_response(503)
            self.send_header("Retry-After", "0")
            body = b'{"error":"warming up"}\n'
        elif mode == "400":
            self.send_response(400)
            body = b'{"error":"bad request"}\n'
        elif mode == "401":
            self.send_response(401)
            body = b'{"error":"unauthorized"}\n'
        elif mode == "redirect":
            self.send_response(302)
            self.send_header("Location", "/v1/admin/instance/resume")
            body = b'{"error":"redirect"}\n'
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

created_count() {
    sed -n 's/.*created=\([0-9][0-9]*\).*/\1/p' "$1" | sort -u | wc -l | tr -d '[:space:]'
}

cat >"$tmp/expected-body" <<'EOF'
{"paused_scopes":[],"owner":"test-owner","expires_at":0}
EOF

start_server connection-reset
run_pause connection-reset "$tmp/connection-reset.out" "$tmp/connection-reset.err"
cmp -s "$tmp/expected-body" "$tmp/connection-reset.out"
[[ "$(<"$tmp/connection-reset.count")" == 2 ]]
[[ "$(created_count "$tmp/connection-reset.signatures")" == 2 ]]
grep -q 'transport failure (curl=52)' "$tmp/connection-reset.err"
stop_server

start_server retry-after-zero
run_pause retry-after-zero "$tmp/retry-after-zero.out" "$tmp/retry-after-zero.err"
cmp -s "$tmp/expected-body" "$tmp/retry-after-zero.out"
[[ "$(<"$tmp/retry-after-zero.count")" == 2 ]]
[[ "$(created_count "$tmp/retry-after-zero.signatures")" == 2 ]]
grep -q 'retrying resume after HTTP 503 (attempt 1, backoff 1s)' "$tmp/retry-after-zero.err"
stop_server

start_server redirect
if run_pause redirect "$tmp/redirect.out" "$tmp/redirect.err"; then
    echo 'instance-pause accepted a 3xx response' >&2
    exit 1
else
    status=$?
fi
[[ "$status" == 22 ]]
[[ "$(<"$tmp/redirect.count")" == 1 ]]
grep -q 'redirect' "$tmp/redirect.err"
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
    [[ "$(wc -l <"$tmp/$mode.signatures" | tr -d '[:space:]')" == 1 ]]
    stop_server
done

echo 'instance-pause transport/status retry and client error tests passed'
