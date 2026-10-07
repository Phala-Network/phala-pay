#!/usr/bin/env bash
# bash for pipefail and inherit_errexit: several checks pipe docker or psql output into a filter.
set -euo pipefail
shopt -s inherit_errexit

# Contract deployments use the same pinned Foundry profile as the sandbox/rehearsal.
# shellcheck source=../contracts/common.sh
source "$(dirname -- "$0")/../contracts/common.sh"
source "$(dirname -- "$0")/price-fixtures.sh"

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
# No bind mounts: CI's Docker daemon cannot see the checkout (see restore-drill.compose.yml).
drill_compose="$root/deploy/local/restore-drill.compose.yml"
mode=${1:-all}

case "$mode" in
    all)
        "$0" controlled
        "$0" crash
        exit 0
        ;;
    controlled|crash) ;;
    *) echo "usage: $0 [controlled|crash|all]" >&2; exit 64 ;;
esac

for command in docker forge cast anvil jq python3 openssl; do
    require_command "$command"
done

# TOPUP_RESTORE_DRILL_ID lets a caller (the weekly workflow) find and clean up its own projects.
drill_id=${TOPUP_RESTORE_DRILL_ID:-$$}
case "$drill_id" in
    ''|*[!a-z0-9]*) echo "TOPUP_RESTORE_DRILL_ID must be lowercase alphanumeric" >&2; exit 64 ;;
esac
project="topup-restore-drill-$mode-$drill_id"
# Per-run image tags keep concurrent checkouts from replacing this drill's images mid-run.
export TOPUP_LOCAL_SERVICE_IMAGE="phala-pay:$project"
export TOPUP_LOCAL_POSTGRES_IMAGE="phala-pay-postgres-walg:$project"
export TOPUP_LOCAL_DSTACK_IMAGE="phala-pay-dstack-simulator:$project"
export TOPUP_LOCAL_PRODUCT_IMAGE="phala-pay-reference-product:$project"
writer_pid=
samples_file=
env_dir=
compose_ready=false
admin_dir=
switch_lsn=
seed_container="$project-seed"
foundry_out_created=false
if [ ! -e "$root/contracts/out" ]; then
    foundry_out_created=true
fi
chain_overlay=()
service_overlay=()
# The source runs the service variant of the rendered compose; the replacement boots the
# restore-check variant (deploy/RESTORE.md). The controlled drill adds the merchant's product.
variant=()
profiles=()
if [ "$mode" = controlled ]; then
    profiles=(--profile merchant)
fi
dc() {
    local service_files=()
    if [ "${#variant[@]}" -eq 0 ]; then
        service_files=("${service_overlay[@]}")
    fi
    "$root/deploy/local/compose.sh" "${variant[@]}" --environment-dir "$env_dir" -p "$project" \
        -f "$drill_compose" "${chain_overlay[@]}" "${service_files[@]}" "${profiles[@]}" "$@"
}

