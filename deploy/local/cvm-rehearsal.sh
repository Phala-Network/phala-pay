#!/usr/bin/env bash
# CVM rehearsal: runs the staging deployment artifact the way a CVM would, without Phala Cloud.
#
# 1. Builds the three images and pushes them to a throwaway loopback registry, so the composes are
#    rendered by deploy/render.sh with immutable repository@sha256 references.
# 2. Starts Anvil with Sepolia's chain id, installs the canonical Multicall3 that Sepolia carries
#    (install_anvil_multicall3), and deploys the forwarder factory with the A2 scripts
#    (deploy/contracts: canonical proxy, mock Safe checked by verify-safe.sh, deploy-factory.sh,
#    verify-deployment.sh), then the test token and sanctions oracle (deploy-test-contracts.sh).
#    A second Anvil with Base Sepolia's chain id gets the same, without the Safe, for the Base
#    Sepolia routes.
# 3. Writes a staging-shaped environment directory: Phala's staging topup.yaml with the committed
#    routes on those addresses and on the Anvils' providers (`topup config show` and jq), and the
#    staging overlay with local object storage. It renders it as Deploy provisions.
#    PHA uses local Chainlink and Uniswap V2-compatible fixtures on the production-chain-id
#    Anvils; no live mainnet or public RPC is contacted. dstack-ingress does not run
#    (cvm-rehearsal.compose.yml).
# 4. Writes the unsealed `.env` as Deploy does (the rendered compose's sealed names, all empty),
#    and runs `docker compose up` on the rendered file plus cvm-rehearsal.compose.yml (simulator,
#    S3, Anvil), as dstack's app-compose runner does. Without storage credentials PostgreSQL must
#    refuse to initialize (the prefix cannot be listed). Then it seals the secrets (the owner's
#    `envs update`), and PostgreSQL initializes from the provably empty prefix. Re-rendered with
#    a changed topup.yaml (an upgrade), topup must be recreated with the new configuration, and
#    PostgreSQL and `keys` must not.
# 5. Asserts: migrate exits 0, topup passes its startup contract check and serves /healthz, the
#    attestation endpoint answers an account's API key through the simulator and binds the account's
#    webhook key (matching `topup attest`), Sentry reporting is off with the empty DSN, and WAL
#    archiving writes a fresh backup marker; the derived key and database credentials are mode 0600
#    files owned by PostgreSQL and in no container environment.
# 6. Runs the reference-product CVM the same way: its staging environment rendered with the pushed
#    image, an unsealed env of its sealed names, the rehearsal's config (its chain, account, and
#    compose-network URLs) mounted under its digest, first with a provisional public URL and then
#    the real one (the container must be recreated with the new config), then the sealed key. One quote-first deposit, driven from another container with the deposit
#    driver (`python -m reference_product deposit`), is credited end to end and recorded
#    once in the product's ledger. Then it removes everything and asserts that no container,
#    volume, network, or image of the run is left.
#
# Nothing is bind-mounted and the workload publishes no host port (see cvm-rehearsal.compose.yml).
# Requires docker (Compose 2.24.4+), Foundry v1.8.3 with contracts/lib checked out, jq, python3,
# and OpenSSL 3. Internet is used only to pull the pinned images.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
source "$root/deploy/contracts/common.sh"
source "$root/deploy/local/price-fixtures.sh"
for command in docker forge cast jq python3 openssl; do
    require_command "$command"
done

project="topup-cvm-rehearsal-$$"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/topup-cvm-rehearsal.XXXXXX")
printf '%s\n' '{"services":{}}' >"$tmp/rpc-tls.json"
cvm="$tmp/cvm"
mkdir -p "$cvm"
: >"$cvm/.env"
registry_image="registry:3.1.1@sha256:325b4b29b041e82803abeb703e201655e4e23ab83264ec1a7c9ddb0a5b14a6e0"
# The pinned uv/Python image of deploy/sandbox/run-local.sh; it runs the SDK tools and the deposit
# driver (the reference product itself runs from its own image).
client_image="ghcr.io/astral-sh/uv:0.12.18-python3.14-trixie-slim@sha256:00facf17b58b02b725155862c5cd637f688f906bf7eb5b5194647886d8805cf3"
export TOPUP_TEST_TLS_IMAGE="$client_image" TOPUP_TEST_TLS_DIR="$tmp/tls"
registry="$project-registry"
client="$project-client"
product_project="$project-product"
owner="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
export TOPUP_LOCAL_DSTACK_IMAGE="phala-pay-dstack-simulator:$project"
export SANDBOX_ANVIL_PORT REHEARSAL_BASE_SEPOLIA_ANVIL_PORT
export REHEARSAL_MAINNET_PRICE_ANVIL_PORT REHEARSAL_BASE_MAINNET_PRICE_ANVIL_PORT
export REHEARSAL_PRICE_ANVIL_TIMESTAMP
SANDBOX_ANVIL_PORT=$(free_port)
REHEARSAL_BASE_SEPOLIA_ANVIL_PORT=$(free_port)
REHEARSAL_MAINNET_PRICE_ANVIL_PORT=$(free_port)
REHEARSAL_BASE_MAINNET_PRICE_ANVIL_PORT=$(free_port)
registry_port=$(free_port)
rpc_url="http://127.0.0.1:$SANDBOX_ANVIL_PORT"
base_rpc_url="http://127.0.0.1:$REHEARSAL_BASE_SEPOLIA_ANVIL_PORT"
mainnet_price_rpc_url="http://127.0.0.1:$REHEARSAL_MAINNET_PRICE_ANVIL_PORT"
base_mainnet_price_rpc_url="http://127.0.0.1:$REHEARSAL_BASE_MAINNET_PRICE_ANVIL_PORT"
# The sealed names of the rendered compose (filled in after the first render).
env_names=()
local_images=()
# The rehearsal's environment directory (deploy/render.sh ENV_DIR).
environment="$tmp/environment"
mkdir "$environment"
{
    sed -n '/^services:$/,$p' "$root/deploy/environments/phala-network/staging/topup/compose.yaml" |
        sed -e 's|WALG_S3_PREFIX: .*|WALG_S3_PREFIX: s3://topup-backups/postgres|' \
            -e 's|AWS_ENDPOINT: .*|AWS_ENDPOINT: http://s3:3900|' \
            -e 's|AWS_REGION: .*|AWS_REGION: us-east-1|'
} >"$environment/compose.yaml"
cp "$root/deploy/environments/phala-network/staging/topup/topup.yaml" "$environment/topup.yaml"

