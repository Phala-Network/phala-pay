#!/usr/bin/env bash
# `docker compose` for the local stacks (make up, the sandbox, the restore drill): the attested
# compose rendered by deploy/render.sh exactly as for a CVM, under the local project's name, then
# the local overlay (deploy/local/docker-compose.yml with local/service.yml or
# local/restore-check.yml) and any further `-f` files the caller passes.
# The overlay builds the images from this checkout, so the rendered digests are placeholders.
# --restore-check renders the restore verification variant (deploy/RESTORE.md), as the restore
# drill's replacement boots it. --environment-dir is the environment to render (default: the one
# deploy/local/environment.sh writes).
#
# The rendered compose reads the sealed names from the environment; only these are passed, from
# TOPUP_LOCAL_* (sandbox and drill drivers), so a developer's AWS_* never reaches the local stack.
#
# Usage: deploy/local/compose.sh [--restore-check] [--environment-dir DIR] -p PROJECT
#          [COMPOSE OPTIONS] COMMAND [ARGS...]
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
variant=() environment_dir=""
while (($#)); do
    case "$1" in
        --restore-check) variant=(--restore-check); shift ;;
        --environment-dir) environment_dir=${2:?}; shift 2 ;;
        *) break ;;
    esac
done
[[ "${1:-}" == -p && -n "${2:-}" ]] || { echo "usage: $0 [--restore-check] [--environment-dir DIR] -p PROJECT ..." >&2; exit 64; }
project=$2
shift 2
tmp=$(mktemp -d "${TMPDIR:-/tmp}/topup-local-compose.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if [[ -z "$environment_dir" ]]; then
    environment_dir="$tmp/environment"
    "$root/deploy/local/environment.sh" "$environment_dir"
fi
printf '%s\n' '{"phala-pay": "phala-pay-local@sha256:1111111111111111111111111111111111111111111111111111111111111111",
  "postgres-walg": "phala-pay-postgres-walg-local@sha256:2222222222222222222222222222222222222222222222222222222222222222"}' \
    >"$tmp/images.json"
if ((${#variant[@]})); then
    origin=(--origin https://topup.localhost) overlay=restore-check.yml
    storage_key=${TOPUP_LOCAL_RESTORE_S3_ACCESS_KEY_ID:-topup-restore-read}
    storage_secret=${TOPUP_LOCAL_RESTORE_S3_SECRET_ACCESS_KEY:-topup-restore-read-secret}
else
    origin=(--gateway-domain gateway.localhost) overlay=service.yml
    storage_key=${TOPUP_LOCAL_S3_ACCESS_KEY_ID:-topup-s3}
    storage_secret=${TOPUP_LOCAL_S3_SECRET_ACCESS_KEY:-topup-s3-secret-key}
fi
"$root/deploy/render.sh" "${variant[@]}" --images "$tmp/images.json" "${origin[@]}" \
    --project-name "$project" "$environment_dir" >"$tmp/rendered.yml"
# Relative paths in the overlays resolve against deploy/, as for deploy/compose.yaml itself.
env -u AWS_ACCESS_KEY_ID -u AWS_SECRET_ACCESS_KEY -u SENTRY_DSN \
    AWS_ACCESS_KEY_ID="$storage_key" AWS_SECRET_ACCESS_KEY="$storage_secret" \
    docker compose -p "$project" --project-directory "$root/deploy" -f "$tmp/rendered.yml" \
    -f "$root/deploy/local/docker-compose.yml" -f "$root/deploy/local/$overlay" "$@"