cleanup() {
    local status=$?
    if [ "$status" -ne 0 ] && [ "$compose_ready" = true ]; then
        dc logs --no-log-prefix --tail 40 postgres topup restore-check >&2 || true
    fi
    if [ -n "$writer_pid" ]; then
        kill "$writer_pid" >/dev/null 2>&1 || true
        wait "$writer_pid" >/dev/null 2>&1 || true
    fi
    if [ -n "$samples_file" ]; then
        rm -f "$samples_file"
    fi
    docker rm -f "$seed_container" >/dev/null 2>&1 || true
    if [ "$compose_ready" = true ]; then
        dc --profile tools down --volumes --remove-orphans >/dev/null 2>&1 || true
    fi
    docker image rm "$TOPUP_LOCAL_SERVICE_IMAGE" "$TOPUP_LOCAL_POSTGRES_IMAGE" \
        "$TOPUP_LOCAL_DSTACK_IMAGE" "$TOPUP_LOCAL_PRODUCT_IMAGE" >/dev/null 2>&1 || true
    if [ "$foundry_out_created" = true ] && [ -d "$root/contracts/out" ] && [ ! -L "$root/contracts/out" ]; then
        find "$root/contracts/out" -depth -delete
    fi
    if [ -n "$env_dir" ]; then
        rm -rf "$env_dir"
    fi
    if [ -n "$admin_dir" ]; then
        rm -rf "$admin_dir"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

wait_for() {
    description=$1
    shift
    attempts=150
    while [ "$attempts" -gt 0 ]; do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        attempts=$((attempts - 1))
        sleep 1
    done
    echo "timed out waiting for $description" >&2
    return 1
}

wait_for_fast() {
    description=$1
    shift
    attempts=750
    while [ "$attempts" -gt 0 ]; do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        attempts=$((attempts - 1))
        sleep 0.2
    done
    echo "timed out waiting for $description" >&2
    return 1
}

psql_value() {
    dc exec -T postgres psql -U postgres -d topup -Atq -v ON_ERROR_STOP=1 -c "$1"
}

# PostgreSQL creates archive_status/<segment>.ready when the segment closes, and the archiver's
# rename to .done keeps that mtime. pg_ls_archive_statusdir() truncates mtime to whole seconds, so
# stat the file itself, and record the time as soon as the file appears: a later checkpoint may
# recycle the segment and remove its .done file.
segment_closed_at=
wal_closed() {
    local closed
    closed=$(dc exec -T postgres sh -c '
        cd "$PGDATA/pg_wal/archive_status"
        stat -c %y "$1.ready" 2>/dev/null || stat -c %y "$1.done"
    ' sh "$1") || return 1
    segment_closed_at=$(date -u -d "$closed" +%s.%3N)
}

wal_object_visible() {
    dc exec -T backup wal-g st ls wal_005/ | grep -F " $1."
}

# Upload time of a WAL object as recorded by object storage, in epoch seconds with milliseconds.
wal_object_uploaded_epoch() {
    line=$(dc exec -T backup wal-g st ls wal_005/ | grep -F " $1.")
    # shellcheck disable=SC2086  # split the listing line into its fields
    set -- $line
    test "$#" -ge 7 || {
        echo "WAL object listing has no upload time for $1" >&2
        return 1
    }
    date -u -d "$3 $4" +%s.%3N
}

seconds_between() {
    awk -v start="$1" -v end="$2" 'BEGIN { printf "%.6f", end - start }'
}

recovery_promoted() {
    test "$(psql_value 'SELECT NOT pg_is_in_recovery()')" = t
}

record_sample() {
    psql_value "INSERT INTO heartbeat DEFAULT VALUES; INSERT INTO restore_drill_marker(mode) VALUES ('$mode') RETURNING id" | tail -1
}

# Records the first sample and the WAL segment holding it, in the same statement as the insert.
first_sample() {
    psql_value "INSERT INTO heartbeat DEFAULT VALUES; \
        INSERT INTO restore_drill_marker(mode) VALUES ('$mode') \
        RETURNING id || ' ' || pg_walfile_name(pg_current_wal_insert_lsn())" | tail -1
}

marker_epoch() {
    psql_value "SELECT extract(epoch FROM recorded_at)::numeric(20,3) FROM restore_drill_marker WHERE id = $1"
}

seed_reconciliation_fixture() {
    # A merchant credential committed before backup must authenticate after service recovery.
    resume_merchant_key=$(new_api_key)
    dc exec -T postgres psql -U postgres -d topup -v ON_ERROR_STOP=1 -v merchant_key="$resume_merchant_key" <<'SQL'
CREATE TABLE restore_drill_marker (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    mode text NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

INSERT INTO accounts (id, name)
VALUES ('11111111-1111-1111-1111-111111111111', 'restore-drill');
INSERT INTO api_keys (id, account_id, livemode, kind, prefix, last4, key_hash, created_by)
VALUES (gen_random_uuid(), '11111111-1111-1111-1111-111111111111', false, 'secret',
        'ppay_sk_test_', right(:'merchant_key', 4), sha256(convert_to(:'merchant_key', 'UTF8')), 'admin');
INSERT INTO customers (id, account_id, livemode, client_reference_id)
VALUES (
    '22222222-2222-2222-2222-222222222222',
    '11111111-1111-1111-1111-111111111111',
    true,
    'restore-drill-customer'
);
-- A quote keeps the terms it was issued with, and a deposit is bound to the account's payment
-- settings revision (docs/design/payment-settings.md §7); the account's `unconfigured` revision is
-- the one its creation made.
INSERT INTO quotes (
    id, account_id, livemode, customer_id, route, amount_atomic, price_scaled, credit_minor,
    expires_at, status, closed_at, route_version, settings_revision_id, terms
)
SELECT
    '55555555-5555-5555-5555-555555555555',
    '11111111-1111-1111-1111-111111111111',
    true,
    '22222222-2222-2222-2222-222222222222',
    'restore-drill',
    1000,
    25000000,
    250,
    '2026-09-22T00:15:00Z',
    'expired',
    '2026-09-22T00:15:00Z',
    1,
    current_revision_id,
    '{"quote_ttl_seconds": 900, "quote_spread_bps": 50, "quote_tolerance_bps": 100,
      "quote_amount_decimals": 4, "min_amount": 100, "min_deposit_atomic": "0",
      "max_deposit_atomic": "1000000", "min_refund_atomic": "1", "confirmations": "finalized"}'
FROM payment_settings_state
WHERE account_id = '11111111-1111-1111-1111-111111111111' AND livemode;
INSERT INTO addresses (
    id, account_id, livemode, chain_id, quote_id, salt, treasury, address
)
VALUES (
    '33333333-3333-3333-3333-333333333333',
    '11111111-1111-1111-1111-111111111111',
    true,
    1,
    '55555555-5555-5555-5555-555555555555',
    '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee',
    '0x0000000000000000000000000000000000007ea5',
    '0xdddddddddddddddddddddddddddddddddddddddd'
);
INSERT INTO deposits (
    id, account_id, livemode, customer_id, chain_id, tx_hash, log_index, receipt_log_index,
    block_number, block_hash, block_time, address_id, route, route_version, asset_contract,
    from_address, tx_from, tx_nonce, amount_atomic, state, next_attempt_at, valuation_at,
    price_scaled, price_source, credit_minor, final_at, settings_revision_id
)
VALUES (
    '44444444-4444-4444-4444-444444444444',
    '11111111-1111-1111-1111-111111111111',
    true,
    '22222222-2222-2222-2222-222222222222',
    1,
    '0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    0,
    0,
    100,
    '0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff',
    '2026-09-22T00:00:00Z',
    '33333333-3333-3333-3333-333333333333',
    'restore-drill',
    1,
    '0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
    '0xcccccccccccccccccccccccccccccccccccccccc',
    '0xcccccccccccccccccccccccccccccccccccccccc',
    0,
    1000,
    'credited',
    now(),
    '2026-09-22T00:00:00Z',
    25000000,
    'spot',
    250,
    '2026-09-22T00:00:00Z',
    (SELECT current_revision_id FROM payment_settings_state
     WHERE account_id = '11111111-1111-1111-1111-111111111111' AND livemode)
);
INSERT INTO heartbeat DEFAULT VALUES;
INSERT INTO restore_drill_marker(mode) VALUES ('base');
SQL
}

# Business consistency after a restore (controlled mode; deploy/runbooks/restore.md): a test-mode
# account whose key revocation, deposit address rotation, quote, and delivered deposit.credited
# happen after the last archived WAL, so the restore loses them. Its merchant is the reference
# product (deploy/product), whose ledger survives the loss: the operator brings back what the
# restore lost from the records the product exports, not from records the drill makes up. The
# addresses are the deposit address formula's for this account, customer, route factory, and
# treasury (docs/design/multi-tenant.md §5a); the ids are the deterministic deposit and event ids
# of the transfer (chain 11155111, transaction 0x5a…5a, receipt position 0).
consistency_account=66666666-6666-6666-6666-666666666666
consistency_account_id=acct_66666666666666666666666666666666
consistency_treasury=0x0000000000000000000000000000000000007ea6
consistency_salt_v1=0x5364d14f27c908c6861df22196c51b7b693306887fa5984420aeaf04b35527f1
consistency_address_v1=0xcac987989e30d486588c3fcdd059ce71168a8c64
consistency_salt_v2=0xb284965b0e0bc5759251ed751eee59732336798de341530456cbb3c57373f457
consistency_address_v2=0xfbf725ff86da685728ec7275c9fc155b4443ea35
consistency_address_v2_id=da_99999999999999999999999999999992
consistency_address_v2_created=1790000000
consistency_event=c371cbc5-44c4-5e44-a795-6242ab4606d9
# A change of the chain's treasury (A) to T2 (B), pending in the backup with its time-lock ended,
# applies after the last archived WAL (at treasury_applied_at, set then), replacing A; the
# customer's version 3 is issued over T2 after it. replaced_delivery is A's `treasury.updated`.
consistency_treasury_id=trs_77777777777777777777777777777773
consistency_treasury_v2=0x0000000000000000000000000000000000007ea7
consistency_treasury_v2_id=trs_77777777777777777777777777777775
consistency_salt_v3=0x022d0f0b136e5721ff46ee4522a0b741609049f8f67b9345f2c6cb368368942d
consistency_address_v3=0x2ab8636aa082933ff69e28dcb938ca8dc8fae00b
consistency_address_v3_id=da_99999999999999999999999999999993
treasury_applied_at=
# The seed of the product's driver key (driver/v1), hex, with which the operator fetches the
# merchant's records; restore_point is the restore's, from GET /v1/admin/restore.
driver_seed=
restore_point=
# When the merchant created the quote, after the backup: re-issue refuses a quote the backup does
# not hold that was created more than five minutes before the restore point.
quote_created=
replaced_delivery=
# Deposits to the version 2 address the merchant was told of after the backup, by their
# deterministic ids (chain 11155111, receipt position 0): an unsupported-token transfer
# (transaction 0x6b…6b) rejected, never valued, then reversed; and a transfer (transaction 0x7c…7c)
# credited as D0, reversed by a reorganization, and recorded again as D1 (revision 1), which
# replaces it.
unvalued_deposit=bcd5eacf-3542-5175-9502-cbbf70ec9baa
unvalued_event=evt_40c4dfa74e6053a494d88c8fb32cacf3
reorged_deposit=dd1da4cd-1092-5496-8382-ca14b8734919
reorged_event=evt_9760e10d32995d689263ea3c4de08055
successor_deposit=5e0d5b86-cde2-5a86-885b-a4d8122337ae
successor_event=evt_80ee71be371554f0b955be8bacabd517
# A quote the merchant created after the backup, for another customer; its address is the quote
# salt formula's for this account, customer, and id over the route factory and treasury.
consistency_quote=qt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
consistency_quote_address=0x0594d2d54372b0c02108a544d224a8e3311614f8
delivered_event=$(jq -cn --arg address "$consistency_address_v2" '{
    id: "evt_c371cbc544c45e44a7956242ab4606d9", object: "event",
    account: "acct_66666666666666666666666666666666", livemode: false,
    type: "deposit.credited", created: 1790000000, actor: "system",
    data: {object: {
        id: "dep_e2facb389b5c57c69f7501e57d34b8d5", object: "deposit", livemode: false,
        client_reference_id: "restore-drill-da",
        deposit_address: "da_99999999999999999999999999999992", status: "credited",
        chain_id: 11155111, address: $address,
        tx_hash: "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a",
        asset_contract: "0x8f40e7e99678f44c88158f049e62817580ab113b",
        from_address: "0x00000000000000000000000000000000000000f7",
        receipt_log_index: 0, revision: 0, log_index: 0, block_number: 199,
        block_hash: "0x1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a",
        block_time: 1789999990, replaces: null, replaced_by: null, created: 1789999995,
        metadata: {},
        amount_atomic: "1000000000000000000", amount: 25, currency: "usd",
        exchange_rate: "0.25000000", price_source: "spot", valued_at: 1790000000}}}')
kept_key=
lost_key=
public_origin=
delivered_delivery=

# What the simulator derives at a dstack path, as the service derives its keys (the 32 key bytes
# of GetKey): `pem PATH` prints them as an ed25519 private key (a webhook key is the seed itself),
# and `secret PATH ACCOUNT ID` issues a client secret of object ID of ACCOUNT under them,
# `{id}_secret_{nonce}{tag}` with the nonce 8 random bytes and their owner tag, the first 8 bytes
# of HMAC-SHA256 of `owner:ACCOUNT:ID:{random hex}` under the subkey HMAC-SHA256 of
# `client-secret/owner/v1`, and the tag the first 16 bytes of HMAC-SHA256
# (crates/topup/src/client_secret.rs).
dstack_key() {
    dc exec -T mock-product python3 -c '
import base64, hashlib, hmac, http.client, json, secrets, socket, sys

class Dstack(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect("/run/dstack/dstack.sock")

connection = Dstack("dstack")
connection.request("POST", "/GetKey", json.dumps(
    {"path": sys.argv[2], "purpose": "", "algorithm": "secp256k1"}),
    {"Content-Type": "application/json"})
response = connection.getresponse()
if response.status != 200:
    sys.exit("GetKey answered %d" % response.status)
key = bytes.fromhex(json.loads(response.read())["key"])
if len(key) != 32:
    sys.exit("the derived key is not 32 bytes")
if sys.argv[1] == "pem":
    der = bytes.fromhex("302e020100300506032b657004220420") + key
    print("-----BEGIN PRIVATE KEY-----")
    print(base64.b64encode(der).decode())
    print("-----END PRIVATE KEY-----")
else:
    account, object_id, random = sys.argv[3], sys.argv[4], secrets.token_hex(8)
    subkey = hmac.new(key, b"client-secret/owner/v1", hashlib.sha256).digest()
    owner = "owner:%s:%s:%s" % (account, object_id, random)
    signed = object_id + "_secret_" + random + hmac.new(
        subkey, owner.encode(), hashlib.sha256).hexdigest()[:16]
    print(signed + hmac.new(key, signed.encode(), hashlib.sha256).hexdigest()[:32])
' "$@" </dev/null
}

webhook_key_path="settlement/$consistency_account_id/test/v1"

# The delivery of the event on stdin as the service sends it (Standard Webhooks headers and the
# raw body), signed `v1a` now with the account's test-mode webhook key version 1, which the
# simulator derives at the service's dstack path (crates/core/src/signer.rs). The drill signs it
# as the service's delivery worker does because no service runs on the source: `topup run` checks
# every route's contracts on a chain before it starts, and the drill has none.
sign_delivery() {
    local body id timestamp
    timestamp=$(date +%s)
    body=$(jq -c .)
    id=$(jq -r .id <<<"$body")
    dstack_key pem "$webhook_key_path" >"$admin_dir/webhook.pem"
    printf '%s.%s.%s' "$id" "$timestamp" "$body" >"$admin_dir/webhook-content"
    jq -cn --arg id "$id" --arg timestamp "$timestamp" --arg body "$body" \
        --arg signature "v1a,$(openssl pkeyutl -sign -rawin -inkey "$admin_dir/webhook.pem" \
            -in "$admin_dir/webhook-content" | openssl base64 -A)" \
        '{webhook_id: $id, webhook_timestamp: $timestamp, webhook_signature: $signature,
          body: $body}'
    rm -f "$admin_dir/webhook.pem" "$admin_dir/webhook-content"
}

# The public key of that webhook key in Standard Webhooks' form, as an attestation lists it.
webhook_public_key() {
    dstack_key pem "$webhook_key_path" >"$admin_dir/webhook.pem"
    printf 'whpk_%s' "$(openssl pkey -in "$admin_dir/webhook.pem" -pubout -outform DER | tail -c 32 |
        openssl base64 -A)"
    rm -f "$admin_dir/webhook.pem"
}

# A well-formed test-mode secret key (crates/topup/src/api_keys.rs): 43 random base62 characters
# and the base62 CRC-32 of everything before the checksum.
new_api_key() {
    dc exec -T mock-product python3 -c '
import secrets, zlib
alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
body = "ppay_sk_test_" + "".join(secrets.choice(alphabet) for _ in range(43))
value, checksum = zlib.crc32(body.encode()), ""
for _ in range(6):
    checksum, value = alphabet[value % 62] + checksum, value // 62
print(body + checksum)
'
}

# Runs the product's Python in its container, as its service user, with its ledger at /data.
product_python() {
    dc exec -T product /opt/venv/bin/python "$@"
}

# Sends a delivery (sign_delivery's output on stdin) to the merchant's webhook receiver as the
# service's delivery worker does: the raw body with the three Standard Webhooks headers. Prints
# the answer's status.
deliver_webhook() {
    dc exec -T mock-product python3 -c '
import json, sys, urllib.error, urllib.request
delivery = json.load(sys.stdin)
request = urllib.request.Request(
    "http://product:8089/webhooks", data=delivery["body"].encode(), method="POST")
request.add_header("content-type", "application/json")
for name in ("webhook_id", "webhook_timestamp", "webhook_signature"):
    request.add_header(name.replace("_", "-"), delivery[name])
try:
    response = urllib.request.urlopen(request, timeout=10)
except urllib.error.HTTPError as error:
    response = error
print(response.status)
'
}

# The product records a service response in its ledger, as it does when the service answers
# `POST /v1/quotes` or `POST /v1/deposit_addresses`: `product_record quote|deposit_address
# WORKSPACE`, the response on stdin.
product_record() {
    product_python -c '
import json, sys
from reference_product.ledger import ProductLedger
kind, team = sys.argv[1:]
ledger = ProductLedger("/data/ledger.sqlite3")
response = json.load(sys.stdin)
if kind == "quote":
    ledger.record_quote(team, response)
else:
    ledger.record_deposit_address(team, response)
' "$@"
}

# The merchant: the reference product's webhook receiver for the consistency account, with the
# account's test-mode webhook key pinned (a merchant pins it from attestation; no service runs on
# the source to attest it), and a workspace for each of the drill's customers.
start_merchant_product() {
    TOPUP_DRILL_ACCOUNT=$consistency_account_id
    TOPUP_DRILL_TREASURY=$consistency_treasury
    TOPUP_DRILL_PRODUCT_API_KEY=$kept_key
    TOPUP_DRILL_WEBHOOK_PUBLIC_KEY=$(webhook_public_key)
    # An ed25519 private key's DER ends with its 32-byte seed.
    openssl genpkey -algorithm ed25519 -out "$admin_dir/driver.pem" 2>/dev/null
    driver_seed=$(openssl pkey -in "$admin_dir/driver.pem" -outform DER | tail -c 32 |
        od -An -tx1 | tr -d ' \n')
    TOPUP_DRILL_DRIVER_PUBLIC_KEY=$(openssl pkey -in "$admin_dir/driver.pem" -pubout \
        -outform DER | tail -c 32 | base64)
    rm -f "$admin_dir/driver.pem"
    export TOPUP_DRILL_ACCOUNT TOPUP_DRILL_TREASURY TOPUP_DRILL_PRODUCT_API_KEY \
        TOPUP_DRILL_WEBHOOK_PUBLIC_KEY TOPUP_DRILL_DRIVER_PUBLIC_KEY
    dc up -d --no-deps product
    wait_for product product_python -c \
        "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8089/healthz')"
    product_python -c '
from reference_product.ledger import ProductLedger
from reference_product.server import register_team
ledger = ProductLedger("/data/ledger.sqlite3")
for team in ("restore-drill-da", "restore-drill-qt"):
    register_team(ledger, team)
'
}

seed_consistency_fixture() {
    kept_key=$(new_api_key)
    lost_key=$(new_api_key)
    dc exec -T postgres psql -U postgres -d topup -v ON_ERROR_STOP=1 \
        -v account="$consistency_account" -v treasury="$consistency_treasury" \
        -v treasury_v2="$consistency_treasury_v2" -v kept="$kept_key" -v lost="$lost_key" \
        -v salt="$consistency_salt_v1" -v address="$consistency_address_v1" <<'SQL'
INSERT INTO accounts (id, name) VALUES (:'account', 'restore-drill-consistency');
-- The merchant accepts the drill's Sepolia PHA route in test mode, as POST /v1/payment_settings
-- writes it.
WITH revision AS (
    INSERT INTO payment_settings_revisions (id, account_id, livemode, kind, document, created_by)
    VALUES (gen_random_uuid(), :'account', false, 'configured',
            '{"chains": [{"chain_id": 11155111, "assets": [{"asset": "pha"}]}]}', 'restore drill')
    RETURNING id
)
UPDATE payment_settings_state SET status = 'configured', current_revision_id = revision.id
FROM revision WHERE account_id = :'account' AND NOT livemode;
INSERT INTO api_keys (id, account_id, livemode, kind, prefix, last4, key_hash, created_by)
VALUES ('77777777-7777-7777-7777-777777777771', :'account', false, 'secret', 'ppay_sk_test_',
        right(:'kept', 4), sha256(convert_to(:'kept', 'UTF8')), 'admin'),
       ('77777777-7777-7777-7777-777777777772', :'account', false, 'secret', 'ppay_sk_test_',
        right(:'lost', 4), sha256(convert_to(:'lost', 'UTF8')), 'admin');
INSERT INTO treasuries (
    id, account_id, livemode, chain_id, address, kind, proof_message, proof_signature,
    verified_at, effective_at, screened_at, applied_at, created_by
)
VALUES ('77777777-7777-7777-7777-777777777773', :'account', false, 11155111, :'treasury', 'eoa',
        'restore drill', '0x', to_timestamp(1789000000), to_timestamp(1789000000), now(),
        to_timestamp(1789000000), 'key_77777777777777777777777777777771');
-- In force since before the quote's `created`, which re-issue derives its address over; then a
-- change to T2 whose time-lock ended, pending until it applies after the backup.
INSERT INTO treasuries (
    id, account_id, livemode, chain_id, address, kind, proof_message, proof_signature,
    verified_at, effective_at, screened_at, created_by
)
VALUES ('77777777-7777-7777-7777-777777777775', :'account', false, 11155111, :'treasury_v2',
        'eoa', 'restore drill', '0x', to_timestamp(1789500000), to_timestamp(1789500000), now(),
        'key_77777777777777777777777777777771');
INSERT INTO webhook_endpoints (id, account_id, livemode, url)
VALUES ('77777777-7777-7777-7777-777777777774', :'account', false,
        'http://product:8089/webhooks');
INSERT INTO customers (id, account_id, livemode, client_reference_id)
VALUES ('88888888-8888-8888-8888-888888888888', :'account', false, 'restore-drill-da');
INSERT INTO deposit_addresses (id, account_id, livemode, customer_id, version)
VALUES ('99999999-9999-9999-9999-999999999991', :'account', false,
        '88888888-8888-8888-8888-888888888888', 1);
INSERT INTO addresses (
    id, account_id, livemode, chain_id, deposit_address_id, salt, treasury, address
)
VALUES ('99999999-9999-9999-9999-9999999999a1', :'account', false, 11155111,
        '99999999-9999-9999-9999-999999999991', :'salt', :'treasury', :'address');
SQL
}

# An event of the consistency account in test mode, as the service renders it: `event_envelope
# TYPE ID CREATED`, its `data` on stdin.
event_envelope() {
    jq -c --arg type "$1" --arg id "$2" --argjson created "$3" \
        --arg account "$consistency_account_id" \
        '{id: $id, object: "event", account: $account, livemode: false, type: $type,
          created: $created, actor: "system", request: null, data: .}'
}

