#!/usr/bin/env bash
# Read-only Base RPC; owner/compose/device and both reviewed EIP-1967 implementation pins.
set -euo pipefail
[[ $# == 4 ]] || { echo "usage: $0 DSTACK_APP SAFE COMPOSE_HASH DEVICE_ID" >&2; exit 64; }
exec node "$(dirname -- "$0")/tools/onchain-kms.mjs" verify "$@"