# Compose as the CVM runs it: the rendered file with its `.env`. The staging names are removed
# from the calling environment so a developer's or CI's AWS_* or TOPUP_* cannot override
# the `.env` file. The project directory anchors the overlay's `extends` paths.
dc() {
    local unset=() name
    for name in "${env_names[@]}"; do
        unset+=(-u "$name")
    done
    env "${unset[@]}" docker compose --progress quiet -p "$project" --project-directory "$root/deploy/local" \
        --env-file "$cvm/.env" -f "$compose_file" \
        -f "$root/deploy/local/cvm-rehearsal.compose.yml" \
        -f "$root/deploy/local/test-tls.compose.yml" -f "$tmp/rpc-tls.json" "$@"
}

# The reference-product CVM: its rendered compose with its own `.env`, joined to the rehearsal
# network as `product` and publishing no host port (overlay written below).
pc() {
    docker compose --progress quiet -p "$product_project" --env-file "$tmp/product.env" \
        -f "$tmp/product.yml" -f "$tmp/product-overlay.yml" "$@"
}

leftovers() {
    {
        docker ps -aq --filter "label=com.docker.compose.project=$project"
        docker ps -aq --filter "label=com.docker.compose.project=$product_project"
        docker volume ls -q --filter "label=com.docker.compose.project=$product_project"
        docker ps -aq --filter "name=^$registry\$" --filter "name=^$client\$"
        docker volume ls -q --filter "label=com.docker.compose.project=$project"
        docker network ls -q --filter "label=com.docker.compose.project=$project"
        local image
        for image in "${local_images[@]}" "$TOPUP_LOCAL_DSTACK_IMAGE"; do
            docker image inspect --format "image $image" "$image"
        done
    } 2>/dev/null
}

cleanup() {
    status=$?
    set +e
    if ((status != 0)) && [[ -n "${compose_file:-}" ]]; then
        echo "--- workload logs (last 60 lines per service) ---" >&2
        dc logs --no-color --tail 60 keys postgres migrate topup topup-tls backup >&2
    fi
    docker rm -f "$client" >/dev/null 2>&1
    if ((status != 0)) && [[ -f "$tmp/product.yml" ]]; then
        echo "--- product logs (last 40 lines) ---" >&2
        pc logs --no-color --tail 40 product >&2
    fi
    [[ -f "$tmp/product.yml" ]] && pc down --volumes --remove-orphans --timeout 10 >/dev/null 2>&1
    if [[ -n "${compose_file:-}" ]]; then
        dc down --volumes --remove-orphans --timeout 10 >/dev/null 2>&1
    fi
    docker rm -f -v "$registry" >/dev/null 2>&1
    # Failures show up in leftovers() below.
    docker image rm "${local_images[@]}" "$TOPUP_LOCAL_DSTACK_IMAGE" >/dev/null 2>&1
    rm -rf "$tmp"
    if [[ -n "$(leftovers)" ]]; then
        echo "FAIL: containers, volumes, networks, or images of $project were left behind:" >&2
        leftovers >&2
        status=1
    else
        echo "== shutdown left no container, volume, network, or image of $project"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Trust only this run's certificate; HTTPS checks stay enabled in the SDK and HTTPX.
mkdir "$TOPUP_TEST_TLS_DIR"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -addext 'basicConstraints=critical,CA:FALSE' -subj /CN=topup-tls \
    -addext subjectAltName=DNS:topup-tls,DNS:api.kraken.com,DNS:data-api.binance.vision,DNS:price-stub,DNS:*.rpc.test \
    -keyout "$TOPUP_TEST_TLS_DIR/key.pem" \
    -out "$TOPUP_TEST_TLS_DIR/cert.pem" >/dev/null 2>&1
python3 - "$TOPUP_TEST_TLS_DIR/cert.pem" "$TOPUP_TEST_TLS_DIR/ca.pem" <<'PYTHON'
import ssl, sys
from pathlib import Path
roots = "".join(ssl.DER_cert_to_PEM_cert(c) for c in ssl.create_default_context().get_ca_certs(binary_form=True))
Path(sys.argv[2]).write_text(roots + Path(sys.argv[1]).read_text())
PYTHON
export TOPUP_TEST_TLS_PROXY TOPUP_TEST_TLS_CERTIFICATE TOPUP_TEST_TLS_KEY
export TOPUP_PRICE_STUB_SERVER
TOPUP_TEST_TLS_PROXY="$(<"$root/deploy/local/tls_proxy.py")"
TOPUP_TEST_TLS_CERTIFICATE="$(<"$TOPUP_TEST_TLS_DIR/cert.pem")"
TOPUP_TEST_TLS_KEY="$(<"$TOPUP_TEST_TLS_DIR/key.pem")"
TOPUP_PRICE_STUB_SERVER="$(<"$root/deploy/local/price_stub.py")"

wait_for() {
    local description=$1 attempts=$2
    shift 2
    until "$@" >/dev/null 2>&1; do
        attempts=$((attempts - 1))
        ((attempts > 0)) || { echo "timed out waiting for $description" >&2; return 1; }
        sleep 2
    done
}

# Runs Python in the client container on the compose network.
product_python() {
    docker exec -i -e UV_PROJECT_ENVIRONMENT=/opt/venv -e UV_PYTHON_DOWNLOADS=never \
        -e PYTHONDONTWRITEBYTECODE=1 -e PYTHONPATH=/opt/repo/deploy/product -w /opt/repo "$client" \
        uv run --locked --project sdk/python --quiet python "$@"
}

echo "== building images and pushing them to a loopback registry"
docker run -d --name "$registry" -p "127.0.0.1:$registry_port:5000" \
    --mount type=tmpfs,destination=/var/lib/registry "$registry_image" >/dev/null
wait_for "the registry" 30 curl -fsS "http://127.0.0.1:$registry_port/v2/"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --pretty=%ct)}
# publish NAME VARIABLE BUILD_ARGS...: builds, pushes, and sets VARIABLE to repository@sha256.
publish() {
    local name=$1 tag="127.0.0.1:$registry_port/$1:rehearsal" variable=$2 digest
    shift 2
    if [[ "${REHEARSAL_PREBUILT:-0}" == 1 ]]; then
        docker tag "$name:rehearsal" "$tag"
    else
        docker build --quiet --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH" \
            --build-arg "BUILD_JOBS=${CARGO_BUILD_JOBS:-}" -t "$tag" "$@" >/dev/null
    fi
    local_images+=("$tag")
    docker push --quiet "$tag" >/dev/null
    digest=$(docker image inspect --format '{{json .RepoDigests}}' "$tag" |
        jq -er --arg repository "${tag%:rehearsal}" '.[] | select(startswith($repository + "@"))')
    local_images+=("$digest")
    printf -v "$variable" '%s' "$digest"
    export "${variable?}"
}
publish phala-pay TOPUP_IMAGE "$root"
publish postgres-walg POSTGRES_WALG_IMAGE -f "$root/deploy/Dockerfile.postgres-walg" "$root"
publish phala-pay-reference-product PRODUCT_IMAGE \
    -f "$root/deploy/Dockerfile.reference-product" "$root"
