# Valid runbook fixture

This file must pass `deploy/runbooks/check.sh`. It covers command shapes that are easy to
misclassify; it is not an operator runbook.

```sh
cargo run --release --locked -p topup --example inspect_sdn -- /path/to/SDN.XML
cargo test --locked -p topup --test refunds refund_flow -- --nocapture
docker compose -f deploy/docker-compose.staging.yml exec -T topup topup restore-check
docker compose -f deploy/docker-compose.staging.yml exec -T postgres psql -c "SELECT 'topup bogus'"
topup reconcile \
  --config deploy/environments/phala-network/staging/topup/topup.yaml
cargo run --locked -q -p topup -- config check deploy/environments/phala-network/staging/topup/topup.yaml
export TOPIC="$(cast keccak Flushed)"
psql "$DATABASE_URL" <<'SQL'
topup bogus --not-a-command
SQL
mapfile -t headers < <(deploy/runbooks/sign-admin-request.sh POST "$BASE_URL/v1/admin/routes/$ROUTE/pause" /tmp/body.json "$ADMIN_KEY_FILE" "$ADMIN_KEY_ID")
curl --fail-with-body -sS -X POST -H "${headers[0]}" "$BASE_URL/v1/admin/deposits/$DEPOSIT_ID/nudge"
curl --fail-with-body -sS "$BASE_URL/v1/attestation?nonce=00"
admin POST "/v1/admin/routes/$ROUTE/pause" '{"scopes":["refunds"]}'
deploy/render.sh --restore-check --images images.json --origin "$RESTORE_URL" \
  deploy/environments/<owner>/<Environment>/topup >restore-check.yml
deploy/verify-attestation.sh attestation.json info.json "$APP_ID" restore-check.yml restore-check "$EXPECTED_OS_IMAGE_HASH"
```
