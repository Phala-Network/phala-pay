# Production launch

This is the ordered, **HUMAN-ONLY** launch procedure for Phala's production stablecoin pilot.
The committed routes are live USDC and USDT on Ethereum (chain 1) and Base (chain 8453); PHA is
not enabled. Production and staging share the Ankr and Infura accounts, so the combined provider
caps in [chain reads §5.2](../../docs/design/chain-reads.md#52-worst-case-daily-budget-staging--production-combined)
apply to both environments.

Run commands that create or change cloud resources, DNS, contracts, GitHub settings, or secrets
from the operator's release checkout. Never commit a private key or an env file.

## 1. Create the workspace and GitHub Environment

Create the production Phala Cloud workspace and an API key scoped to it. Confirm that the workspace
offers `dstack-0.5.9` on a `tdx.medium` node. In the repository, create the protected GitHub
Environment named `production` and allow deployments from `main`; do not print the API key:

```sh
export GITHUB_REPOSITORY=Phala-Network/phala-pay
export PHALA_WORKSPACE="<production workspace display name>"
gh secret set PHALA_CLOUD_API_KEY --repo "$GITHUB_REPOSITORY" --env production < phala-cloud-production.api-key
gh variable set PHALA_WORKSPACE --repo "$GITHUB_REPOSITORY" --env production --body "$PHALA_WORKSPACE"
gh variable set TOPUP_MAINTENANCE_KEY_ID --repo "$GITHUB_REPOSITORY" --env production \
  --body maintenance/production-v1
gh secret set TOPUP_MAINTENANCE_PRIVATE_KEY_PEM --repo "$GITHUB_REPOSITORY" --env production \
  < maintenance/production.pem
```

The public admin key (`admin/production-v1`) and maintenance public key
(`maintenance/production-v1`) are committed in `topup.yaml`. Keep the corresponding admin seed and
maintenance PEM offline; neither belongs in Git or the CVM image.

## 2. Create the production R2 bucket

The owner must create the empty bucket **`phala-pay-production`** before provisioning. The
committed WAL-G prefix is `s3://phala-pay-production/production-v1`; it must not be shared with
staging. Create a read-write R2 token for the service and retain a separate read-only token for a
restore drill:

```sh
export AWS_ENDPOINT=https://1d694298c092ffa09c793cbca4587812.r2.cloudflarestorage.com
export AWS_ACCESS_KEY_ID=<production-r2-access-key>
export AWS_SECRET_ACCESS_KEY=<production-r2-secret-key>
aws s3api create-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT" --region auto
aws s3api head-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT"
```

The service uses `AWS_REGION=auto` and path-style addressing. Seal the two live access values only
after the rendered compose has passed preflight in step 6. A restore-check instance uses
`RESTORE_AWS_ACCESS_KEY_ID` and `RESTORE_AWS_SECRET_ACCESS_KEY` from an Object Read only token.

## 3. Deploy and verify the factories

The deterministic factory and implementation must be deployed and byte-for-byte identical on both
payment chains before any live route is enabled. Follow [CONTRACTS.md](../CONTRACTS.md#mainnet)
and its [Base mainnet section](../CONTRACTS.md#base-mainnet). This is a human broadcast; keep
`PRIVATE_KEY` only in the operator's process environment:

```sh
read -rsp "Factory deployer private key: " PRIVATE_KEY && printf '\n'
export PRIVATE_KEY
export MAINNET_RPC_A=<ethereum-mainnet-rpc-a> MAINNET_RPC_B=<ethereum-mainnet-rpc-b>
export BASE_RPC_A=<base-mainnet-rpc-a> BASE_RPC_B=<base-mainnet-rpc-b>

kit/deploy/contracts/deploy-proxy.sh --rpc-url "$MAINNET_RPC_A"
kit/deploy/contracts/deploy-factory.sh --rpc mainnet/a="$MAINNET_RPC_A" --dry-run
kit/deploy/contracts/deploy-factory.sh --rpc mainnet/a="$MAINNET_RPC_A" --broadcast
kit/deploy/contracts/verify-deployment.sh \
  --rpc mainnet/a="$MAINNET_RPC_A" --rpc mainnet/b="$MAINNET_RPC_B" \
  > mainnet-contract-verification.json
jq -e '.passed == true' mainnet-contract-verification.json

kit/deploy/contracts/deploy-proxy.sh --rpc-url "$BASE_RPC_A"
kit/deploy/contracts/deploy-factory.sh --rpc base/a="$BASE_RPC_A" --dry-run
kit/deploy/contracts/deploy-factory.sh --rpc base/a="$BASE_RPC_A" --broadcast
kit/deploy/contracts/verify-deployment.sh \
  --rpc base/a="$BASE_RPC_A" --rpc base/b="$BASE_RPC_B" \
  > base-contract-verification.json
jq -e '.passed == true' base-contract-verification.json
unset PRIVATE_KEY
```

Keep both verification reports with the release record. A factory, implementation, forwarder, or
runtime code-hash mismatch blocks the launch.

## 4. Provision the topup CVM

From the environment repository's `main`, dispatch provisioning exactly once. Provisioning creates
the CVM; it does not prove service health:

```sh
gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
  -f environment=production -f target=topup -f mode=provision
gh run watch --repo "$GITHUB_REPOSITORY" "$(gh run list --repo "$GITHUB_REPOSITORY" \
  --workflow deploy-phala.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Copy the CVM id from the run summary and store it as the protected Environment variable. Do not
put it in the committed compose or route config:

```sh
export TOPUP_CVM_ID=<cvm-id-from-provision-summary>
gh variable set TOPUP_CVM_ID --repo "$GITHUB_REPOSITORY" --env production --body "$TOPUP_CVM_ID"
```

## 5. Create DNS records

Read the gateway returned by the provisioned CVM, then create DNS-only records. The TXT value names
the CVM instance and the CNAME points to the gateway host:

```sh
kit/deploy/phala cvms get "$TOPUP_CVM_ID" --json > cvm.json
export APP_ID="$(jq -er '.app_id' cvm.json)"
export GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' cvm.json)"
echo "CNAME pay-api.phala.com gateway.$GATEWAY_DOMAIN"
echo "TXT _dstack-app-address.pay-api.phala.com $TOPUP_CVM_ID:443"
dig +short CNAME pay-api.phala.com
dig +short TXT _dstack-app-address.pay-api.phala.com
```

Create `pay-api.phala.com -> gateway.$GATEWAY_DOMAIN` and
`_dstack-app-address.pay-api.phala.com -> $TOPUP_CVM_ID:443` with the DNS provider. Wait for both
records to resolve before upgrading; the committed `public_origin` and ingress `DOMAIN` are
`https://pay-api.phala.com` and `pay-api.phala.com`.

## 6. Render, preflight, seal, and upgrade

Use the release kit and its digest-pinned `images.json`. The first preflight is offline and
unsealed; the second supplies every production secret and requires Sentry:

```sh
export ENV_DIR=deploy/environments/phala-network/production/topup
kit/deploy/render.sh --images images.json --gateway-domain "$GATEWAY_DOMAIN" "$ENV_DIR" \
  > docker-compose.production.yml
umask 077
cat > .env.production.unsealed <<'EOF'
AWS_ACCESS_KEY_ID=
AWS_SECRET_ACCESS_KEY=
SENTRY_DSN=
TOPUP_RPC_ANKR_KEY=
TOPUP_RPC_INFURA_KEY=
EOF
kit/deploy/preflight.sh --env .env.production.unsealed --compose docker-compose.production.yml \
  --environment-dir "$ENV_DIR" --offline --unsealed

cat > .env.production <<'EOF'
AWS_ACCESS_KEY_ID=<production-r2-access-key>
AWS_SECRET_ACCESS_KEY=<production-r2-secret-key>
SENTRY_DSN=<production-sentry-dsn>
TOPUP_RPC_ANKR_KEY=<shared-ankr-key>
TOPUP_RPC_INFURA_KEY=<shared-infura-key>
EOF
kit/deploy/preflight.sh --env .env.production --compose docker-compose.production.yml \
  --environment-dir "$ENV_DIR" --offline --require-sentry
kit/deploy/phala envs update "$TOPUP_CVM_ID" -e .env.production
gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
  -f environment=production -f target=topup -f mode=upgrade
gh run watch --repo "$GITHUB_REPOSITORY" "$(gh run list --repo "$GITHUB_REPOSITORY" \
  --workflow deploy-phala.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
shred -u .env.production.unsealed .env.production
```

The sealed names are `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `SENTRY_DSN`,
`TOPUP_RPC_ANKR_KEY`, and `TOPUP_RPC_INFURA_KEY`. Never reuse a staging prefix or access token.

## 7. Verify attestation, ingress, and health

After the upgrade, verify the guest attestation against the exact rendered compose and verify the
certificate evidence for the custom domain:

```sh
kit/deploy/phala cvms get "$TOPUP_CVM_ID" --json > cvm.json
kit/deploy/phala cvms attestation "$TOPUP_CVM_ID" --json > attestation.json
export APP_ID="$(jq -er '.app_id' cvm.json)"
export GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' cvm.json)"
curl -fsS "https://${APP_ID#0x}-8090.$GATEWAY_DOMAIN/prpc/Info" > info.json
kit/deploy/verify-attestation.sh attestation.json info.json "$APP_ID" docker-compose.production.yml service
kit/deploy/verify-ingress-evidence.sh pay-api.phala.com "$APP_ID"
test "$(curl -fsS -o /dev/null -w '%{http_code}' https://pay-api.phala.com/healthz)" = 200
```

Stop if the attested compose, app id, TCB, certificate evidence, or health response does not match
the release. Do not call the service ready from the provisioning result alone.

## 8. Onboard Phala Cloud and run a small payment

Complete Phala Cloud's due diligence and create its account with the admin key holder. Keep the
returned account and secret key in the operator's password manager; use the merchant key for all
merchant setup. Prove an Ethereum and a Base treasury Safe, configure payment settings for the
live USDC and USDT routes, pin the attested webhook keys, and register the webhook endpoint:

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn \
  '{name:"Phala Cloud", contact:{name:"<name>",email:"<security email>"},
    due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},
    charges_enabled:false, reason:"production pilot onboarding"}')" > account.json)
export ACCOUNT_ID="$(jq -er '.id' account.json)"
export MERCHANT_SECRET_KEY=<merchant-secret-key>
curl -fsS "$ORIGIN/v1/payment_settings" -H "Authorization: Bearer $MERCHANT_SECRET_KEY" \
  -H 'content-type: application/json' -d '{"chains":[{"chain_id":1,"assets":[{"asset":"usdc"},{"asset":"usdt"}]},{"chain_id":8453,"assets":[{"asset":"usdc"},{"asset":"usdt"}]}]}'
curl -fsS -X POST "$ORIGIN/v1/webhook_endpoints" -H "Authorization: Bearer $MERCHANT_SECRET_KEY" \
  -H 'content-type: application/json' -d '{"url":"<https webhook receiver>","enabled_events":["*"]}'
```

For a $1 USDC smoke payment, create one live quote, transfer the exact `amount_atomic` to its
address on Ethereum mainnet, and wait for the quote's expanded deposit to be `credited` and the
matching signed webhook to arrive:

```sh
export ORIGIN=https://pay-api.phala.com
export QUOTE_JSON="$(curl -fsS -X POST "$ORIGIN/v1/quotes" \
  -H "Authorization: Bearer $MERCHANT_SECRET_KEY" -H 'content-type: application/json' \
  -H 'Idempotency-Key: production-smoke-1' \
  -d '{"client_reference_id":"production-smoke-1","amount":100,"currency":"usd","chain_id":1,"asset":"usdc"}')"
export QUOTE_ID="$(jq -er '.id' <<<"$QUOTE_JSON")"
export DEPOSIT_ADDRESS="$(jq -er '.address' <<<"$QUOTE_JSON")"
export AMOUNT_ATOMIC="$(jq -er '.amount_atomic' <<<"$QUOTE_JSON")"
cast send --rpc-url "$MAINNET_RPC_A" --private-key "$PAYER_PRIVATE_KEY" \
  0xA0b86991c6218b36c1d19d4a2e9eb0ce3606eb48 \
  'transfer(address,uint256)' "$DEPOSIT_ADDRESS" "$AMOUNT_ATOMIC"
until curl -fsS "$ORIGIN/v1/quotes/$QUOTE_ID?expand[]=deposit" \
  -H "Authorization: Bearer $MERCHANT_SECRET_KEY" | \
  jq -e '.status == "complete" and .deposit.status == "credited"' >/dev/null; do sleep 10; done
```

The payment must be exactly $1 and the event must be verified with the attested webhook key. Only
after the payment, webhook, treasury, payment settings, and reconciliation checks pass may the
admin key holder enable live charges:

```sh
admin POST "/v1/admin/accounts/$ACCOUNT_ID" \
  '{"charges_enabled":true,"reason":"production smoke payment and onboarding approved"}'
```

## 9. Run the production restore drill

Use a separate Object Read only R2 token. Render the restore-check variant with its provisional
origin, run the offline preflight, and create the throwaway instance with an env file; follow the
[restore procedure](../RESTORE.md#restore) and do not upgrade a drill instance to the service
compose:

```sh
export RESTORE_ENV_DIR="$(mktemp -d)"
umask 077
printf '%s\n' \
  'RESTORE_AWS_ACCESS_KEY_ID=<read-only-r2-access-key>' \
  'RESTORE_AWS_SECRET_ACCESS_KEY=<read-only-r2-secret-key>' \
  'SENTRY_DSN=<production-sentry-dsn>' \
  'TOPUP_RPC_ANKR_KEY=<shared-ankr-key>' \
  'TOPUP_RPC_INFURA_KEY=<shared-infura-key>' > "$RESTORE_ENV_DIR/restore.env"
kit/deploy/render.sh --restore-check --images images.json --origin https://pending.invalid "$ENV_DIR" \
  > restore-check.yml
kit/deploy/preflight.sh --env "$RESTORE_ENV_DIR/restore.env" --compose restore-check.yml \
  --environment-dir "$ENV_DIR" --restore-check --offline --require-sentry
kit/deploy/phala instances add --app-id "$APP_ID" --compose-file restore-check.yml \
  --env-file "$RESTORE_ENV_DIR/restore.env" --name phala-pay-production-restore --json > restore-instance.json
export RESTORE_CVM_ID="$(jq -er '.id' restore-instance.json)"
curl -fsS "https://${APP_ID#0x}-8081.$GATEWAY_DOMAIN/healthz" | tee restore-healthz.json | \
  jq -e '.mode == "read-only" and .restore_check.status == "ok" and .restore_check.post_restore_reconciliation.status == "complete"'
kit/deploy/phala cvms delete "$RESTORE_CVM_ID" --force
shred -u "$RESTORE_ENV_DIR/restore.env"
rmdir "$RESTORE_ENV_DIR"
```

The report must have `restore_check.status == "ok"` and
`post_restore_reconciliation.status == "complete"`; any failure aborts the drill and charges
remain disabled. Never seal the live read-write credentials into the restore instance.

## Operator-only inputs that remain

The following values and actions are intentionally outside this PR: the production Phala Cloud
workspace and API key; GitHub `production` Environment values `PHALA_CLOUD_API_KEY`,
`PHALA_WORKSPACE`, `TOPUP_MAINTENANCE_PRIVATE_KEY_PEM`, `TOPUP_MAINTENANCE_KEY_ID`, and the
post-provision `TOPUP_CVM_ID`; sealed `SENTRY_DSN`, `TOPUP_RPC_ANKR_KEY`, `TOPUP_RPC_INFURA_KEY`,
`AWS_ACCESS_KEY_ID`, and `AWS_SECRET_ACCESS_KEY`; creation of the `phala-pay-production` bucket,
its live read-write token, and the restore read-only token; the admin seed and factory deployer
private key plus transaction broadcasts; DNS provider access and the gateway-derived records;
Phala Cloud's treasury Safe addresses and owner approvals; the merchant secret/live key and
webhook receiver; and the restore-drill approval and release records. The owner must also approve
pilot caps and enable charges only after the smoke payment and restore checks pass.
