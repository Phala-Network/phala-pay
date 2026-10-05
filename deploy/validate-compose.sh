#!/usr/bin/env bash
# Checks every committed environment's attested compose, rendered as Deploy renders it
# (deploy/render.sh, which applies deploy/compose-policy.jq), and the local, drill, sandbox, and
# rehearsal overlays that run on top of it. CI runs it on every pull request.
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/topup-validate.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
compose=$("$root/deploy/pinned-compose.sh")
fail() {
    echo "validate-compose: $*" >&2
    exit 1
}

jq -n '{"phala-pay": "ghcr.io/phala-network/phala-pay@sha256:\("1" * 64)",
    "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)",
    "phala-pay-reference-product": "ghcr.io/phala-network/phala-pay-reference-product@sha256:\("3" * 64)"}' \
    >"$tmp/images.json"
# render NAME ENV_DIR [--restore-check | --template]: the compose as Deploy (or the release, for
# the template) renders it, and its JSON.
render() {
    local name=$1 env_dir=$2 inputs=(--gateway-domain gateway.dstack-pha-prod5.phala.network)
    case "${3:-}" in
        --restore-check) inputs=(--restore-check --origin https://0123abcd-8081.dstack-pha-prod5.phala.network) ;;
        --template) inputs=(--template) ;;
    esac
    "$root/deploy/render.sh" "${inputs[@]}" --images "$tmp/images.json" "$env_dir" >"$tmp/$name.yml" ||
        fail "$env_dir does not render"
    "$compose" -f "$tmp/$name.yml" config --no-interpolate --format json >"$tmp/$name.json"
}
sealed() {
    "$compose" -f "$tmp/$1.yml" config --variables | awk 'NR > 1 && NF > 0 { print $1 }' | sort |
        tr '\n' ' '
}

staging="$root/deploy/environments/phala-network/staging"
render service "$staging/topup"
render restore-check "$staging/topup" --restore-check
render example "$root/deploy/environments/example/topup"
render example-restore-check "$root/deploy/environments/example/topup" --restore-check
render product "$staging/product"
render template "$root/deploy/environments/phala-cloud-template/topup" --template

# Staging's sealed names are the ones sealed in its CVM: a change needs a re-seal
# (deploy/README.md, "Sealing the secrets") before the upgrade that makes it.
[[ "$(sealed service)" == "AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY SENTRY_DSN " ]] ||
    fail "staging's sealed names changed: $(sealed service)"
[[ "$(sealed restore-check)" == "RESTORE_AWS_ACCESS_KEY_ID RESTORE_AWS_SECRET_ACCESS_KEY SENTRY_DSN " ]] ||
    fail "staging's restore-check sealed names changed: $(sealed restore-check)"
[[ "$(sealed product)" == "PRODUCT_API_KEY " ]] || fail "the product's sealed names changed"

# The restore-check variant is the service with the service-only services removed, topup read-only
# on 8081, and PostgreSQL restoring with the restore instance's own credentials: nothing else.
jq -e --slurpfile service "$tmp/service.json" '
    def normal: del(.services["dstack-ingress", "smokescreen", "heartbeat", "backup", "capacity", "restore-check"])
        | .configs |= with_entries(select(.key | startswith("capacity_probe_") | not))
        | del(.services.topup.command, .services.topup.ports)
        | del(.services.postgres.environment.TOPUP_RESTORE_FROM_BACKUP,
            .services.postgres.environment.AWS_ACCESS_KEY_ID,
            .services.postgres.environment.AWS_SECRET_ACCESS_KEY)
        | del(.volumes.ingress_certs, .volumes.ingress_evidences);
    normal == ($service[0] | normal)' "$tmp/restore-check.json" >/dev/null ||
    fail "the restore-check variant differs from the service in more than its declared changes"

# The template is the service without dstack-ingress, topup published on 80, and the deploy form's
# values at runtime: topup's origin and admin key from its environment, its own topup.yaml
# (staging's routes and providers), the backup location, and no keyed provider.
jq -e --slurpfile service "$tmp/service.json" '
    def normal: del(.services["dstack-ingress", "restore-check"], .services.topup.ports)
        | .services.topup.command |= .[0:6]
        | del(.services.postgres.environment["WALG_S3_PREFIX", "AWS_ENDPOINT", "AWS_REGION"],
            .services.backup.environment["WALG_S3_PREFIX", "AWS_ENDPOINT", "AWS_REGION"],
            .services.topup.environment["DSTACK_APP_DOMAIN", "TOPUP_ADMIN_PUBLIC_KEY"])
        | .services.topup.environment |= with_entries(select(.key | startswith("TOPUP_RPC_") | not))
        | .configs |= with_entries(select(.key | startswith("topup_") | not))
        | .services[].configs[]? |= (if .source | startswith("topup_") then .source = "topup" else . end)
        | del(.volumes.ingress_certs, .volumes.ingress_evidences);
    normal == ($service[0] | normal)' "$tmp/template.json" >/dev/null ||
    fail "the template variant differs from the service in more than its declared changes"
