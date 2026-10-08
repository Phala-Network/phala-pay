#!/usr/bin/env bash
# `npm run diagrams`: renders the docs' Mermaid diagrams to public/diagrams (scripts/diagrams.ts)
# in Playwright's official image, the one the e2e tests' browser runs in (sdk/js/e2e/docker.sh, the
# image's single pin), so that every machine, and CI's check, writes the same bytes. The checkout
# is mounted at its own path, and the files are written as the calling user.
set -euo pipefail

web="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$web/../../.." && pwd)"
image="$(sed -n 's/^image="\(.*\)"$/\1/p' "$root/sdk/js/e2e/docker.sh")"
version="$(node -p 'require("@playwright/test/package.json").version')"
if [[ -z $image || $image != *":v$version-"* ]]; then
  echo "@playwright/test $version does not match the browser image '$image' (sdk/js/e2e/docker.sh)" >&2
  exit 1
fi

docker run --rm --init --network none --volume "$root:$root" --workdir "$web" \
  --user "$(id -u):$(id -g)" --env HOME=/tmp "$image" node scripts/diagrams.ts
