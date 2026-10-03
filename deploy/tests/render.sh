#!/usr/bin/env bash
# deploy/render.sh: its three deploy-time inputs and their formats, digest-named configs (a changed
# file changes exactly the services that mount it), a reproducible output, and
# the environment overlay limited to its settings, and deploy/compose-policy.jq refusing an artifact
# that breaks it.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
compose=$("$root/deploy/pinned-compose.sh")
staging="$root/deploy/environments/phala-network/staging/topup"
gateway=(--gateway-domain gateway.dstack-pha-prod5.phala.network)
origin=(--restore-check --origin https://0123abcd-8081.dstack-pha-prod5.phala.network)

images() {
    jq -n --arg topup "$1" '{"phala-pay": $topup,
        "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)",
        "phala-pay-reference-product": "ghcr.io/phala-network/phala-pay-reference-product@sha256:\("3" * 64)"}'
}
topup=ghcr.io/phala-network/phala-pay@sha256:$(printf '1%.0s' {1..64})
images "$topup" >"$tmp/images.json"
render() {
    "$root/deploy/render.sh" --images "$tmp/images.json" "$@"
}
# refused NAME MESSAGE ARGS...: render.sh fails with MESSAGE on stderr.
refused() {
    local name=$1 message=$2
    shift 2
    if "$root/deploy/render.sh" "$@" >"$tmp/$name.out" 2>"$tmp/$name.err"; then
        echo "render.sh accepted $name" >&2
        exit 1
    fi
    grep -F -- "$message" "$tmp/$name.err" >/dev/null || {
        echo "render.sh refused $name for an unexpected reason:" >&2
        cat "$tmp/$name.err" >&2
        exit 1
    }
}

render "${gateway[@]}" "$staging" >"$tmp/service.yml"
render "${gateway[@]}" "$staging" | cmp -s - "$tmp/service.yml" ||
    { echo "two renders of the same inputs differ" >&2; exit 1; }
grep -F "image: $topup" "$tmp/service.yml" >/dev/null
"$compose" -f "$tmp/service.yml" config --no-interpolate --format json >"$tmp/service.json"
# Every config is inline content named after its digest, and the services mount it by that name.
jq -e '(.configs | keys | all(test("^(postgres_init|topup)_[0-9a-f]{12}$")))
    and ([.services[].configs[]?.source] | unique) == (.configs | keys)' "$tmp/service.json" >/dev/null
# Its content is the committed script with every `$` escaped for Compose, as Compose prints it.
jq -j '.configs | to_entries[] | select(.key | startswith("postgres_init_")) | .value.content' \
    "$tmp/service.json" | cmp -s - <(sed 's/[$]/&&/g' "$root/deploy/postgres-init/10-topup-role.sh") ||
    { echo "the init script is not the committed one" >&2; exit 1; }

# A changed topup.yaml changes the definition of exactly the services that mount it: the service,
# and migrate, whose cutover backfill reads the routes (docs/design/payment-settings.md §10).
cp -r "$staging" "$tmp/changed"
sed -i 's|id: admin/staging-v1|id: admin/staging-v2|' "$tmp/changed/topup.yaml"
render "${gateway[@]}" "$tmp/changed" >"$tmp/changed.yml"
changed=$({ diff <("$compose" -f "$tmp/service.yml" config --hash '*') \
    <("$compose" -f "$tmp/changed.yml" config --hash '*') || true; } | awk '/^>/ { print $2 }' |
    tr '\n' ' ')
[[ "$changed" == "migrate topup " ]] || { echo "a changed topup.yaml changed: $changed" >&2; exit 1; }

# The inputs: formats, and each only in its variant.
images phala-pay:latest >"$tmp/bare.json"
refused bare-tag "--images must map image names" --images "$tmp/bare.json" "${gateway[@]}" "$staging"
images "ghcr.io/phala-network/phala-pay@sha256:$(printf '0%.0s' {1..64})" >"$tmp/zero.json"
refused zero-digest "--images must map image names" --images "$tmp/zero.json" "${gateway[@]}" "$staging"
jq 'del(.["postgres-walg"])' "$tmp/images.json" >"$tmp/missing.json"
refused unpinned "not a release or kit-pinned image: backup postgres" \
    --images "$tmp/missing.json" "${gateway[@]}" "$staging"
refused no-gateway "needs --gateway-domain HOST" --images "$tmp/images.json" "$staging"
refused bad-gateway "needs --gateway-domain HOST" --images "$tmp/images.json" \
    --gateway-domain 'gw.example"x' "$staging"
refused origin-in-service "needs --gateway-domain HOST (and no --origin)" --images "$tmp/images.json" \
    "${gateway[@]}" --origin https://x.example.net "$staging"
refused http-origin "--restore-check needs --origin https://HOST" --images "$tmp/images.json" \
    --restore-check --origin http://x.example.net "$staging"
