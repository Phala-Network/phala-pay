#!/usr/bin/env bash
# Runs the end-to-end tests with Chromium served from Playwright's official image, for hosts
# without Chromium's system libraries (the self-hosted CI runner). The tests, Vite, and Anvil run
# here; the browser container uses the host's network (as the runner does), so both sides reach
# each other on 127.0.0.1. PLAYWRIGHT_DOCKER_NETWORK overrides the network. Run it from the package
# whose tests to run (pnpm scripts do); extra arguments go to `playwright test`.
set -euo pipefail

image="mcr.microsoft.com/playwright:v1.64.1-noble@sha256:7852dc5a7d72f5fe458a597d690f6c42fbc8e9e26ddfa95f8e6ef552558d9bbf"
version="$(node -p 'require("@playwright/test/package.json").version')"
if [[ $image != *":v$version-"* ]]; then
  echo "@playwright/test $version does not match the browser image $image" >&2
  exit 1
fi
network="${PLAYWRIGHT_DOCKER_NETWORK:-host}"
port="${PLAYWRIGHT_SERVER_PORT:-3799}"
name="phala-pay-playwright-$$"

docker run --detach --rm --init --name "$name" --network "$network" \
  --user pwuser --workdir /home/pwuser "$image" \
  npx -y "playwright@$version" run-server --port "$port" --host 127.0.0.1 >/dev/null
trap 'docker rm --force "$name" >/dev/null 2>&1 || true' EXIT

for _ in $(seq 120); do
  if docker logs "$name" 2>&1 | grep -q "Listening on"; then
    break
  fi
  sleep 1
done
docker logs "$name" 2>&1 | grep -q "Listening on" || { docker logs "$name" >&2; exit 1; }

PLAYWRIGHT_WS_ENDPOINT="ws://127.0.0.1:$port/" pnpm exec playwright test "$@"