# Signs the event on stdin as the service does and delivers it to the merchant's receiver, which
# must keep it; prints the delivery.
deliver_event() {
    local delivery status
    delivery=$(sign_delivery)
    status=$(deliver_webhook <<<"$delivery")
    test "$status" = 204 || {
        printf 'the product answered %s to %s\n' "$status" "$(jq -r .body <<<"$delivery")" >&2
        dc logs --no-log-prefix --tail 20 product >&2
        return 1
    }
    printf '%s\n' "$delivery"
}

# A treasury object of the consistency account on chain 11155111: `treasury_object ID ADDRESS
# STATUS EFFECTIVE_AT REPLACED_AT`, REPLACED_AT `null` for none.
treasury_object() {
    jq -cn --arg id "$1" --arg address "$2" --arg status "$3" --argjson effective "$4" \
        --argjson replaced "$5" \
        '{id: $id, object: "treasury", livemode: false, chain_id: 11155111, address: $address,
          kind: "eoa", status: $status, effective_at: $effective, created: $effective,
          replaced_at: $replaced, canceled_at: null, cancellation_reason: null,
          crediting_paused: false, crediting_paused_by: []}'
}

# A deposit snapshot to the version 2 address (transfer fields and identity), with FIELDS (JSON)
# over it: `deposit_snapshot ID TX REVISION FIELDS`.
deposit_snapshot() {
    jq -cn --arg id "$1" --arg tx "$2" --argjson revision "$3" --argjson fields "$4" \
        --arg address "$consistency_address_v2" --arg da "$consistency_address_v2_id" '{
        id: $id, object: "deposit", livemode: false, client_reference_id: "restore-drill-da",
        quote: null, deposit_address: $da, status: "credited", rejection_reason: null,
        final: false, swept: false, chain_id: 11155111, asset: "pha",
        asset_contract: "0x8f40e7e99678f44c88158f049e62817580ab113b",
        address: $address, from_address: "0x00000000000000000000000000000000000000f7",
        tx_hash: $tx, log_index: 0, receipt_log_index: 0, revision: $revision,
        block_number: 200,
        block_hash: "0x2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b",
        block_time: 1790000100, amount_atomic: "4000000000000000000", amount: 100,
        currency: "usd", exchange_rate: "0.25000000", price_source: "spot",
        valued_at: 1790000110, amount_refunded: 0, amount_reversed: 0, replaces: null,
        replaced_by: null, metadata: {}, created: 1790000100} + $fields'
}

