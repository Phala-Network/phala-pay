#!/usr/bin/env bash
# Local (offline) preflight checks against Phala's staging environment rendered as Deploy renders
# it: a complete env file passes, and every other case is refused with its reason. The cases are
# an env file with other names, a placeholder, or empty secrets; a stale render; the wrong variant;
# the example environment's values; RPC keys that do not fit their URLs; a URL with an embedded
# key; a configuration topup refuses; another OS image; and a malformed Sentry DSN. No key or DSN
# may be printed. TOPUP is the topup binary preflight runs its config checks with (cargo build).
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
preflight="$root/deploy/preflight.sh"
: "${TOPUP:?set TOPUP to a topup binary, for example target/debug/topup}"
export TOPUP
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

jq -n '{"phala-pay": "ghcr.io/phala-network/phala-pay@sha256:\("1" * 64)",
    "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)"}' >"$tmp/images.json"
staging="$root/deploy/environments/phala-network/staging/topup"
gateway=gateway.dstack-pha-prod5.phala.network
# render NAME ENV_DIR [--restore-check]
render() {
    local inputs=(--gateway-domain "$gateway")
    [[ "${3:-}" != --restore-check ]] || inputs=(--restore-check --origin https://0123-8081.example.net)
    "$root/deploy/render.sh" "${inputs[@]}" --images "$tmp/images.json" "$2" >"$tmp/$1.yml"
}
# env_file NAME COMPOSE [NAME=VALUE...]: the compose's sealed names, the storage ones filled.
env_file() {
    local file=$tmp/$1.env name
    "$("$root/deploy/pinned-compose.sh")" -f "$2" config --variables |
        awk 'NR > 1 && NF > 0 { print $1 }' | sort | while read -r name; do
        case "$name" in
            *AWS_ACCESS_KEY_ID | *AWS_SECRET_ACCESS_KEY | TOPUP_RPC_ANKR_KEY | TOPUP_RPC_INFURA_KEY) echo "$name=staging-value" ;;
            *) echo "$name=" ;;
        esac
    done >"$file"
    shift 2
    for assignment in "$@"; do
        sed -i "s|^${assignment%%=*}=.*|$assignment|" "$file"
    done
}
# expect_failure NAME EXPECTED_MESSAGE ARGS...: preflight must fail and print the message.
expect_failure() {
    local name=$1 message=$2
    shift 2
    if "$preflight" "$@" --offline >"$tmp/$name.out" 2>"$tmp/$name.err"; then
        echo "preflight accepted $name" >&2
        exit 1
    fi
    grep -F -- "$message" "$tmp/$name.err" >/dev/null || {
        echo "preflight rejected $name for an unexpected reason:" >&2
        cat "$tmp/$name.err" >&2
        exit 1
    }
}
passes() {
    "$preflight" "$@" --offline >"$tmp/pass.out" 2>"$tmp/pass.err" || {
        echo "preflight refused $*:" >&2
        cat "$tmp/pass.err" >&2
        exit 1
    }
    grep -Fq 'ok: configuration `/dev/stdin` is valid' "$tmp/pass.out" || {
        echo "preflight passed without checking the configuration:" >&2
        cat "$tmp/pass.out" >&2
        exit 1
    }
}

render service "$staging"
env_file complete "$tmp/service.yml"
passes --env "$tmp/complete.env" --compose "$tmp/service.yml" --environment-dir "$staging"

# The env file names only the compose's sealed names, with no placeholder; an optional one left
# out is unset, a required one is refused.
{ cat "$tmp/complete.env"; echo "EXTRA_SECRET=x"; } >"$tmp/extra.env"
expect_failure extra-name "the env may set only the compose's sealed names, not EXTRA_SECRET" \
    --env "$tmp/extra.env" --compose "$tmp/service.yml" --environment-dir "$staging"
grep -v '^SENTRY_DSN=' "$tmp/complete.env" >"$tmp/subset.env"
passes --env "$tmp/subset.env" --compose "$tmp/service.yml" --environment-dir "$staging"
grep -v '^AWS_ACCESS_KEY_ID=' "$tmp/complete.env" >"$tmp/no-storage.env"
expect_failure no-storage "AWS_ACCESS_KEY_ID is empty or not set" \
    --env "$tmp/no-storage.env" --compose "$tmp/service.yml" --environment-dir "$staging"
