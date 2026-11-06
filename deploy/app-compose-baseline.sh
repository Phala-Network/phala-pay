#!/bin/sh
# Read the reviewed full Base app-compose; never use Cloud's response as a template.
# Sourced by phala-onchain.sh and verify-attestation.sh.
load_app_compose_baseline() {
    if [ -f "$1/app-compose.json" ]; then
        jq -e 'select(type == "object")' "$1/app-compose.json" >"$2"
    elif [ -f "$1/provision.json" ]; then
        jq -e '.compose_file | select(type == "object")' "$1/provision.json" >"$2"
    else
        echo "Base KMS requires reviewed app-compose.json or provision.json compose_file" >&2
        return 1
    fi
}