refused gateway-in-restore-check "--restore-check needs --origin" --images "$tmp/images.json" \
    "${origin[@]}" "${gateway[@]}" "$staging"
render "${origin[@]}" "$staging" >"$tmp/restore-check.yml"
refused product-restore-check "--restore-check and --template render a topup environment only" \
    --images "$tmp/images.json" "${origin[@]}" "$root/deploy/environments/phala-network/staging/product"

# The environment's compose.yaml sets only its documented settings: overlay NAME YAML is staging's
# topup.yaml with an overlay of YAML alone, which render.sh must refuse before anything else.
overlay() {
    mkdir "$tmp/$1"
    cp "$staging/topup.yaml" "$tmp/$1/topup.yaml"
    printf 'services:\n%s\n' "$2" >"$tmp/$1/compose.yaml"
}
settable="may set only WAL-G's location, dstack-ingress's DOMAIN, and TOPUP_RPC_<ID>_KEY names"
overlay image '  backup:
    image: ghcr.io/phala-network/postgres-walg@sha256:'"$(printf '4%.0s' {1..64})"
overlay entrypoint '  postgres:
    entrypoint: [sh, -c, "wal-g backup-push /tmp"]'
overlay command '  heartbeat:
    command: [topup, heartbeat, --interval-s, "1"]'
overlay service '  extra:
    image: busybox@sha256:'"$(printf '5%.0s' {1..64})"
overlay port '  migrate:
    ports: ["5432:5432"]'
overlay mount '  migrate:
    volumes: [/var/run/dstack.sock:/var/run/dstack.sock]'
overlay archive '  postgres:
    environment:
      TOPUP_RESTORE_FROM_BACKUP: "on"'
overlay http-store '  postgres:
    environment:
      TOPUP_OBJECT_STORE_ALLOW_HTTP: "on"'
overlay removal '  smokescreen: !reset null'
for name in image entrypoint command service port mount archive http-store removal; do
    refused "$name" "$settable" "${gateway[@]}" --images "$tmp/images.json" "$tmp/$name"
done
# A setting's value is still judged by the policy: a sealed value fills only its own key, and the
# domain must be the origin's host.
overlay sealed-domain '  dstack-ingress:
    environment:
      DOMAIN: ${AWS_SECRET_ACCESS_KEY:-}'
refused sealed-domain "a sealed value may not fill services.dstack-ingress.environment.DOMAIN" \
    --images "$tmp/images.json" "${gateway[@]}" "$tmp/sealed-domain"
overlay sealed-key '  topup:
    environment:
      TOPUP_RPC_PROVIDER_A_KEY: ${AWS_SECRET_ACCESS_KEY:-}'
refused sealed-key "a sealed value may not fill services.topup.environment.TOPUP_RPC_PROVIDER_A_KEY" \
    --images "$tmp/images.json" "${gateway[@]}" "$tmp/sealed-key"
cp -r "$staging" "$tmp/other-domain"
sed -i 's|DOMAIN: pay-api-staging.phala.com|DOMAIN: other.phala.com|' "$tmp/other-domain/compose.yaml"
refused other-domain "dstack-ingress must serve the host of topup's public_origin" \
    --images "$tmp/images.json" "${gateway[@]}" "$tmp/other-domain"
# A `$` in topup.yaml is escaped: it never becomes an interpolated reference.
cp -r "$staging" "$tmp/dollar"
sed -i 's|^environment: staging$|environment: staging # ${SENTRY_DSN:-x}|' "$tmp/dollar/topup.yaml"
render "${gateway[@]}" "$tmp/dollar" >"$tmp/dollar.yml"
grep -F 'environment: staging # $${SENTRY_DSN:-x}' "$tmp/dollar.yml" >/dev/null ||
    { echo "render.sh did not escape a \$ in the configuration" >&2; exit 1; }
cmp -s <("$compose" -f "$tmp/service.yml" config --variables | sort) \
    <("$compose" -f "$tmp/dollar.yml" config --variables | sort) ||
    { echo "a \$ in topup.yaml became an interpolated reference" >&2; exit 1; }

# The policy judges any artifact, including one render.sh did not produce (preflight,
# verify-attestation.sh): policy NAME VARIANT FILE JQ MESSAGE applies JQ to the rendered FILE and
# expects the policy to report MESSAGE.
policy() {
    jq "$4" "$3" | jq -r -L "$root/deploy" --arg variant "$2" \
        'include "compose-policy"; violations($variant; "dstack")[]' >"$tmp/$1.violations"
    grep -F -- "$5" "$tmp/$1.violations" >/dev/null || {
        echo "the policy did not refuse $1:" >&2
        cat "$tmp/$1.violations" >&2
        exit 1
    }
}
"$compose" -f "$tmp/restore-check.yml" config --no-interpolate --format json >"$tmp/restore-check.json"
service=(service "$tmp/service.json")
policy secret-command "${service[@]}" '.services.heartbeat.command += ["${SENTRY_DSN:-60}"]' \
    "a sealed value may not fill services.heartbeat.command"