jq -j '.configs | to_entries[] | select(.key | startswith("topup_")) | .value.content' \
    "$tmp/template.json" | sed -n '/^routes:$/,$p' |
    cmp -s - <(sed -n '/^routes:$/,$p' "$staging/topup/topup.yaml") ||
    fail "the template's routes must be staging's"

# The reference product calls staging's topup and pins its keys there; its demo API allows only
# the website's origin. Its chains are Sepolia and Base Sepolia, each with a committed keyless https
# RPC; Base Sepolia's treasury is never 0x936c…4504, whose Base Sepolia copy has a destroyed owner.
origin=$(jq -r '.configs | to_entries[] | select(.key | startswith("topup_")) | .value.content' \
    "$tmp/service.json" | sed -n 's/^public_origin:[[:space:]]*//p')
jq -e --arg origin "$origin" '.configs | to_entries[] | select(.key | startswith("product_"))
    | .value.content | fromjson
    | .service_url == $origin
    and ([.chains[].chain_id] == [11155111, 84532])
    and all(.chains[]; .rpc_url | startswith("https://"))
    and all(.chains[] | select(.chain_id == 84532);
        .treasury | ascii_downcase != "0x936c1991f8da9a919fa11b557a3514719f5a4504")
    and .web_origin == "https://pay.phala.com"' "$tmp/product.json" >/dev/null ||
    fail "the product config must call staging's origin, carry its two chains, and allow only the website"

# The local stacks are the rendered compose plus overlays (deploy/local/compose.sh). The drill and
# the rehearsal run on CI's runner, whose Docker daemon cannot see the checkout: no bind mount, and
# no host port but the rehearsal's Anvils.
local_stack() {
    "$root/deploy/local/compose.sh" "$@" config --format json
}
local_stack -p validate-local >"$tmp/local.json"
jq -e '[.services[].volumes[]? | select(.source == "/var/run/dstack.sock")] == []' "$tmp/local.json" \
    >/dev/null || fail "the local overlay must replace the host dstack socket with the simulator's"
local_stack -p validate-sandbox -f "$root/deploy/sandbox/docker-compose.local.yml" >/dev/null
local_stack --restore-check -p validate-drill -f "$root/deploy/local/restore-drill.compose.yml" \
    >"$tmp/drill.json"
jq -e '[.services[].volumes[]? | select(.type == "bind")] == []' "$tmp/drill.json" >/dev/null ||
    fail "the restore-drill stack bind-mounts a host path; CI's Docker daemon cannot see it"
jq -e '[.services[].ports[]?] == []' "$tmp/drill.json" >/dev/null ||
    fail "the restore-drill stack must not publish host ports"
TOPUP_LOCAL_DSTACK_IMAGE=validate "$compose" -p validate-rehearsal \
    --project-directory "$root/deploy/local" -f "$tmp/service.yml" \
    -f "$root/deploy/local/cvm-rehearsal.compose.yml" config --format json >"$tmp/rehearsal.json"
# Compare both through the runtime loader: it normalizes memory units and omits false defaults.
"$compose" -f "$tmp/service.yml" config --format json >"$tmp/service-runtime.json"
jq -e --slurpfile service "$tmp/service-runtime.json" '
    [.services.topup, .services.heartbeat, .services.smokescreen
     | {read_only, user, tmpfs, cap_drop, security_opt, mem_limit, pids_limit, configs}]
    == ([$service[0].services.topup, $service[0].services.heartbeat, $service[0].services.smokescreen
         | {read_only, user, tmpfs, cap_drop, security_opt, mem_limit, pids_limit, configs}])' \
    "$tmp/rehearsal.json" >/dev/null || fail "the CVM rehearsal must preserve release hardening and configs"
jq -e '[.services[].volumes[]? | select(.type == "bind")] == []' "$tmp/rehearsal.json" >/dev/null ||
    fail "the CVM rehearsal stack bind-mounts a host path"
jq -e '[.services | to_entries[] | select((.value.ports // []) | length > 0) | .key] | sort
    == ["anvil", "anvil-base-mainnet-price", "anvil-base-sepolia", "anvil-mainnet-price"]' "$tmp/rehearsal.json" >/dev/null ||
    fail "only the Anvils may publish a port in the CVM rehearsal stack"

echo "every committed environment renders and passes deploy/compose-policy.jq; the local overlays apply"