sed 's/^AWS_ACCESS_KEY_ID=.*/AWS_ACCESS_KEY_ID=replace-me/' "$tmp/complete.env" >"$tmp/example.env"
expect_failure replace-me "AWS_ACCESS_KEY_ID still contains replace-me" \
    --env "$tmp/example.env" --compose "$tmp/service.yml" --environment-dir "$staging"

# Deploy's unsealed env file (every secret empty) passes only with --unsealed.
env_file unsealed "$tmp/service.yml" AWS_ACCESS_KEY_ID= AWS_SECRET_ACCESS_KEY=
expect_failure unsealed "AWS_ACCESS_KEY_ID is empty" \
    --env "$tmp/unsealed.env" --compose "$tmp/service.yml" --environment-dir "$staging"
passes --env "$tmp/unsealed.env" --compose "$tmp/service.yml" --environment-dir "$staging" --unsealed

# A stale or hand-edited render is refused; so is a policy violation it carries.
sed 's|s3://crypto-topup-test/staging-v030|s3://other/postgres|' "$tmp/service.yml" >"$tmp/edited.yml"
expect_failure edited "differs from a fresh render" \
    --env "$tmp/complete.env" --compose "$tmp/edited.yml" --environment-dir "$staging"
sed 's|DOMAIN: pay-api-staging.phala.com|DOMAIN: other.phala.com|' "$tmp/service.yml" >"$tmp/other-domain.yml"
expect_failure other-domain "dstack-ingress must serve the host of topup's public_origin" \
    --env "$tmp/complete.env" --compose "$tmp/other-domain.yml" --environment-dir "$staging"

# Each variant passes only as itself.
render restore-check "$staging" --restore-check
env_file restore-check "$tmp/restore-check.yml"
passes --env "$tmp/restore-check.env" --compose "$tmp/restore-check.yml" --environment-dir "$staging" \
    --restore-check
expect_failure restore-check-as-service "the service runs exactly" \
    --env "$tmp/restore-check.env" --compose "$tmp/restore-check.yml" --environment-dir "$staging"
expect_failure service-as-restore-check "restore-check runs exactly" \
    --env "$tmp/complete.env" --compose "$tmp/service.yml" --environment-dir "$staging" --restore-check

# The example environment's placeholders are refused.
example="$root/deploy/environments/example/topup"
render example "$example"
env_file example-values "$tmp/example.yml" TOPUP_RPC_ANKR_KEY=sealed-key-0123456789
expect_failure example "still holds values of deploy/environments/example" \
    --env "$tmp/example-values.env" --compose "$tmp/example.yml" --environment-dir "$example"

# edited_environment NAME SED_SCRIPT [KEY_NAME]: staging's environment with its topup.yaml edited,
# its overlay declaring the sealed provider key KEY_NAME to topup, as a keyed provider's does.
edited_environment() {
    cp -r "$staging" "$tmp/$1"
    sed -i "$2" "$tmp/$1/topup.yaml"
    if [[ -n "${3:-}" ]]; then
        printf '  topup:\n    environment:\n      %s: ${%s:-}\n' "$3" "$3" >>"$tmp/$1/compose.yaml"
    fi
    render "$1" "$tmp/$1"
}
# Both declared keys must arrive through the candidate env and compose mapping.
for key in TOPUP_RPC_ANKR_KEY TOPUP_RPC_INFURA_KEY; do
    grep -v "^$key=" "$tmp/complete.env" >"$tmp/missing-$key.env"
    expect_failure "missing-$key" "$key" \
        --env "$tmp/missing-$key.env" --compose "$tmp/service.yml" --environment-dir "$staging"
done
passes --env "$tmp/unsealed.env" --compose "$tmp/service.yml" --environment-dir "$staging" --unsealed
edited_environment host-key 's|https://rpc.ankr.com/eth_sepolia/{key}|https://{key}.example.net/rpc|'
expect_failure host-key "may have {key} only as a whole path segment or a whole query value" \
    --env "$tmp/complete.env" --compose "$tmp/host-key.yml" --environment-dir "$tmp/host-key"
