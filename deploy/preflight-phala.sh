#!/usr/bin/env bash
# shellcheck disable=SC2154  # tmp, like fail and ok, comes from the sourcing script
# Preflight checks shared by deploy/preflight.sh and deploy/product/preflight.sh; sourced.
# The caller defines fail, ok, and tmp (a private directory), and sources contracts/common.sh.

source "$(dirname -- "${BASH_SOURCE[0]}")/deadline.sh"

# tool_error FILE: the last lines of a tool's stderr, on one line, for a failure message.
tool_error() {
    grep -v '^stage=' "$1" | tail -n 3 | paste -sd ' ' -
}

# embeds_key URL: whether URL looks like it carries a credential, which an attested setting would
# publish: user info, or a path segment or query value of 20 or more letters, digits, - and _
# with a digit among them, the shape of the API keys of Alchemy, Infura, QuickNode, and Ankr.
embeds_key() {
    local rest=${1#*://} host
    host=${rest%%[/?#]*}
    [[ "$host" == *@* ]] && return 0
    tr '/?&=#;' '\n' <<<"${rest:${#host}}" | grep -E '^[A-Za-z0-9_-]{20,}$' | grep -q '[0-9]'
}

# check_anonymous_pulls FILE: every image named in FILE (one per line) pulls without credentials.
# Every failure is retried with backoff, since registries also refuse transiently; the message
# names the package as private only when the last error is a refusal.
check_anonymous_pulls() {
    local docker_host image attempt
    stage_start anonymous-pulls "${PULL_STAGE_SECONDS:-600}"
    echo "== images (anonymous pull)"
    # `docker pull` of a digest always asks the registry, even when the daemon has the image cached;
    # an empty client config sends no credentials, as the CVM does, so its errors hold no secret.
    # The empty config also drops the current Docker context, so keep its daemon endpoint.
    docker_host=${DOCKER_HOST:-$(stage_call 10 docker context inspect --format '{{.Endpoints.docker.Host}}' 2>/dev/null)} ||
        docker_host=""
    mkdir "$tmp/docker-anonymous"
    while IFS= read -r image; do
        for attempt in 1 2 3; do
            if DOCKER_HOST=${docker_host:-unix:///var/run/docker.sock} DOCKER_CONFIG="$tmp/docker-anonymous" \
                stage_call 180 docker pull --quiet --platform linux/amd64 "$image" >/dev/null 2>"$tmp/pull.err" </dev/null; then
                ok "$image pulls anonymously"
                continue 2
            fi
            if ! stage_remaining; then
                stage_expired
                fail "anonymous image pull stage exceeded its deadline"
                return 1
            fi
            ((attempt == 3)) || stage_sleep $((attempt * 10))
        done
        if grep -Eiq 'unauthorized|denied|not found|manifest unknown' "$tmp/pull.err"; then
            fail "$image cannot be pulled anonymously; make the package public: $(tool_error "$tmp/pull.err")"
        else
            fail "$image did not pull in 3 attempts: $(tool_error "$tmp/pull.err")"
        fi
    done <"$1"
}

# check_phala_cloud WORKSPACE OS_IMAGE: the CLI version, the logged-in workspace, and a production
# OS image offered by a node of the workspace.
check_phala_cloud() {
    local workspace=$1 os_image=$2 phala version current count offering
    stage_start phala-preflight 120
    echo "== Phala Cloud (read-only)"
    read -r -a phala <<<"${PHALA:-$REPO_ROOT/deploy/phala}"
    version=$(stage_call 30 "${phala[@]}" --version 2>"$tmp/phala.err") || version="error: $(tool_error "$tmp/phala.err")"
    [[ "$version" == v1.1.22* || "$version" == 1.1.22* ]] ||
        fail "the Phala CLI is $version; these steps are verified against 1.1.22"
    # Best effort: `status --json` in CLI 1.1.22 reports only the workspace display name (team_name),
    # no workspace id, so two workspaces with the same name cannot be told apart here.
    if ! stage_call 30 "${phala[@]}" status --json >"$tmp/status.json" 2>"$tmp/phala.err"; then
        fail "the CLI is not logged in to workspace '$workspace': $(tool_error "$tmp/phala.err")"
    elif jq -e --arg workspace "$workspace" '.team_name == $workspace' "$tmp/status.json" >/dev/null; then
        ok "logged in to a workspace named $workspace (display name; best effort)"
    else
        current=$(jq -r '.team_name // empty' "$tmp/status.json" 2>/dev/null) || current=""
        fail "the CLI is not logged in to workspace '$workspace' (current: ${current:-not logged in})"
    fi
    if ! stage_call 30 "${phala[@]}" os-images --prod --all --json >"$tmp/os-images.json" 2>"$tmp/phala.err"; then
        fail "'os-images --prod' failed: $(tool_error "$tmp/phala.err")"
    elif jq -e --arg image "$os_image" \
        'any(.items[]; .name == $image and .is_dev == false and (.version | test("^v?0[.]5[.]9$")))' \
        "$tmp/os-images.json" >/dev/null; then
        ok "OS image $os_image is a production (non-dev) dstack 0.5.9 image"
    else
        fail "OS image $os_image is not listed as a production dstack 0.5.9 image by 'os-images --prod'"
    fi
    # The platform picks the node; it must be one whose images include this one.
    offering='[.nodes[] | select(any(.images[]; .name == $image and .is_dev == false and .version[0:3] == [0, 5, 9]))]'
    if ! stage_call 30 "${phala[@]}" api /teepods/available >"$tmp/nodes.json" 2>"$tmp/phala.err"; then
        fail "'api /teepods/available' failed: $(tool_error "$tmp/phala.err")"
    elif count=$(jq -er --arg image "$os_image" "$offering | length" "$tmp/nodes.json") && ((count > 0)); then
        ok "$count node(s) of the workspace offer OS image $os_image"
    else
        fail "no node of the workspace offers OS image $os_image; offered:" \
            "$(jq -r '[.nodes[].images[] | select(.is_dev == false) | .name] | unique | join(", ")' \
                "$tmp/nodes.json" 2>/dev/null)"
    fi
}

# read_env_file FILE: FILE's KEY=VALUE lines into the array `env`, each value as written. The Phala
# Cloud CLI reads the same file as dotenv does (@phala/cloud's parseEnv), which cuts a value at `#`
# and strips quotes and surrounding whitespace; a value with any of those is refused, so that every
# accepted value is the one the CLI seals. Values are never printed.
read_env_file() {
    local file=$1 line value
    if grep -Evq '^([[:space:]]*($|#)|[A-Za-z_][A-Za-z0-9_]*=)' "$file"; then
        fail "$file has a line that is not KEY=VALUE"
    fi
    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*($|#) ]] && continue
        [[ -v "env[${line%%=*}]" ]] && fail "$file sets ${line%%=*} twice"
        value=${line#*=}
        if [[ "$value" == *[\#\'\"\`]* || "$value" == [[:space:]]* || "$value" == *[[:space:]] ]]; then
            fail "$file: ${line%%=*} has a #, a quote, or surrounding whitespace, which the Phala Cloud" \
                "CLI's env parser would change; such a value is not supported"
        fi
        env[${line%%=*}]=$value
    done <"$file"
}

# check_artifact ENV_FILE COMPOSE ENV_DIR VARIANT: the checks every attested compose shares, run
# with the pinned Compose (deploy/pinned-compose.sh, never downloaded here). Leaves the compose as
# JSON in $tmp/compose.json, its images in $tmp/images, and the env file in the array `env`.
# - The env file names only the compose's sealed names (the CLI makes them allowed_envs; one left
#   out is unset).
# - The compose passes deploy/compose-policy.jq for VARIANT.
# - It is byte for byte a fresh render of ENV_DIR with its own images and gateway or origin, so
#   no stale render or hand edit reaches the CLI.
check_artifact() {
    local env_file=$1 compose=$2 env_dir=$3 variant=$4 compose_bin line violations inputs=()
    compose_bin=$("$REPO_ROOT/deploy/pinned-compose.sh" --no-download 2>"$tmp/pinned.err") || {
        fail "$(tool_error "$tmp/pinned.err")"
        return
    }
    echo "== env file"
    read_env_file "$env_file"
    echo "== compose"
    if ! "$compose_bin" -f "$compose" config --no-interpolate --format json >"$tmp/compose.json" \
        2>"$tmp/compose.err"; then
        fail "docker compose cannot parse $compose: $(head -c 300 "$tmp/compose.err")"
        : >"$tmp/images"
        return
    fi
    jq -r '.services[].image' "$tmp/compose.json" | sort -u >"$tmp/images"
    # The template's DSTACK_APP_DOMAIN is no env: the pre-launch script exports it (verify-attestation.sh).
    "$compose_bin" -f "$compose" config --variables 2>/dev/null |
        awk -v variant="$variant" 'NR > 1 && NF > 0 && !(variant == "template" && $1 == "DSTACK_APP_DOMAIN") { print $1 }' |
        sort >"$tmp/sealed"
    printf '%s\n' "${!env[@]}" | jq -R 'select(. != "")' | jq -s . >"$tmp/env-names.json"
    violations=$(jq -r -L "$REPO_ROOT/deploy" --arg variant "$variant" --slurpfile names "$tmp/env-names.json" \
        'include "compose-policy"; (violations($variant; "dstack") + allowed_envs_violations($variant; $names[0]))[]' \
        "$tmp/compose.json")
    while IFS= read -r line; do
        [[ -z "$line" ]] || fail "policy: $line"
    done <<<"$violations"
    # The images by name, as the release's images.json has them.
    jq '[.services[].image | {key: (split("@")[0] | split("/")[-1]), value: .}
        | select(.key | test("^[a-z0-9][a-z0-9._-]*$"))] | from_entries' \
        "$tmp/compose.json" >"$tmp/release-images.json"
    if [[ "$variant" == restore-check ]]; then
        inputs=(--restore-check --origin "$(jq -r '.services.topup.command[-1]' "$tmp/compose.json")")
    elif [[ "$variant" == template ]]; then
        inputs=(--template)
    else
        inputs=(--gateway-domain
            "$(jq -r '.services["dstack-ingress"].environment.GATEWAY_DOMAIN // ""' "$tmp/compose.json")")
    fi
    if PINNED_COMPOSE=$compose_bin "$REPO_ROOT/deploy/render.sh" "${inputs[@]}" \
        --images "$tmp/release-images.json" "$env_dir" >"$tmp/fresh.yml" 2>"$tmp/render.err"; then
        cmp -s "$tmp/fresh.yml" "$compose" ||
            fail "$compose differs from a fresh render of $env_dir with its images and inputs;" \
                "render it again from the commit being deployed"
    else
        fail "$env_dir does not render with the inputs of $compose: $(tool_error "$tmp/render.err")"
    fi
}

# refuse_example_values FILE...: the placeholders of deploy/environments/example.
refuse_example_values() {
    if grep -Eq 'example\.com|11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=|s3://BUCKET/|ACCOUNT\.r2\.' "$@"; then
        fail "the compose still holds values of deploy/environments/example (example.com, the example" \
            "admin key, s3://BUCKET/, or ACCOUNT.r2)"
    fi
}