# After the last archived WAL: the merchant revokes a key, rotates the customer's deposit address,
# creates a quote, and receives deposit.credited; the treasury change to T2 applies and the
# customer's address rotates again over T2; and the merchant is told of the rejected and the
# reorganized deposits. Its receiver keeps every delivery. None of it reaches object storage; the
# source's rows for the events after deposit.credited are left out, since the restore loses them
# all the same.
lose_consistency_changes() {
    local secret
    # The quote is created from the run's clock, as no service runs on the source to create it,
    # while treasury A is still in force; T2 applies a second later.
    quote_created=$(date +%s)
    treasury_applied_at=$((quote_created + 1))
    dc exec -T postgres psql -U postgres -d topup -v ON_ERROR_STOP=1 \
        -v account="$consistency_account" -v treasury="$consistency_treasury" \
        -v salt="$consistency_salt_v2" -v address="$consistency_address_v2" \
        -v treasury_v2="$consistency_treasury_v2" -v salt_v3="$consistency_salt_v3" \
        -v address_v3="$consistency_address_v3" -v applied="$treasury_applied_at" \
        -v event="$consistency_event" -v data="$(jq -c .data <<<"$delivered_event")" <<'SQL'
UPDATE api_keys SET revoked_at = now() WHERE id = '77777777-7777-7777-7777-777777777772';
UPDATE deposit_addresses SET status = 'retired', retired_at = now()
WHERE id = '99999999-9999-9999-9999-999999999991';
INSERT INTO deposit_addresses (id, account_id, livemode, customer_id, version)
VALUES ('99999999-9999-9999-9999-999999999992', :'account', false,
        '88888888-8888-8888-8888-888888888888', 2);
INSERT INTO addresses (
    id, account_id, livemode, chain_id, deposit_address_id, salt, treasury, address
)
VALUES ('99999999-9999-9999-9999-9999999999a2', :'account', false, 11155111,
        '99999999-9999-9999-9999-999999999992', :'salt', :'treasury', :'address');
INSERT INTO events (id, account_id, livemode, type, object_type, object_id, actor, data, created)
VALUES (:'event', :'account', false, 'deposit.credited', 'deposit',
        'e2facb38-9b5c-57c6-9f75-01e57d34b8d5', 'system', :'data'::jsonb,
        to_timestamp(1790000000));
UPDATE treasuries SET replaced_at = to_timestamp(:applied)
WHERE id = '77777777-7777-7777-7777-777777777773';
UPDATE treasuries SET applied_at = to_timestamp(:applied)
WHERE id = '77777777-7777-7777-7777-777777777775';
UPDATE deposit_addresses SET status = 'retired', retired_at = now()
WHERE id = '99999999-9999-9999-9999-999999999992';
INSERT INTO deposit_addresses (id, account_id, livemode, customer_id, version)
VALUES ('99999999-9999-9999-9999-999999999993', :'account', false,
        '88888888-8888-8888-8888-888888888888', 3);
INSERT INTO addresses (
    id, account_id, livemode, chain_id, deposit_address_id, salt, treasury, address
)
VALUES ('99999999-9999-9999-9999-9999999999a3', :'account', false, 11155111,
        '99999999-9999-9999-9999-999999999993', :'salt_v3', :'treasury_v2', :'address_v3');
SQL
    # Each event's delivery reaches the merchant's receiver, which verifies it and keeps it as
    # received in its webhook inbox: deposit.credited, then the two treasury.updated events of T2
    # applying (at their `created`): B's, pending to active, and A's, replaced; then the rejected
    # deposit's reversal (unvalued), and D0's reversal and D1's credit.
    delivered_delivery=$(sign_delivery <<<"$delivered_event")
    test "$(deliver_webhook <<<"$delivered_delivery")" = 204
    treasury_object "$consistency_treasury_v2_id" "$consistency_treasury_v2" active 1789500000 \
        null | jq -c '{object: ., previous_attributes: {status: "pending"}}' |
        event_envelope treasury.updated "evt_$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')" \
            "$treasury_applied_at" | deliver_event >/dev/null
    replaced_delivery=$(treasury_object "$consistency_treasury_id" "$consistency_treasury" \
        replaced 1789000000 "$treasury_applied_at" |
        jq -c '{object: ., previous_attributes: {status: "active", replaced_at: null}}' |
        event_envelope treasury.updated "evt_$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')" \
            "$treasury_applied_at" | deliver_event)
    deposit_snapshot "dep_${unvalued_deposit//-/}" "0x$(printf '6b%.0s' {1..32})" 0 \
        "$(jq -cn '{status: "reversed", rejection_reason: "unsupported_asset", asset: null,
          asset_contract: "0x287e3577c66866a3f5cb7a8dac6761eb43608392", amount_atomic: "5",
          amount: null, exchange_rate: null, price_source: null, valued_at: null}')" |
        jq -c '{object: .}' | event_envelope deposit.reversed "$unvalued_event" 1790000200 |
        deliver_event >/dev/null
    deposit_snapshot "dep_${reorged_deposit//-/}" "0x$(printf '7c%.0s' {1..32})" 0 \
        "$(jq -cn --arg successor "dep_${successor_deposit//-/}" \
            '{status: "reversed", amount_reversed: 100, replaced_by: $successor}')" |
        jq -c '{object: .}' | event_envelope deposit.reversed "$reorged_event" 1790000300 |
        deliver_event >/dev/null
    deposit_snapshot "dep_${successor_deposit//-/}" "0x$(printf '7c%.0s' {1..32})" 1 \
        "$(jq -cn --arg reorged "dep_${reorged_deposit//-/}" \
            '{amount_atomic: "3600000000000000000", amount: 90, block_number: 201,
              block_hash: "0x3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c",
              block_time: 1790000400, valued_at: 1790000410, replaces: $reorged,
              created: 1790000400}')" |
        jq -c '{object: .}' | event_envelope deposit.credited "$successor_event" 1790000410 |
        deliver_event >/dev/null
    # The product records the rotated address and the quote as the service returned them, each with
    # a client secret the service's key tags for the account and id (a merchant created the quote,
    # for which no service runs here).
    secret=$(dstack_key secret client-secret/v1 "$consistency_account_id" \
        "$consistency_address_v2_id")
    jq -n --arg id "$consistency_address_v2_id" --arg address "$consistency_address_v2" \
        --arg salt "$consistency_salt_v2" --arg treasury "$consistency_treasury" \
        --arg secret "$secret" --argjson created "$consistency_address_v2_created" \
        '{id: $id, object: "deposit_address", livemode: false,
          client_reference_id: "restore-drill-da", version: 2, status: "active", salt: $salt,
          address: $address, networks: [{chain_id: 11155111, address: $address,
            treasury: $treasury, assets: [{asset: "pha"}]}],
          payments: [], metadata: {}, created: $created, retired_at: null,
          client_secret: $secret}' | product_record deposit_address restore-drill-da
    secret=$(dstack_key secret client-secret/v1 "$consistency_account_id" \
        "$consistency_address_v3_id")
    jq -n --arg id "$consistency_address_v3_id" --arg address "$consistency_address_v3" \
        --arg salt "$consistency_salt_v3" --arg treasury "$consistency_treasury_v2" \
        --arg secret "$secret" --argjson created "$treasury_applied_at" \
        '{id: $id, object: "deposit_address", livemode: false,
          client_reference_id: "restore-drill-da", version: 3, status: "active", salt: $salt,
          address: $address, networks: [{chain_id: 11155111, address: $address,
            treasury: $treasury, assets: [{asset: "pha"}]}],
          payments: [], metadata: {}, created: $created, retired_at: null,
          client_secret: $secret}' | product_record deposit_address restore-drill-da
    secret=$(dstack_key secret client-secret/v1 "$consistency_account_id" "$consistency_quote")
    jq -n --arg id "$consistency_quote" --arg address "$consistency_quote_address" \
        --arg treasury "$consistency_treasury" --arg secret "$secret" \
        --argjson created "$quote_created" \
        '{id: $id, object: "quote", livemode: false, client_reference_id: "restore-drill-qt",
          amount: 25, currency: "usd", chain_id: 11155111, asset: "pha",
          amount_atomic: "1000000000000000000", exchange_rate: "0.25000000", address: $address,
          treasury: $treasury, payment_uri: "ethereum:…", status: "open",
          expires_at: ($created + 900), created: $created, payment: null, deposit: null,
          client_secret: $secret, metadata: {}}' |
        product_record quote restore-drill-qt
}

# One request to topup on the compose network, its body on stdin; prints the status, the
# Retry-After header or `-`, and the body, one per line.
topup_call() {
    dc exec -T mock-product python3 -c '
import sys, urllib.error, urllib.request
method, path, *headers = sys.argv[1:]
body = sys.stdin.buffer.read()
request = urllib.request.Request("http://topup:8080" + path, data=body or None, method=method)
for header in headers:
    name, value = header.split(": ", 1)
    request.add_header(name, value)
try:
    response = urllib.request.urlopen(request, timeout=10)
except urllib.error.HTTPError as error:
    response = error
print(response.status)
print(response.headers.get("Retry-After") or "-")
print(response.read().decode())
' "$@"
}

# An admin-signed request with the drill's admin key (deploy/runbooks/sign-admin-request.sh), for
# the replacement's own origin (its --public-origin). A signature is single-use, so each is made in its own
# second.
admin_call() {
    sleep 1
    printf '%s' "${3:-}" >"$admin_dir/body"
    local headers
    mapfile -t headers < <("$root/deploy/runbooks/sign-admin-request.sh" "$1" \
        "$public_origin$2" "$admin_dir/body" "$admin_dir/admin.pem" local-admin/v1)
    topup_call "$1" "$2" 'content-type: application/json' "${headers[@]}" <"$admin_dir/body"
}

merchant_call() {
    topup_call "$1" "$2" "authorization: Bearer $3" 'content-type: application/json'
}

call_status() { sed -n 1p <<<"$1"; }
call_retry_after() { sed -n 2p <<<"$1"; }
call_body() { tail -n +3 <<<"$1"; }

expect_call() {
    test "$(call_status "$2")" = "$1" || {
        printf 'expected %s, got: %s\n' "$1" "$2" >&2
        return 1
    }
}

# The records the merchant's product exports for the operator since the restore point ($1;
# deploy/runbooks/restore.md, step 2), each already the body of its admin request, without the
# reason: fetched from its account API signed with the driver key, as an operator does for the
# staging product, and the same as the export from its ledger file, opened read-only next to the
# running product.
merchant_records() {
    local fetched exported
    fetched=$(product_python -m reference_product fetch-restore-records \
        --config /etc/product/config.json --driver-seed-file /dev/stdin \
        --since "$1" <<<"$driver_seed")
    exported=$(product_python -m reference_product export-restore-records \
        --config /etc/product/config.json --since "$1" </dev/null)
    test "$fetched" = "$exported"
    printf '%s\n' "$fetched"
}

# Verifies the merchant's treasuries, B's then A's, and requires the result and restored status of
# each: `treasuries_verified REQUEST B_RESULT B_STATUS A_RESULT A_STATUS`.
treasuries_verified() {
    local answer
    answer=$(admin_call POST /v1/admin/restore/treasuries/verify "$1")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e --arg b "$consistency_treasury_v2_id" \
        --arg a "$consistency_treasury_id" --arg b_result "$2" --arg b_status "$3" \
        --arg a_result "$4" --arg a_status "$5" \
        '.data == [
          {id: $b, received_status: "active", status: $b_status, result: $b_result,
           crediting: "matches"},
          {id: $a, received_status: "replaced", status: $a_status, result: $a_result,
           crediting: "matches"}]' >/dev/null
}

