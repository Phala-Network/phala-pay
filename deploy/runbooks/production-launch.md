# Production launch

This is the ordered, **HUMAN-ONLY** launch procedure for Phala's production stablecoin pilot. The
committed routes are live USDC and USDT on Ethereum (chain 1) and live USDC on Base (chain 8453).
Base USDC has a sequencer uptime grace period and uses a separately deployed Base factory.
Production and staging share the Ankr and Infura accounts.

> **Screening launch gates:** Before enabling production charges, verify a fresh OFAC SDN
> snapshot and a sanctions smoke check proving a known SDN hit. Add and audit all four
> [EU/UK supplements](sanctions-list.md#launch-supplements-from-eu-and-uk-lists), and verify that
> destination re-screening completed successfully. The deprecated oracle addresses remain only
> for rollback compatibility; a passing binary/database rollback drill is not screening approval.

The approved parallel pilot limits are production D=60, R=2, N=1, factory=60/day and Safe=10/day;
staging D=20, R=1, N=1, factory=20/day and Safe=2/day. Each environment has H=80 hint tasks/day,
Q=60 fresh quote snapshots per price chain/UTC day and a 150-call extra reserve per endpoint/day.
R counts concurrent attached pending refunds; N counts new attachments in a rolling 24 hours.
The permanent `ISSUED_ADDRESS_CAP=1000` counts all historical addresses per payment chain.
Production contributes three custody routes and staging six, nine in total. Monitor
`topup_rpc_endpoint_ready`, `topup_rpc_errors_total`, `topup_coverage_lag_seconds`,
`topup_addresses_lagging`, `topup_daily_budget_used`, Sentry RPC alerts, daily reports, and provider
dashboards. D, factory and Safe are operational caps with no code enforcement: tally work,
including recovery and failed proof attempts, and stop new load at each limit. Stop adding
merchants or other load when Ankr's seven-day run rate reaches 25,000 calls/day, Infura reaches
1,500,000 daily credits, or a chain approaches the address cap. Follow
[RPC operating modes and stop actions](../RPC.md#pilot-limits-and-operating-modes) before resuming.

Finance must sign off every route amount and the explicit `max_unfinalized_credit` pilot value
before upgrade and again before enabling charges. The amount defaults copied into `topup.yaml` are
illustrative until Finance confirms them.

Run commands that create or change cloud resources, DNS, contracts, GitHub settings, or secrets
from the operator's release checkout. Never commit a private key or an env file.

> **One shell session:** Run §§7–10 in one shell session. If a new session is unavoidable, re-export
> `APP_ID`, `ATTESTED_APP_ID`, `COMPOSE_HASH`, and `ORIGIN`, re-define the `admin` helper, and
> recreate `SMOKE_DEPOSIT_IDS` before continuing.

## 1. Create the workspace and GitHub Environment

Create the production Phala Cloud workspace and an API key scoped to it. Confirm that it offers
`dstack-0.5.9` on a `tdx.medium` node. Create the protected GitHub Environment named `production`,
restrict it to `main`, and add the required secrets and variables without printing key values:

```sh
export GITHUB_REPOSITORY=Phala-Network/phala-pay
export PHALA_WORKSPACE="<production workspace display name>"
gh api --method PUT "repos/$GITHUB_REPOSITORY/environments/production" --input - <<'JSON'
{"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}}
JSON
gh api --method POST "repos/$GITHUB_REPOSITORY/environments/production/deployment-branch-policies" \
  -f name=main
gh secret set PHALA_CLOUD_API_KEY --repo "$GITHUB_REPOSITORY" --env production < phala-cloud-production.api-key
gh secret set SENTRY_DSN --repo "$GITHUB_REPOSITORY" --env production < production-sentry-dsn
gh variable set PHALA_WORKSPACE --repo "$GITHUB_REPOSITORY" --env production --body "$PHALA_WORKSPACE"
gh variable set TOPUP_MAINTENANCE_KEY_ID --repo "$GITHUB_REPOSITORY" --env production \
  --body maintenance/production-v1
gh secret set TOPUP_MAINTENANCE_PRIVATE_KEY_PEM --repo "$GITHUB_REPOSITORY" --env production \
  < maintenance/production.pem
```

The public admin key (`admin/production-v1`) and maintenance public key
(`maintenance/production-v1`) are committed in `topup.yaml`. Keep the corresponding admin seed and
maintenance PEM offline. Set `TOPUP_CVM_ID` only after provisioning in step 4.

Set up production monitoring once, following [Deploy README one-time setup item 5](../README.md#one-time-setup-human-only-repository-owner): keep Sentry Data Scrubber, Use Default Scrubbers,
and Prevent Storing of IP Addresses on; alert on issue creation/regression for `staging` and
`production` (never `*-restore`) without a level filter; confirm Crons monitors connect; create an
Uptime monitor for `https://pay-api.phala.com/healthz` with one-minute interval, ten-second timeout,
and Environment `production`; and seal the project DSN as the `SENTRY_DSN` Environment secret.

## 2. Create the production R2 bucket

The owner must create the empty **`phala-pay-production`** bucket before provisioning. The
committed prefix is `s3://phala-pay-production/production-v1`, never staging's prefix. First create
the bucket with account-level R2 credentials, then issue a dedicated read-write token and a separate
Object Read only token for restore drills:

```sh
export AWS_ENDPOINT=https://1d694c298092ffa09c793cbca4587812.r2.cloudflarestorage.com
read -rs -p "R2 account-level access key: " AWS_ACCESS_KEY_ID; echo
export AWS_ACCESS_KEY_ID
read -rs -p "R2 account-level secret key: " AWS_SECRET_ACCESS_KEY; echo
export AWS_SECRET_ACCESS_KEY
aws s3api create-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT" --region auto
aws s3api head-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT"
# In the Cloudflare R2 dashboard/API, issue the production read-write and restore Object Read only tokens.
```

The service uses `AWS_REGION=auto` and path-style addressing. Seal the read-write values only after
step 6 preflight; restore-check uses `RESTORE_AWS_ACCESS_KEY_ID` and
`RESTORE_AWS_SECRET_ACCESS_KEY` from the read-only token.

## 3. Deploy and verify the factories

Use a checkout of Phala Pay at the release tag, with submodules, for the contract scripts. This is
a human broadcast; keep `PRIVATE_KEY` only in the operator's process environment:

```sh
cd /path/to/phala-pay-release
git checkout "<release tag>"
git submodule update --init --recursive
read -rs -p "Factory deployer private key: " PRIVATE_KEY && printf '\n'
export PRIVATE_KEY
export MAINNET_RPC_A="<ethereum-mainnet-rpc-a>" MAINNET_RPC_B="<ethereum-mainnet-rpc-b>"
export BASE_RPC_A="<base-mainnet-rpc-a>" BASE_RPC_B="<base-mainnet-rpc-b>"

deploy/contracts/deploy-proxy.sh --rpc-url "$MAINNET_RPC_A"
deploy/contracts/deploy-factory.sh --rpc mainnet/a="$MAINNET_RPC_A" --dry-run
deploy/contracts/deploy-factory.sh --rpc mainnet/a="$MAINNET_RPC_A" --broadcast
deploy/contracts/verify-deployment.sh --rpc mainnet/a="$MAINNET_RPC_A" --rpc mainnet/b="$MAINNET_RPC_B" > mainnet-contract-verification.json
jq -e '.passed == true' mainnet-contract-verification.json

deploy/contracts/deploy-proxy.sh --rpc-url "$BASE_RPC_A"
deploy/contracts/deploy-factory.sh --rpc base/a="$BASE_RPC_A" --dry-run
deploy/contracts/deploy-factory.sh --rpc base/a="$BASE_RPC_A" --broadcast
deploy/contracts/verify-deployment.sh --rpc base/a="$BASE_RPC_A" --rpc base/b="$BASE_RPC_B" > base-contract-verification.json
jq -e '.passed == true' base-contract-verification.json
unset PRIVATE_KEY
```

Keep both verification reports. A factory, implementation, forwarder, or runtime code-hash mismatch
blocks the launch.

## 4. Provision the topup CVM

From the environment repository's `main`, dispatch provisioning exactly once. Record a timestamp
and use the exact workflow run returned by the API query; never select the newest unrelated run:

```sh
provision_topup() {
  PROVISION_REQUESTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" || return 1
  export PROVISION_REQUESTED_AT
  gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
    -f environment=production -f target=topup -f mode=provision || return 1
  PROVISION_RUN_ID=""
  PROVISION_LOOKUP_DEADLINE=$((SECONDS + 300))
  while [[ -z "$PROVISION_RUN_ID" && SECONDS -lt $PROVISION_LOOKUP_DEADLINE ]]; do
    PROVISION_RUN_ID="$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/deploy-phala.yml/runs" \
      -f event=workflow_dispatch -f branch=main -f "created=>=$PROVISION_REQUESTED_AT" -F per_page=100 \
      --jq '.workflow_runs | sort_by(.created_at) | last | .id' 2>/dev/null || true)"
    [[ -n "$PROVISION_RUN_ID" ]] || sleep 5
  done
  [[ -n "$PROVISION_RUN_ID" ]] || {
    echo "timed out waiting for the provisioning workflow run; stop here" >&2
    return 1
  }
  gh run watch "$PROVISION_RUN_ID" --repo "$GITHUB_REPOSITORY" || return 1
  gh run view "$PROVISION_RUN_ID" --repo "$GITHUB_REPOSITORY" || return 1
  # Copy TOPUP_CVM_ID from the run summary; it must never enter committed config.
  TOPUP_CVM_ID="<cvm-id-from-provision-summary>"
  export TOPUP_CVM_ID
  gh variable set TOPUP_CVM_ID --repo "$GITHUB_REPOSITORY" --env production --body "$TOPUP_CVM_ID" || return 1
}
provision_topup || {
  echo "Provisioning did not complete; stop here and do not run later steps" >&2
  false
}
```

The provisioning run summary also prints the CNAME and TXT records used in step 5.

## 5. Create DNS records from the provision summary

Copy the exact CNAME and TXT records printed in the provision run summary. The TXT value uses the
`instance_id` from the attestation, not `TOPUP_CVM_ID`:

```sh
export GATEWAY_DOMAIN="<base_domain-from-provision-summary>"
export CNAME_VALUE="<copy-the-CNAME-value-exactly-as-printed-in-the-provision-summary>"
export INSTANCE_ID="<instance_id-from-the-attestation-in-the-summary>"
echo "CNAME pay-api.phala.com $CNAME_VALUE"
echo "TXT _dstack-app-address.pay-api.phala.com $INSTANCE_ID:443"
dig +short CNAME pay-api.phala.com
dig +short TXT _dstack-app-address.pay-api.phala.com
```

Create `pay-api.phala.com -> $CNAME_VALUE` and
`_dstack-app-address.pay-api.phala.com -> $INSTANCE_ID:443` with the DNS provider. Wait for both
records before upgrading. The committed `public_origin` and ingress `DOMAIN` are
`https://pay-api.phala.com` and `pay-api.phala.com`.

## 6. Render, preflight, seal, and upgrade

Use the release kit's digest-pinned `images.json`. `render.sh` receives the gateway host computed
by `phala-cvm.sh`: `gateway.<base_domain>`. Retain the rendered compose and use the compose artifact
attached to the upgrade run as the final attestation record:

```sh
export ENV_DIR=deploy/environments/phala-network/production/topup
export GATEWAY_HOST="gateway.$GATEWAY_DOMAIN"
kit/deploy/render.sh --images images.json --gateway-domain "$GATEWAY_HOST" "$ENV_DIR" > docker-compose.production.yml
umask 077
: > .env.production.unsealed
${EDITOR:-vi} .env.production.unsealed
kit/deploy/preflight.sh --env .env.production.unsealed --compose docker-compose.production.yml \
  --environment-dir "$ENV_DIR" --offline --unsealed
: > .env.production
${EDITOR:-vi} .env.production
kit/deploy/preflight.sh --env .env.production --compose docker-compose.production.yml \
  --environment-dir "$ENV_DIR" --offline --require-sentry
```

The unsealed file and complete file must each contain exactly `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `SENTRY_DSN`, `TOPUP_RPC_ANKR_KEY`, and `TOPUP_RPC_INFURA_KEY`; the first
has empty values and the second has the production values. Use the editor, never a heredoc or
`printf` containing credentials. Finance's amount and `max_unfinalized_credit` approvals are hard
gates before this upgrade preflight.

```sh
upgrade_topup() {
  kit/deploy/phala envs update "$TOPUP_CVM_ID" -e .env.production || return 1
  UPGRADE_REQUESTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" || return 1
  export UPGRADE_REQUESTED_AT
  gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
    -f environment=production -f target=topup -f mode=upgrade || return 1
  UPGRADE_RUN_ID=""
  UPGRADE_LOOKUP_DEADLINE=$((SECONDS + 300))
  while [[ -z "$UPGRADE_RUN_ID" && SECONDS -lt $UPGRADE_LOOKUP_DEADLINE ]]; do
    UPGRADE_RUN_ID="$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/deploy-phala.yml/runs" \
      -f event=workflow_dispatch -f branch=main -f "created=>=$UPGRADE_REQUESTED_AT" -F per_page=100 \
      --jq '.workflow_runs | sort_by(.created_at) | last | .id' 2>/dev/null || true)"
    [[ -n "$UPGRADE_RUN_ID" ]] || sleep 5
  done
  [[ -n "$UPGRADE_RUN_ID" ]] || {
    echo "timed out waiting for the upgrade workflow run; stop here" >&2
    return 1
  }
  gh run watch "$UPGRADE_RUN_ID" --repo "$GITHUB_REPOSITORY" || return 1
  gh run view "$UPGRADE_RUN_ID" --repo "$GITHUB_REPOSITORY" || return 1
  shred -u .env.production.unsealed .env.production || return 1
}
upgrade_topup || {
  echo "Upgrade did not complete; stop here and do not run later steps" >&2
  false
}
```

## 7. Verify attestation, ingress, and health

Use the upgrade run's rendered compose artifact and verify the guest attestation and certificate:

```sh
UPGRADE_ATTEMPT="$(gh run view "$UPGRADE_RUN_ID" --repo "$GITHUB_REPOSITORY" --json attempt --jq .attempt)"
ART="$(mktemp -d)"
gh run download "$UPGRADE_RUN_ID" --repo "$GITHUB_REPOSITORY" \
  -n "production-topup-deploy-$UPGRADE_RUN_ID-$UPGRADE_ATTEMPT" -D "$ART"
mapfile -t UPGRADE_COMPOSE_FILES < <(find "$ART" -type f -name 'docker-compose.*.yml' -print)
test "${#UPGRADE_COMPOSE_FILES[@]}" -eq 1
export UPGRADE_COMPOSE="${UPGRADE_COMPOSE_FILES[0]}"
kit/deploy/phala cvms get "$TOPUP_CVM_ID" --json > cvm.json
kit/deploy/phala cvms attestation "$TOPUP_CVM_ID" --json > attestation.json
export APP_ID="$(jq -er '.app_id' cvm.json)"
export ATTESTED_APP_ID="$(jq -er '.app_id | ltrimstr("0x") | ascii_downcase' cvm.json)"
export COMPOSE_HASH="$(jq -j '.compose_file' attestation.json | sha256sum | cut -d' ' -f1)"
export INSTANCE_ID="$(jq -er '[.tcb_info.event_log[] | select(.event == "instance-id") | .event_payload | ascii_downcase | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' attestation.json)"
export GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' cvm.json)"
curl -fsS "https://${INSTANCE_ID}-8090.$GATEWAY_DOMAIN/prpc/Info" > info.json
EXPECTED_OS_IMAGE_HASH="$(cat deploy/environments/phala-network/production/topup/os-image-hash)"
kit/deploy/verify-attestation.sh attestation.json info.json "$APP_ID" "$UPGRADE_COMPOSE" service "$EXPECTED_OS_IMAGE_HASH"
kit/deploy/verify-ingress-evidence.sh pay-api.phala.com "$APP_ID"
test "$(curl -fsS -o /dev/null -w '%{http_code}' https://pay-api.phala.com/healthz)" = 200
```

Stop if the attested compose, app id, TCB, certificate evidence, or health response does not match
the release.

## 8. Create the dedicated smoke account and run three live payments

This account is owned by Phala and exists only to prove production routes. Define the origin before
merchant requests and initialize the admin helper as in [runbooks/README.md#environment](README.md#environment):

```sh
export ORIGIN=https://pay-api.phala.com
export BASE_URL="$ORIGIN"
export ADMIN_KEY_FILE=admin.pem ADMIN_KEY_ID=admin/production-v1
(umask 077 && { printf '302e020100300506032b657004220420'; tr -d '\n' < admin.seed; } | xxd -r -p | openssl pkey -inform DER -out "$ADMIN_KEY_FILE")
admin() {
  printf '%s' "${3:-}" > /tmp/topup-admin-body
  mapfile -t headers < <(deploy/runbooks/sign-admin-request.sh "$1" "$BASE_URL$2" /tmp/topup-admin-body "$ADMIN_KEY_FILE" "$ADMIN_KEY_ID")
  curl --fail-with-body -sS -X "$1" -H 'content-type: application/json' -H "${headers[0]}" -H "${headers[1]}" -H "${headers[2]}" --data-binary @/tmp/topup-admin-body "$BASE_URL$2"
}

prove_live_treasuries() {
  local merchant_key=$1 eth_safe=$2 base_safe=$3 prefix=$4 safe_address challenge_message safe_signature CHAIN_ID
  for CHAIN_ID in 1 8453; do
    safe_address="$eth_safe"; [ "$CHAIN_ID" = 8453 ] && safe_address="$base_safe"
    curl -fsS -X POST "$ORIGIN/v1/treasuries/challenge" \
      -H "Authorization: Bearer $merchant_key" -H 'content-type: application/json' \
      -d "{\"chain_id\":$CHAIN_ID,\"address\":\"$safe_address\"}" \
      > "${prefix}-treasury-challenge-$CHAIN_ID.json" || return 1
    challenge_message="$(jq -er '.message' "${prefix}-treasury-challenge-$CHAIN_ID.json")" || return 1
    safe_signature="<Safe{Core}-threshold-signature-for-the-unchanged-message>"
    (
      set -o pipefail
      jq -cn --arg message "$challenge_message" --arg signature "$safe_signature" \
        --argjson chain_id "$CHAIN_ID" '{chain_id:$chain_id,message:$message,signature:$signature}' \
        | curl -fsS -X POST "$ORIGIN/v1/treasuries" \
          -H "Authorization: Bearer $merchant_key" -H 'content-type: application/json' \
          --data-binary @-
    ) || return 1
  done
}

configure_live_payment_settings() {
  local merchant_key=$1
  curl -fsS -X POST "$ORIGIN/v1/payment_settings" \
    -H "Authorization: Bearer $merchant_key" -H 'content-type: application/json' \
    -d '{"chains":[{"chain_id":1,"assets":[{"asset":"usdc"},{"asset":"usdt"}]},{"chain_id":8453,"assets":[{"asset":"usdc"}]}]}'
}

register_live_webhook() {
  local merchant_key=$1
  curl -fsS -X POST "$ORIGIN/v1/webhook_endpoints" \
    -H "Authorization: Bearer $merchant_key" -H 'content-type: application/json' \
    -d '{"url":"<https webhook receiver>","enabled_events":["*"]}'
}

pin_live_webhook_keys() {
  local merchant_key=$1 account_id=$2 prefix=$3 nonce
  local report_data
  nonce="$(openssl rand -hex 32)" || return 1
  curl -fsS -H "Authorization: Bearer $merchant_key" "$ORIGIN/v1/attestation?nonce=$nonce" \
    > "${prefix}-public-attestation.json" || return 1
  jq '{quote: null, attestation: .tdx_quote}' "${prefix}-public-attestation.json" \
    | deploy/dstack-verifier.sh > "${prefix}-public-verification.json" || return 1
  report_data="$(jq -er '.report_data' "${prefix}-public-attestation.json")" || return 1
  jq -e --arg app "$ATTESTED_APP_ID" --arg compose "$COMPOSE_HASH" \
    --arg report_data "$report_data" \
    '.details.tcb_status == "UpToDate" and .details.app_info.app_id == $app
     and .details.app_info.compose_hash == $compose
     and .details.report_data == $report_data + ("0" * 64)' \
    "${prefix}-public-verification.json" || return 1
  NONCE="$nonce" ACCOUNT_ID="$account_id" ATTESTATION_FILE="${prefix}-public-attestation.json" \
    uv run --project sdk/python --locked python -c '
import json, os
from topup_client.models import AttestationResponse
from topup_sdk import verify_attestation_binding
with open(os.environ["ATTESTATION_FILE"]) as stream:
    response = AttestationResponse.from_dict(json.load(stream))
verify_attestation_binding(
    response,
    bytes.fromhex(os.environ["NONCE"]),
    expected_account=os.environ["ACCOUNT_ID"],
    expected_livemode=True,
)
print("pin these verified live webhook keys:", [key.to_dict()["public_key"] for key in response.webhook_keys])' || return 1
}
```

Create the smoke account with charges disabled. Finance must confirm the explicit pilot value;
template-copied amounts remain illustrative. Complete the verified screening gates and every
other launch gate before enabling charges:

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name:"Phala production smoke",contact:{name:"<name>",email:"<security email>"},due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},charges_enabled:false,reason:"production route smoke account"}')" > smoke-account.json)
export SMOKE_ACCOUNT_ID="$(jq -er '.id' smoke-account.json)"
export FINANCE_MAX_UNFINALIZED_CREDIT="<Finance-confirmed-integer-cents>"
(umask 077 && admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID" \
  "{\"max_unfinalized_credit\":$FINANCE_MAX_UNFINALIZED_CREDIT,\"reason\":\"Finance-approved pilot value\"}" \
  > smoke-account-limits.json)
```

After the attestation, contract, sanctions, and Finance gates pass, enable charges and save the
response under mode 0600. The response contains the first live key; extract it without printing it:

```sh
(umask 077 && admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID" \
  '{"charges_enabled":true,"reason":"verified screening and Finance gates approved"}' \
  > smoke-live.json)
export SMOKE_MERCHANT_SECRET_KEY="$(jq -er '.api_keys[] | select(.livemode == true and .type == "secret") | .secret' smoke-live.json)"
shred -u smoke-account.json smoke-account-limits.json smoke-live.json
```

With that live key, prove the Ethereum and Base Safe treasuries, configure exactly the three
committed live assets, register the live webhook endpoint, and pin its keys from the verified live
attestation. The owner signing steps are in [integration §1.6](../../docs/integration.md#16-treasuries):

```sh
export ETH_TREASURY_SAFE="<ethereum-smoke-safe>" BASE_TREASURY_SAFE="<base-smoke-safe>"
prove_live_treasuries "$SMOKE_MERCHANT_SECRET_KEY" "$ETH_TREASURY_SAFE" "$BASE_TREASURY_SAFE" smoke &&
  configure_live_payment_settings "$SMOKE_MERCHANT_SECRET_KEY" &&
  register_live_webhook "$SMOKE_MERCHANT_SECRET_KEY" &&
  pin_live_webhook_keys "$SMOKE_MERCHANT_SECRET_KEY" "$SMOKE_ACCOUNT_ID" smoke || {
  echo "smoke live configuration failed; stop" >&2
  false
}
```

Only after those live configuration steps pass, create the three required $1 quotes. Each payment
uses a Foundry keystore and waits with a bounded 15-minute timeout for a credited deposit and signed
webhook. On any payment or wait failure, stop and do not continue to the next payment. Base is a
separate factory and sequencer-gated:

```sh
export PAYER_ACCOUNT="<foundry-keystore-account>" PAYER_PASSWORD_FILE="<mode-0600-keystore-password-file>"
wait_for_credit() {
  local quote_id=$1 deadline=$((SECONDS + 900)) response
  while (( SECONDS < deadline )); do
    if response="$(curl -fsS "$ORIGIN/v1/quotes/$quote_id?expand[]=deposit" \
      -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY")" \
      && jq -e '.status == "complete" and .deposit.status == "credited"' <<<"$response" >/dev/null; then
      CREDITED_DEPOSIT_ID="$(jq -er '.deposit.id' <<<"$response")"
      return 0
    fi
    sleep 10
  done
  echo "quote $quote_id was not credited within 900 seconds" >&2
  return 1
}

run_smoke_payments() {
QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -H 'Idempotency-Key: production-smoke-ethereum-usdc-1' -d '{"client_reference_id":"production-smoke-ethereum-usdc","amount":100,"currency":"usd","chain_id":1,"asset":"usdc"}')"; QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"; ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"; AMOUNT="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" --rpc-url "$MAINNET_RPC_A" 0xA0b86991c6218b36c1d19d4a2e9eb0ce3606eb48 'transfer(address,uint256)' "$ADDRESS" "$AMOUNT" || { echo "smoke payment 1 failed; stop" >&2; false; } || return 1; wait_for_credit "$QUOTE_ID" || { echo "smoke payment 1 failed; stop" >&2; false; } || return 1; export SMOKE_ETH_USDC_DEPOSIT_ID="$CREDITED_DEPOSIT_ID"
QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -H 'Idempotency-Key: production-smoke-ethereum-usdt-1' -d '{"client_reference_id":"production-smoke-ethereum-usdt","amount":100,"currency":"usd","chain_id":1,"asset":"usdt"}')"; QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"; ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"; AMOUNT="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" --rpc-url "$MAINNET_RPC_A" 0xdAC17F958D2ee523a2206206994597C13D831ec7 'transfer(address,uint256)' "$ADDRESS" "$AMOUNT" || { echo "smoke payment 2 failed; stop" >&2; false; } || return 1; wait_for_credit "$QUOTE_ID" || { echo "smoke payment 2 failed; stop" >&2; false; } || return 1; export SMOKE_ETH_USDT_DEPOSIT_ID="$CREDITED_DEPOSIT_ID"
QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -H 'Idempotency-Key: production-smoke-base-usdc-1' -d '{"client_reference_id":"production-smoke-base-usdc","amount":100,"currency":"usd","chain_id":8453,"asset":"usdc"}')"; QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"; ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"; AMOUNT="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" --rpc-url "$BASE_RPC_A" 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 'transfer(address,uint256)' "$ADDRESS" "$AMOUNT" || { echo "smoke payment 3 failed; stop" >&2; false; } || return 1; wait_for_credit "$QUOTE_ID" || { echo "smoke payment 3 failed; stop" >&2; false; } || return 1; export SMOKE_BASE_USDC_DEPOSIT_ID="$CREDITED_DEPOSIT_ID"
}
run_smoke_payments || { echo "smoke payment sequence failed; stop" >&2; false; }
```

Record the three deposits' original state before starting the restore drill. The restore check must
return the same state for each id:

```sh
SMOKE_DEPOSIT_IDS=("$SMOKE_ETH_USDC_DEPOSIT_ID" "$SMOKE_ETH_USDT_DEPOSIT_ID" "$SMOKE_BASE_USDC_DEPOSIT_ID")
for INDEX in "${!SMOKE_DEPOSIT_IDS[@]}"; do
  admin GET "/v1/admin/deposits/${SMOKE_DEPOSIT_IDS[$INDEX]}" > "smoke-deposit-$INDEX-before.json"
  jq -e --arg id "${SMOKE_DEPOSIT_IDS[$INDEX]}" '.id == $id' "smoke-deposit-$INDEX-before.json" >/dev/null
  jq '{id,status,amount,amount_atomic,amount_refunded,amount_refunded_atomic,amount_reversed,replaces,replaced_by,revision}' \
    "smoke-deposit-$INDEX-before.json" > "smoke-deposit-$INDEX-state.json"
done
```

## 9. Run the production restore drill before Phala Cloud charges

Use a separate Object Read only R2 token and follow [RESTORE.md, Staging restore drill](../RESTORE.md#staging-restore-drill)
steps 1–5 with production values. The `live_isolated` check is a hard abort: run it before the
instance exists, immediately after creation, before each restore step, and at least every five
minutes. Any failure means delete the drill instance by its `.vm_uuid` and record it as aborted.
The function and `RETURN` trap below ensure that a failed or successful drill always deletes its
throwaway instance and removes the temporary env file:

```sh
live_isolated() {
  for _ in $(seq 20); do
    test "$(curl -sS -o live-healthz.body -w '%{http_code}' "$ORIGIN/healthz")" = 200 &&
      test ! -s live-healthz.body || return 1
  done
}

restore_drill() {
  local BASE_URL=$BASE_URL
  restore_cleanup() {
    trap - RETURN
    if [[ -n "${RESTORE_CVM_ID:-}" ]]; then
      kit/deploy/phala cvms delete "$RESTORE_CVM_ID" --force ||
        echo "failed to delete restore instance $RESTORE_CVM_ID; delete it by vm_uuid before proceeding" >&2
      unset RESTORE_CVM_ID
    fi
    if [[ -n "${RESTORE_ENV_DIR:-}" && -e "$RESTORE_ENV_DIR/restore.env" ]]; then
      shred -u "$RESTORE_ENV_DIR/restore.env"
      rmdir "$RESTORE_ENV_DIR" || true
      unset RESTORE_ENV_DIR
    fi
    unset PHALA_CLOUD_API_KEY
  }
  trap restore_cleanup RETURN

  read -rs -p "Phala Cloud API key: " PHALA_CLOUD_API_KEY || return 1
  echo
  export PHALA_CLOUD_API_KEY
  live_isolated || { restore_cleanup; return 1; }

  RESTORE_ENV_DIR="$(mktemp -d)" || return 1
  export RESTORE_ENV_DIR
  umask 077
  : > "$RESTORE_ENV_DIR/restore.env"
  ${EDITOR:-vi} "$RESTORE_ENV_DIR/restore.env" || return 1
  # The file contains exactly the restore-check sealed names, including the read-only R2 token.
  kit/deploy/render.sh --restore-check --images images.json --origin https://pending.invalid \
    "$ENV_DIR" > restore-check.yml || return 1
  kit/deploy/preflight.sh --env "$RESTORE_ENV_DIR/restore.env" --compose restore-check.yml \
    --environment-dir "$ENV_DIR" --restore-check --offline --require-sentry || return 1

  # Restore step 1: create a new instance of the original app with its own read-only env.
  live_isolated || { restore_cleanup; return 1; }
  kit/deploy/phala instances add --app-id "$APP_ID" --compose-file restore-check.yml \
    --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh --env-file "$RESTORE_ENV_DIR/restore.env" \
    --name phala-pay-production-restore --json > restore-instance.json || return 1
  RESTORE_CVM_ID="$(jq -er '.vm_uuid' restore-instance.json)" || return 1
  export RESTORE_CVM_ID

  # Restore step 2: fetch by vm_uuid, address the guest by its attested instance id, and verify.
  live_isolated || { restore_cleanup; return 1; }
  export PHALA_ATTESTATION_URL="https://cloud-api.phala.com/api/v1/cvms/$RESTORE_CVM_ID/attestation"
  curl -fsS -H "X-API-Key: $PHALA_CLOUD_API_KEY" "$PHALA_ATTESTATION_URL" \
    > restore-attestation.json || return 1
  kit/deploy/phala cvms get "$RESTORE_CVM_ID" --json > restore-cvm.json || return 1
  RESTORE_INSTANCE_ID="$(jq -er '[.tcb_info.event_log[] | select(.event == "instance-id") | .event_payload | ascii_downcase | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' restore-attestation.json)" || return 1
  export RESTORE_INSTANCE_ID
  RESTORE_GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' restore-cvm.json)" || return 1
  export RESTORE_GATEWAY_DOMAIN
  curl -fsS "https://${RESTORE_INSTANCE_ID}-8090.$RESTORE_GATEWAY_DOMAIN/prpc/Info" \
    > restore-info.json || return 1
  EXPECTED_OS_IMAGE_HASH="$(cat deploy/environments/phala-network/production/topup/os-image-hash)" || return 1
  kit/deploy/verify-attestation.sh restore-attestation.json restore-info.json "$APP_ID" \
    restore-check.yml restore-check "$EXPECTED_OS_IMAGE_HASH" || return 1

  # Restore step 3: wait for the report, checking live isolation at least every five minutes.
  live_isolated || { restore_cleanup; return 1; }
  export RESTORE_URL="https://${APP_ID#0x}-8081.$RESTORE_GATEWAY_DOMAIN"
  RESTORE_DEADLINE=$((SECONDS + 3600))
  RESTORE_NEXT_ISOLATION=$SECONDS
  while (( SECONDS < RESTORE_DEADLINE )); do
    if (( SECONDS >= RESTORE_NEXT_ISOLATION )); then
      live_isolated || { restore_cleanup; return 1; }
      RESTORE_NEXT_ISOLATION=$((SECONDS + 300))
    fi
    if curl -fsS "$RESTORE_URL/healthz" | tee restore-healthz.json | \
      jq -e '.mode == "read-only" and .restore_check.status == "ok" and .restore_check.post_restore_reconciliation.status == "complete"' >/dev/null; then
      break
    fi
    sleep 15
  done
  (( SECONDS < RESTORE_DEADLINE )) || { echo "restore report did not complete within one hour" >&2; return 1; }

  # Restore step 4: give the drill its own origin and rerun the restore-check variant.
  live_isolated || { restore_cleanup; return 1; }
  kit/deploy/render.sh --restore-check --images images.json --origin "$RESTORE_URL" \
    "$ENV_DIR" > restore-check.yml || return 1
  live_isolated || { restore_cleanup; return 1; }
  kit/deploy/phala deploy --json --cvm-id "$RESTORE_CVM_ID" --compose restore-check.yml \
    --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh --no-public-logs --no-public-sysinfo --wait || return 1

  # The report is rerun after the origin change; keep the same one-hour bound and isolation checks.
  RESTORE_DEADLINE=$((SECONDS + 3600))
  RESTORE_NEXT_ISOLATION=$SECONDS
  while (( SECONDS < RESTORE_DEADLINE )); do
    if (( SECONDS >= RESTORE_NEXT_ISOLATION )); then
      live_isolated || { restore_cleanup; return 1; }
      RESTORE_NEXT_ISOLATION=$((SECONDS + 300))
    fi
    if curl -fsS "$RESTORE_URL/healthz" | tee restore-healthz-final.json | \
      jq -e '.mode == "read-only" and .restore_check.status == "ok" and .restore_check.post_restore_reconciliation.status == "complete"' >/dev/null; then
      break
    fi
    sleep 15
  done
  (( SECONDS < RESTORE_DEADLINE )) || { echo "restore report did not complete after the origin update" >&2; return 1; }

  # Verify the final restore attestation against the compose re-rendered in step 4.
  live_isolated || { restore_cleanup; return 1; }
  curl -fsS -H "X-API-Key: $PHALA_CLOUD_API_KEY" "$PHALA_ATTESTATION_URL" \
    > restore-attestation-final.json || return 1
  RESTORE_INSTANCE_ID="$(jq -er '[.tcb_info.event_log[] | select(.event == "instance-id") | .event_payload | ascii_downcase | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' restore-attestation-final.json)" || return 1
  export RESTORE_INSTANCE_ID
  curl -fsS "https://${RESTORE_INSTANCE_ID}-8090.$RESTORE_GATEWAY_DOMAIN/prpc/Info" \
    > restore-info-final.json || return 1
  EXPECTED_OS_IMAGE_HASH="$(cat deploy/environments/phala-network/production/topup/os-image-hash)" || return 1
  kit/deploy/verify-attestation.sh restore-attestation-final.json restore-info-final.json "$APP_ID" \
    restore-check.yml restore-check "$EXPECTED_OS_IMAGE_HASH" || return 1

  # Restore step 5: verify a nonce-bound admin attestation, the frozen state, and every smoke deposit.
  live_isolated || { restore_cleanup; return 1; }
  BASE_URL="$RESTORE_URL"
  export BASE_URL
  NONCE="$(openssl rand -hex 32)" || return 1
  export NONCE
  admin GET "/v1/admin/attestation?account=$SMOKE_ACCOUNT_ID&livemode=true&nonce=$NONCE" \
    > restore-public-attestation.json || return 1
  jq -e '.compose_file != null' restore-attestation-final.json >/dev/null || return 1
  RESTORE_COMPOSE_HASH="$(jq -j '.compose_file' restore-attestation-final.json | sha256sum | cut -d' ' -f1)" || return 1
  export RESTORE_COMPOSE_HASH
  RESTORE_PUBLIC_REPORT_DATA="$(jq -er '.report_data' restore-public-attestation.json)" || return 1
  export RESTORE_PUBLIC_REPORT_DATA
  jq '{quote: null, attestation: .tdx_quote}' restore-public-attestation.json \
    | deploy/dstack-verifier.sh > restore-public-verification.json || return 1
  jq -e --arg app "$ATTESTED_APP_ID" --arg compose "$RESTORE_COMPOSE_HASH" \
    --arg report_data "$RESTORE_PUBLIC_REPORT_DATA" \
    '.details.tcb_status == "UpToDate" and .details.app_info.app_id == $app
     and .details.app_info.compose_hash == $compose
     and .details.report_data == $report_data + ("0" * 64)' \
    restore-public-verification.json || return 1
  NONCE="$NONCE" ACCOUNT_ID="$SMOKE_ACCOUNT_ID" ATTESTATION_FILE=restore-public-attestation.json \
    uv run --project sdk/python --locked python -c '
import json, os
from topup_client.models import AttestationResponse
from topup_sdk import verify_attestation_binding
with open(os.environ["ATTESTATION_FILE"]) as stream:
    response = AttestationResponse.from_dict(json.load(stream))
verify_attestation_binding(
    response,
    bytes.fromhex(os.environ["NONCE"]),
    expected_account=os.environ["ACCOUNT_ID"],
    expected_livemode=True,
)' || return 1
  admin GET /v1/admin/restore | jq -e '.frozen == true' >/dev/null || return 1
  for INDEX in "${!SMOKE_DEPOSIT_IDS[@]}"; do
    admin GET "/v1/admin/deposits/${SMOKE_DEPOSIT_IDS[$INDEX]}" \
      > "restore-deposit-$INDEX.json" || return 1
    jq -e --slurpfile expected "smoke-deposit-$INDEX-state.json" \
      '({id,status,amount,amount_atomic,amount_refunded,amount_refunded_atomic,amount_reversed,replaces,replaced_by,revision}) == $expected[0]' \
      "restore-deposit-$INDEX.json" >/dev/null || return 1
  done
  return 0
}
restore_drill
```

The restore env must contain exactly `RESTORE_AWS_ACCESS_KEY_ID`, `RESTORE_AWS_SECRET_ACCESS_KEY`,
`SENTRY_DSN`, `TOPUP_RPC_ANKR_KEY`, and `TOPUP_RPC_INFURA_KEY`, using the read-only token. The
final platform attestation must match the original app id and the compose hash of the `--origin
"$RESTORE_URL"` render. The report must be complete, the service must remain frozen, all three
smoke deposits must retain their recorded states, and live isolation must pass throughout. Never
seal live read-write credentials into the drill.

## 10. Onboard Phala Cloud and enable its charges last

Only after the smoke account's three payments and the restore drill pass may Phala Cloud be
onboarded. Open its account with charges disabled and set the same Finance-approved
`max_unfinalized_credit`; template-copied amounts remain illustrative until Finance confirms them:

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name:"Phala Cloud",contact:{name:"<name>",email:"<security email>"},due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},charges_enabled:false,reason:"production onboarding after smoke and restore"}')" > phala-cloud-account.json)
export PHALA_CLOUD_ACCOUNT_ID="$(jq -er '.id' phala-cloud-account.json)"
(umask 077 && admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID" \
  "{\"max_unfinalized_credit\":$FINANCE_MAX_UNFINALIZED_CREDIT,\"reason\":\"Finance-approved pilot value\"}" \
  > phala-cloud-account-limits.json)
```

After the fresh OFAC snapshot, SDN-hit smoke, audited EU/UK supplements and successful re-screening,
Finance sign-off, the smoke payments, the restore
report, and the monitoring gates all pass, enable charges and save the response under mode 0600.
Extract the first live key without printing it:

```sh
(umask 077 && admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID" \
  '{"charges_enabled":true,"reason":"verified screening, Finance, smoke, and restore gates approved"}' \
  > phala-cloud-live.json)
export PHALA_CLOUD_MERCHANT_SECRET_KEY="$(jq -er '.api_keys[] | select(.livemode == true and .type == "secret") | .secret' phala-cloud-live.json)"
```

With that live key, prove Phala Cloud's Ethereum and Base Safe treasuries, configure exactly
Ethereum USDC/USDT and Base USDC, register its live webhook endpoint, and pin its live webhook
keys from the verified attestation:

```sh
export PHALA_ETH_TREASURY_SAFE="<ethereum-phala-cloud-safe>" PHALA_BASE_TREASURY_SAFE="<base-phala-cloud-safe>"
prove_live_treasuries "$PHALA_CLOUD_MERCHANT_SECRET_KEY" "$PHALA_ETH_TREASURY_SAFE" "$PHALA_BASE_TREASURY_SAFE" phala-cloud &&
  configure_live_payment_settings "$PHALA_CLOUD_MERCHANT_SECRET_KEY" &&
  register_live_webhook "$PHALA_CLOUD_MERCHANT_SECRET_KEY" &&
  pin_live_webhook_keys "$PHALA_CLOUD_MERCHANT_SECRET_KEY" "$PHALA_CLOUD_ACCOUNT_ID" phala-cloud || {
  echo "Phala Cloud live configuration failed; stop" >&2
  false
}
```

Only after all live configuration succeeds, hand the account id and live key to Phala Cloud's
recorded contact through an encrypted channel. Never print the key; shred the account and live
response files after the hand-over:

```sh
shred -u phala-cloud-account.json phala-cloud-account-limits.json phala-cloud-live.json
```

## Operator-only inputs that remain

The following values and actions remain outside this PR: the production Phala Cloud workspace and
API key; GitHub `production` Environment values `PHALA_CLOUD_API_KEY`, `PHALA_WORKSPACE`,
`SENTRY_DSN`, `TOPUP_MAINTENANCE_PRIVATE_KEY_PEM`, `TOPUP_MAINTENANCE_KEY_ID`, and post-provision
`TOPUP_CVM_ID`; sealed `SENTRY_DSN`, `TOPUP_RPC_ANKR_KEY`, `TOPUP_RPC_INFURA_KEY`,
`AWS_ACCESS_KEY_ID`, and `AWS_SECRET_ACCESS_KEY`; creation of the `phala-pay-production` bucket,
account-level setup credentials, live read-write token, and restore read-only token; the admin seed
and factory deployer private key plus transaction broadcasts; DNS provider access and gateway-derived
records; smoke and Phala Cloud treasury Safe addresses, owners, and signatures; merchant secret/live
keys, webhook receivers, and attested webhook-key pins; Finance's route amount and
`max_unfinalized_credit` approvals; the fresh OFAC snapshot, SDN-hit smoke and audited EU/UK
supplement/re-screening gates;
and restore-drill approval and release records.