edited_environment same-host 's|https://sepolia.infura.io/v3/{key}|https://rpc.ankr.com/verify/{key}|'
expect_failure same-host "read and verify endpoints must have different hosts" \
    --env "$tmp/complete.env" --compose "$tmp/same-host.yml" --environment-dir "$tmp/same-host"
edited_environment zero-route 's|forwarder_factory: "0x[0-9a-fA-F]*"|forwarder_factory: "0x0000000000000000000000000000000000000000"|'
expect_failure zero-route "must not be the zero address" \
    --env "$tmp/complete.env" --compose "$tmp/zero-route.yml" --environment-dir "$tmp/zero-route"
if grep -rqF 'sealed-key-0123456789' "$tmp"/*.out "$tmp"/*.err; then
    echo "preflight printed an RPC key" >&2; exit 1
fi

# Only the approved production image passes.
for image in dstack-0.6.0-rc5 dstack-dev-0.5.9 dstack-nvidia-0.5.9 dstack-0.5.8; do
    expect_failure "image-$image" "OS image $image is not the approved dstack-0.5.9" \
        --env "$tmp/complete.env" --compose "$tmp/service.yml" --environment-dir "$staging" \
        --os-image "$image"
done

# A sealed Sentry DSN is accepted; a malformed one is refused without printing it.
env_file sentry "$tmp/service.yml" \
    SENTRY_DSN=https://0123456789abcdef0123456789abcdef@o1.ingest.us.sentry.io/2
passes --env "$tmp/sentry.env" --compose "$tmp/service.yml" --environment-dir "$staging"
env_file bad-sentry "$tmp/service.yml" SENTRY_DSN=http://sentry-secret@example
expect_failure bad-sentry "SENTRY_DSN must be empty or the project's DSN" \
    --env "$tmp/bad-sentry.env" --compose "$tmp/service.yml" --environment-dir "$staging"
if grep -q sentry-secret "$tmp/bad-sentry.out" "$tmp/bad-sentry.err"; then
    echo "preflight printed the SENTRY_DSN value" >&2
    exit 1
fi

# Without TOPUP, the configuration is checked in the pinned image, which offline never pulls.
TOPUP='' expect_failure absent-image "the pinned image is not present locally; docker pull" \
    --env "$tmp/complete.env" --compose "$tmp/service.yml" --environment-dir "$staging"

# Exercise the same online RPC failure path, without pulling images or reaching live systems.
source "$root/deploy/contracts/common.sh"
source "$root/deploy/preflight-phala.sh"
source "$root/deploy/preflight-rpc.sh"
(
    topup() { printf '%s\n' '{"level":"ERROR","message":"endpoint infura-sepolia finalized head: RPC transport failure"}' >&2; return 1; }
    ok() { :; }
    fail() { printf 'FAIL: %s\n' "$*" >&2; }
    redact() { printf '%s' "$1"; }
    check_rpc_endpoints "$tmp"
) >"$tmp/rpc.out" 2>"$tmp/rpc.err"
grep -Fq 'FAIL: RPC endpoint preflight failed:' "$tmp/rpc.err"
grep -Fq 'infura-sepolia finalized head: RPC transport failure' "$tmp/rpc.err"
jq -e '. == []' "$tmp/healthy.json" >/dev/null
[[ ! -s "$tmp/rpc.out" ]]
echo "preflight local checks and RPC diagnostics tests passed"

# Production must validate the real CLI's DSN requirement, including the unsealed deploy path.
expect_failure production-missing-sentry "topup config check refused" \
    --env "$tmp/complete.env" --compose "$tmp/service.yml" --environment-dir "$staging" --require-sentry
valid_dsn=https://0123456789abcdef0123456789abcdef@o123.ingest.sentry.io/456
sed "s|^SENTRY_DSN=.*|SENTRY_DSN=$valid_dsn|" "$tmp/complete.env" >"$tmp/sentry.env"
passes --env "$tmp/sentry.env" --compose "$tmp/service.yml" --environment-dir "$staging" --require-sentry --unsealed
if grep -Fq "$valid_dsn" "$tmp/pass.out" "$tmp/pass.err"; then
    echo "preflight printed the Sentry DSN" >&2; exit 1
fi
echo 'production Sentry requirement passed'
