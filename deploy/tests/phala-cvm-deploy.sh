#!/usr/bin/env bash
# Real deploy wrapper against a CLI stub: stdout JSON survives failure, stderr diagnostics stay
# separate, and the CLI's exit status takes precedence over parsing its response.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "$root/.cvm-deploy-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir "$tmp/deploy"
cp "$root/deploy/phala-cvm.sh" "$root/deploy/deadline.sh" "$tmp/deploy/"
cat >"$tmp/deploy/phala" <<'STUB'
#!/usr/bin/env bash
[[ "$*" == 'deploy --json --name test-cvm' ]] || exit 64
cat "$STUB_OUTPUT"
echo 'CLI stderr diagnostic' >&2
exit "$STUB_STATUS"
STUB
chmod +x "$tmp/deploy/phala"
export STUB_OUTPUT=$tmp/cli.out STUB_STATUS=0
fail() { echo "phala-cvm deploy: $*" >&2; exit 1; }
deploy() {
    rm -f "$tmp/deploy.json"
    "$tmp/deploy/phala-cvm.sh" deploy "$tmp/deploy.json" --name test-cvm >"$tmp/out" 2>"$tmp/err"
}

printf '%s\n' 'Provisioning CVM test-cvm...' \
    '{"success":true,"vm_uuid":"cvm-1","app_id":"app-1"}' >"$STUB_OUTPUT"
deploy || fail 'successful CLI deployment failed'
jq -e '.success and .vm_uuid == "cvm-1" and .app_id == "app-1"' "$tmp/deploy.json" >/dev/null
[[ ! -s "$tmp/out" ]] || fail 'log output polluted stdout'
grep -Fq 'CLI stderr diagnostic' "$tmp/err" || fail 'CLI stderr was lost'
grep -Fq 'Provisioning CVM test-cvm...' "$tmp/err" || fail 'provisioning output was lost'

printf '%s\n' '{"success":false,"error":"Creation failed","error_code":"ERR-01-004","request_id":"req-test"}' >"$STUB_OUTPUT"
for STUB_STATUS in 1 42 124; do
    status=0
    deploy || status=$?
    [[ "$status" == "$STUB_STATUS" ]] || fail "CLI status $STUB_STATUS became $status"
    [[ ! -s "$tmp/out" && ! -e "$tmp/deploy.json" ]] || fail 'failed deployment produced a success document'
    grep -Fxq "$(cat "$STUB_OUTPUT")" "$tmp/err" || fail 'CLI JSON error was lost'
    grep -Fq 'CLI stderr diagnostic' "$tmp/err" || fail 'CLI stderr was lost on failure'
done
# A zero CLI status still requires its response to say success.
STUB_STATUS=0
! deploy || fail 'a success:false response was accepted'
echo 'CVM deploy output and exit-status tests passed'