if [[ "${REHEARSAL_PREBUILT:-0}" == 1 ]]; then
    docker tag phala-pay-dstack-simulator:rehearsal "$TOPUP_LOCAL_DSTACK_IMAGE"
else
    docker build --quiet -t "$TOPUP_LOCAL_DSTACK_IMAGE" \
        -f "$root/deploy/local/Dockerfile.dstack-simulator" "$root" >/dev/null
fi
jq -n --arg topup "$TOPUP_IMAGE" --arg postgres "$POSTGRES_WALG_IMAGE" --arg product "$PRODUCT_IMAGE" \
    '{"phala-pay": $topup, "postgres-walg": $postgres, "phala-pay-reference-product": $product}' \
    >"$tmp/images.json"
cat "$tmp/images.json"
# render_topup: the environment rendered as Deploy provisions it, for this project's volumes.
render_topup() {
    "$root/deploy/render.sh" --images "$tmp/images.json" \
        --gateway-domain gateway.dstack-pha-prod5.phala.network --project-name "$project" \
        "$environment" >"$cvm/docker-compose.yaml"
}
# The first render, with staging's own routes, only brings up the Anvils below.
render_topup
compose_file="$cvm/docker-compose.yaml"
mapfile -t env_names < <(docker compose -f "$compose_file" config --variables |
    awk 'NR > 1 && NF > 0 { print $1 }' | sort)

echo "== starting Anvil (asset and hermetic price chains) and the client container"
# Anchor after image builds; build duration must not age the price fixtures.
REHEARSAL_PRICE_ANVIL_TIMESTAMP=$(($(date +%s) - 1950))
dc up -d --wait anvil anvil-base-sepolia anvil-mainnet-price anvil-base-mainnet-price >/dev/null
# Both chains carry the canonical Multicall3 that topup's balance and addressOf reads go through.
install_anvil_multicall3 "$rpc_url"
install_anvil_multicall3 "$base_rpc_url"
docker run -d --name "$client" --network "${project}_default" "$client_image" sleep infinity \
    >/dev/null
# `docker cp` streams through the API, so this works where the daemon cannot see the checkout.
docker exec "$client" mkdir /opt/repo
docker cp "$TOPUP_TEST_TLS_DIR/ca.pem" "$client:/opt/test-ca.pem"
tar -C "$root" --exclude=.venv --exclude='*_cache' --exclude=__pycache__ -cf - sdk/python \
    deploy/product/reference_product |
    docker cp - "$client:/opt/repo"

echo "== deploying contracts with the A2 and sandbox scripts"
export FOUNDRY_BROADCAST="$tmp/broadcast"
"$DEPLOY_CONTRACTS_DIR/deploy-proxy.sh" --rpc-url "$rpc_url" --local-fund --broadcast >/dev/null
nonce=$(cast nonce "$owner" --rpc-url "$rpc_url")
safe_singleton=$(cast compute-address "$owner" --nonce "$nonce" | awk '{print $NF}')
safe=$(cast compute-address "$owner" --nonce $((nonce + 1)) | awk '{print $NF}')
(cd "$CONTRACTS_DIR" && SAFE_OWNER="$owner" forge script test/DeployMockSafe.s.sol:DeployMockSafe \
    --rpc-url "$rpc_url" --broadcast --private-key "$ANVIL_PRIVATE_KEY" -q) >/dev/null
[[ "$(cast code "$safe" --rpc-url "$rpc_url")" != 0x ]] || die "mock Safe was not deployed"
jq -n --arg safe "$safe" --arg owner "$owner" \
    --arg code_hash "$(code_hash "$rpc_url" "$safe")" --arg singleton "$safe_singleton" \
    --arg singleton_code_hash "$(code_hash "$rpc_url" "$safe_singleton")" --arg zero "$ZERO_ADDRESS" \
    '{configured: true, networks: {sepolia: {chain_id: 11155111}}, treasury: $safe,
      safes: [{address: $safe, owners: [$owner], threshold: 1, proxy_code_hashes: [$code_hash],
               singleton: $singleton, singleton_code_hash: $singleton_code_hash,
               modules: [], guard: $zero, fallback_handler: $zero}]}' >"$tmp/safe-expectations.json"
"$DEPLOY_CONTRACTS_DIR/verify-safe.sh" --expectations "$tmp/safe-expectations.json" \
    --rpc "sepolia/a=$rpc_url" >/dev/null || die "verify-safe.sh rejected the mock treasury Safe"
PRIVATE_KEY="$ANVIL_PRIVATE_KEY" \
    "$DEPLOY_CONTRACTS_DIR/deploy-factory.sh" --rpc "sepolia/a=$rpc_url" --broadcast >/dev/null 2>&1 ||
    die "deploy-factory.sh failed"
"$DEPLOY_CONTRACTS_DIR/verify-deployment.sh" \
    --rpc "sepolia/a=$rpc_url" --rpc "sepolia/b=$rpc_url" >"$tmp/verification.json" ||
    die "verify-deployment.sh failed: $(jq -c '[.chains[].checks]' "$tmp/verification.json")"
factory=$(jq -er '.chains[0].factory' "$tmp/verification.json")
implementation=$(jq -er '.chains[0].implementation' "$tmp/verification.json")
"$root/deploy/sandbox/deploy-test-contracts.sh" --anvil-unlocked "$owner" --rpc-url "$rpc_url" \
    >"$tmp/test-contracts.json"
