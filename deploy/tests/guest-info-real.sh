#!/usr/bin/env bash
# Public archived v0.13.6 production evidence only: a leaf quote, event log and public Info.
# Unused public certificate/compose content and duplicate log were removed, never sealed env.
# Normalize through the actual SDK/CLI, then exercise the pinned official verifier, not a stub.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
server_pid=""
cleanup() {
    if [[ -n "$server_pid" ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
    find "$tmp" -depth -delete
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
fixture=$root/deploy/tests/fixtures/guest-info
# A one-request loopback fixture server stands in for the socket's v0 HTTP transport.
python3 - "$fixture/info.json" "$tmp/port" <<'PY' &
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import sys
body = Path(sys.argv[1]).read_bytes()
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != '/Info':
            self.send_error(404)
            return
        self.rfile.read(int(self.headers['Content-Length']))
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *_):
        pass
with HTTPServer(('127.0.0.1', 0), Handler) as server:
    Path(sys.argv[2]).write_text(str(server.server_port))
    server.handle_request()
PY
server_pid=$!
for ((attempt=0; attempt<50; attempt++)); do [[ ! -s "$tmp/port" ]] || break; sleep 0.1; done
DSTACK_SIMULATOR_ENDPOINT="http://127.0.0.1:$(cat "$tmp/port")" \
    "${TOPUP:-$root/target/debug/topup}" guest-info >"$tmp/info.json"
wait "$server_pid"
server_pid=""
jq -e --slurpfile raw "$fixture/info.json" '.vm_config == $raw[0].vm_config and (.tcb_info | type) == "object"' \
    "$tmp/info.json" >/dev/null
jq --slurpfile info "$tmp/info.json" '{attestation:null,
    quote:.app_certificates[0].quote,event_log:(.tcb_info.event_log | tojson),vm_config:$info[0].vm_config}' \
    "$fixture/attestation.json" >"$tmp/request.json"
"$root/deploy/dstack-verifier.sh" <"$tmp/request.json" >"$tmp/verified.json"
jq -e '.is_valid == true and .details.quote_verified == true and .details.event_log_verified == true
    and .details.os_image_hash_verified == true and .details.tcb_status == "UpToDate"
    and .details.app_info.app_id == "2ed68f31bdcc5a914afaefbfb33e8fbbf7c4dcc1"
    and .details.app_info.os_image_hash == "ec43cb8c5e04b6bb6c53b33b16076a01d83d01f7f4b703f2f78a0d947d6247e6"' \
    "$tmp/verified.json" >/dev/null
jq '.vm_config |= (fromjson | .cpu_count += 1 | tojson)' "$tmp/request.json" >"$tmp/tampered.json"
if "$root/deploy/dstack-verifier.sh" <"$tmp/tampered.json" >"$tmp/rejected.json"; then
    echo "tampered vm_config accepted" >&2; exit 1
fi
jq -e '.is_valid == false and .details.quote_verified == true and .details.os_image_hash_verified == false
    and (.reason | startswith("OS image hash verification failed:"))' "$tmp/rejected.json" >/dev/null
# Pre-seal verification must require the signature AND RTMR3, including payload validation.
jq 'del(.vm_config)' "$tmp/request.json" >"$tmp/partial.json"
"$root/deploy/dstack-verifier.sh" --partial <"$tmp/partial.json" >"$tmp/partial-result.json"
jq -e '.is_valid == false and .details.quote_verified == true and .details.os_image_hash_verified == false' \
    "$tmp/partial-result.json" >/dev/null
if "$root/deploy/dstack-verifier.sh" <"$tmp/partial.json" >"$tmp/not-acceptance.json"; then
    echo "partial-only evidence passed full acceptance" >&2; exit 1
fi
jq '.event_log |= (fromjson | map(if .imr == 3 and .event == "app-id" then .event_payload = ("00" * 20) else . end) | tojson)' \
    "$tmp/partial.json" >"$tmp/tampered-log.json"
if "$root/deploy/dstack-verifier.sh" --partial <"$tmp/tampered-log.json" >"$tmp/rejected-log.json"; then
    echo "tampered event payload accepted in partial verification" >&2; exit 1
fi
jq -e '.details.quote_verified == false' "$tmp/rejected-log.json" >/dev/null
echo "real SDK Info + pinned verifier: valid quote/config passed, tampered config/log rejected; partial cannot pass acceptance"