# The replacement is frozen; the operator's reconciliation brings back the lost security change,
# treasury application, deposit addresses, and quote from the merchant's records, and keeps the
# delivered events as delivered, the reversed deposits restored (deploy/runbooks/restore.md).
# Every record comes from the merchant's product: `merchant_records`.
check_consistency_after_restore() {
    local answer key records request secret
    public_origin=$(dc config --format json |
        jq -er '.services.topup.command as $c | $c[($c | index("--public-origin")) + 1]')
    answer=$(admin_call GET /v1/admin/restore)
    expect_call 200 "$answer"
    call_body "$answer" | jq -e --arg id "$(jq -r .restore_id <<<"$restore_report")" \
        '.frozen and .restore.detected_by == "restore_check" and .restore.id == $id' >/dev/null
    restore_point=$(call_body "$answer" | jq -er '.restore.restore_point')
    answer=$(merchant_call POST /v1/deposit_addresses "$kept_key" \
        <<<'{"client_reference_id":"restore-drill-da"}')
    expect_call 503 "$answer"
    test "$(call_retry_after "$answer")" = 300
    call_body "$answer" | jq -e '.error.code == "service_restoring"' >/dev/null

    # Restore step 5: the operator attests the instance with the admin API; merchant keys cannot.
    expect_call 503 "$(merchant_call GET /v1/attestation?nonce=00ff "$kept_key" </dev/null)"
    answer=$(admin_call GET "/v1/admin/attestation?account=$consistency_account_id&livemode=false&nonce=00112233445566778899aabbccddeeff")
    expect_call 200 "$answer"
    key=$(webhook_public_key)
    call_body "$answer" | jq -e --arg account "$consistency_account_id" \
        --arg key "$key" '.account == $account and .livemode == false
        and .webhook_keys[0].version == 1 and .webhook_keys[0].public_key == $key
        and (.report_data | length) == 64 and (.tdx_quote | length) > 0' >/dev/null

    # The restore made the key revoked after the backup valid again, but while frozen no key
    # authenticates, reads included; it is revoked again, by prefix, before the unfreeze.
    local lost_revoked="SELECT revoked_at IS NOT NULL FROM api_keys \
        WHERE id = '77777777-7777-7777-7777-777777777772'"
    test "$(psql_value "$lost_revoked")" = f || {
        echo "the key revocation was not lost: the segment holding it was archived" >&2
        return 1
    }
    for key in "$lost_key" "$kept_key"; do
        answer=$(merchant_call GET /v1/account "$key" </dev/null)
        expect_call 503 "$answer"
        call_body "$answer" | jq -e '.error.code == "service_restoring"' >/dev/null
    done
    answer=$(admin_call POST /v1/admin/restore/api_keys/revoke "$(jq -cn \
        --arg account "$consistency_account_id" --arg last4 "${lost_key: -4}" \
        '{account: $account, prefix: "ppay_sk_test_", last4: $last4,
          reason: "restore drill: revoked after the backup"}')")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e '.status == "revoked"' >/dev/null
    test "$(psql_value "$lost_revoked")" = t

    # The merchant's product exports what it recorded: the treasuries of its treasury events, the
    # addresses and quote as the service returned them, and the deliveries as its receiver got
    # them.
    records=$(merchant_records "$restore_point")

    # The address given out after the backup is re-issued identically from the merchant's record,
    # with its client secret, so the payer's page reads it again.
    test "$(psql_value "SELECT count(*) FROM deposit_addresses WHERE version = 2")" = 0
    request=$(jq -ce --arg id "$consistency_address_v2_id" \
        '[.deposit_addresses[] | select(.id == $id)] | select(length == 1) | .[0]
         | .reason = "restore drill: issued after the backup"' <<<"$records")
    answer=$(admin_call POST /v1/admin/restore/deposit_addresses "$request")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e --arg address "$consistency_address_v2" \
        --arg id "$consistency_address_v2_id" \
        '.reissued and .deposit_address.id == $id and .deposit_address.version == 2
         and .deposit_address.address == $address and .deposit_address.status == "active"' \
        >/dev/null
    secret=$(jq -er .client_secret <<<"$request")
    expect_call 200 "$(topup_call GET \
        "/v1/deposit_addresses/$consistency_address_v2_id?client_secret=$secret" </dev/null)"

    # The treasury change that applied after the backup is pending again: the merchant's latest
    # treasury objects, B's then A's, show the application and A's replacement lost, and the
    # address it issued over T2 is refused until the operator restores the application from B's
    # signed treasury.updated (step 3); a body changed after signing, or A's delivery, is refused.
    # It applies at the event's time, once, and both treasuries then match.
    local treasuries_request address_request application
    treasuries_request=$(jq -ce --arg b "$consistency_treasury_v2_id" \
        --arg b_address "$consistency_treasury_v2" --arg a "$consistency_treasury_id" \
        --arg a_address "$consistency_treasury" \
        '.treasuries | select(length == 1) | .[0]
         | select(.treasuries == [
             {id: $b, status: "active", chain_id: 11155111, address: $b_address,
              crediting_paused_by: []},
             {id: $a, status: "replaced", chain_id: 11155111, address: $a_address,
              crediting_paused_by: []}])
         | .reason = "restore drill: applied after the backup"' <<<"$records")
    treasuries_verified "$treasuries_request" application_lost pending replacement_lost active
    address_request=$(jq -ce --arg id "$consistency_address_v3_id" \
        '[.deposit_addresses[] | select(.id == $id)] | select(length == 1) | .[0]
         | .reason = "restore drill: issued over the treasury applied after the backup"' \
        <<<"$records")
    expect_call 400 "$(admin_call POST /v1/admin/restore/deposit_addresses "$address_request")"
    application=$(jq -ce --arg id "$consistency_treasury_v2_id" \
        '.treasury_applications | select(length == 1) | .[0]
         | select(.delivery.body | fromjson | .data.object.id == $id)
         | .reason = "restore drill: applied after the backup"' <<<"$records")
    answer=$(admin_call POST /v1/admin/restore/treasuries/apply "$(jq -c \
        '.delivery.body |= (fromjson | .created -= 1 | tojson)' <<<"$application")")
    expect_call 400 "$answer"
    call_body "$answer" | jq -e '.error.param == "delivery"' >/dev/null
    answer=$(admin_call POST /v1/admin/restore/treasuries/apply "$(jq -c \
        '{delivery: ., reason: "restore drill: the replaced treasury"}' <<<"$replaced_delivery")")
    expect_call 400 "$answer"
    call_body "$answer" | jq -e '.error.param == "delivery"' >/dev/null
    for applied in true false; do
        answer=$(admin_call POST /v1/admin/restore/treasuries/apply "$application")
        expect_call 200 "$answer"
        call_body "$answer" | jq -e --argjson applied "$applied" \
            --arg id "$consistency_treasury_v2_id" \
            '.applied == $applied and .treasury.id == $id and .treasury.status == "active"' \
            >/dev/null
    done
    test "$(psql_value "SELECT extract(epoch FROM applied_at)::bigint FROM treasuries \
        WHERE address = '$consistency_treasury_v2'")" = "$treasury_applied_at"
    treasuries_verified "$treasuries_request" matches active matches replaced
    answer=$(admin_call POST /v1/admin/restore/deposit_addresses "$address_request")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e --arg address "$consistency_address_v3" \
        --arg id "$consistency_address_v3_id" \
        '.reissued and .deposit_address.id == $id and .deposit_address.version == 3
         and .deposit_address.address == $address and .deposit_address.status == "active"' \
        >/dev/null

    # The quote created after the backup is re-issued from the merchant's record with the client
    # secret the service tagged, so the payer's page reads it again; a secret of another quote, or
    # of this one issued to another account, is refused.
    local quote_request refused
    quote_request=$(jq -ce --arg id "$consistency_quote" \
        '.quotes | select(length == 1) | .[0] | select(.id == $id)
         | .reason = "restore drill: quoted after the backup"' <<<"$records")
    secret=$(jq -r .client_secret <<<"$quote_request")
    for refused in "$consistency_account_id qt_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" \
        "acct_restoredrillother $consistency_quote"; do
        # shellcheck disable=SC2086 # the account and the quote id, split on purpose
        answer=$(admin_call POST /v1/admin/restore/quotes "$(jq -c --arg secret \
            "$(dstack_key secret client-secret/v1 $refused)" \
            '.client_secret = $secret' <<<"$quote_request")")
        expect_call 400 "$answer"
    done
    answer=$(admin_call POST /v1/admin/restore/quotes "$quote_request")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e --arg address "$consistency_quote_address" \
        '.reissued and .quote.address == $address and .quote.amount == 25' >/dev/null
    # A repeat finds the quote held already and issues nothing.
    answer=$(admin_call POST /v1/admin/restore/quotes "$quote_request")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e '.reissued == false' >/dev/null
    expect_call 200 "$(topup_call GET "/v1/quotes/$consistency_quote?client_secret=$secret" </dev/null)"
    # Its address is watched, its history not read yet (no scanner runs here).
    test "$(psql_value "SELECT backfilled FROM addresses \
        WHERE address = '$consistency_quote_address'")" = f

    # The events delivered after the backup are imported from the deliveries the merchant's
    # receiver kept, the first byte for byte as the service sent it, as delivered, and never sent
    # again; a body changed after signing is refused and changes nothing. Each reversed deposit,
    # to the re-issued version 2 address, is restored reversed at its revision: the unvalued
    # rejected one, and D0, whose successor is D1.
    test "$(psql_value "SELECT count(*) FROM events WHERE id = '$consistency_event'")" = 0
    request=$(jq -ce --argjson delivered "$delivered_delivery" \
        '.events | select(length == 1) | .[0] | select(.deliveries[0] == $delivered)
         | .reason = "restore drill: delivered after the backup"' <<<"$records")
    answer=$(admin_call POST /v1/admin/restore/events "$request")
    expect_call 200 "$answer"
    call_body "$answer" | jq -e --arg unvalued "$unvalued_event" --arg reorged "$reorged_event" \
        --arg successor "$successor_event" \
        '.data == [
          {id: "evt_c371cbc544c45e44a7956242ab4606d9", result: "imported", reversed_deposit: null},
          {id: $unvalued, result: "imported", reversed_deposit: "restored"},
          {id: $reorged, result: "imported", reversed_deposit: "restored"},
          {id: $successor, result: "imported", reversed_deposit: null}]' >/dev/null
    test "$(psql_value "SELECT string_agg(state, ',' ORDER BY revision) FROM deposits \
        WHERE id IN ('$unvalued_deposit', '$reorged_deposit')")" = reversed,reversed
    test "$(psql_value "SELECT successor_id FROM restore_deposit_tombstones \
        WHERE deposit_id = '$reorged_deposit'")" = "$successor_deposit"
    # Its credit is kept for the deposit the rescan re-derives, which is valued at it.
    test "$(psql_value "SELECT credit_minor || '|' || price_scaled || '|' || price_source \
        FROM restore_delivered_credits WHERE deposit_id = 'e2facb38-9b5c-57c6-9f75-01e57d34b8d5'")" \
        = '25|25000000|spot'
    answer=$(admin_call POST /v1/admin/restore/events \
        "$(jq -c '.deliveries[0].body |= (fromjson | .data.object.amount = 26 | tojson)
            | .reason = "restore drill: re-valued"' <<<"$request")")
    expect_call 400 "$answer"
    call_body "$answer" | jq -e '.error.param == "deliveries"' >/dev/null
    test "$(psql_value "SELECT (data = '$(jq -c .data <<<"$delivered_event")'::jsonb)::text \
        || ':' || (SELECT count(*) FROM webhook_deliveries WHERE event_id = '$consistency_event') \
        FROM events WHERE id = '$consistency_event'")" = 'true:0'

    # Nothing scans on the restore-check instance, so the freeze cannot be lifted there.
    answer=$(admin_call POST /v1/admin/restore/unfreeze '{"reason":"restore drill",
        "security_changes_reapplied":true,"deposit_addresses_reissued":true,"quotes_reissued":true,
        "delivered_events_imported":true}')
    expect_call 400 "$answer"
    call_body "$answer" | jq -e '.error.code == "restore_rescan_incomplete"' >/dev/null
    test "$(psql_value 'SELECT count(*) FROM restores WHERE unfrozen_at IS NULL')" = 1
}

startup_base_backup_listed() {
    dc exec -T backup sh -c 'wal-g backup-list --json | jq -e "length > 0"'
}

# Full object listing with modification times, to prove a drill instance wrote nothing.
storage_listing() {
    dc run --rm --no-deps restore 'wal-g st ls -r' | sort
}

# Creates the overlay's project volume and copies the mock product into it through the API.
seed_drill_volumes() {
    docker volume create \
        --label "com.docker.compose.project=$project" \
        --label "com.docker.compose.volume=drill_mock_product" \
        "${project}_drill_mock_product" >/dev/null
    docker create --name "$seed_container" \
        --label "com.docker.compose.project=$project" \
        --volume "${project}_drill_mock_product:/seed/mock-product" \
        --entrypoint /bin/true "$TOPUP_LOCAL_POSTGRES_IMAGE" >/dev/null
    docker cp "$root/deploy/local/mock-product.py" "$seed_container:/seed/mock-product/"
    docker rm "$seed_container" >/dev/null
}

remove_volume() {
    volume=$(docker volume ls -q \
        --filter "label=com.docker.compose.project=$project" \
        --filter "label=com.docker.compose.volume=$1")
    if [ -z "$volume" ]; then
        echo "could not locate drill volume $1" >&2
        return 1
    fi
    docker volume rm "$volume" >/dev/null
}

remove_pgdata_volume() {
    remove_volume pgdata
}

# The restore-check variant runs no background work: no heartbeat, base backups, webhook egress,
# or ingress, and `up --remove-orphans` removed the source's.
no_background_work() {
    local running
    running=$(dc ps --all --format '{{.Service}}' | sort | tr '\n' ' ')
    case " $running" in
        *" heartbeat "* | *" backup "* | *" smokescreen "* | *" dstack-ingress "*)
            echo "the replacement runs background work: $running" >&2
            return 1
            ;;
    esac
}

# The replacement's PostgreSQL reads the restore instance's own read-only credentials
# (RESTORE_AWS_*), even with the live read-write names in its environment.
restore_credentials_only() {
    test "$(dc exec -T postgres printenv AWS_ACCESS_KEY_ID)" = topup-restore-read || {
        echo "the replacement's PostgreSQL does not use the read-only restore credentials" >&2
        return 1
    }
}

# Only Anvil publishes ephemeral loopback ports for deployment; the mock product reaches topup
# on the compose network.
topup_request() {
    dc exec -T mock-product python3 - "$1" "$2" <<'PY'
import sys, urllib.error, urllib.request
request = urllib.request.Request("http://topup:8080" + sys.argv[2], method=sys.argv[1])
try:
    with urllib.request.urlopen(request, timeout=10) as response:
        print(response.status)
        print(response.read().decode())
except urllib.error.HTTPError as error:
    print(error.code)
PY
}

topup_status() {
    topup_request "$1" "$2" | head -1
}

topup_get() {
    topup_request GET "$1" | tail -n +2
}

restore_report_served() {
    topup_get /healthz | jq -e '.restore_check != null'
}

# The restored cluster's application login, with the password the replacement derived.
app_login_works() {
    dc exec -T postgres sh -c '
        PGPASSFILE=/run/db-app/topup_service.pgpass \
            psql -h postgres -U topup_service -d topup -XAtq -c "SELECT current_user"
    '
}

storage_write_probe() {
    dc run --rm --no-deps restore \
        'printf probe >/tmp/probe && wal-g st put --no-compress --no-encrypt /tmp/probe drill-write-probe'
}

# Positive control with the source credentials, so the read-only check below cannot pass on a
# broken probe command.
storage_probe_writes() {
    storage_write_probe >/dev/null 2>&1 || {
        echo "object-storage write probe failed with read-write credentials" >&2
        return 1
    }
    dc run --rm --no-deps restore 'wal-g st rm drill-write-probe' >/dev/null
}

# The replacement's storage credentials must not be able to write.
storage_is_read_only() {
    if storage_write_probe >/dev/null 2>&1; then
        echo "replacement object-storage credentials can write" >&2
        return 1
    fi
}

# restore_command must abort recovery (126), not end it, when a segment does not decrypt: a wrong
# key must never promote a partial restore. The same call with the backup key is the control.
test_restore_failures_are_fatal() {
    set +e
    dc exec -T backup sh -c '
        od -An -tx1 -N32 /dev/urandom | tr -d " \n" >/tmp/wrong.key
        WALG_LIBSODIUM_KEY_PATH=/tmp/wrong.key walg-restore-command "$1" /tmp/wrong-key-wal
    ' sh "$1" >/dev/null 2>&1
    status=$?
    set -e
    test "$status" -eq 126 || {
        echo "wrong WAL key returned $status instead of 126" >&2
        return 1
    }
    dc exec -T backup walg-restore-command "$1" /tmp/restored-wal >/dev/null 2>&1 || {
        echo "WAL $1 does not restore with the backup key" >&2
        return 1
    }
}

export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --pretty=%ct)}

# The replacement's admin key, for the operator's restore reconciliation, in the drill's local
# environment (the committed routes; deploy/local/environment.sh).
admin_dir=$(mktemp -d)
openssl genpkey -algorithm ed25519 -out "$admin_dir/admin.pem" 2>/dev/null
env_dir=$(mktemp -d)
"$root/deploy/local/environment.sh" --admin-key-id local-admin/v1 --admin-public-key \
    "$(openssl pkey -in "$admin_dir/admin.pem" -pubout -outform DER | tail -c 32 | base64)" \
    "$env_dir"

# Keep the chain fixtures alive across source loss and replacement boot. Each B endpoint is a
# forwarding HTTP server on a different port/domain, sharing A's canonical Anvil state.
export FOUNDRY_CACHE_PATH="$admin_dir/cache"
export FOUNDRY_BROADCAST="$admin_dir/broadcast"
python3 - "$admin_dir/chains.json" "$admin_dir/service.json" <<'PYCHAINS'
import json
import sys
proxy = """
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import urllib.request
class Proxy(BaseHTTPRequestHandler):
    def do_POST(self):
        try:
            body = self.rfile.read(int(self.headers['Content-Length']))
            request = urllib.request.Request(self.server.upstream, body,
                                             {'Content-Type': 'application/json'})
            with urllib.request.urlopen(request, timeout=10) as response:
                result = response.read()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(result)))
            self.end_headers()
            self.wfile.write(result)
        except Exception:
            self.send_error(502, 'local chain unavailable')
    def log_message(self, *args):
        pass
servers = []
for port, upstream in [(18545, 'http://anvil:8545'), (18546, 'http://anvil-base:8545')]:
    server = ThreadingHTTPServer(('0.0.0.0', port), Proxy)
    server.upstream = upstream
    servers.append(server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Event().wait()
"""
image = 'ghcr.io/foundry-rs/foundry:v1.8.3@sha256:2e4287278639262de76db72477301d5d3212fa1b1cce710d7d148750a46ce9e7'
services = {}
for name, chain_id, alias in [('anvil', 11155111, 'sepolia.drill-a.test'),
                              ('anvil-base', 84532, 'base.drill-a.test'),
                              ('anvil-mainnet-price', 1, 'mainnet.drill-a.test'),
                              ('anvil-base-mainnet-price', 8453, 'base-mainnet.drill-a.test')]:
    services[name] = {
        'image': image, 'entrypoint': ['anvil'],
        'command': ['--host', '0.0.0.0', '--chain-id', str(chain_id),
                    '--block-time', '1', '--slots-in-an-epoch', '4'],
        'ports': ['127.0.0.1::8545'],
        'networks': {'default': {'aliases': [alias]}},
        'healthcheck': {'test': ['CMD', 'cast', 'chain-id', '--rpc-url',
                                 'http://127.0.0.1:8545'],
                        'interval': '2s', 'timeout': '2s', 'retries': 30},
        'restart': 'no'}
    if chain_id in (1, 8453):
        services[name]['command'] += ['--timestamp', '${REHEARSAL_PRICE_ANVIL_TIMESTAMP:-1}']
        services[name]['networks']['default']['aliases'].append(
            'mainnet.drpc.test' if chain_id == 1 else 'base-mainnet.drill-b.test')
services['chain-b'] = {
    'image': 'python:3.14-slim-trixie@sha256:caaf356f40667c496d405780745b9ac25771c189a51dfcc42430d531ea09f8a2',
    'command': ['python3', '-c', proxy],
    'networks': {'default': {'aliases': ['sepolia.drill-b.test', 'base.drill-b.test']}},
    'restart': 'no'}
services['price-stub'] = {
    'image': services['chain-b']['image'],
    'command': ['python3', '/etc/price-stub/server.py', '8080'],
    'networks': {'default': {'aliases': ['api.kraken.com', 'data-api.binance.vision']}},
    'configs': [{'source': 'price_stub_server', 'target': '/etc/price-stub/server.py'}],
    'restart': 'no'}
with open(sys.argv[1], 'w') as output:
    json.dump({'services': services, 'configs': {
        'price_stub_server': {'content': '${TOPUP_PRICE_STUB_SERVER}'}}}, output)
with open(sys.argv[2], 'w') as output:
    json.dump({'services': {
        'heartbeat': {'command': ['topup', 'heartbeat', '--interval-s', '15']},
        'topup': {'environment': {
                     'TOPUP_TEST_KRAKEN_ENDPOINT': 'http://price-stub:8080/0/public/Ticker',
                     'TOPUP_TEST_BINANCE_ENDPOINT': 'http://price-stub:8080/api/v3/ticker/price'},
                 'command': ['topup', 'run', '--config', '/etc/topup/topup.yaml',
                             '--public-origin', 'http://topup:8080']}
    }}, output)
PYCHAINS
chain_overlay=(-f "$admin_dir/chains.json")
service_overlay=(-f "$admin_dir/service.json")
export TOPUP_PRICE_STUB_SERVER REHEARSAL_PRICE_ANVIL_TIMESTAMP
TOPUP_PRICE_STUB_SERVER="$(<"$root/deploy/local/price_stub.py")"

compose_version=$(docker compose version --short)
if [ "$(printf '%s\n' 2.24.4 "$compose_version" | sort -V | head -1)" != 2.24.4 ]; then
    echo "Docker Compose $compose_version is too old; the drill overlay needs 2.24.4+ (!override)" >&2
    exit 1
fi
dc --profile tools config --format json |
    jq -e '[.services[].volumes[]? | select(.type == "bind")] | length == 0' >/dev/null || {
    echo "the drill stack bind-mounts a host path; CI's Docker daemon cannot see it" >&2
    exit 1
}
compose_ready=true

dc build --build-arg BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" postgres dstack-simulator topup
if [ "$mode" = controlled ]; then
    dc build product
fi
# Deploy the canonical factory/implementation and Multicall3, then install local token/oracle
# runtimes at the configured addresses so the merchant's signed fixture records stay valid.
export FOUNDRY_OUT="$CONTRACTS_DIR/out"
REHEARSAL_PRICE_ANVIL_TIMESTAMP=$(($(date +%s) - 1950))
dc up -d --wait anvil anvil-base chain-b anvil-mainnet-price anvil-base-mainnet-price price-stub
mainnet_price_rpc_url="http://$(dc port anvil-mainnet-price 8545)"
base_mainnet_price_rpc_url="http://$(dc port anvil-base-mainnet-price 8545)"
install_anvil_price_fixtures "$mainnet_price_rpc_url" "$base_mainnet_price_rpc_url"
for chain in sepolia base-sepolia; do
    service=anvil
    [ "$chain" != base-sepolia ] || service=anvil-base
    rpc_url="http://$(dc port "$service" 8545)"
    install_anvil_multicall3 "$rpc_url"
    "$DEPLOY_CONTRACTS_DIR/deploy-proxy.sh" --rpc-url "$rpc_url" --local-fund --broadcast >/dev/null
    PRIVATE_KEY="$ANVIL_PRIVATE_KEY" "$DEPLOY_CONTRACTS_DIR/deploy-factory.sh" \
        --rpc "$chain/a=$rpc_url" --broadcast >/dev/null
    if [ "$chain" = sepolia ]; then
        derived=$(cast call 0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747 \
            'addressOf(address,bytes32)(address)' "$consistency_treasury" "$consistency_salt_v1" \
            --rpc-url "$rpc_url")
        [ "$(lower "$derived")" = "$consistency_address_v1" ] || die "factory fixture address changed"
    fi
    owner=$(cast wallet address --private-key "$ANVIL_PRIVATE_KEY")
    "$root/deploy/sandbox/deploy-test-contracts.sh" --rpc-url "$rpc_url" \
        --anvil-unlocked "$owner" >"$admin_dir/$chain.json"
    token_code=$(cast code "$(jq -er .test_token "$admin_dir/$chain.json")" --rpc-url "$rpc_url")
    usdc=$(cd "$CONTRACTS_DIR" && forge create test/mocks/MockTokens.sol:UsdcLikeToken \
        --rpc-url "$rpc_url" --unlocked --from "$owner" --broadcast --json | jq -er .deployedTo)
    usdt=$(cd "$CONTRACTS_DIR" && forge create test/mocks/MockTokens.sol:UsdtLikeToken \
        --rpc-url "$rpc_url" --unlocked --from "$owner" --broadcast --json | jq -er .deployedTo)
    usdc_code=$(cast code "$usdc" --rpc-url "$rpc_url")
    usdt_code=$(cast code "$usdt" --rpc-url "$rpc_url")
    oracle_code=$(cast code "$(jq -er .sanctions_oracle "$admin_dir/$chain.json")" --rpc-url "$rpc_url")
    docker run --rm -i --network none "$TOPUP_LOCAL_SERVICE_IMAGE" topup config show /dev/stdin \
        <"$env_dir/topup.yaml" >"$admin_dir/config.json"
    chain_id=$(cast chain-id --rpc-url "$rpc_url")
    while read -r token symbol oracle; do
        case "$symbol" in
            pha) code=$token_code ;;
            usdc) code=$usdc_code ;;
            usdt) code=$usdt_code ;;
            *) die "unexpected drill asset: $symbol" ;;
        esac
        cast rpc --rpc-url "$rpc_url" anvil_setCode "$token" "$code" >/dev/null
        cast rpc --rpc-url "$rpc_url" anvil_setCode "$oracle" "$oracle_code" >/dev/null
    done < <(jq -r --argjson id "$chain_id" '.routes[] | select(.chain.chain_id == $id) |
        [.asset.contract, .asset.symbol, .chain.sanctions_oracle] | @tsv' "$admin_dir/config.json")
done
# Real independent HTTPS names and verified certificates for both local sources.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -addext 'basicConstraints=critical,CA:FALSE' -subj /CN=rpc.test \
    -addext 'subjectAltName=DNS:*.rpc.test' -keyout "$admin_dir/rpc-key.pem" \
    -out "$admin_dir/rpc-cert.pem" >/dev/null 2>&1
cp "$admin_dir/config.json" "$admin_dir/topup.json"
python3 "$root/deploy/local/rpc-tls.py" "$admin_dir/topup.json" "$admin_dir/rpc-tls.json" --restore-check \
    --certificate "$admin_dir/rpc-cert.pem" --key "$admin_dir/rpc-key.pem" \
    --image python:3.14-slim-trixie@sha256:caaf356f40667c496d405780745b9ac25771c189a51dfcc42430d531ea09f8a2 \
    --chain 11155111=http://anvil:8545 --chain 84532=http://anvil-base:8545 \
    --chain 1=http://anvil-mainnet-price:8545 --chain 8453=http://anvil-base-mainnet-price:8545
chain_overlay+=(-f "$admin_dir/rpc-tls.json")
docker run --rm -i --network none "$TOPUP_LOCAL_SERVICE_IMAGE" topup config check /dev/stdin \
    <"$admin_dir/topup.json"
mv "$admin_dir/topup.json" "$env_dir/topup.yaml"

seed_drill_volumes
dc up -d keys s3-init mock-product
wait_for keys dc exec -T keys topup keys --check \
    --backup-dir /run/wal-g --owner-dir /run/db-owner --app-dir /run/db-app
dc up -d --no-deps postgres
wait_for postgres dc exec -T postgres pg_isready -U postgres -d topup
# The object store is empty, so the bootstrap listed no base backup and initialized a new cluster.
dc logs --no-log-prefix postgres 2>&1 |
    grep -Fx 'the backup prefix holds no base backup; initializing a new cluster' >/dev/null || {
    echo "the source did not initialize from a provably empty backup prefix" >&2
    exit 1
}
dc up -d --no-deps backup
# A new cluster has no base backup on its timeline, so backup takes one at start.
wait_for "startup base backup" startup_base_backup_listed
wait_for mock-product dc exec -T mock-product python3 -c \
    "import urllib.request; urllib.request.urlopen('http://localhost:8081/health')"
dc run --rm --no-deps migrate >/dev/null
seed_reconciliation_fixture
if [ "$mode" = controlled ]; then
    seed_consistency_fixture
    start_merchant_product
fi

# WAL-G 3.0.9 `backup-list --json` has `backup_name` and `time`; the newest is the one just pushed.
backup_name=$(dc exec -T backup sh -c 'wal-g backup-push "$PGDATA" >&2 && wal-g backup-list --json' |
    jq -er 'max_by(.time | sub("[.][0-9]+"; "") | fromdateiso8601) | .backup_name')
case "$backup_name" in
    base_*) ;;
    *) echo "could not determine WAL-G base backup name" >&2; exit 1 ;;