token=$(jq -er .test_token "$tmp/test-contracts.json")
oracle=$(jq -er .sanctions_oracle "$tmp/test-contracts.json")
printf 'factory=%s implementation=%s safe=%s token=%s sanctions_oracle=%s\n' \
    "$factory" "$implementation" "$safe" "$token" "$oracle"
# Base Sepolia: the same deterministic factory, and test contracts of its own.
"$DEPLOY_CONTRACTS_DIR/deploy-proxy.sh" --rpc-url "$base_rpc_url" --local-fund --broadcast >/dev/null
PRIVATE_KEY="$ANVIL_PRIVATE_KEY" \
    "$DEPLOY_CONTRACTS_DIR/deploy-factory.sh" --rpc "base-sepolia/a=$base_rpc_url" --broadcast \
    >/dev/null 2>&1 || die "deploy-factory.sh failed on Base Sepolia"
"$DEPLOY_CONTRACTS_DIR/verify-deployment.sh" \
    --rpc "base-sepolia/a=$base_rpc_url" --rpc "base-sepolia/b=$base_rpc_url" \
    >"$tmp/base-verification.json" ||
    die "verify-deployment.sh failed on Base Sepolia: $(jq -c '[.chains[].checks]' "$tmp/base-verification.json")"
[[ "$(jq -er '.chains[0].factory' "$tmp/base-verification.json")" == "$factory" ]] ||
    die "the Base Sepolia factory is not the Sepolia one"
"$root/deploy/sandbox/deploy-test-contracts.sh" --anvil-unlocked "$owner" --rpc-url "$base_rpc_url" \
    >"$tmp/base-test-contracts.json"
base_token=$(jq -er .test_token "$tmp/base-test-contracts.json")
base_oracle=$(jq -er .sanctions_oracle "$tmp/base-test-contracts.json")
# rehearsal_token CONTRACT RPC_URL: deploys six-decimal USDC/USDT fixtures matching the routes.
rehearsal_token() {
    (cd "$CONTRACTS_DIR" && forge create "test/mocks/MockTokens.sol:$1" --rpc-url "$2" \
        --unlocked --from "$owner" --broadcast --json) | jq -er .deployedTo
}
base_second_token=$(rehearsal_token UsdcLikeToken "$base_rpc_url")
base_third_token=$(rehearsal_token UsdtLikeToken "$base_rpc_url")
printf 'base-sepolia: token=%s second_token=%s third_token=%s sanctions_oracle=%s\n' \
    "$base_token" "$base_second_token" "$base_third_token" "$base_oracle"

echo "== installing hermetic mainnet price fixtures"
install_anvil_price_fixtures "$mainnet_price_rpc_url" "$base_mainnet_price_rpc_url"

echo "== writing the configuration and rendering the staging compose"
# Phala's staging configuration with this network's addresses: on each chain the test token stands
# in for PHA, the reference product's asset, a six-decimal mock token for USDC, and a USDT-like one
# for USDT. Provider A is keyless, as staging's; provider B is attested with a `{key}`, as a paid
# provider is, and Anvil ignores the query that carries the key. Base Sepolia's two providers are
# keyless, at two URLs of its Anvil.
second_token=$(rehearsal_token UsdcLikeToken "$rpc_url")
third_token=$(rehearsal_token UsdtLikeToken "$rpc_url")
# The owner's admin key, in the PEM form deploy/runbooks/sign-admin-request.sh signs with.
openssl genpkey -algorithm ed25519 -out "$tmp/admin.pem"
admin_public_key=$(openssl pkey -in "$tmp/admin.pem" -pubout -outform DER | tail -c 32 | base64)
# write_config ADMIN_KEY_ID: the rehearsal's topup.yaml (resolved JSON, which is YAML).
write_config() {
    # Every route is kept on its committed on-chain pricing path. The local production-chain-id
    # Anvils below provide Chainlink, Uniswap and sequencer fixture state; exchange calls, when
    # required by the staging-only PHA check, resolve only to the local stub.
    docker run --rm -i --network none "$TOPUP_IMAGE" topup config show /dev/stdin \
        <"$root/deploy/environments/phala-network/staging/topup/topup.yaml" |
        jq --arg id "$1" --arg key "$admin_public_key" --arg factory "$factory" \
            --arg implementation "$implementation" \
            --argjson assets "$(jq -n --arg pha "$token" --arg usdc "$second_token" \
                --arg usdt "$third_token" --arg base_pha "$base_token" \
                --arg base_usdc "$base_second_token" --arg base_usdt "$base_third_token" \
                --arg oracle "$oracle" --arg base_oracle "$base_oracle" '{
                    "phala-cloud-sepolia-pha-usd": [$pha, $oracle],
                    "phala-cloud-sepolia-usdc-usd": [$usdc, $oracle],
                    "phala-cloud-sepolia-usdt-usd": [$usdt, $oracle],
                    "phala-cloud-base-sepolia-pha-usd": [$base_pha, $base_oracle],
                    "phala-cloud-base-sepolia-usdc-usd": [$base_usdc, $base_oracle],
                    "phala-cloud-base-sepolia-usdt-usd": [$base_usdt, $base_oracle]}')" '
            .admin_key = {id: $id, public_key: $key}
            | .routes |= map(($assets[.route] // error("no rehearsal token for \(.route)")) as $asset
                | .chain.forwarder_factory = $factory | .chain.implementation = $implementation
                | .asset.contract = $asset[0] | .chain.sanctions_oracle = $asset[1]
                | if .asset.symbol == "pha" then
                    if .livemode then error("price rehearsal requires test mode") else
                        .price.allow_unclear_sources = true
                    end
                  else . end)' \
        >"$environment/topup.yaml"
    python3 "$root/deploy/local/rpc-tls.py" "$environment/topup.yaml" "$tmp/rpc-tls.json" \
        --certificate "$TOPUP_TEST_TLS_DIR/cert.pem" --key "$TOPUP_TEST_TLS_DIR/key.pem" \
        --image "$client_image" --chain 11155111=http://anvil:8545 \
        --chain 84532=http://anvil-base-sepolia:8545 --chain 1=http://anvil-mainnet-price:8545 \
        --chain 8453=http://anvil-base-mainnet-price:8545
    docker run --rm -i --network none "$TOPUP_IMAGE" topup config check /dev/stdin \
        <"$environment/topup.yaml" >/dev/null || die "the rehearsal configuration is invalid"
}
write_config rehearsal-admin/v0
render_topup
dc config --format json >"$tmp/stack.json"
jq -j '.configs | to_entries[] | select(.key | startswith("topup_")) | .value.content' \
    "$tmp/stack.json" | cmp -s - "$environment/topup.yaml" ||
    die "the rendered compose does not carry the rehearsal configuration"
