# Production launch

This is the ordered, **HUMAN-ONLY** launch procedure for Phala's production stablecoin pilot. The
committed routes are live USDC and USDT on Ethereum (chain 1) and live USDC on Base (chain 8453).
Base USDC has a sequencer uptime grace period and uses a separately deployed Base factory.
Production and staging share the Ankr and Infura accounts.

> **BLOCKER:** Chainalysis has deprecated its on-chain sanctions oracle and says it is not
> recommended for production sanctions screening. Its official notice records the last oracle
> update as March 18, 2026 ([Chainalysis oracle documentation](https://go.chainalysis.com/chainalysis-oracle-docs.html)).
> The configured addresses remain for compatibility, but no production charges may be enabled
> until the replacement screening decision is made and implemented.

The combined pilot limits are D=100 deposits/day (stop adding merchants above a seven-day average
of 80), Q=60 fresh quote snapshots per price chain and Environment per UTC day, at most 12 custody
routes, and `ISSUED_ADDRESS_CAP=1000` issued addresses per chain. Production contributes three
routes and staging contributes six, nine custody routes in use under the 12-route ceiling. Monitor
`topup_rpc_endpoint_ready`, `topup_rpc_errors_total`, `topup_coverage_lag_seconds`,
`topup_addresses_lagging`, `topup_daily_budget_used`, Sentry RPC alerts, daily reports, and provider
dashboards. Stop adding merchants or other load when the seven-day deposit average is above 80,
Ankr's seven-day run rate reaches 25,000 calls/day, Infura reaches 50% daily credits, or a chain
approaches the address cap; follow [RPC operations](../RPC.md#outages-lag-and-quotas) before resuming.

Finance must sign off every route amount and the explicit `max_unfinalized_credit` pilot value
before upgrade and again before enabling charges. The amount defaults copied into `topup.yaml` are
illustrative until Finance confirms them.

Run commands that create or change cloud resources, DNS, contracts, GitHub settings, or secrets
from the operator's release checkout. Never commit a private key or an env file.

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
export AWS_ENDPOINT=https://1d694298c092ffa09c793cbca4587812.r2.cloudflarestorage.com
export AWS_ACCESS_KEY_ID=<account-level-r2-access-key>
export AWS_SECRET_ACCESS_KEY=<account-level-r2-secret-key>
aws s3api create-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT" --region auto
aws s3api head-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT"
# In the Cloudflare R2 dashboard/API, issue the production read-write and restore Object Read only tokens.
export AWS_ACCESS_KEY_ID=<production-r2-read-write-access-key>
export AWS_SECRET_ACCESS_KEY=<production-r2-read-write-secret-key>
```

The service uses `AWS_REGION=auto` and path-style addressing. Seal the read-write values only after
step 6 preflight; restore-check uses `RESTORE_AWS_ACCESS_KEY_ID` and
`RESTORE_AWS_SECRET_ACCESS_KEY` from the read-only token.

## 3. Deploy and verify the factories

Use a checkout of Phala Pay at the release tag, with submodules, for the contract scripts. This is
a human broadcast; keep `PRIVATE_KEY` only in the operator's process environment:

```sh
cd /path/to/phala-pay-release
git checkout <release tag>
git submodule update --init --recursive
read -rs -p "Factory deployer private key: " PRIVATE_KEY && printf '\n'
export PRIVATE_KEY
export MAINNET_RPC_A=<ethereum-mainnet-rpc-a> MAINNET_RPC_B=<ethereum-mainnet-rpc-b>
export BASE_RPC_A=<base-mainnet-rpc-a> BASE_RPC_B=<base-mainnet-rpc-b>

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
export PROVISION_REQUESTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
  -f environment=production -f target=topup -f mode=provision
export PROVISION_RUN_ID="$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/deploy-phala.yml/runs" \
  -f event=workflow_dispatch -f branch=main -f "created=>=$PROVISION_REQUESTED_AT" -F per_page=100 \
  --jq '.workflow_runs | sort_by(.created_at) | last | .id')"
gh run watch "$PROVISION_RUN_ID" --repo "$GITHUB_REPOSITORY"
gh run view "$PROVISION_RUN_ID" --repo "$GITHUB_REPOSITORY"
```

Copy `TOPUP_CVM_ID` from the run summary and set the protected Environment variable. The same
summary prints the CNAME and TXT records used in step 5. Do not put the id in committed config:

```sh
export TOPUP_CVM_ID=<cvm-id-from-provision-summary>
gh variable set TOPUP_CVM_ID --repo "$GITHUB_REPOSITORY" --env production --body "$TOPUP_CVM_ID"
```

## 5. Create DNS records from the provision summary

Copy the exact CNAME and TXT records printed in the provision run summary. The TXT value uses the
`instance_id` from the attestation, not `TOPUP_CVM_ID`:

```sh
export GATEWAY_DOMAIN=<base_domain-from-provision-summary>
export INSTANCE_ID=<instance_id-from-the-attestation-in-the-summary>
echo "CNAME pay-api.phala.com gateway.$GATEWAY_DOMAIN"
echo "TXT _dstack-app-address.pay-api.phala.com $INSTANCE_ID:443"
dig +short CNAME pay-api.phala.com
dig +short TXT _dstack-app-address.pay-api.phala.com
```

Create `pay-api.phala.com -> gateway.$GATEWAY_DOMAIN` and
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
kit/deploy/phala envs update "$TOPUP_CVM_ID" -e .env.production
export UPGRADE_REQUESTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
  -f environment=production -f target=topup -f mode=upgrade
export UPGRADE_RUN_ID="$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/deploy-phala.yml/runs" \
  -f event=workflow_dispatch -f branch=main -f "created=>=$UPGRADE_REQUESTED_AT" -F per_page=100 \
  --jq '.workflow_runs | sort_by(.created_at) | last | .id')"
gh run watch "$UPGRADE_RUN_ID" --repo "$GITHUB_REPOSITORY"
gh run view "$UPGRADE_RUN_ID" --repo "$GITHUB_REPOSITORY"
shred -u .env.production.unsealed .env.production
```

## 7. Verify attestation, ingress, and health

Use the upgrade run's rendered compose artifact and verify the guest attestation and certificate:

```sh
kit/deploy/phala cvms get "$TOPUP_CVM_ID" --json > cvm.json
kit/deploy/phala cvms attestation "$TOPUP_CVM_ID" --json > attestation.json
export APP_ID="$(jq -er '.app_id' cvm.json)"
export INSTANCE_ID="$(jq -er '[.tcb_info.event_log[] | select(.event == "instance-id") | .event_payload | ascii_downcase | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' attestation.json)"
export GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' cvm.json)"
curl -fsS "https://${INSTANCE_ID}-8090.$GATEWAY_DOMAIN/prpc/Info" > info.json
kit/deploy/verify-attestation.sh attestation.json info.json "$APP_ID" upgrade-compose.yml service
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
```

Create the smoke account with charges disabled. Finance must confirm the explicit pilot value;
template-copied amounts remain illustrative:

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name:"Phala production smoke",contact:{name:"<name>",email:"<security email>"},due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},charges_enabled:false,reason:"production route smoke account"}')" > smoke-account.json)
export SMOKE_ACCOUNT_ID="$(jq -er '.id' smoke-account.json)"
export SMOKE_MERCHANT_SECRET_KEY=<smoke-account-secret-key>
export FINANCE_MAX_UNFINALIZED_CREDIT=<Finance-confirmed-integer-cents>
admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID" "{\"max_unfinalized_credit\":$FINANCE_MAX_UNFINALIZED_CREDIT,\"reason\":\"Finance-approved pilot value\"}"
```

For Ethereum and Base Safe treasuries, request one challenge per chain, have Safe owners sign the
unchanged message as an EIP-712 `SafeMessage(bytes message)` with Safe{Core} Protocol Kit, and
submit the threshold signature. This is the exact API exchange; the owner signing steps are in
[integration §1.6](../../docs/integration.md#16-treasuries):

```sh
export ETH_TREASURY_SAFE=<ethereum-smoke-safe> BASE_TREASURY_SAFE=<base-smoke-safe>
for CHAIN_ID in 1 8453; do
  SAFE_ADDRESS="$ETH_TREASURY_SAFE"; [ "$CHAIN_ID" = 8453 ] && SAFE_ADDRESS="$BASE_TREASURY_SAFE"
  curl -fsS -X POST "$ORIGIN/v1/treasuries/challenge" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -d "{\"chain_id\":$CHAIN_ID,\"address\":\"$SAFE_ADDRESS\"}" > "treasury-challenge-$CHAIN_ID.json"
  CHALLENGE_MESSAGE="$(jq -er '.message' "treasury-challenge-$CHAIN_ID.json")"
  SAFE_SIGNATURE=<Safe{Core}-threshold-signature-for-the-unchanged-message>
  curl -fsS -X POST "$ORIGIN/v1/treasuries" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -d "{\"chain_id\":$CHAIN_ID,\"message\":\"$CHALLENGE_MESSAGE\",\"signature\":\"$SAFE_SIGNATURE\"}"
done
```

Configure exactly the three committed live assets for the smoke account:

```sh
curl -fsS -X POST "$ORIGIN/v1/payment_settings" \
  -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' \
  -d '{"chains":[{"chain_id":1,"assets":[{"asset":"usdc"},{"asset":"usdt"}]},{"chain_id":8453,"assets":[{"asset":"usdc"}]}]}'
```

Pin the smoke account's live webhook keys from a nonce-bound, independently verified attestation,
then register its endpoint:

```sh
export NONCE="$(openssl rand -hex 32)"
curl -fsS -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" "$ORIGIN/v1/attestation?nonce=$NONCE" > smoke-public-attestation.json
uv run --locked python -c 'import json,os; from topup_client.models import AttestationResponse; from topup_sdk import verify_attestation_binding; r=AttestationResponse.from_dict(json.load(open("smoke-public-attestation.json"))); keys=verify_attestation_binding(r,bytes.fromhex(os.environ["NONCE"]),expected_account=os.environ["SMOKE_ACCOUNT_ID"],expected_livemode=True); print("pin these verified live webhook keys:",[key.public_key for key in keys])'
curl -fsS -X POST "$ORIGIN/v1/webhook_endpoints" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -d '{"url":"<https webhook receiver>","enabled_events":["*"]}'
```

Only after the sanctions replacement decision and Finance gates pass, enable charges for this smoke
account. Each required $1 quote below uses a Foundry keystore and waits with a bounded 15-minute
timeout for a credited deposit and signed webhook. Base is a separate factory and sequencer-gated:

```sh
admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID" '{"charges_enabled":true,"reason":"sanctions replacement and Finance gates approved"}'
export PAYER_ACCOUNT=<foundry-keystore-account> PAYER_PASSWORD_FILE=<mode-0600-keystore-password-file>
wait_for_credit() { local quote_id=$1 deadline=$((SECONDS + 900)); while (( SECONDS < deadline )); do if curl -fsS "$ORIGIN/v1/quotes/$quote_id?expand[]=deposit" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" | jq -e '.status == "complete" and .deposit.status == "credited"' >/dev/null; then return 0; fi; sleep 10; done; echo "quote $quote_id was not credited within 900 seconds" >&2; return 1; }

QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -H 'Idempotency-Key: production-smoke-ethereum-usdc-1' -d '{"client_reference_id":"production-smoke-ethereum-usdc","amount":100,"currency":"usd","chain_id":1,"asset":"usdc"}')"; QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"; ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"; AMOUNT="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" --rpc-url "$MAINNET_RPC_A" 0xA0b86991c6218b36c1d19d4a2e9eb0ce3606eb48 'transfer(address,uint256)' "$ADDRESS" "$AMOUNT"; wait_for_credit "$QUOTE_ID"
QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -H 'Idempotency-Key: production-smoke-ethereum-usdt-1' -d '{"client_reference_id":"production-smoke-ethereum-usdt","amount":100,"currency":"usd","chain_id":1,"asset":"usdt"}')"; QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"; ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"; AMOUNT="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" --rpc-url "$MAINNET_RPC_A" 0xdAC17F958D2ee523a2206206994597C13D831ec7 'transfer(address,uint256)' "$ADDRESS" "$AMOUNT"; wait_for_credit "$QUOTE_ID"
QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' -H 'Idempotency-Key: production-smoke-base-usdc-1' -d '{"client_reference_id":"production-smoke-base-usdc","amount":100,"currency":"usd","chain_id":8453,"asset":"usdc"}')"; QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"; ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"; AMOUNT="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" --rpc-url "$BASE_RPC_A" 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 'transfer(address,uint256)' "$ADDRESS" "$AMOUNT"; wait_for_credit "$QUOTE_ID"
```

## 9. Run the production restore drill before Phala Cloud charges

Use a separate Object Read only R2 token and follow [RESTORE.md, Staging restore drill](../RESTORE.md#staging-restore-drill)
steps 1–5 with production values. The `live_isolated` check is a hard abort: run it before the
instance exists, immediately after creation, before each restore step, and at least every five
minutes. Any failure means delete the drill instance by its `vm_uuid` and record it as aborted:

```sh
live_isolated() { for _ in $(seq 20); do test "$(curl -sS -o live-healthz.body -w '%{http_code}' "$ORIGIN/healthz")" = 200 && test ! -s live-healthz.body || return 1; done; }
live_isolated
export RESTORE_ENV_DIR="$(mktemp -d)"; umask 077; : > "$RESTORE_ENV_DIR/restore.env"; ${EDITOR:-vi} "$RESTORE_ENV_DIR/restore.env"
```

The restore env must contain exactly `RESTORE_AWS_ACCESS_KEY_ID`, `RESTORE_AWS_SECRET_ACCESS_KEY`,
`SENTRY_DSN`, `TOPUP_RPC_ANKR_KEY`, and `TOPUP_RPC_INFURA_KEY`, using the read-only token. Render
and preflight the provisional artifact, then create it with the required pre-launch script:

```sh
kit/deploy/render.sh --restore-check --images images.json --origin https://pending.invalid "$ENV_DIR" > restore-check.yml
kit/deploy/preflight.sh --env "$RESTORE_ENV_DIR/restore.env" --compose restore-check.yml --environment-dir "$ENV_DIR" --restore-check --offline --require-sentry
kit/deploy/phala instances add --app-id "$APP_ID" --compose-file restore-check.yml --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh --env-file "$RESTORE_ENV_DIR/restore.env" --name phala-pay-production-restore --json > restore-instance.json
export RESTORE_CVM_ID="$(jq -er '.vm_uuid' restore-instance.json)"; live_isolated
export PHALA_ATTESTATION_URL="https://cloud-api.phala.com/api/v1/cvms/$RESTORE_CVM_ID/attestation"
curl -fsS -H "X-API-Key: $PHALA_CLOUD_API_KEY" "$PHALA_ATTESTATION_URL" > restore-attestation.json
kit/deploy/phala cvms get "$RESTORE_CVM_ID" --json > restore-cvm.json
export RESTORE_INSTANCE_ID="$(jq -er '[.tcb_info.event_log[] | select(.event == "instance-id") | .event_payload | ascii_downcase | select(test("^[0-9a-f]{40}$"))] | select(length == 1)[0]' restore-attestation.json)"
export RESTORE_GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' restore-cvm.json)"
curl -fsS "https://${RESTORE_INSTANCE_ID}-8090.$RESTORE_GATEWAY_DOMAIN/prpc/Info" > restore-info.json
kit/deploy/verify-attestation.sh restore-attestation.json restore-info.json "$APP_ID" restore-check.yml restore-check; live_isolated
export RESTORE_URL="https://${APP_ID#0x}-8081.$RESTORE_GATEWAY_DOMAIN"
kit/deploy/render.sh --restore-check --images images.json --origin "$RESTORE_URL" "$ENV_DIR" > restore-check.yml
kit/deploy/phala deploy --json --cvm-id "$RESTORE_CVM_ID" --compose restore-check.yml --pre-launch-script kit/deploy/phala-cloud-pre-launch.sh --no-public-logs --no-public-sysinfo --wait
```

Wait for complete reconciliation with a one-hour timeout, hard-aborting if it does not converge:

```sh
deadline=$((SECONDS + 3600)); while (( SECONDS < deadline )); do if curl -fsS "$RESTORE_URL/healthz" | tee restore-healthz.json | jq -e '.mode == "read-only" and .restore_check.status == "ok" and .restore_check.post_restore_reconciliation.status == "complete"' >/dev/null; then break; fi; sleep 15; done; (( SECONDS < deadline )) || { echo "restore report did not complete within one hour" >&2; exit 1; }
live_isolated
export BASE_URL="$RESTORE_URL" NONCE="$(openssl rand -hex 32)"
admin GET "/v1/admin/attestation?account=$SMOKE_ACCOUNT_ID&livemode=true&nonce=$NONCE" > restore-public-attestation.json
admin GET /v1/admin/restore | jq -e '.frozen == true'
export RESTORE_VM_UUID="$(jq -er '.vm_uuid' restore-instance.json)"; kit/deploy/phala cvms delete "$RESTORE_VM_UUID" --force; shred -u "$RESTORE_ENV_DIR/restore.env"; rmdir "$RESTORE_ENV_DIR"
```

The restore attestation must match the original app id and compose, the report must be complete,
and live isolation must pass throughout. Never seal live read-write credentials into the drill.

## 10. Onboard Phala Cloud and enable its charges last

Only after the smoke account's three payments and the restore drill pass may Phala Cloud be
onboarded. Create its account with charges disabled, prove its Ethereum and Base treasuries,
configure exactly Ethereum USDC/USDT and Base USDC, pin live webhook keys, and set the same
Finance-approved `max_unfinalized_credit` explicitly. Repeat the treasury challenge and webhook
pinning commands from step 8 with Phala Cloud's keys and Safe addresses, then apply the exact
payment settings:

```sh
curl -fsS -X POST "$ORIGIN/v1/payment_settings" \
  -H "Authorization: Bearer $PHALA_CLOUD_MERCHANT_SECRET_KEY" -H 'content-type: application/json' \
  -d '{"chains":[{"chain_id":1,"assets":[{"asset":"usdc"},{"asset":"usdt"}]},{"chain_id":8453,"assets":[{"asset":"usdc"}]}]}'
```

After the sanctions replacement decision, Finance sign-off, smoke payments, restore report, and
monitoring checks all pass, enable charges only for Phala Cloud:

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name:"Phala Cloud",contact:{name:"<name>",email:"<security email>"},due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},charges_enabled:false,reason:"production onboarding after smoke and restore"}')" > phala-cloud-account.json)
export PHALA_CLOUD_ACCOUNT_ID="$(jq -er '.id' phala-cloud-account.json)" PHALA_CLOUD_MERCHANT_SECRET_KEY=<phala-cloud-secret-key>
admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID" "{\"max_unfinalized_credit\":$FINANCE_MAX_UNFINALIZED_CREDIT,\"reason\":\"Finance-approved pilot value\"}"
admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID" '{"charges_enabled":true,"reason":"sanctions replacement, Finance, smoke, and restore gates approved"}'
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
`max_unfinalized_credit` approvals; the sanctions-screening replacement decision and implementation;
and restore-drill approval and release records.
