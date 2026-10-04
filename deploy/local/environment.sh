#!/usr/bin/env bash
# Writes a local stack's environment directory (deploy/render.sh ENV_DIR) into OUT_DIR: the local
# overlay settings of deploy/local/environment/compose.yaml, and a topup.yaml with local public
# settings over the routes of Phala's staging configuration (the `routes:` section, verbatim), so
# the local stacks load exactly the committed routes. The providers are unreachable placeholders;
# callers that run a chain write their own topup.yaml.
#
# Usage: deploy/local/environment.sh [--admin-key-id ID] [--admin-public-key BASE64] OUT_DIR
set -euo pipefail

root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
admin_key_id=local-admin/v1
# The public key of the local stacks' well-known development admin key.
admin_public_key=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=
while (($#)); do
    case "$1" in
        --admin-key-id) admin_key_id=${2:?}; shift 2 ;;
        --admin-public-key) admin_public_key=${2:?}; shift 2 ;;
        -*) echo "usage: $0 [--admin-key-id ID] [--admin-public-key BASE64] OUT_DIR" >&2; exit 64 ;;
        *) break ;;
    esac
done
out=${1:?usage: $0 [--admin-key-id ID] [--admin-public-key BASE64] OUT_DIR}
mkdir -p "$out"
cp "$root/deploy/local/environment/compose.yaml" "$out/compose.yaml"
{
    cat <<YAML
environment: local
public_origin: https://topup.localhost
admin_key:
  id: $admin_key_id
  public_key: $admin_public_key
YAML
    sed -n '/^rpc_companies:/,/^routes:$/p' "$root/deploy/environments/phala-network/staging/topup/topup.yaml" |
        sed '$d; s|domains:|domains:|; s|tenderly.co|127.0.0.1|g; s|publicnode.com|localhost|g; s|https://sepolia.gateway.127.0.0.1|http://127.0.0.1:1|; s|https://base-sepolia.gateway.127.0.0.1|http://127.0.0.1:3|; s|https://ethereum-sepolia-rpc.localhost|http://localhost:2|; s|https://base-sepolia-rpc.localhost|http://localhost:4|; s|https://mainnet.gateway.127.0.0.1|http://127.0.0.1:5|; s|https://ethereum-rpc.localhost|http://localhost:6|; s|https://base.gateway.127.0.0.1|http://127.0.0.1:7|; s|https://base-rpc.localhost|http://localhost:8|'
    sed -n '/^routes:$/,$p' "$root/deploy/environments/phala-network/staging/topup/topup.yaml"
} >"$out/topup.yaml"