jq -e --arg topup "$TOPUP_IMAGE" --arg postgres "$POSTGRES_WALG_IMAGE" \
    '[.services[] | select(.image | startswith("127.0.0.1:")) | .image] | unique == ([$topup, $postgres] | sort)' \
    "$tmp/stack.json" >/dev/null || die "the rendered compose does not use the pushed digests"
jq -e '[.services[].volumes[]? | select(.type == "bind")] | length == 0' "$tmp/stack.json" \
    >/dev/null || die "the rehearsal stack bind-mounts a host path"

echo "== writing the unsealed .env, as Deploy does"
# The owner-sealed secrets, the only env values.
declare -A values=(
    [AWS_ACCESS_KEY_ID]=topup-s3
    [AWS_SECRET_ACCESS_KEY]=topup-s3-secret-key
    # Empty: the rehearsal proves the service runs unchanged with Sentry reporting off.
    [SENTRY_DSN]=''
    # Provider A's URL is keyless; topup reaches provider B only with its key in place of `{key}`.
    [TOPUP_RPC_ANKR_KEY]=rehearsal-rpc-key
    [TOPUP_RPC_INFURA_KEY]=rehearsal-rpc-key
)
((${#values[@]} == ${#env_names[@]})) || die "the rehearsal .env and the compose's sealed names differ"
for name in "${env_names[@]}"; do
    [[ -v "values[$name]" ]] || die "no rehearsal value for $name"
done
printf '%s=\n' "${env_names[@]}" >"$cvm/.env"
grep -qx 'AWS_SECRET_ACCESS_KEY=' "$cvm/.env" || die "the unsealed .env carries the S3 secret"

echo "== docker compose up unsealed (the CVM's app-compose command)"
# The entrypoint rejects missing S3 credentials before attempting the backup listing, so
# PostgreSQL never initializes or becomes healthy: `up` fails like dstack's boot.
if dc up -d --remove-orphans >/dev/null 2>&1; then
    die "the unsealed stack started"
fi
refused() {
    dc logs --no-color postgres 2>&1 |
        grep -F 'AWS_ACCESS_KEY_ID must be set: S3 storage needs both credentials' \
            >/dev/null
}
wait_for "PostgreSQL to refuse initialization" 90 refused
if docker run --rm --entrypoint test -v "${project}_pgdata:/var/lib/postgresql" "$POSTGRES_WALG_IMAGE" \
    -e /var/lib/postgresql/data/PG_VERSION; then
    die "PostgreSQL initialized a cluster without listing the backup prefix"
fi
echo "ok: unsealed, PostgreSQL refuses to initialize without a listed backup prefix"

echo "== sealing the secrets (the owner's envs update: same names, restart)"
for name in "${env_names[@]}"; do
    printf '%s=%s\n' "$name" "${values[$name]}"
done >"$cvm/.env"
dc up -d --remove-orphans >/dev/null
dc logs --no-color postgres 2>&1 |
    grep -F 'the backup prefix holds no base backup; initializing a new cluster' >/dev/null ||
    die "PostgreSQL did not initialize from the provably empty backup prefix"
echo "ok: sealed, PostgreSQL listed an empty backup prefix and initialized a new cluster"
# `keys` derives the backup key and the database credentials into files only PostgreSQL reads.
[[ "$(dc exec -T postgres stat -c '%a:%u:%g' /run/wal-g/backup.key /run/db-owner/postgres.password \
    /run/db-owner/postgres.pgpass /run/db-app/topup_service.pgpass | sort -u)" == 600:999:999 ]] ||
    die "the derived key and credential files are not postgres-owned mode 0600"
if dc ps -q | xargs docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' |
    grep -Eq '^(WALG_LIBSODIUM_KEY|POSTGRES_PASSWORD|PGPASSWORD)='; then
    die "a container environment carries a key or password"
fi
echo "ok: the derived key and credentials are mode 0600 files, in no container environment"
migrate_exited() {
    [[ "$(dc ps -a --format json migrate | jq -rs 'flatten | .[0].State')" == exited ]]
}
wait_for "migrate" 90 migrate_exited
migrate_exit=$(dc ps -a --format json migrate | jq -rs 'flatten | .[0].ExitCode')
[[ "$migrate_exit" == 0 ]] || { dc logs migrate >&2; die "migrate exited with $migrate_exit"; }
echo "ok: migrate exited 0"

echo "== seeding the hermetic thirty-minute TWAP window"
# Keep the sampler out of the compressed history; resume it against the completed window.
dc stop --timeout 10 topup >/dev/null
# The production sampler persists one sample per minute. Rehearsal time is compressed by mining
# those timestamps on the local production-chain-id Anvil; every row still carries a real local
# block hash, so the reader's restart/reorg checks remain exercised.
twap_policy='{"window_s":1800,"max_sample_age_s":180,"min_weth_reserve_usd":100000,"max_spot_deviation_bps":300,"max_sample_jump_bps":500}'
twap_sql="$tmp/twap.sql"
: >"$twap_sql"
pair_timestamp_last=$((ANVIL_PRICE_PAIR_TIMESTAMP - 1800))
window_start=$(($(date +%s) - 1860))
twap_spot=$(python3 - <<'PY'
print((100 * 10**18 << 112) // (100_000 * 10**18))
PY
)
for i in $(seq 1 31); do
    sample_timestamp=$((window_start + (i - 1) * 60))
    cast rpc --rpc-url "$mainnet_price_rpc_url" anvil_setNextBlockTimestamp "$sample_timestamp" >/dev/null
    cast rpc --rpc-url "$mainnet_price_rpc_url" evm_mine >/dev/null
    # Capture number/hash/timestamp together, from the block actually mined.
    sample_header=$(cast block latest --json --rpc-url "$mainnet_price_rpc_url" | jq -ce '.data')
    sample_block=$(jq -r '.number' <<<"$sample_header")
    sample_timestamp=$(jq -r '.timestamp' <<<"$sample_header")
    read -r sample_block sample_timestamp < <(python3 - "$sample_block" "$sample_timestamp" <<'PY'
import sys
print(*(int(value, 0) for value in sys.argv[1:]))
PY
)
    sample_hash=$(jq -r '.hash' <<<"$sample_header")
    sample_cumulative=$(python3 - "$sample_timestamp" "$pair_timestamp_last" "$twap_spot" <<'PY'
import sys
timestamp, previous, spot = map(int, sys.argv[1:])
print(f"0x{(spot * ((timestamp - previous) & 0xffffffff)) % (1 << 256):064x}")
PY
)
    sample_json=$(jq -cn --arg block "$sample_block" --arg hash "$sample_hash" \
        --arg timestamp "$sample_timestamp" --arg cumulative "$sample_cumulative" \
        --arg spot "$(python3 - "$twap_spot" <<'PY'
import sys
print(f"0x{int(sys.argv[1]):064x}")
PY
)" \
        '{block:($block|tonumber),hash:$hash,timestamp:($timestamp|tonumber),cumulative:$cumulative,spot:$spot}')
    printf "INSERT INTO price_twap_observations (policy, block_number, block_timestamp, sample) VALUES ('%s', %s, %s, '%s');\n" \
        "$twap_policy" "$sample_block" "$sample_timestamp" "$sample_json" >>"$twap_sql"
done
dc exec -T postgres psql -U postgres -d topup -X -v ON_ERROR_STOP=1 <"$twap_sql" >/dev/null
# Price readers pin two blocks below head; make the final sample available at that height.
for i in 1 2; do
    cast rpc --rpc-url "$mainnet_price_rpc_url" evm_mine >/dev/null
done
cast rpc --rpc-url "$base_mainnet_price_rpc_url" evm_mine >/dev/null
cast rpc --rpc-url "$base_mainnet_price_rpc_url" evm_mine >/dev/null
cast rpc --rpc-url "$mainnet_price_rpc_url" anvil_setIntervalMining 1 >/dev/null
cast rpc --rpc-url "$base_mainnet_price_rpc_url" anvil_setIntervalMining 1 >/dev/null
dc start topup >/dev/null
echo "ok: persisted 31 local samples covering the minimum TWAP window"

http_status() {
    product_python -c 'import sys, httpx; print(httpx.get(sys.argv[1], timeout=5).status_code)' "$1"
}
healthy() {
    [[ "$(http_status http://topup:8080/healthz)" == 200 ]]
}
wait_for "GET /healthz" 90 healthy
tls_healthy() {
    product_python -c 'import ssl, httpx; print(httpx.get("https://topup-tls:8443/healthz", verify=ssl.create_default_context(cafile="/opt/test-ca.pem"), timeout=5).status_code)' |
        grep -qx 200
}
wait_for "verified HTTPS GET /healthz" 30 tls_healthy
echo "ok: the disposable TLS ingress serves /healthz with certificate verification"
# `topup run` checks the route's contracts on every provider before it touches the database
# and binds the listener, so a served /healthz means the check passed.
if dc logs topup 2>&1 | grep -q 'on-chain contract check failed'; then
    die "topup logged a failed startup contract check"
fi
echo "ok: topup passed its startup contract check; GET /healthz is 200"
# The sealed SENTRY_DSN stays empty here: reporting must be off and the service unchanged.
dc logs --no-color topup 2>&1 | grep -F '"error reporting configured"' |
    grep -qF '"sentry_enabled":false' || die "topup did not start with Sentry reporting off"
echo "ok: topup runs with Sentry reporting off (empty SENTRY_DSN)"

marker_fresh() {
    local marker
    marker=$(dc exec -T backup cat /run/topup-observability/last-backup-unix-seconds) || return 1
    (($(date +%s) - marker <= 180))
}
wait_for "a fresh backup marker" 120 marker_fresh
echo "ok: WAL archiving refreshed the backup marker"

echo "== re-rendering with a changed configuration (Deploy's upgrade)"
# The config is named after its digest, so exactly the services that mount it get a new
# definition: topup, and dstack-ingress, which depends on it (Compose recreates dependents).
# PostgreSQL and `keys` keep their containers.
postgres_before=$(dc ps -q postgres) keys_before=$(dc ps -q keys) topup_before=$(dc ps -q topup)
write_config rehearsal-admin/v1
render_topup
dc up -d --remove-orphans >/dev/null
[[ -n "$(dc ps -q topup)" && "$(dc ps -q topup)" != "$topup_before" ]] ||
    die "a changed configuration did not recreate topup"
[[ "$(dc ps -q postgres)" == "$postgres_before" && "$(dc ps -q keys)" == "$keys_before" ]] ||
    die "a changed configuration recreated PostgreSQL or keys"
# The image is distroless (no `cat`): read the file through the API.
docker cp "$(dc ps -q topup):/etc/topup/topup.yaml" - | tar -xO |
    jq -e '.admin_key.id == "rehearsal-admin/v1"' >/dev/null ||
    die "topup does not read the re-rendered configuration"
wait_for "GET /healthz after the upgrade" 90 healthy
echo "ok: the changed configuration recreated topup only; PostgreSQL and keys kept their containers"

echo "== one quote-first deposit against the reference product"
# A CVM has no database access, so the operator creates the account through the signed admin API;
# the answer's first test key goes straight to the client container, never to this shell's output.
# `-j` omits the trailing newline, so the body passes through an argument byte for byte.
jq -cjn '{name: "phala-cloud", contact: {name: "Rehearsal", email: "rehearsal@example.com"},
      due_diligence: {reference: "rehearsal", reviewed_at: "2026-09-28", reviewed_by: "cvm-rehearsal"},
      charges_enabled: false, reason: "CVM rehearsal"}' \
    >"$tmp/product.json"
mapfile -t headers < <("$root/deploy/runbooks/sign-admin-request.sh" POST \
    http://topup:8080/v1/admin/accounts "$tmp/product.json" "$tmp/admin.pem" rehearsal-admin/v1)
account=$(product_python - "$(<"$tmp/product.json")" "${headers[@]}" <<'PY'
import sys, httpx
headers = dict(header.split(": ", 1) for header in sys.argv[2:])
headers["content-type"] = "application/json"
response = httpx.post("http://topup:8080/v1/admin/accounts", content=sys.argv[1].encode(),
                      headers=headers, timeout=30)
assert response.status_code == 200, (response.status_code, response.text)
account = response.json()
secret = account["api_keys"][0]["secret"]
with open("/opt/product.key", "w", encoding="ascii") as key:
    key.write(secret)
# The merchant, not the operator, registers its webhook endpoint, with its own key.
endpoint = httpx.post("http://topup:8080/v1/webhook_endpoints",
                      json={"url": "http://product:8089/webhooks", "enabled_events": ["*"]},
                      headers={"Authorization": f"Bearer {secret}"}, timeout=30)
assert endpoint.status_code == 200, (endpoint.status_code, endpoint.text)
print(account["id"])
PY
) || die "POST /v1/admin/accounts did not create the account and its webhook endpoint"
echo "ok: POST /v1/admin/accounts created $account with its first test key; its endpoint is registered"

# The account proves its test-mode treasury through the API (design D10), here the owner EOA: the
# mock Safe above implements no EIP-1271. The challenge is signed on this host with `cast`.
treasury=$owner
message=$(product_python - "$treasury" <<'PY'
import sys, httpx
with open("/opt/product.key", encoding="ascii") as key:
    headers = {"Authorization": f"Bearer {key.read().strip()}"}
response = httpx.post("http://topup:8080/v1/treasuries/challenge", headers=headers, timeout=30,
                      json={"chain_id": 11155111, "address": sys.argv[1]})
assert response.status_code == 200, (response.status_code, response.text)
print(response.json()["message"], end="")
PY
) || die "POST /v1/treasuries/challenge failed"
signature=$(cast wallet sign --private-key "$ANVIL_PRIVATE_KEY" "$message")
product_python - "$message" "$signature" <<'PY' || die "POST /v1/treasuries did not set the treasury"
import sys, httpx
with open("/opt/product.key", encoding="ascii") as key:
    headers = {"Authorization": f"Bearer {key.read().strip()}"}
response = httpx.post("http://topup:8080/v1/treasuries", headers=headers, timeout=30,
                      json={"chain_id": 11155111, "message": sys.argv[1], "signature": sys.argv[2]})
assert response.status_code == 200, (response.status_code, response.text)
assert response.json()["status"] == "active", response.text
PY
echo "ok: POST /v1/treasuries set the account's treasury $treasury with a signed challenge"

# A new account accepts nothing until the merchant lists what it takes
# (docs/design/payment-settings.md).
product_python - <<'PY' || die "POST /v1/payment_settings did not accept the route"
import httpx
with open("/opt/product.key", encoding="ascii") as key:
    headers = {"Authorization": f"Bearer {key.read().strip()}"}
response = httpx.post("http://topup:8080/v1/payment_settings", headers=headers, timeout=30,
                      json={"chains": [{"chain_id": 11155111, "assets": [{"asset": "pha"}]}]})
assert response.status_code == 200, (response.status_code, response.text)
assert response.json()["status"] == "configured", response.text
PY
echo "ok: POST /v1/payment_settings accepts the rehearsal route"

# The merchant learns its webhook key only from /v1/attestation, with its API key: production has
# no logs or SSH.
nonce=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
attestation=$(product_python - "$nonce" "$account" <<'PY'
import json, sys, httpx
from topup_client.models import AttestationResponse
from topup_sdk import verify_attestation_binding
nonce, account = bytes.fromhex(sys.argv[1]), sys.argv[2]
url = "http://topup:8080/v1/attestation"
anonymous = httpx.get(url, params={"nonce": nonce.hex()}, timeout=30)
assert anonymous.status_code == 401, anonymous.status_code
with open("/opt/product.key", encoding="ascii") as key:
    headers = {"Authorization": f"Bearer {key.read().strip()}"}
body = httpx.get(url, params={"nonce": nonce.hex()}, headers=headers, timeout=30).raise_for_status().json()
verify_attestation_binding(
    AttestationResponse.from_dict(body), nonce, expected_account=account, expected_livemode=False
)
assert len(body["tdx_quote"]) > 0, "empty quote"
assert "operators" not in body, "the service sends no transactions, so it attests no operator"
keys = [{"version": key["version"], "public_key": key["public_key"]} for key in body["webhook_keys"]]
print(json.dumps({"webhook_keys": keys, "report_data": body["report_data"]}))
PY
)
echo "ok: GET /v1/attestation needs an API key and binds the nonce and the account's webhook key"
cli_attestation=$(dc exec -T topup topup attest --nonce "$nonce" --account "$account" |
    jq -c '{webhook_keys, report_data}')
[[ "$(jq -S . <<<"$cli_attestation")" == "$(jq -S . <<<"$attestation")" ]] ||
    die "topup attest and GET /v1/attestation disagree"
echo "ok: topup attest reports the same webhook key and report_data"

echo "== the reference-product CVM: rendered compose, unsealed env, public URL, then the sealed key"
driver_key=$(product_python -m topup_sdk keygen --keyid driver/v1 --seed-out /opt/driver.seed)
# The product's staging environment, rendered as Deploy renders it (its public_url is the host of
# its domain, as the policy requires). The CVM would read that config; here the rehearsal mounts
# its own, the staging config with this network's chain, account, and compose-network URLs, named
# after its digest as render.sh names configs.
cp -r "$root/deploy/environments/phala-network/staging/product" "$tmp/product-environment"
"$root/deploy/render.sh" --images "$tmp/images.json" \
    --gateway-domain gateway.dstack-pha-prod5.phala.network --project-name "$product_project" \
    "$tmp/product-environment" >"$tmp/product.yml"
# product_overlay PUBLIC_URL: the overlay with the rehearsal's product config.
product_overlay() {
    local config digest
    config=$(jq --arg factory "$factory" --arg implementation "$implementation" \
        --arg account "$account" --arg treasury "$treasury" --arg token "$token" --arg url "$1" \
        --arg driver "$(jq -er .public_key <<<"$driver_key")" '
        .factory = $factory | .implementation = $implementation | .account = $account
        | .service_url = "https://topup-tls:8443" | .public_url = $url | .driver_public_key = $driver
        | .chains = [{chain_id: 11155111, name: "Sepolia", rpc_url: "http://anvil:8545",
            treasury: $treasury, test_tokens: [{symbol: "PHA", address: $token}]}]' \
        "$tmp/product-environment/config.json")
    config=$(jq -c . <<<"$config")
    [[ "$config" != *[\'\$]* ]] || die "the product config cannot be quoted in the overlay"
    digest=$(printf '%s' "$config" | sha256sum | cut -c1-12)
    cat >"$tmp/product-overlay.yml" <<YAML
services:
  product:
    environment:
      SSL_CERT_FILE: /etc/test-tls/ca.pem
    networks:
      default:
        aliases: [product]
    configs: !override
      - source: rehearsal_product_$digest
        target: /etc/product/config.json
      - source: rehearsal_tls_certificate
        target: /etc/test-tls/ca.pem
  # As in topup's overlay (cvm-rehearsal.compose.yml): the custom domain needs a real CVM.
  dstack-ingress:
    profiles: [cvm]
configs:
  rehearsal_product_$digest:
    content: '$config'
  rehearsal_tls_certificate:
    content: |
$(sed 's/^/      /' "$TOPUP_TEST_TLS_DIR/ca.pem")
networks:
  default:
    name: ${project}_default
    external: true
YAML
}
product_overlay https://pending.invalid
docker compose -f "$tmp/product.yml" config --variables |
    awk 'NR > 1 && NF > 0 { print $1 "=" }' >"$tmp/product.env"
[[ "$(<"$tmp/product.env")" == PRODUCT_API_KEY= ]] || die "the unsealed product env is not only an empty key"
pc up -d >/dev/null
product_healthy() {
    [[ "$(http_status http://product:8089/healthz)" == 200 ]]
}
wait_for "the product's /healthz" 90 product_healthy
echo "ok: the unsealed product serves /healthz; it pins its webhook key once the key is sealed"
# Like the provisioning run's public-URL upgrade: a config with new content, named after its
# digest, must recreate the container with the new config.
provisional=$(pc ps -q product)
product_overlay http://product:8089
pc up -d >/dev/null
[[ "$(pc ps -q product)" != "$provisional" ]] || die "a changed product setting did not recreate the container"
pc exec -T product cat /etc/product/config.json | jq -e '.public_url == "http://product:8089"' >/dev/null ||
    die "the recreated product does not read the re-rendered public URL"
wait_for "the product's /healthz after the public URL" 90 product_healthy
echo "ok: a re-rendered setting recreated the product with the new config"
api_key=$(docker exec "$client" cat /opt/product.key)
sed -i "s/^PRODUCT_API_KEY=\$/PRODUCT_API_KEY=$api_key/" "$tmp/product.env"
unset api_key
pc up -d >/dev/null
wait_for "the product's /healthz after sealing" 90 product_healthy
jq -n --arg factory "$factory" --arg implementation "$implementation" --arg token "$token" \
    --arg payer "$owner" --arg treasury "$treasury" --arg account "$account" \
    '{service_url: "http://topup:8080", account: $account,
      factory: $factory, implementation: $implementation,
      chains: [{chain_id: 11155111, name: "Sepolia", rpc_url: "http://anvil:8545",
        treasury: $treasury, test_tokens: [{symbol: "PHA", address: $token}]}],
      public_url: "http://product:8089", payer: $payer}' |
    docker exec -i "$client" sh -c 'cat >/opt/driver.json'
product_python -m reference_product deposit --config /opt/driver.json \
    --driver-seed-file /opt/driver.seed --amount-minor 2500 --timeout 420
echo "ok: the deposit driver's quote-first deposit is credited once in the product's ledger"
# Capture the actual receiver ledger, resend through the service's signed delivery worker,
# wait for acknowledgement, then compare the same rows. A queued event alone is not proof.
ledger_credits() {
    pc exec -T product /opt/venv/bin/python - <<'PYTHON'
import json, sqlite3
with sqlite3.connect("file:/data/ledger.sqlite3?mode=ro", uri=True) as db:
    rows = db.execute("SELECT id, team_id, order_id, amount_minor FROM credit_transactions ORDER BY id").fetchall()
    assert len(rows) == 1 and rows[0][3] == 2500, rows
    bonuses = db.execute("SELECT id, team_id, order_id, amount_minor, reason FROM bonus_credits ORDER BY id").fetchall()
    adjustments = db.execute("SELECT id, team_id, order_id, amount_minor, reason FROM credit_adjustments ORDER BY id").fetchall()
    orders = db.execute("SELECT id, team_id, provider_order_id, credit_transaction_id, status FROM orders ORDER BY id").fetchall()
    assert len(orders) == 1, orders
    balance = sum(row[3] for row in rows + bonuses + adjustments)
    print(json.dumps({"credits": rows, "bonuses": bonuses, "adjustments": adjustments,
                      "orders": orders, "balance_minor": balance}))
PYTHON
}
credits_before=$(ledger_credits)
product_python - <<'PYTHON'
import time
from pathlib import Path
import httpx

with httpx.Client(base_url="http://topup:8080", timeout=30,
                  headers={"Authorization": "Bearer " + Path("/opt/product.key").read_text()}) as api:
    response = api.get("/v1/events", params={"type": "deposit.credited", "limit": 100})
    response.raise_for_status()
    events = [event for event in response.json()["data"] if event["type"] == "deposit.credited"]
    assert len(events) == 1, events
    event_id = events[0]["id"]
    response = api.get("/v1/webhook_endpoints")
    response.raise_for_status()
    endpoints = response.json()["data"]
    assert len(endpoints) == 1, endpoints
    response = api.post(f"/v1/events/{event_id}/resend",
                        json={"webhook_endpoint": endpoints[0]["id"]})
    response.raise_for_status()
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        response = api.get(f"/v1/events/{event_id}")
        response.raise_for_status()
        if response.json()["pending_webhooks"] == 0:
            break
        time.sleep(1)
    else:
        raise RuntimeError("signed webhook redelivery was not acknowledged")
PYTHON
[[ "$(ledger_credits)" == "$credits_before" ]] || die "redelivery changed the receiver's credit"
echo "ok: a signed deposit.credited redelivery was acknowledged without a second credit"

# The route prices from local Chainlink and Uniswap fixtures, so a priced lock proves the
# hermetic pinned-read path without contacting a live exchange or public RPC.
priced_locks=$(dc exec -T postgres psql -U postgres -d topup -XAtq -c \
    "SELECT count(*) FROM quotes WHERE route = 'phala-cloud-sepolia-pha-usd' AND price_scaled > 0")
((priced_locks >= 1)) || die "no rate lock was priced from the local on-chain fixtures"
echo "ok: topup priced a lock from hermetic Chainlink/Uniswap fixtures"

echo "== workload memory (tdx.medium has 4 GiB)"
dc ps -q keys postgres topup heartbeat backup |
    xargs docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' | tee "$tmp/memory"
echo "cvm-rehearsal: all checks passed"
