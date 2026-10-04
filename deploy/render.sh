#!/usr/bin/env bash
# Renders the attested compose of one CVM (docs/design/deploy-config.md, "Rendering") with the
# pinned Docker Compose the CVM runs (pinned-compose.sh):
#
#   1. Merges the stack with the environment directory ENV_DIR (`compose.yaml` and `topup.yaml` for
#      a topup CVM, `compose.yaml` and `config.json` for the reference product,
#      deploy/product/compose.yaml) and, for a topup CVM, its variant: compose.service.yaml,
#      compose.restore-check.yaml with --restore-check, or compose.template.yaml with --template
#      (the Phala Cloud template).
#      `config --no-interpolate` keeps the sealed secrets as `${NAME:-}` references. The
#      environment's compose.yaml may set only the documented settings (`settable` below): the
#      merge with it may differ from the merge without it in those environment keys and nowhere
#      else, so it can change no image, command, entrypoint, config, mount, port, or service.
#   2. Applies the three deploy-time inputs and nothing else: --images pins each image the stack
#      names by image name, and every image must then be the manifest's or one the kit pins by
#      digest; --gateway-domain is dstack-ingress's gateway (the service variant and the product),
#      and --origin is the restore instance's own origin (restore-check only). Every
#      config file is inlined as content named after its digest, so a changed file changes the
#      definition of exactly the services that mount it. Its `$` are escaped, so a config never
#      reads the environment.
#   3. Prints Compose's canonical YAML after deploy/compose-policy.jq accepted it.
#
# The project is `dstack`, the name dstack gives the stack it runs in /dstack, so the volumes keep
# their names; --project-name is for local rehearsals only. Values are never printed on error.
#
# Usage: deploy/render.sh [--restore-check | --template] --images FILE
#          [--gateway-domain HOST | --origin URL] [--project-name NAME] [--no-download] ENV_DIR
#          >docker-compose.yml
set -euo pipefail
export LC_ALL=C

root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
usage() {
    echo "usage: $0 [--restore-check | --template] --images FILE" \
        "[--gateway-domain HOST | --origin URL] [--project-name NAME] [--no-download] ENV_DIR" >&2
    exit 64
}
restore_check=0 template=0 images="" gateway="" origin="" project=dstack download=()
while (($#)); do
    case "$1" in
        --restore-check) restore_check=1; shift ;;
        --template) template=1; shift ;;
        --images) images=${2:-}; shift 2 ;;
        --gateway-domain) gateway=${2:-}; shift 2 ;;
        --origin) origin=${2:-}; shift 2 ;;
        --project-name) project=${2:-}; shift 2 ;;
        --no-download) download=(--no-download); shift ;;
        -*) usage ;;
        *) break ;;
    esac