esac

# Exercise wrong-key and credential controls before timing the failure window.
last_archived_wal=$(psql_value 'SELECT last_archived_wal FROM pg_stat_archiver')
test -n "$last_archived_wal"
test_restore_failures_are_fatal "$last_archived_wal"
storage_probe_writes

# Time the segment that holds the first write, not whichever segment is current beforehand.
first=$(first_sample)
read -r first_marker timed_wal <<<"$first"
test -n "$first_marker" && test -n "$timed_wal"
if [ "$mode" = controlled ]; then
    last_marker=$(record_sample)
    # The end of the switched segment: everything up to it is archived, so it is restored.
    switch_lsn=$(psql_value 'SELECT pg_switch_wal()')
    wait_for_fast "forced WAL close" wal_closed "$timed_wal"
else
    samples_file=$(mktemp)
    printf '%s\n' "$first_marker" >"$samples_file"
    (
        while :; do
            sleep 1
            record_sample >>"$samples_file"
        done
    ) &
    writer_pid=$!
    wait_for_fast "archive_timeout WAL close" wal_closed "$timed_wal"
fi
wait_for_fast "archived WAL" wal_object_visible "$timed_wal"
if [ "$mode" = crash ]; then
    kill "$writer_pid" >/dev/null 2>&1 || true
    wait "$writer_pid" >/dev/null 2>&1 || true
    writer_pid=
    last_marker=$(tail -1 "$samples_file")
    rm -f "$samples_file"
    samples_file=
