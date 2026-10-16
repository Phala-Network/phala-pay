#!/usr/bin/env bash
# The online checks of deploy/preflight-phala.sh against stub `docker` and Phala CLI commands: a
# transient pull failure is retried, a refusal names the package as private, and every failure
# message carries the tool's own error.
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

mkdir "$tmp/bin"
# docker pull IMAGE: "flaky" fails once, "private" is refused, "down" never reaches the registry.
cat >"$tmp/bin/docker" <<'STUB'
#!/usr/bin/env bash
[[ "$1" == pull ]] || exit 1
image=${*: -1}
echo "$image" >>"$STUB_LOG"
case "$image" in
    flaky) (($(grep -cx flaky "$STUB_LOG") > 1)) && exit 0
        echo "Error response from daemon: Get \"https://ghcr.io/v2/\": net/http: TLS handshake timeout" >&2 ;;
    private) echo "Error response from daemon: denied" >&2 ;;
    down) echo "Error response from daemon: Get \"https://ghcr.io/v2/\": dial tcp: i/o timeout" >&2 ;;
esac
exit 1
STUB
printf '#!/bin/sh\necho "request failed: ECONNRESET" >&2\nexit 1\n' >"$tmp/bin/phala"
chmod +x "$tmp/bin/docker" "$tmp/bin/phala"
printf '%s\n' flaky private down >"$tmp/images"

export PATH="$tmp/bin:$PATH" STUB_LOG="$tmp/pulls" PHALA="$tmp/bin/phala" DOCKER_HOST=unix:///stub
(
    fail() { printf 'FAIL: %s\n' "$*"; }
    ok() { printf 'ok: %s\n' "$*"; }
    sleep() { :; }
    source "$root/deploy/preflight-phala.sh"
    check_anonymous_pulls "$tmp/images"
    check_phala_cloud workspace dstack-0.6.1
) >"$tmp/out"

expect() {
    grep -qxF -- "$1" "$tmp/out" || {
        printf 'missing: %s\noutput:\n' "$1" >&2
        cat "$tmp/out" >&2
        exit 1
    }
}
expect "ok: flaky pulls anonymously"
expect "FAIL: private cannot be pulled anonymously; make the package public: Error response from daemon: denied"
expect "FAIL: down did not pull in 3 attempts: Error response from daemon: Get \"https://ghcr.io/v2/\": dial tcp: i/o timeout"
[[ "$(sort "$tmp/pulls" | uniq -c | awk '{ print $2 "=" $1 }' | tr '\n' ' ')" == "down=3 flaky=2 private=3 " ]] || {
    echo "unexpected pull attempts:" >&2
    sort "$tmp/pulls" | uniq -c >&2
    exit 1
}
expect "FAIL: the Phala CLI is error: request failed: ECONNRESET; these steps are verified against 1.1.22"
expect "FAIL: the CLI is not logged in to workspace 'workspace': request failed: ECONNRESET"
expect "FAIL: 'os-images --prod' failed: request failed: ECONNRESET"
expect "FAIL: 'api /teepods/available' failed: request failed: ECONNRESET"

# The catalogue uses a version string, while available nodes use a version tuple.
cat >"$tmp/bin/phala" <<'STUB'
#!/usr/bin/env bash
case "$1" in
    --version) echo v1.1.22 ;;
    status) echo '{"team_name":"workspace"}' ;;
    os-images) jq -nc --arg version "$STUB_VERSION" --argjson dev "$STUB_DEV" \
        '{items: [{name: "dstack-0.6.1", version: $version, is_dev: $dev}]}' ;;
    api) jq -nc --argjson version "$STUB_NODE_VERSION" --argjson dev "$STUB_DEV" \
        '{nodes: [{images: [{name: "dstack-0.6.1", version: $version, is_dev: $dev}]}]}' ;;
    *) exit 64 ;;
esac
STUB
check_listing() {
    (
        export STUB_VERSION=$1 STUB_NODE_VERSION=$2 STUB_DEV=$3
        fail() { printf 'FAIL: %s\n' "$*"; }
        ok() { printf 'ok: %s\n' "$*"; }
        source "$root/deploy/preflight-phala.sh"
        check_phala_cloud workspace dstack-0.6.1
    ) >"$tmp/out"
}
check_listing 0.6.1 '[0,6,1]' false
expect 'ok: OS image dstack-0.6.1 is a production (non-dev) dstack 0.6.1 image'
expect 'ok: 1 node(s) of the workspace offer OS image dstack-0.6.1'
for rejected in '0.5.9 [0,5,9] false' '0.5.12 [0,5,12] false' '0.6.1 [0,6,1] true'; do
    read -r version node_version dev <<<"$rejected"
    check_listing "$version" "$node_version" "$dev"
    expect "FAIL: OS image dstack-0.6.1 is not listed as a production dstack 0.6.1 image by 'os-images --prod'"
    grep -qF 'FAIL: no node of the workspace offers OS image dstack-0.6.1' "$tmp/out"
done

echo "preflight online checks test passed"
