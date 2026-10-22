#!/usr/bin/env bash
# Exercise the actual workflow step with API, version and policy fixtures.
set -euo pipefail
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/deploy" "$tmp/scripts" "$tmp/runner"
awk '
    /name: Resolve and verify the previous stable release/ { step=1; next }
    step && /run: \|/ { body=1; next }
    body && /^          / { sub(/^          /, ""); print; next }
    body { exit }
' "$root/.github/workflows/deploy-rollback.yml" >"$tmp/step.sh"
ln -s "$root/deploy/deadline.sh" "$tmp/deploy/deadline.sh"
ln -s "$root/deploy/github-read.sh" "$tmp/deploy/github-read.sh"
cat >"$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
printf 'HTTP/2 200 OK\r\n\r\n%s\n' "$STUB_RELEASES"
STUB
cat >"$tmp/scripts/version.sh" <<'STUB'
#!/usr/bin/env bash
echo 9.9.10
exit "${STUB_VERSION_STATUS:-0}"
STUB
cat >"$tmp/deploy/rollback-policy.sh" <<'STUB'
#!/usr/bin/env bash
echo "$STUB_MODE"
exit "${STUB_POLICY_STATUS:-0}"
STUB
cat >"$tmp/deploy/verify-release.sh" <<'STUB'
#!/usr/bin/env bash
echo reached-verifier >&2
exit 1
STUB
chmod +x "$tmp/bin/gh" "$tmp/scripts/version.sh" "$tmp/deploy/rollback-policy.sh" "$tmp/deploy/verify-release.sh"
export PATH="$tmp/bin:$PATH" RUNNER_TEMP="$tmp/runner" GITHUB_ENV="$tmp/env" \
    GITHUB_STEP_SUMMARY="$tmp/summary" GITHUB_REPOSITORY=example/repo GITHUB_REF_NAME=current \
    STUB_RELEASES='[{"draft":false,"prerelease":false,"tag_name":"v9.9.9"}]' STUB_MODE=declared
run() { (cd "$tmp" && bash -e step.sh) >"$tmp/out" 2>"$tmp/err"; }
fail() { echo "rollback-release: $*" >&2; exit 1; }
run || { cat "$tmp/err" >&2; fail 'refused a valid declared release'; }
grep -q 'ROLLBACK_MODE=declared' "$tmp/env" || fail 'did not export the policy mode'
for releases in '[]' '[{"draft":false,"prerelease":false,"tag_name":null}]' \
    '[{"draft":false,"prerelease":false,"tag_name":""}]' 'invalid'; do
    ! STUB_RELEASES=$releases run || fail "accepted $releases"
    grep -q 'no previous stable tag or invalid release data' "$tmp/err" || fail 'missing lookup diagnostic'
done
! STUB_RELEASES='[{"draft":false,"prerelease":false,"tag_name":"sdk-js-v9.9.9"}]' run || fail 'accepted a non-release tag'
grep -q 'Invalid previous stable release tag' "$tmp/err" || fail 'invalid tag has no diagnostic'
! STUB_VERSION_STATUS=1 run || fail 'ignored a failed version substitution'
grep -q 'Cannot determine the current version' "$tmp/err" || fail 'version failure has no diagnostic'
! STUB_POLICY_STATUS=64 run || fail 'ignored a policy failure'
grep -q 'Rollback policy failed' "$tmp/err" || fail 'policy failure has no diagnostic'
! STUB_MODE=invalid run || fail 'accepted an unknown policy mode'
grep -q 'Invalid rollback policy mode' "$tmp/err" || fail 'unknown mode has no diagnostic'
! STUB_MODE=rollback run || fail 'ignored a verifier failure'
grep -q 'reached-verifier' "$tmp/err" || fail 'did not invoke verifier in rollback mode'
grep -q 'Rollback release resolution failed' "$tmp/err" || fail 'verifier failure has no step diagnostic'
echo 'Rollback release lookup and policy diagnostics passed'