fi
# Server-side timestamps: first write into the segment, segment close, and object upload.
first_write_at=$(marker_epoch "$first_marker")
test -n "$segment_closed_at"
object_uploaded_at=$(wal_object_uploaded_epoch "$timed_wal")
archive_wait_seconds=$(seconds_between "$first_write_at" "$segment_closed_at")
upload_latency_seconds=$(seconds_between "$segment_closed_at" "$object_uploaded_at")
# File mtimes come from the kernel's coarse clock, up to one tick (<=10 ms) behind clock_timestamp().
# Each interval spans at least one psql round trip, so anything below that tolerance is an error.
for seconds in "$archive_wait_seconds" "$upload_latency_seconds"; do
    awk -v value="$seconds" 'BEGIN { exit !(value >= -0.010) }' || {
        echo "WAL timing is negative (${seconds}s); the timed segment is wrong" >&2
        exit 1
    }
done

if [ "$mode" = controlled ]; then
    expected_lsn=$switch_lsn
else
    expected_lsn=$(psql_value 'SELECT pg_current_wal_lsn()')
fi
expected_marker=$(psql_value 'SELECT max(id) FROM restore_drill_marker')
if [ "$mode" = controlled ]; then
    # Freeze archival progress at the forced switch, so every subsequent business change is
    # genuinely lost regardless of delivery latency or the 30-second automatic switch. This
    # is fault injection on the disposable source only; crash mode leaves the archiver running.
    dc exec -T postgres sh -eu -c '
        pid=$(pgrep -f "^postgres: archiver")
        kill -STOP "$pid"
        test "$(ps -o stat= -p "$pid" | cut -c 1)" = T
    '
    lose_consistency_changes
