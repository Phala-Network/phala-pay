#!/usr/bin/env bash
# deploy/verify-ingress-evidence.sh against stub evidence, verifier, and TLS endpoint: it passes when
# the domain serves the certificate of its evidence, and a domain that does not answer the TLS
# handshake fails within the handshake's bound (`timeout 30`, shortened here) instead of hanging.
# Real curl requests to a local HTTP server cover evidence withdrawn with 404 and bounded retries.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
server_pid=""
cleanup() {
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT INT TERM
curl_cli=$(command -v curl)
timeout_cli=$(command -v timeout)
domain=pay.example.com app_id=95174fcad739ae76c7510dc1b2bc00a32e25c644
mkdir -p "$tmp/kit/deploy" "$tmp/bin" "$tmp/evidence"
cp "$root/deploy/verify-ingress-evidence.sh" "$tmp/kit/deploy/"

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 -subj "/CN=$domain" \
    -keyout "$tmp/key.pem" -out "$tmp/evidence/cert-$domain.pem" 2>/dev/null
echo '{"registration": {"uri": "https://acme.example/acct/1"}}' >"$tmp/evidence/acme-account.json"
echo '{"quote": "00", "vm_config": "{}", "event_log": "[]"}' >"$tmp/evidence/quote.json"
(cd "$tmp/evidence" && sha256sum acme-account.json "cert-$domain.pem" >sha256sum.txt)
manifest=$(sha256sum "$tmp/evidence/sha256sum.txt" | cut -c1-64)
jq -n --arg app_id "$app_id" --arg report_data "$manifest$(printf '0%.0s' {1..64})" \
    '{details: {tcb_status: "UpToDate", report_data: $report_data,
        app_info: {app_id: $app_id, compose_hash: "ab"}}}' >"$tmp/result.json"

printf '#!/bin/sh\ncat >/dev/null\ncat "%s"\n' "$tmp/result.json" >"$tmp/kit/deploy/dstack-verifier.sh"
cat >"$tmp/bin/curl" <<'STUB'
#!/usr/bin/env bash
if [[ -n "${STUB_EVIDENCE_URL:-}" ]]; then
    args=("$@")
    url=${!#}
    args[${#args[@]}-1]="$STUB_EVIDENCE_URL/${url##*/}"
    exec "$STUB_CURL" "${args[@]}"
fi
while (($# > 1)); do [[ "$1" != -o ]] || output=$2; shift; done
cp "$STUB_EVIDENCE/${1##*/}" "$output"
STUB
# s_client serves the evidence's certificate, or hangs; every other openssl command is the real one.
cat >"$tmp/bin/openssl" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == s_client ]]; then
    [[ "\$STUB_SERVE" == certificate ]] || exec sleep 600
    echo CONNECTED
    exec cat "$tmp/evidence/cert-$domain.pem"
fi
exec $(command -v openssl) "\$@"
STUB
# timeout records its bound, then enforces 2 seconds.
cat >"$tmp/bin/timeout" <<STUB
#!/usr/bin/env bash
echo "\$1" >>"$tmp/timeouts"
shift
exec $(command -v timeout) 2 "\$@"
STUB
chmod +x "$tmp/kit/deploy/dstack-verifier.sh" "$tmp/bin/curl" "$tmp/bin/openssl" "$tmp/bin/timeout"
export PATH="$tmp/bin:$PATH" STUB_EVIDENCE="$tmp/evidence"

fail() {
    echo "verify-ingress-evidence: $*" >&2
    exit 1
}

STUB_SERVE=certificate "$tmp/kit/deploy/verify-ingress-evidence.sh" "$domain" "0x$app_id" >"$tmp/out" 2>&1 ||
    { cat "$tmp/out" >&2; fail "refused the certificate its evidence names"; }
grep -q "certificate evidence of $domain passed" "$tmp/out" || fail "did not report the passed evidence"

# 2.8 withdraws evidence until it can regenerate it. Exercise curl's actual HTTP retry behavior,
# serving each file with 404 once and then the valid fixture, or with 404 on every request.
cat >"$tmp/server.py" <<'PY'
import http.server
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
attempts = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        mode = (root / "mode").read_text().strip()
        key = (mode, self.path)
        attempts[key] = attempts.get(key, 0) + 1
        status = 404 if mode == "unavailable" or attempts[key] == 1 else 200
        with (root / "requests").open("a") as log:
            log.write(f"{mode} {self.path} {status}\n")
        if status == 404:
            self.send_error(404)
        else:
            data = (root / "evidence" / self.path.removeprefix("/")).read_bytes()
            self.send_response(200)
            self.end_headers()
            self.wfile.write(data)

    def log_message(self, *_args):
        pass


with http.server.HTTPServer(("127.0.0.1", 0), Handler) as server:
    (root / "port").write_text(str(server.server_port))
    server.serve_forever()
PY
echo transient >"$tmp/mode"
python3 "$tmp/server.py" "$tmp" >"$tmp/server.log" 2>&1 &
server_pid=$!
start=$SECONDS
until [[ -s "$tmp/port" ]]; do
    if ! kill -0 "$server_pid" 2>/dev/null || ((SECONDS - start >= 5)); then
        cat "$tmp/server.log" >&2
        fail "local evidence server did not start"
    fi
    sleep 0.05
done
STUB_EVIDENCE_URL="http://127.0.0.1:$(cat "$tmp/port")"
export STUB_CURL="$curl_cli" STUB_EVIDENCE_URL
STUB_SERVE=certificate "$tmp/kit/deploy/verify-ingress-evidence.sh" "$domain" "$app_id" >"$tmp/out" 2>&1 ||
    { cat "$tmp/out" >&2; fail "did not recover from temporarily unavailable evidence"; }
grep -q "certificate evidence of $domain passed" "$tmp/out" || fail "did not verify recovered evidence"
for file in quote.json sha256sum.txt acme-account.json "cert-$domain.pem"; do
    [[ "$(grep -cxF "transient /$file 404" "$tmp/requests")" == 1 &&
        "$(grep -cxF "transient /$file 200" "$tmp/requests")" == 1 ]] ||
        fail "did not retry $file after a 404"
done

echo unavailable >"$tmp/mode"
result=0
STUB_SERVE=certificate "$timeout_cli" 20 "$tmp/kit/deploy/verify-ingress-evidence.sh" \
    "$domain" "$app_id" >"$tmp/out" 2>&1 || result=$?
[[ "$result" == 22 ]] || fail "permanently unavailable evidence did not fail with curl's HTTP error"
[[ "$(grep -cxF 'unavailable /quote.json 404' "$tmp/requests")" == 4 ]] ||
    fail "did not stop after the initial request and three retries"
unset STUB_EVIDENCE_URL

start=$SECONDS
! STUB_SERVE=hang "$tmp/kit/deploy/verify-ingress-evidence.sh" "$domain" "$app_id" >"$tmp/out" 2>&1 ||
    fail "passed without a TLS handshake"
((SECONDS - start < 20)) || fail "did not bound the TLS handshake"
grep -q "$domain:443 served no certificate within 30 seconds" "$tmp/out" || fail "does not say why it failed"
[[ "$(sort -u "$tmp/timeouts")" == 30 ]] || fail "did not bound the handshake by 30 seconds"
echo "ingress evidence test passed"