policy secret-env "${service[@]}" '.services.migrate.environment.TOPUP_RPC_PROVIDER_A_KEY = "${TOPUP_RPC_PROVIDER_A_KEY:-}"' \
    "a sealed value may not fill services.migrate.environment.TOPUP_RPC_PROVIDER_A_KEY"
policy image "${service[@]}" '.services.backup.image = "postgres:18"' \
    "every image must be a nonzero repository@sha256 digest"
policy port "${service[@]}" '.services.migrate.ports = [{mode: "ingress", target: 5432, published: "5432", protocol: "tcp"}]' \
    "only dstack-ingress may publish a port, 443"
policy env-file "${service[@]}" '.services.heartbeat.env_file = [{path: "/dstack/.host-shared/.decrypted-env"}]' \
    "no service may build, read an env_file, extend, or carry a profile"
policy socket "${service[@]}" '.services.migrate.volumes += [{type: "bind", source: "/var/run/dstack.sock", target: "/var/run/dstack.sock"}]' \
    "only keys, topup, and dstack-ingress may mount the dstack socket"
policy smokescreen "${service[@]}" '.services.smokescreen.command |= .[0:4]' \
    "smokescreen must run its exact deny list from the service image"
policy extra-mounter "${service[@]}" '.services.heartbeat.volumes += [{type: "volume", source: "walg_key", target: "/run/wal-g", read_only: true}]' \
    "walg_key must be mounted by exactly backup, keys, postgres"
policy writable-mounter "${service[@]}" '(.services.heartbeat.volumes[] | select(.source == "db_app")).read_only = false' \
    "only keys may mount db_app writable"
policy on-disk "${service[@]}" 'del(.volumes.db_owner.driver_opts)' \
    "db_owner must be a tmpfs volume (uid=999,gid=999,mode=0700)"
policy no-archive "${service[@]}" '.services.postgres.environment.TOPUP_RESTORE_FROM_BACKUP = "on"' \
    "the service must archive"
policy admin-key "${service[@]}" '.services.topup.environment.TOPUP_ADMIN_PUBLIC_KEY = "${TOPUP_ADMIN_PUBLIC_KEY:-}"' \
    "a sealed value may not fill services.topup.environment.TOPUP_ADMIN_PUBLIC_KEY"
# Stateless services must carry finite budgets and cannot regain privileges.
for value in 0 -1 null; do
    policy "memory-$value" "${service[@]}" ".services.topup.mem_limit = $value" \
        "topup must set a positive finite memory limit"
done
for value in 0 -1 null; do
    policy "pids-$value" "${service[@]}" ".services.topup.pids_limit = $value" \
        "topup must set a positive pids limit"
done
policy memory-absent "${service[@]}" 'del(.services.topup.mem_limit)' \
    "topup must set a positive finite memory limit"
policy pids-absent "${service[@]}" 'del(.services.topup.pids_limit)' \
    "topup must set a positive pids limit"
policy privileged "${service[@]}" '.services.topup.privileged = true' \
    "topup must not be privileged"
policy cap-add "${service[@]}" '.services.topup.cap_add = ["NET_ADMIN"]' \
    "topup must not add capabilities"
policy security-opt "${service[@]}" '.services.topup.security_opt += ["seccomp:unconfined"]' \
    "topup must use only no-new-privileges"

# The restore-check variant never reads the live storage credentials.
policy live-credentials restore-check "$tmp/restore-check.json" \
    '.services.postgres.environment.AWS_ACCESS_KEY_ID = "${AWS_ACCESS_KEY_ID:-}"' \
    "postgres must read the restore instance's own storage credentials"