fi
dc stop backup >/dev/null
failure_at=$(date +%s.%N)
# The controlled source is killed too, once its consistency writes are made: a clean shutdown
# switches and archives the last segment (PostgreSQL's ShutdownXLOG with archiving on), which would
# keep them.
docker kill "${project}-postgres-1" >/dev/null
dc rm -f backup postgres heartbeat migrate >/dev/null 2>&1 || true
remove_pgdata_volume
# The key tmpfs volumes die with the source CVM; the replacement derives the backup key and
# database credentials again, which only the same app id (here: the same simulator keys) reproduces.
dc rm -s -f keys >/dev/null
for volume in walg_key db_owner db_app; do
    remove_volume "$volume"
done

# Replacement boot, exactly as dstack's app-compose.sh starts a CVM: the whole restore-check
# variant of deploy/RESTORE.md comes up at once. Its PostgreSQL reads the read-only restore
# credentials (RESTORE_AWS_*), while the live read-write names stay in the environment, as on an
# instance created without its own env file. PostgreSQL restores the newest base backup into the
# empty volume, replays every archived segment, and never archives; no heartbeat, backup, egress,
# or ingress runs; topup is read-only. No command runs inside the stack. Nothing may reach object
# storage from here on; the drill's own storage tool uses the replacement's read-only key.
storage_before=$(storage_listing)
test -n "$storage_before"
variant=(--restore-check)
export TOPUP_DRILL_S3_ACCESS_KEY_ID=topup-restore-read
export TOPUP_DRILL_S3_SECRET_ACCESS_KEY=topup-restore-read-secret
dc up --remove-orphans -d
dc logs --no-log-prefix postgres 2>&1 |
    grep -Fx "restoring base backup $backup_name" >/dev/null || {
    echo "the replacement did not restore the newest base backup $backup_name" >&2
    exit 1
}
recovery_promoted
test "$(psql_value 'SHOW archive_mode')" = off
no_background_work
restore_credentials_only
storage_is_read_only

# The operator's only view of the replacement: /healthz and the read API on its restore URL (in a
# CVM, port 8081 of the app's gateway URL, which the attested variant publishes instead of running
# dstack-ingress; validate-compose.sh checks that; here, topup's container port on the compose
# network).
wait_for "restore-check report on /healthz" restore_report_served
health=$(topup_get /healthz)
restore_report=$(printf '%s\n' "$health" | jq -ce '.restore_check')
printf '%s\n' "$health" | jq -e '.mode == "read-only"' >/dev/null
test "$(printf '%s\n' "$restore_report" | jq -er '.status')" = ok || {
    printf 'restore-check: %s\n' "$restore_report" >&2
    exit 1
}
test "$(topup_status POST /v1/admin/accounts)" = 503
# Frozen by restore-check: no merchant request is authenticated, reads included. One without a
# well-formed key is refused before the freeze gate (401); any well-formed key, at it (503).
test "$(topup_status GET '/v1/deposits?tx_hash=0x00')" = 401
test "$(call_status "$(merchant_call GET '/v1/deposits?tx_hash=0x00' "$(new_api_key)" \
    </dev/null)")" = 503

# restore-check logged in as the owner, and the application login works too: the restored roles
# carry the source's derived passwords, which the replacement derived again.
test "$(app_login_works)" = topup_service

# The boot-time report is unanchored; compare it with the source point recorded above, as the
# operator compares it with theirs.
# The marker is a committed transaction recovered from WAL. Heartbeat sampling does not
# grant an extra minute; the failure instant includes segment close and upload latency.
last_replayed_commit_at=$(psql_value 'SELECT extract(epoch FROM max(recorded_at)) FROM restore_drill_marker')
measured_rpo=$(seconds_between "$last_replayed_commit_at" "$failure_at")
allowed_rpo=$(printf '%s\n' "$restore_report" | jq -er '.allowed_rpo_seconds')
latest_applied_lsn=$(printf '%s\n' "$restore_report" | jq -er '.latest_applied_lsn')
wal_bytes_behind=$(psql_value \
    "SELECT GREATEST(pg_wal_lsn_diff('$expected_lsn', '$latest_applied_lsn'), 0)::bigint")
test "$(printf '%s\n' "$restore_report" | jq -er '.rpo_basis')" = unanchored
reconciliation=$(printf '%s\n' "$restore_report" | jq -er '.post_restore_reconciliation.status')
restored_marker=$(psql_value 'SELECT max(id) FROM restore_drill_marker')
restored_pricing=$(psql_value \
    "SELECT state || '|' || credit_minor::text || '|' || price_scaled::text FROM deposits WHERE id = '44444444-4444-4444-4444-444444444444'")

test "$reconciliation" = complete
# The service's own record is authoritative: the restore asks the product nothing and keeps it.
test "$restored_pricing" = 'credited|250|25000000'
test "$allowed_rpo" -eq 60
awk -v measured="$measured_rpo" 'BEGIN { exit !(measured >= 0 && measured <= 60) }'
if [ "$mode" = controlled ]; then
    test "$restored_marker" -eq "$expected_marker"
    test "$wal_bytes_behind" -eq 0
    check_consistency_after_restore
fi

# Promotion wrote a new timeline; close its segment and confirm nothing reached object storage.
psql_value 'SELECT pg_switch_wal()' >/dev/null
psql_value 'CHECKPOINT' >/dev/null
restored_timeline=$(psql_value 'SELECT timeline_id FROM pg_control_checkpoint()')
test "$restored_timeline" -gt 1
test "$(storage_listing)" = "$storage_before" || {
    echo "the restore-check instance changed object storage" >&2
    exit 1
}

# RTO ends only when the service variant has rescanned, an administrator lifts the freeze,
# and a real merchant credential successfully reads the API. No synthetic cursor updates.
variant=()
export TOPUP_DRILL_S3_ACCESS_KEY_ID=topup-s3
export TOPUP_DRILL_S3_SECRET_ACCESS_KEY=topup-s3-secret-key
# Admin signatures bind to the serving variant's origin, which differs from read-only boot.
# Initialize it here in both modes; crash has no preceding merchant reconciliation phase.
public_origin=$(dc config --format json |
    jq -er '.services.topup.command as $c | $c[($c | index("--public-origin")) + 1]')
dc up --remove-orphans -d
service_admin_ready() {
    answer=$(admin_call GET /v1/admin/restore)
    [ "$(call_status "$answer")" = 200 ]
}
wait_for "service admin API" service_admin_ready
service_rescanned() {
    answer=$(admin_call GET /v1/admin/restore)
    [ "$(call_status "$answer")" = 200 ] &&
        call_body "$answer" | jq -e '.rescan | all(.complete and (.blocked | not))' >/dev/null
}
wait_for "service rescan" service_rescanned
answer=$(admin_call POST /v1/admin/restore/unfreeze '{"reason":"restore drill: checks and merchant reconciliation complete",
    "security_changes_reapplied":true,"deposit_addresses_reissued":true,"quotes_reissued":true,
    "delivered_events_imported":true}')
expect_call 200 "$answer"
answer=$(merchant_call GET /v1/account "$resume_merchant_key" </dev/null)
expect_call 200 "$answer"
rto_elapsed=$(seconds_between "$failure_at" "$(date +%s.%N)")
awk -v elapsed="$rto_elapsed" 'BEGIN { exit !(elapsed >= 0 && elapsed <= 3600) }'

printf 'mode=%s\n' "$mode"
printf 'restore-check: %s\n' "$restore_report"
printf 'base_backup=%s\n' "$backup_name"
printf 'source_marker_range=%s..%s expected_last=%s restored_last=%s\n' \
    "$first_marker" "$last_marker" "$expected_marker" "$restored_marker"
printf 'measured_rpo_seconds=%s\n' "$measured_rpo"
printf 'allowed_rpo_seconds=%s\n' "$allowed_rpo"
printf 'wal_bytes_behind=%s\n' "$wal_bytes_behind"
printf 'archive_window_seconds=30\n'
printf 'archive_wait_seconds=%s\n' "$archive_wait_seconds"
printf 'upload_latency_seconds=%s\n' "$upload_latency_seconds"
printf 'restored_timeline=%s storage_unchanged_by_restore_check=true\n' "$restored_timeline"
printf 'elapsed_rto_seconds=%s\n' "$rto_elapsed"
if [ "$mode" = controlled ]; then
    echo 'restore_mode=frozen, merchant reads refused; attested by the admin API; lost key revoked again; lost treasury application restored from its signed event; lost deposit addresses and quote re-issued identically; the deliveries its receiver kept imported, credit kept, reversed deposits restored; all from the product'"'"'s records'
fi
echo "restore drill $mode passed"