done
(($# == 1)) || usage
env_dir=$(CDPATH='' cd -- "$1" && pwd) || { echo "$1 is not a directory" >&2; exit 64; }
[[ -f "$env_dir/compose.yaml" ]] || { echo "$env_dir has no compose.yaml" >&2; exit 64; }
[[ -f "$images" ]] || { echo "--images must name the release's images.json" >&2; exit 64; }
[[ "$project" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || { echo "invalid --project-name" >&2; exit 64; }

if [[ -f "$env_dir/topup.yaml" && ! -f "$env_dir/config.json" ]]; then
    stack=(-f "$root/deploy/compose.yaml") config_name=topup config_file="$env_dir/topup.yaml"
    variant=service variant_file=(-f "$root/deploy/compose.service.yaml")
elif [[ -f "$env_dir/config.json" && ! -f "$env_dir/topup.yaml" ]]; then
    stack=(-f "$root/deploy/product/compose.yaml") config_name=product
    config_file="$env_dir/config.json" variant=product variant_file=()
    ((!restore_check && !template)) ||
        { echo "--restore-check and --template render a topup environment only" >&2; exit 64; }
else
    echo "$env_dir must hold topup.yaml (a topup CVM) or config.json (the product)" >&2
    exit 64
fi
((!restore_check || !template)) || usage
if ((template)); then
    variant=template variant_file=(-f "$root/deploy/compose.template.yaml")
    [[ -z "$gateway" && -z "$origin" ]] || {
        echo "--template serves the app's gateway domain: no --gateway-domain or --origin" >&2
        exit 64
    }
elif ((restore_check)); then
    variant=restore-check variant_file=(-f "$root/deploy/compose.restore-check.yaml")
    [[ -z "$gateway" && "$origin" =~ ^https://[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:[0-9]{1,5})?$ ]] || {
        echo "--restore-check needs --origin https://HOST (and no --gateway-domain)" >&2
        exit 64
    }
else
    [[ -z "$origin" && "$gateway" =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$ ]] || {
        echo "the $variant variant needs --gateway-domain HOST (and no --origin)" >&2
        exit 64
    }
fi
jq -e 'type == "object" and all(to_entries[];
        (.key | test("^[a-z0-9][a-z0-9._-]*$"))
        and (.value | test("^[a-z0-9]+([._/:-][a-z0-9]+)*@sha256:[0-9a-f]{64}$"))
        and (.value | test("@sha256:0{64}$") | not))' "$images" >/dev/null || {
    echo "--images must map image names to nonzero repository@sha256:<64 hex> references" >&2
    exit 64
}

compose=$("$root/deploy/pinned-compose.sh" "${download[@]}")
tmp=$(mktemp -d "${TMPDIR:-/tmp}/topup-render.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# The environment's own configuration file stands in for the stack's placeholder.
jq -n --arg name "$config_name" --arg file "$config_file" '{configs: {($name): {file: $file}}}' \
    >"$tmp/config.json"
# merge OUT [OVERLAY...]: the stack, the overlays, the variant, and the configuration file, as JSON
# with every environment a map.
merge() {
    local out=$1
    shift
    "$compose" -p "$project" --project-directory "$root/deploy" "${stack[@]}" "$@" "${variant_file[@]}" \
        -f "$tmp/config.json" config --no-interpolate --format json |
        jq '.services |= map_values(if has("environment") then .environment |= (if type == "array"
            then map(capture("^(?<key>[^=]+)=(?<value>.*)$")) | from_entries else . end) else . end)' \
            >"$out"
}
merge "$tmp/base.json"
merge "$tmp/merged.json" -f "$env_dir/compose.yaml"
# The environment's settings: every leaf the overlay adds, changes, or removes must be one of them.
unsettable=$(jq -r --slurpfile base "$tmp/base.json" '
    def settable: length == 4 and .[0] == "services" and .[2] == "environment" and (
        ((.[1] == "postgres" or .[1] == "backup")
            and (.[3] | IN("WALG_S3_PREFIX", "AWS_ENDPOINT", "AWS_REGION", "AWS_S3_FORCE_PATH_STYLE",
                "WALG_WAL_TIMEOUT_SECONDS", "WALG_WAL_ATTEMPTS", "WALG_BASE_TIMEOUT_SECONDS",
                "WALG_BASE_ATTEMPTS", "WALG_RESTORE_TIMEOUT_SECONDS", "WALG_RESTORE_ATTEMPTS",
                "WALG_OBSERVABILITY_DIR", "WALG_BIN")))
        or (.[1] == "dstack-ingress" and .[3] == "DOMAIN")
        or ((.[1] == "topup" or .[1] == "restore-check") and (.[3] | test("^TOPUP_RPC_[A-Z0-9_]+_KEY$"))));
    [tostream | select(length == 2)] as $with | [$base[0] | tostream | select(length == 2)] as $without
    | ($with - $without) + ($without - $with) | map(.[0]) | unique[]
    | select(settable | not) | map(tostring) | join(".")' "$tmp/merged.json")
if [[ -n "$unsettable" ]]; then
    echo "render.sh: $env_dir/compose.yaml may set only WAL-G's location, dstack-ingress's DOMAIN," \
        "and TOPUP_RPC_<ID>_KEY names (deploy/README.md, \"Attested settings\"), not:" >&2
    printf '  %s\n' "${unsettable//$'\n'/$'\n'  }" >&2
    exit 1
fi

# Every file config, read here: its content with `$` escaped for Compose, and its digest.
: >"$tmp/contents.jsonl"
while IFS=$'\t' read -r name file; do
    [[ -f "$file" ]] || { echo "config $name: $file does not exist" >&2; exit 1; }
    digest=$(if command -v sha256sum >/dev/null; then sha256sum "$file"; else shasum -a 256 "$file"; fi |
        cut -c1-12)
    jq -n --arg name "$name" --arg digest "$digest" --rawfile content "$file" \
        '{name: $name, renamed: "\($name)_\($digest)", content: ($content | gsub("\\$"; "$$"))}' \
        >>"$tmp/contents.jsonl"
done < <(jq -r '.configs // {} | to_entries[] | select(.value.file) | [.key, .value.file] | @tsv' \
    "$tmp/merged.json")

jq --slurpfile images "$images" --slurpfile configs <(jq -s . "$tmp/contents.jsonl") \
    --arg variant "$variant" --arg gateway "$gateway" --arg origin "$origin" '
    ($configs[0] | map({key: .name, value: .}) | from_entries) as $files
    | .services |= with_entries(.value |= (
        (if has("image") and ($images[0][.image] != null) then .image = $images[0][.image] else . end)
        | (if has("configs") then .configs |= map(.source |= ($files[.].renamed // .)) else . end)))
    | .configs |= with_entries(if $files[.key] then
        {key: $files[.key].renamed, value: {content: $files[.key].content}} else . end)
    | if $variant == "restore-check" then
        .services.topup.command += ["--public-origin", $origin]
      elif $variant == "template" then .
      else
        .services["dstack-ingress"].environment.GATEWAY_DOMAIN = $gateway
      end
' "$tmp/merged.json" >"$tmp/pinned.json"

# Every image is the release manifest's, or a third-party image the kit's stack pins by digest.
foreign=$(jq -r --slurpfile images "$images" --slurpfile base "$tmp/base.json" '
    ([$images[0][]] + [$base[0].services[].image | select(test("@sha256:[0-9a-f]{64}$"))]) as $known
    | [.services | to_entries[] | select(.value.image as $image | $known | index($image) | not) | .key]
    | join(" ")' "$tmp/pinned.json")
[[ -z "$foreign" ]] || { echo "render.sh: not a release or kit-pinned image: $foreign" >&2; exit 1; }

"$compose" -p "$project" -f "$tmp/pinned.json" config --no-interpolate >"$tmp/rendered.yml"
"$compose" -p "$project" -f "$tmp/rendered.yml" config --no-interpolate --format json \
    >"$tmp/rendered.json"
violations=$(jq -r -L "$root/deploy" --arg variant "$variant" --arg project "$project" \
    'include "compose-policy"; violations($variant; $project)[]' "$tmp/rendered.json")
if [[ -n "$violations" ]]; then
    echo "render.sh: the rendered $variant compose breaks deploy/compose-policy.jq:" >&2
    printf '  %s\n' "${violations//$'\n'/$'\n'  }" >&2
    exit 1
fi
cat "$tmp/rendered.yml"