# The template variant: the service without dstack-ingress, topup on port 80 for the gateway, and
# only the deploy form's values at runtime (deploy/compose.template.yaml).
template="$root/deploy/environments/phala-cloud-template/topup"
render --template "$template" >"$tmp/template.yml"
[[ "$("$compose" -f "$tmp/template.yml" config --variables | awk 'NR > 1 { print $1 }' | sort |
    tr '\n' ' ')" == "AWS_ACCESS_KEY_ID AWS_ENDPOINT AWS_REGION AWS_SECRET_ACCESS_KEY DSTACK_APP_DOMAIN SENTRY_DSN TOPUP_ADMIN_PUBLIC_KEY WALG_S3_PREFIX " ]] ||
    { echo "the template's runtime names changed" >&2; exit 1; }
refused template-gateway "--template serves the app's gateway domain" --images "$tmp/images.json" \
    --template "${gateway[@]}" "$template"
refused template-restore-check "usage:" --images "$tmp/images.json" --template "${origin[@]}" "$template"
refused template-as-service "a sealed value may not fill services.postgres.environment.WALG_S3_PREFIX" \
    --images "$tmp/images.json" "${gateway[@]}" "$template"
# A runtime value fills only the form's settings, in their own keys: never topup.yaml, whose `$`
# are escaped, another service's environment, or another key; and topup publishes only port 80.
cp -r "$template" "$tmp/template-key-id"
sed -i 's|id: admin/v1|id: ${TOPUP_ADMIN_PUBLIC_KEY:-}|' "$tmp/template-key-id/topup.yaml"
render --template "$tmp/template-key-id" >"$tmp/template-key-id.yml"
cmp -s <("$compose" -f "$tmp/template.yml" config --variables | sort) \
    <("$compose" -f "$tmp/template-key-id.yml" config --variables | sort) ||
    { echo "a \$ in the template's topup.yaml became a runtime reference" >&2; exit 1; }
"$compose" -f "$tmp/template.yml" config --no-interpolate --format json >"$tmp/template.json"
policy template-extra template "$tmp/template.json" \
    '.services.heartbeat.environment.TOPUP_ADMIN_PUBLIC_KEY = "${TOPUP_ADMIN_PUBLIC_KEY:-}"' \
    "a sealed value may not fill services.heartbeat.environment.TOPUP_ADMIN_PUBLIC_KEY"
policy template-port template "$tmp/template.json" \
    '.services.migrate.ports = [{mode: "ingress", target: 5432, published: "5432", protocol: "tcp"}]' \
    "only topup may publish a port, 80"

# allowed_envs, the names the CLI sends: any subset of the sealed names (one left out is unset),
# and never another name, nor the template's DSTACK_APP_DOMAIN, which the pre-launch script sets.
# allowed_envs NAME VARIANT FILE ALLOWED_JSON [MESSAGE]: no violation, or MESSAGE.
allowed_envs() {
    jq -r -L "$root/deploy" --arg variant "$2" --argjson allowed "$4" \
        'include "compose-policy"; allowed_envs_violations($variant; $allowed)[]' "$3" >"$tmp/$1.violations"
    if (($# == 4)); then
        [[ ! -s "$tmp/$1.violations" ]] || { echo "allowed_envs refused $1:" >&2; cat "$tmp/$1.violations" >&2; exit 1; }
    else
        grep -qxF -- "$5" "$tmp/$1.violations" || { echo "allowed_envs did not refuse $1" >&2; exit 1; }
    fi
}
service_names=$("$compose" -f "$tmp/service.yml" config --variables | awk 'NR > 1 { print $1 }' | jq -R . | jq -sc 'sort')
[[ "$(jq -c -L "$root/deploy" 'include "compose-policy"; sealed_names' "$tmp/service.json")" == "$service_names" ]] ||
    { echo "the policy's sealed names are not Compose's" >&2; exit 1; }
allowed_envs service-all service "$tmp/service.json" "$service_names"
allowed_envs service-subset service "$tmp/service.json" '["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]'
allowed_envs service-none service "$tmp/service.json" '[]'
allowed_envs service-extra service "$tmp/service.json" '["AWS_ACCESS_KEY_ID", "DSTACK_APP_DOMAIN"]' \
    "the env may set only the compose's sealed names, not DSTACK_APP_DOMAIN"
allowed_envs template-subset template "$tmp/template.json" \
    '["AWS_ACCESS_KEY_ID", "AWS_ENDPOINT", "AWS_REGION", "AWS_SECRET_ACCESS_KEY", "TOPUP_ADMIN_PUBLIC_KEY", "WALG_S3_PREFIX"]'
allowed_envs template-app-domain template "$tmp/template.json" '["DSTACK_APP_DOMAIN"]' \
    "the env may set only the compose's sealed names, not DSTACK_APP_DOMAIN"
allowed_envs not-a-list service "$tmp/service.json" 'null' "allowed_envs must be a list of names"

# The product:its one sealed name, and its public_url on its domain.
product="$root/deploy/environments/phala-network/staging/product"
render "${gateway[@]}" "$product" >"$tmp/product.yml"
[[ "$("$compose" -f "$tmp/product.yml" config --variables | awk 'NR > 1 { print $1 }')" == PRODUCT_API_KEY ]]
cp -r "$product" "$tmp/product-url"
jq '.public_url = "https://other.phala.com"' "$product/config.json" >"$tmp/product-url/config.json"
refused product-url "dstack-ingress must serve the host of the product's public_url" \
    --images "$tmp/images.json" "${gateway[@]}" "$tmp/product-url"

if grep -rqF 'staging-v2' "$tmp"/*.err; then
    echo "render.sh printed a configuration value" >&2
    exit 1
fi
echo "compose renderer test passed"
