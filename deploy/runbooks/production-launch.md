# Production launch

This is the ordered, **HUMAN-ONLY** go-live procedure for Phala's production stablecoin pilot on
the **approved release** selected in §1.1. Agents and CI run none of it. The committed live routes
are USDC and USDT on Ethereum (chain 1) and USDC on Base (chain 8453), in
[production/topup](../environments/phala-network/production/topup). Base USDC adds a one-hour
sequencer uptime grace period. Both chains use the same deterministic factory, deployed on each chain
separately. Production reads with Ankr and verifies with BlockPI on both mainnets. Before §2,
the reviewed production RPC allocation and complete sealed mappings must be merged and pass
preflight (§1.8). Staging stays on Ankr + Infura; no dRPC move is required. Production D=46,
staging D=20, and every other cap stay unchanged
([RPC operations](../RPC.md#worst-case-pilot-budget)).

Each section ends with its checks. Do not start a section until every check of the previous one
passes, and record each result in the [sign-off checklist](#13-sign-off-checklist). A failed check
means stop, fix the cause, and repeat the step. Never work around it.

> **Screening launch gates:** before any live charges, verify a fresh OFAC SDN snapshot fetched
> from the SLS. Add and audit all four
> [EU/UK supplements](sanctions-list.md#launch-supplements-from-eu-and-uk-lists), confirm their
> re-screening completed, and prove that both a known SDN address and a supplement address are
> refused (§7, §8). The routes' `chain.sanctions_oracle` addresses are parsed only for N-1 rollback
> compatibility and play no part in screening.

**Pilot limits** ([RPC operations](../RPC.md#pilot-limits-and-operating-modes)), production
(staging in brackets):

| Limit | Value | Enforcement |
|---|---:|---|
| Deposits handled per UTC day, D | 46 (20) | Operational: tally and stop |
| Concurrent attached pending refunds, R (`max_attached_pending_refunds`) | 2 (1) | Service: `422 refund_attachment_limit_exceeded` |
| New refund attachments per rolling 24 h, N | 1 (1) | Service: same error |
| Unresolved finality stock S, and its new entries per rolling 24 h | 1 and 1 | Operational: pause the chain's quotes above it |
| Slow-confirmation stock L, and its new entries per rolling 24 h | 1 and 1 | Operational: pause the chain's quotes above it |
| Admitted hint tasks per UTC day, H | 80 (80) | Service: further hints fall back to scanning |
| Fresh quote price snapshots per price chain per UTC day, Q | 60 (60) | Service: `503 price_unavailable` |
| Factory receipt verifications per day | 60 (20) | Operational |
| Safe challenge/proof sets per day | 10 (2) | Operational |
| Extra RPC calls per endpoint per day | 150 (150) | Operational reserve |
| Historical issued addresses per payment chain | 1,000 | Service: `422 address_capacity_reached` |
| Ankr calls/day (shared seven-day run rate), Infura credits/day (staging), BlockPI RU/day (production seven-day run rate) | 25,000; 1,500,000; 1,250,000 | Stop lines: add no load at or above them |

D, the factory and Safe caps, the reserve, S and L have no code enforcement. Count recovery work and
failed attempts against them, and stop new load at each limit (§11). The UTC day starts at 5:00 PM
PDT (4:00 PM PST).

> **One shell session:** run the launch from the private work directory of §1.1, outside the
> repository, with `umask 077`. Keep §§4–10 in one session. If a new session is unavoidable,
> re-export `REPO_DIR`, `KIT_DIR`, `LAUNCH_WORK_DIR`, `APPROVED_COMMIT`,
> `RELEASE_TAG`, `RELEASE_COMMIT`, `GITHUB_REPOSITORY`, `ENV_DIR`, `ORIGIN`, `BASE_URL`,
> `TOPUP_CVM_ID`, `APP_ID`, `ATTESTED_APP_ID`, `COMPOSE_HASH`, `SMOKE_ACCOUNT_ID`,
> `FINANCE_MAX_UNFINALIZED_CREDIT` and `SMOKE_MERCHANT_SECRET_KEY`. Re-define the §1.1 and §6
> helpers and recreate `SMOKE_DEPOSIT_IDS` before continuing.

## 1. Owner prerequisites

Each required check is a gate for §2, including the production RPC allocation in §1.8; §1.10
marks the website setting as an owner decision. Never commit or paste a private key, token, seed,
or env file, and keep them out of chat and tickets.

### 1.1 Release checkout and kit

Keep a checkout of this repository's `main` at the approved commit Deploy will run. Set
`APPROVED_COMMIT` to the owner/Finance/Risk/Operations-approved **main commit**, distinct from
the release commit. Work in a private directory outside every checkout, using
`umask 077`; invoke repository and kit scripts by path. Import only temporary copies of secrets
from the owner's secret store into this directory, including `admin.seed` and
`maintenance/production.pem`; keep the recovery originals in that store. The deploy kit and
`images.json` come from that verified release, never from the checkout
([Verify a release](../../docs/self-hosting.md#verify-a-release)):

```sh
umask 077
export REPO_DIR="/absolute/path/to/phala-pay"
export REPO_DIR="$(cd "$REPO_DIR" && pwd -P)"
git -C "$REPO_DIR" switch main && git -C "$REPO_DIR" pull --ff-only
export APPROVED_COMMIT="<40-character approved main commit>"
[[ "$APPROVED_COMMIT" =~ ^[0-9a-f]{40}$ ]]
test "$(git -C "$REPO_DIR" rev-parse HEAD)" = "$APPROVED_COMMIT"
export LAUNCH_WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/phala-pay-launch.XXXXXX")"
export LAUNCH_WORK_DIR="$(cd "$LAUNCH_WORK_DIR" && pwd -P)"
[[ "$LAUNCH_WORK_DIR/" != "$REPO_DIR/"* ]]
cd "$LAUNCH_WORK_DIR"
export GITHUB_REPOSITORY=Phala-Network/phala-pay
export ENV_DIR="$REPO_DIR/deploy/environments/phala-network/production/topup"
export KIT_DIR="$LAUNCH_WORK_DIR/kit"
# Read the one caller pin at the approved configuration commit, shared by both environments.
git -C "$REPO_DIR" show "$APPROVED_COMMIT:.github/workflows/deploy-phala.yml" > deploy-caller.yml
export RELEASE_TAG="$(sed -n 's/^      version: \(v[^[:space:]]*\)$/\1/p' deploy-caller.yml)"
export RELEASE_COMMIT="$(sed -n 's|^    uses: Phala-Network/phala-pay/.github/workflows/deploy.yml@\([0-9a-f]\{40\}\) # .*|\1|p' deploy-caller.yml)"
[[ "$RELEASE_TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ ]]
[[ "$RELEASE_COMMIT" =~ ^[0-9a-f]{40}$ ]]
grep -Fxq "    uses: Phala-Network/phala-pay/.github/workflows/deploy.yml@$RELEASE_COMMIT # $RELEASE_TAG" deploy-caller.yml
test "$(cat "$ENV_DIR/os-image-hash")" = 8db40cb23008d6213668b5816702d100b37b859a8a5bfd18d938ae3a1b3715bd
test "$(bash "$REPO_DIR/deploy/verify-release.sh" "$RELEASE_TAG" release "$RELEASE_COMMIT")" = "$RELEASE_COMMIT"
mkdir "$KIT_DIR" && tar -xzf "release/phala-pay-deploy-$RELEASE_TAG.tar.gz" -C "$KIT_DIR" --strip-components=1
npm ci --prefix "$KIT_DIR/deploy/tools" --ignore-scripts

# dispatch_deploy MODE: prefer the dispatch's printed run id/URL; otherwise discover one new run
# for this actor, allowing 60 seconds of clock skew. Never redispatch on an ambiguous outcome.
dispatch_deploy() {
  local mode=$1 requested_at actor known_runs remote_main dispatch_output line run_ref head_sha
  local deadline=$((SECONDS + 300))
  DEPLOY_RUN_ID=""
  actor="$(gh api user --jq .login)" || return 1
  requested_at="$(date -u -d '60 seconds ago' +%Y-%m-%dT%H:%M:%SZ)" || return 1
  known_runs="$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/deploy-phala.yml/runs" \
    -f event=workflow_dispatch -f branch=main -f actor="$actor" -f "created=>=$requested_at" \
    -F per_page=100 --jq '[.workflow_runs[].id]')" || return 1
  remote_main="$(git ls-remote "https://github.com/$GITHUB_REPOSITORY" refs/heads/main | cut -f1)" || return 1
  [[ "$remote_main" == "$APPROVED_COMMIT" ]] || {
    echo "remote main is $remote_main, not APPROVED_COMMIT=$APPROVED_COMMIT; refusing to dispatch" >&2
    return 1
  }
  if ! dispatch_output="$(gh workflow run deploy-phala.yml --repo "$GITHUB_REPOSITORY" --ref main \
    -f environment=production -f target=topup -f mode="$mode")"; then
    echo "dispatch outcome uncertain; do not re-dispatch; check the Actions UI" >&2
    return 1
  fi
  printf '%s\n' "$dispatch_output"
  while IFS= read -r line; do
    run_ref="${line#https://github.com/$GITHUB_REPOSITORY/actions/runs/}"
    run_ref="${run_ref%/}"
    if [[ "$run_ref" =~ ^[0-9]+$ ]]; then DEPLOY_RUN_ID=$run_ref; break; fi
  done <<<"$dispatch_output"
  while [[ -z "$DEPLOY_RUN_ID" ]] && (( SECONDS < deadline )); do
    DEPLOY_RUN_ID="$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/deploy-phala.yml/runs" \
      -f event=workflow_dispatch -f branch=main -f actor="$actor" -f "created=>=$requested_at" \
      -F per_page=100 | jq -r --argjson known "$known_runs" '
        [.workflow_runs[] | select(.id as $id | $known | index($id) | not)]
        | if length == 1 then .[0].id else empty end')" || DEPLOY_RUN_ID=""
    [[ -n "$DEPLOY_RUN_ID" ]] || sleep 5
  done
  [[ -n "$DEPLOY_RUN_ID" ]] || {
    echo "no unique $mode run appeared within 300 seconds; do not re-dispatch; check the Actions UI" >&2
    return 1
  }
  head_sha="$(gh run view "$DEPLOY_RUN_ID" --repo "$GITHUB_REPOSITORY" --json headSha -q .headSha)" || return 1
  [[ "$head_sha" == "$APPROVED_COMMIT" ]] || {
    echo "run $DEPLOY_RUN_ID uses $head_sha, not APPROVED_COMMIT=$APPROVED_COMMIT; stop" >&2
    gh run cancel "$DEPLOY_RUN_ID" --repo "$GITHUB_REPOSITORY" ||
      echo "cancellation failed; check the Actions UI immediately" >&2
    if [[ "$mode" == provision ]]; then
      echo "check whether a CVM was created; follow §2's never provision twice recovery before any retry" >&2
    fi
    return 1
  }
  echo "Deploy $mode run: $DEPLOY_RUN_ID at $APPROVED_COMMIT" >&2
  gh run watch "$DEPLOY_RUN_ID" --repo "$GITHUB_REPOSITORY" --exit-status
}

# fetch_record RUN_ID DIR: downloads the run's deployment record (compose, images.json, CLI
# outputs, attestation, verification); it holds no secret.
fetch_record() {
  local run_id=$1 dir=$2 attempt
  attempt="$(gh run view "$run_id" --repo "$GITHUB_REPOSITORY" --json attempt --jq .attempt)" || return 1
  gh run download "$run_id" --repo "$GITHUB_REPOSITORY" \
    -n "production-topup-deploy-$run_id-$attempt" -D "$dir"
}
```

`verify-release.sh` verifies the tag, caller commit, kit, image digests and their attestations.
The workstation needs Docker, Foundry `cast`, `jq`, `gh` logged in with write access, the AWS CLI, `dig`, `openssl`, `xxd`, `uv`, and GNU
`timeout` ([Deployment time budgets](../README.md#deployment-time-budgets)).

**Checks:** every command exits 0. Record `APPROVED_COMMIT`, `RELEASE_TAG`, `RELEASE_COMMIT`
and `release/images.json` in the sign-off. Production and staging intentionally share the one
caller pin: adopting a release updates both environments. Production launches the exact release
staging has soaked; retain staging's successful deployment record and soak approval, and verify
its tag, commit and images match these values before §2. Record the previously deployed
production release's tag, commit, kit, images, configuration commit and deployment record as the
rollback target (§12), or explicitly record that this is the first production deployment.
`LAUNCH_WORK_DIR` is outside the repository and has mode 0700; `umask` is 0077. `os-image-hash` is the approved non-dev `dstack-0.5.12`
hash ([OS image](../README.md#os-image)).

### 1.2 Phala Cloud workspace

Create the production Phala Cloud workspace and an API key scoped to it, separate from staging's.
The online preflight in §4 proves that the workspace offers `dstack-0.5.12` on a `tdx.medium` node.
Retain the [OS-image approval evidence](../README.md#approval-evidence-for-dstack-0512): the
production `os-images` listing, release-archive checksum, and verified `sha256sum.txt` hash and
file checks. Production provisions directly on 0.5.12. Existing CVMs on an older image use
Deploy's in-place image upgrade before compose deployment, with health recovery and a fresh
maintenance lease between the two reboots ([timing and interval](../README.md#planned-upgrade-admission-and-downtime)).
Keep a copy of the key in the owner's secret store for §4 and §9.

```sh
export PHALA_WORKSPACE="<production workspace display name>"
```

**Check:** the workspace's display name is recorded as `PHALA_WORKSPACE`.

### 1.3 GitHub `production` Environment

Restrict the Environment to `main` and set its secrets and variables without printing them. Each
`gh secret set` without `--body` prompts for the value:

```sh
gh api --method PUT "repos/$GITHUB_REPOSITORY/environments/production" --input - <<'JSON'
{"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}}
JSON
gh api --method POST "repos/$GITHUB_REPOSITORY/environments/production/deployment-branch-policies" \
  -f name=main
gh secret set PHALA_CLOUD_API_KEY --repo "$GITHUB_REPOSITORY" --env production
gh secret set SENTRY_DSN --repo "$GITHUB_REPOSITORY" --env production
gh secret set TOPUP_MAINTENANCE_PRIVATE_KEY_PEM --repo "$GITHUB_REPOSITORY" --env production \
  < maintenance/production.pem
gh variable set PHALA_WORKSPACE --repo "$GITHUB_REPOSITORY" --env production --body "$PHALA_WORKSPACE"
# Owner-approved placement: prod5, node id 26, US-WEST-1 in the production workspace.
gh variable set PHALA_NODE_ID --repo "$GITHUB_REPOSITORY" --env production --body 26
gh variable set TOPUP_MAINTENANCE_KEY_ID --repo "$GITHUB_REPOSITORY" --env production \
  --body maintenance/production-v1
```

`SENTRY_DSN` here is only for Deploy's `--require-sentry` preflight. The CVM gets its own sealed
copy in §4. Leave `TOPUP_CVM_ID` unset until §2. Prove that the offline keys match the committed
public keys in `topup.yaml`:

```sh
(umask 077 && { printf '302e020100300506032b657004220420'; tr -d '\n' < admin.seed; } | xxd -r -p | openssl pkey -inform DER -out admin.pem)
test "$(openssl pkey -in admin.pem -pubout -outform DER | tail -c 32 | base64)" = zoojlIGtFOK8IsaTbkXIgtm9oBsfWzzqJh1X8IEpp3U=
test "$(openssl pkey -in maintenance/production.pem -pubout -outform DER | tail -c 32 | base64)" = 3Gnbo5Er8uURAOH1OON7mNZWpghmbJTeyelV7HsLjhE=
gh api "repos/$GITHUB_REPOSITORY/environments/production" --jq '.deployment_branch_policy'
gh api "repos/$GITHUB_REPOSITORY/environments/production/secrets" --jq '[.secrets[].name] | sort'
gh api "repos/$GITHUB_REPOSITORY/environments/production/variables" --jq '[.variables[].name] | sort'
gh variable get PHALA_NODE_ID --repo "$GITHUB_REPOSITORY" --env production
```

**Checks:** the branch policy is `custom_branch_policies: true`, with `main` as the only policy.
The secrets are exactly `PHALA_CLOUD_API_KEY`, `SENTRY_DSN` and `TOPUP_MAINTENANCE_PRIVATE_KEY_PEM`.
The variables are `PHALA_WORKSPACE`, `PHALA_NODE_ID` (`26`) and `TOPUP_MAINTENANCE_KEY_ID`, and
`TOPUP_CVM_ID` is absent. `PHALA_NODE_ID` is optional for other instances: empty or unset means
automatic selection; a set value must be digits. The called Deploy job reads it from the
Environment and passes `--node-id` only on provision; upgrades keep the CVM's node.
Both public keys match: `admin/production-v1` and `maintenance/production-v1`.

### 1.4 R2 bucket and tokens

Create the empty **`phala-pay-production`** bucket with account-level R2 credentials. The committed
prefix is `s3://phala-pay-production/production-v1`, never staging's. Then issue two tokens,
both scoped to that bucket only. The first is read-write and is sealed into the CVM. The second is
**Object Read only**, for backup checks and the restore drill:

```sh
export AWS_ENDPOINT=https://1d694c298092ffa09c793cbca4587812.r2.cloudflarestorage.com
read -rs -p "R2 account-level access key: " AWS_ACCESS_KEY_ID; echo
export AWS_ACCESS_KEY_ID
read -rs -p "R2 account-level secret key: " AWS_SECRET_ACCESS_KEY; echo
export AWS_SECRET_ACCESS_KEY
aws s3api create-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT" --region auto
aws s3api head-bucket --bucket phala-pay-production --endpoint-url "$AWS_ENDPOINT"
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
# In the Cloudflare R2 dashboard, issue the bucket-scoped read-write and Object Read only tokens.
read -rs -p "R2 read-only access key: " AWS_ACCESS_KEY_ID; echo
export AWS_ACCESS_KEY_ID
read -rs -p "R2 read-only secret key: " AWS_SECRET_ACCESS_KEY; echo
export AWS_SECRET_ACCESS_KEY
test -z "$(aws s3 ls s3://phala-pay-production/production-v1/ --recursive --endpoint-url "$AWS_ENDPOINT")"
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
```

**Checks:** the bucket exists and the prefix lists nothing. PostgreSQL initializes only on an empty
prefix ([RESTORE.md](../RESTORE.md#bootstrap-from-backup)). Neither token can reach staging's
`crypto-topup-test` bucket. §5 exports the read-only token again for the backup and restore
checks.

### 1.5 Deploy factories, or verify if already deployed

In a separate shell with `umask 077`, stay in the §1.1 private work directory and call scripts
by path from a separate clone of Phala Pay at `RELEASE_TAG`, with submodules, because
deploying builds the contracts ([CONTRACTS.md, "Mainnet"](../CONTRACTS.md#mainnet) and
["Base mainnet"](../CONTRACTS.md#base-mainnet)). Broadcasting is a human action. Use a funded
throwaway deployer EOA if deployment is needed, and keep `PRIVATE_KEY` only in the process
environment. If a factory is already deployed, skip that chain's deploy commands and run its
verification only. A mismatched deployment blocks launch; never broadcast to repair it. The four RPC URLs
are two independent operator providers per chain, not the service's production keys, so they
consume none of its quota:

```sh
umask 077
export LAUNCH_WORK_DIR="<private work directory from §1.1>"
cd "$LAUNCH_WORK_DIR"
export RELEASE_TAG="<verified release tag from §1.1>"
export RELEASE_COMMIT="<verified release commit from §1.1>"
export CONTRACT_REPO_DIR="/absolute/path/to/phala-pay-contracts"
git -C "$CONTRACT_REPO_DIR" checkout "$RELEASE_TAG"
test "$(git -C "$CONTRACT_REPO_DIR" rev-parse HEAD)" = "$RELEASE_COMMIT"
git -C "$CONTRACT_REPO_DIR" submodule update --init --recursive
read -rs -p "Factory deployer private key: " PRIVATE_KEY && printf '\n'
export PRIVATE_KEY
export MAINNET_RPC_A="<ethereum-mainnet-rpc-a>" MAINNET_RPC_B="<ethereum-mainnet-rpc-b>"
export BASE_RPC_A="<base-mainnet-rpc-a>" BASE_RPC_B="<base-mainnet-rpc-b>"

"$CONTRACT_REPO_DIR/deploy/contracts/deploy-proxy.sh" --rpc-url "$MAINNET_RPC_A"
"$CONTRACT_REPO_DIR/deploy/contracts/deploy-factory.sh" --rpc mainnet/a="$MAINNET_RPC_A" --dry-run
"$CONTRACT_REPO_DIR/deploy/contracts/deploy-factory.sh" --rpc mainnet/a="$MAINNET_RPC_A" --broadcast
"$CONTRACT_REPO_DIR/deploy/contracts/verify-deployment.sh" --rpc mainnet/a="$MAINNET_RPC_A" --rpc mainnet/b="$MAINNET_RPC_B" > mainnet-contract-verification.json
jq -e '.passed == true' mainnet-contract-verification.json

"$CONTRACT_REPO_DIR/deploy/contracts/deploy-proxy.sh" --rpc-url "$BASE_RPC_A"
"$CONTRACT_REPO_DIR/deploy/contracts/deploy-factory.sh" --rpc base/a="$BASE_RPC_A" --dry-run
"$CONTRACT_REPO_DIR/deploy/contracts/deploy-factory.sh" --rpc base/a="$BASE_RPC_A" --broadcast
"$CONTRACT_REPO_DIR/deploy/contracts/verify-deployment.sh" --rpc base/a="$BASE_RPC_A" --rpc base/b="$BASE_RPC_B" > base-contract-verification.json
jq -e '.passed == true' base-contract-verification.json
unset PRIVATE_KEY
```

The dry runs must print factory `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` and implementation
`0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9`. If the code is already deployed, the broadcast sends
nothing.

**Checks:** both reports have `passed: true`. The factory, implementation, sample forwarder, and
runtime code hashes are identical across both reports and the Sepolia report. Keep both reports.
Any mismatch blocks the launch. Update [CONTRACTS.md](../CONTRACTS.md#mainnet) Mainnet and Base
mainnet with the verified deployment details and reports after this step. In that same reviewed
PR, remove each deployed chain (`1` and `8453`) from `PENDING_DEPLOYMENT_CHAINS` in
[verify-contracts.yml](../../.github/workflows/verify-contracts.yml). The pending exception permits
only two exactly empty code results and emits a warning; any non-empty code is fully verified.
Merge the evidence PR before §2, update the checkout and `APPROVED_COMMIT` to the approved main
commit, and repeat the §1.1 pin checks and §1.6 approval for that commit.

### 1.6 Finance, Risk, and Operations sign-off

Finance, Risk, and Operations approve the following in writing
([Deploy, "Production"](../README.md#deploy)):

- every route's `merchant` defaults and bounds: `min_amount`, `max_deposit_atomic`,
  `min_refund_atomic`, and `quote_spread_bps`;
- `confirmations` (Ethereum 2, Base 3) and the Base `grace_s: 3600`;
- the integer-cents `max_unfinalized_credit` for both live accounts;
- any account `limits` that differ from the defaults;
- the pilot limits above.

The owner approved the committed pilot amounts on 2026-10-13: $5 minimum sale and $10,000 maximum
quote, with `max_unfinalized_credit` of 1,000,000 cents ($10,000) per live account. One quote is
bounded by the account's live `max_open_amount_per_customer`, whose default is $5,000, so §10
raises Phala Cloud's to 1,000,000 cents. Each route's maximum deposit is 10,100 tokens, so a
$10,000 quote overpaid within its default 1% tolerance is still accepted.
Any change is a reviewed PR to `topup.yaml`, merged **before** §2. A settings change after
provisioning costs an extra upgrade and changes the compose hash.

**Checks:** a signed approval names the `topup.yaml` commit that §2 will deploy, and records the
`max_unfinalized_credit` value.

### 1.7 Treasury Safes

Finance names the Safes for the smoke account and for Phala Cloud on Ethereum and Base. Their owners
and threshold must be able to sign Safe messages, because §8 and §10 prove each Safe with EIP-1271.
Deploy each Safe before its proof, because ERC-6492 signatures are refused. Then verify each Safe
against a Finance-approved expectations file in the format of
[safe-expectations.json](../contracts/safe-expectations.json), one file per Safe, in the §1.5
shell:

```sh
"$CONTRACT_REPO_DIR/deploy/contracts/verify-safe.sh" --expectations phala-cloud-ethereum-safe.json \
  --rpc mainnet/a="$MAINNET_RPC_A" --rpc mainnet/b="$MAINNET_RPC_B" > phala-cloud-ethereum-safe-verification.json
jq -e '.passed == true' phala-cloud-ethereum-safe-verification.json
```

Repeat the check for the Phala Cloud Base Safe with `base/a` and `base/b`, and for both smoke Safes.

**Checks:** all four reports have `passed: true`. The owners, threshold, singleton, modules, guard,
and fallback handler match Finance's approval.

### 1.8 RPC accounts and production allocation

Production uses Ankr read and BlockPI verify on Ethereum and Base, with
`max_log_blocks: 1000` for BlockPI's measured 1,024-block free-tier cap. Staging retains Ankr
read and Infura verify on every configured chain, including mainnet price observations.
Never register extra free accounts to stack quotas. The two BlockPI chain keys share one account.

1. Merge the reviewed production allocation PR before §2. Its `topup.yaml` selects
   `blockpi-mainnet` and `blockpi-base-mainnet`; production and restore-check map
   `TOPUP_RPC_ANKR_KEY`, `TOPUP_RPC_BLOCKPI_ETHEREUM_KEY` and `TOPUP_RPC_BLOCKPI_BASE_KEY`, with
   no `TOPUP_RPC_INFURA_KEY`. Record the PR, Operations owner and passing configuration,
   compose-path and consecutive RPC checks. Staging configuration stays unchanged.
2. Keep BlockPI archive mode off and create one key for each mainnet in the same free account.
   Store these values in the owner's secret store. §4 seals them with the complete production
   secret set. Production-attested comments are updated before first provisioning; later
   configuration changes require a reviewed upgrade.
3. Accept the [per-account budget](../RPC.md#worst-case-pilot-budget): 17,029 shared Ankr
   calls/day, 736,331 staging-only Infura credits/day and 249,848 production-only BlockPI RU/day.
   Compare both environments' counters with the dashboards. Coordinate any Ankr key rotation
   across both environments. No staging re-seal or upgrade is required by this allocation.

Production D=46, staging D=20, and every other cap stay unchanged. Any later cap change requires
a separately reviewed and approved budget.

**Checks:** the production RPC allocation PR is merged and its configuration, sealed mappings
and preflight pass before §2. Record its PR and Operations owner. Operations accepts the resulting
budget and unchanged caps. Staging remains Ankr + Infura.

### 1.9 Sentry

Use only the project's error DSN (Settings > Client Keys) as `SENTRY_DSN`, sealed into the CVM and
stored as the §1.3 Environment secret. Sentry Uptime monitors are disabled for this project. Do not
create one, per the owner's decision. For this pilot the owner accepts that external TLS, DNS,
and ingress failures between manual checks may go undetected while the CVM still checks in.
`pilot_watch` (§11) checks external health on the accepted cadence. Then complete the following
([Sentry](../README.md#sentry)):

- Settings > Security & Privacy: keep *Data Scrubber* and *Use Default Scrubbers* on, and turn
  *Prevent Storing of IP Addresses* on.
- One issue alert for the environments `staging` and `production`, never `*-restore`, on issue
  created or regressed, with no level filter, notifying the on-call owner.
- The Crons monitors create themselves on their first `production` check-in. §6 checks them.

**Check:** a synthetic staging event from
[Synthetic alert validation](README.md#synthetic-alert-validation) reached the on-call owner
through this rule. Record the issue URL and the notification receipt.

### 1.10 Cloudflare

- **`phala.com` DNS** for `pay-api.phala.com`: the owner can edit the zone. No record exists yet
  for `pay-api.phala.com` or `_dstack-app-address.pay-api.phala.com`. The records in §3 are
  **DNS only** (grey cloud): a proxied name hides the gateway from the CA and from clients
  ([Custom domain](../README.md#custom-domain)). No CAA record on `phala.com` may forbid
  `tls-alpn-01`.
- **`pay.phala.com` website:** it is not on the payment path, but its CSP
  ([public/_headers](../product/web/public/_headers)) allows only its own and hashed scripts. Turn
  off unwanted script injection (Bot Fight Mode, Rocket Loader, Email Address Obfuscation, Web
  Analytics automatic setup, Zaraz) for that hostname. For **JavaScript Detections**, the owner
  chooses either to disable it for `pay.phala.com` or to accept its CSP console violations.
  Record that decision. This website setting is not a payment-path or launch gate.

```sh
dig +short CNAME pay-api.phala.com
dig +short TXT _dstack-app-address.pay-api.phala.com
dig +short CAA phala.com
curl -fsS https://pay.phala.com/ | grep -c '/cdn-cgi/' || true
```

**Checks:** the first two lookups print nothing. CAA is empty or permits
`letsencrypt.org; validationmethods=tls-alpn-01`. Inspect the site's HTML and browser console and
record the owner's JavaScript Detections decision, including any accepted CSP violations.

### 1.11 Staging live SDN smoke prerequisite

Before production provisioning, complete the staging live smoke required by
[sanctions-list.md](sanctions-list.md#n-1-rollback): verify the active OFAC snapshot and a known
SDN hit. Use the §7–8 refused-refund check against staging, with a staging smoke deposit and a
known SDN address from staging's active snapshot. Owner/Compliance signs off the address and the
refused request. Record the snapshot hash, the `400 destination_sanctioned` response and the
absence of a refund; clean up the staging smoke artifacts.

**Checks:** the staging live SDN smoke passed and its evidence and owner/Compliance sign-off are
recorded before §2.

## 2. Provision the topup CVM

Dispatch provisioning exactly once from `main`. These are the workflow's inputs; the caller fixes
everything else:

| Input | Value |
|---|---|
| Workflow, ref | `deploy-phala.yml` ("Deploy Phala's instance"), `main` |
| `environment` | `production` |
| `target` | `topup` |
| `mode` | `provision` (here), then `upgrade` (§5) |
| Fixed by the caller | `version: RELEASE_TAG`, `deploy.yml@RELEASE_COMMIT` from §1.1, `environment_dir: deploy/environments/phala-network/production/topup` |
| Environment placement | `PHALA_NODE_ID: 26` (prod5, US-WEST-1); provision only, upgrade keeps the node |
| Fixed by Deploy | OS image `dstack-0.5.12` non-dev, `tdx.medium`, Phala KMS, name `phala-pay-production-<run id>` |

```sh
dispatch_deploy provision
fetch_record "$DEPLOY_RUN_ID" provision-record
export PROVISION_RUN_ID="$DEPLOY_RUN_ID"
export TOPUP_CVM_ID="$(jq -er '.vm_uuid' provision-record/deploy.json)"
gh variable set TOPUP_CVM_ID --repo "$GITHUB_REPOSITORY" --env production --body "$TOPUP_CVM_ID"
```

Continuing launch requires the run's `headSha == APPROVED_COMMIT`; a mismatch or unknown SHA
means stop. On mismatch the helper requests cancellation, but provisioning may already have
created a CVM: inspect the record and Phala Cloud, preserve any created CVM, and follow the
"never provision twice" recovery below. Resolve the commit mismatch before continuing launch.
If an approved run fails, fetch its record anyway. If `deploy.json` names a CVM, set `TOPUP_CVM_ID` and
continue with §3: never provision twice
([Deploy, step 2](../README.md#deploy)). If no CVM was created, fix the cause and dispatch
`provision` again only after confirming in Actions and Phala Cloud that no CVM exists. Never
re-dispatch merely because run discovery timed out. If `deploy.json` names the CVM but the record
lacks `cvm.json` or `attestation.json`, fetch the missing documents before §3:

```sh
read -rs -p "Production Phala Cloud API key: " PHALA_CLOUD_API_KEY; echo
export PHALA_CLOUD_API_KEY
test -s provision-record/cvm.json ||
  "$KIT_DIR/deploy/phala" cvms get "$TOPUP_CVM_ID" --json > provision-record/cvm.json
test -s provision-record/attestation.json ||
  "$KIT_DIR/deploy/phala" cvms attestation "$TOPUP_CVM_ID" --json > provision-record/attestation.json
```

The new CVM shows `error` in Phala Cloud until §4 seals it, by design. Stop if either read fails.

**Checks:** the run summary lists the CVM id, the app id, release `RELEASE_TAG` at `RELEASE_COMMIT`, and the `phala-pay` and
`postgres-walg` digests that match `release/images.json`. The run's `headSha` equals
`APPROVED_COMMIT` (asserted by `dispatch_deploy`). It also lists OS image `dstack-0.5.12`
and the read-back verification. Record the requested node `26` and the actual CVM node's id,
name and region from the summary and `provision-record/cvm.json` (`.node_info`); require node `26`
(prod5, US-WEST-1) before continuing. `TOPUP_CVM_ID` is set.

## 3. Create the DNS records

Take the records from the provision record. They must equal the run summary's
"Owner: DNS records" table. The TXT value is the attested `instance_id`, not `TOPUP_CVM_ID`:

```sh
export GATEWAY_HOST="$("$KIT_DIR/deploy/phala-cvm.sh" gateway-domain provision-record/cvm.json)"
export INSTANCE_ID="$("$KIT_DIR/deploy/phala-cvm.sh" instance-id provision-record/attestation.json)"
echo "CNAME pay-api.phala.com $GATEWAY_HOST"
echo "TXT _dstack-app-address.pay-api.phala.com $INSTANCE_ID:443"
```

Create both records in Cloudflare as **DNS only**, then wait until public resolvers return them:

```sh
test "$(dig +short CNAME pay-api.phala.com @1.1.1.1)" = "$GATEWAY_HOST."
test "$(dig +short TXT _dstack-app-address.pay-api.phala.com @1.1.1.1)" = "\"$INSTANCE_ID:443\""
```

**Checks:** both tests pass. The committed `public_origin` is `https://pay-api.phala.com` and the
ingress `DOMAIN` is `pay-api.phala.com`.

## 4. Preflight with the production keys and seal

Use the provision record's rendered compose and the release's pinned image. The online preflight
uses the complete env. It runs `topup rpc check` through the compose against all four production
endpoints, plus each route's contract and asset checks and the workspace and OS image checks
([Sealing the secrets](../README.md#sealing-the-secrets)):

```sh
export COMPOSE_PROVISION="provision-record/docker-compose.production.yml"
docker pull "$(jq -er '."phala-pay"' release/images.json)"
read -rs -p "Production Phala Cloud API key: " PHALA_CLOUD_API_KEY; echo
export PHALA_CLOUD_API_KEY
umask 077
: > .env.production
${EDITOR:-vi} .env.production
"$KIT_DIR/deploy/preflight.sh" --env .env.production --compose "$COMPOSE_PROVISION" \
  --environment-dir "$ENV_DIR" --workspace "$PHALA_WORKSPACE" --os-image dstack-0.5.12 \
  --require-sentry
"$KIT_DIR/deploy/phala" envs update "$TOPUP_CVM_ID" -e .env.production
```

`.env.production` holds exactly these sealed names, one `NAME=VALUE` per line:
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` (the read-write token), `SENTRY_DSN`,
`TOPUP_RPC_ANKR_KEY`, `TOPUP_RPC_BLOCKPI_ETHEREUM_KEY` and `TOPUP_RPC_BLOCKPI_BASE_KEY`.
Production does not seal an Infura key. Write it with the editor, never with a heredoc,
`printf`, or shell history.

**Checks:** preflight ends with `preflight: all checks passed`. That includes "both RPC endpoints
passed the typed self-test through compose", `verify-deployment.sh` on chains 1 and 8453, and the
workspace offering `dstack-0.5.12`. `envs update` succeeds.

## 5. Acceptance upgrade

The upgrade first pauses mutations through `https://pay-api.phala.com` with the maintenance key.
That needs the sealed CVM serving its certificate, so wait for the service before dispatching. The
upgrade sends no env, so the sealed secrets stay:

```sh
HEALTH_WAIT_SECONDS=1800 "$KIT_DIR/deploy/phala-cvm.sh" healthz https://pay-api.phala.com
dispatch_deploy upgrade
fetch_record "$DEPLOY_RUN_ID" upgrade-record
export UPGRADE_RUN_ID="$DEPLOY_RUN_ID"
export AWS_ENDPOINT=https://1d694c298092ffa09c793cbca4587812.r2.cloudflarestorage.com
read -rs -p "R2 read-only access key: " AWS_ACCESS_KEY_ID; echo
export AWS_ACCESS_KEY_ID
read -rs -p "R2 read-only secret key: " AWS_SECRET_ACCESS_KEY; echo
export AWS_SECRET_ACCESS_KEY
aws s3 ls "s3://phala-pay-production/production-v1/wal_005/" --endpoint-url "$AWS_ENDPOINT" | tail -1
shred -u .env.production
```

**Checks:** the run passed. That means mutations paused, the CVM is running, `/healthz` answered,
the attested compose verified with the OS pin, the certificate evidence verified, and mutations
resumed. The summary's outage windows and total unavailability are recorded. The newest `wal_005/`
segment is less than two minutes old. If the run failed with mutations still paused, follow
[instance maintenance](instance-maintenance.md).

## 6. Verify attestation, OS image, ingress, health, RPC, sanctions, and metrics

Verify the guest independently of Deploy, using the upgrade record's compose:

```sh
mapfile -t UPGRADE_COMPOSE_FILES < <(find upgrade-record -type f -name 'docker-compose.*.yml' -print)
test "${#UPGRADE_COMPOSE_FILES[@]}" -eq 1
export UPGRADE_COMPOSE="${UPGRADE_COMPOSE_FILES[0]}"
"$KIT_DIR/deploy/phala" cvms get "$TOPUP_CVM_ID" --json > cvm.json
"$KIT_DIR/deploy/phala" cvms attestation "$TOPUP_CVM_ID" --json > attestation.json
export APP_ID="$(jq -er '.app_id' cvm.json)"
export ATTESTED_APP_ID="$(jq -er '.app_id | ltrimstr("0x") | ascii_downcase' cvm.json)"
export COMPOSE_HASH="$(jq -j '.compose_file' attestation.json | sha256sum | cut -d' ' -f1)"
export INSTANCE_ID="$("$KIT_DIR/deploy/phala-cvm.sh" instance-id attestation.json)"
export GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' cvm.json)"
curl -fsS "https://${INSTANCE_ID}-8090.$GATEWAY_DOMAIN/prpc/Info" > info.json
EXPECTED_OS_IMAGE_HASH="$(cat "$ENV_DIR/os-image-hash")"
"$KIT_DIR/deploy/verify-attestation.sh" attestation.json info.json "$APP_ID" "$UPGRADE_COMPOSE" service "$EXPECTED_OS_IMAGE_HASH"
"$KIT_DIR/deploy/verify-ingress-evidence.sh" pay-api.phala.com "$APP_ID"
test "$(curl -sS -o healthz.body -w '%{http_code}' https://pay-api.phala.com/healthz)" = 200
test ! -s healthz.body
```

Set up the admin helper as in [runbook environment](README.md#environment). Then read the metrics
and the daily report:

```sh
export ORIGIN=https://pay-api.phala.com
export BASE_URL="$ORIGIN"
export ADMIN_KEY_FILE="$LAUNCH_WORK_DIR/admin.pem" ADMIN_KEY_ID=admin/production-v1
admin() {
  local body="$LAUNCH_WORK_DIR/admin-body.json"
  printf '%s' "${3:-}" > "$body" || return 1
  mapfile -t headers < <("$REPO_DIR/deploy/runbooks/sign-admin-request.sh" "$1" "$BASE_URL$2" "$body" "$ADMIN_KEY_FILE" "$ADMIN_KEY_ID")
  curl --fail-with-body -sS -X "$1" -H 'content-type: application/json' -H "${headers[0]}" -H "${headers[1]}" -H "${headers[2]}" --data-binary @"$body" "$BASE_URL$2"
}

# metric_lines NAME FILE: the samples of one metric, without the other metrics that share its prefix.
metric_lines() { awk -v m="$1" '{ n = $1; sub(/\{.*/, "", n) } n == m' "$2"; }

admin GET /v1/admin/metrics > metrics-launch.txt
admin GET /v1/admin/reports/daily > daily-launch.json
metric_lines topup_rpc_endpoint_ready metrics-launch.txt
metric_lines topup_rpc_endpoint_ready metrics-launch.txt | awk '$NF + 0 == 1 { ok++ } END { exit !(NR == 4 && ok == 4) }'
metric_lines topup_addresses_lagging metrics-launch.txt
metric_lines topup_coverage_lag_seconds metrics-launch.txt
metric_lines topup_issued_address_cap metrics-launch.txt
metric_lines topup_sanctions_list_verified_age_seconds metrics-launch.txt
jq -e '.sanctions_snapshot != null' daily-launch.json
jq '{sanctions_snapshot, sanctions_manual_entries, reconciliation_blocks, routes: [.routes[] | {route, chain_id}]}' daily-launch.json
```

In Sentry, the `production` environment's Crons monitors connect within about an hour. They are
`topup-fast-scanner-1`, `topup-fast-scanner-8453`, `topup-checkpoint-scanner-1`,
`topup-checkpoint-scanner-8453`, `topup-coverage-scanner-1`, `topup-coverage-scanner-8453`,
`topup-pump-<n>`, `topup-outbox-test`, `topup-outbox-live`, `topup-lock-expiry`,
`topup-finality-watch`, `topup-reconciler`, `topup-backup` and `topup-sanctions-refresh`.

**Checks:**

- The verifier reports `UpToDate` TCB, the app id, the compose hash of the upgrade record's compose,
  and OS image hash `8db40cb2…15bd`.
- The certificate evidence verifies for `$APP_ID`, and `/healthz` answers `200` with an empty body.
- The four endpoints `ankr-mainnet`, `blockpi-mainnet`, `ankr-base-mainnet` and
  `blockpi-base-mainnet` are ready (`1`). `topup_addresses_lagging` is `0`, and the address cap is
  `1000` per chain.
- The daily report's `sanctions_snapshot` is present, with a `publish_date` matching the
  [OFAC SLS page](https://sanctionslist.ofac.treas.gov/Home/SdnList) and a `verified_at` within the
  last hour. `topup_sanctions_list_verified_age_seconds` is below 3600.
- `reconciliation_blocks` is empty, the three live routes are listed, and every Crons monitor
  above is connected and `ok`.

## 7. Sanctions launch gates

Add the four EU/UK supplements exactly as in
[Launch supplements from EU and UK lists](sanctions-list.md#launch-supplements-from-eu-and-uk-lists),
with `BASE_URL=https://pay-api.phala.com`. Then confirm them:

```sh
admin GET /v1/admin/sanctions/manual | jq
admin GET /v1/admin/reports/daily | jq '{sanctions_snapshot, sanctions_manual_entries}'
```

Owner/Compliance signs off using a known SDN EVM address from the active snapshot in §8's
refused refund request. It records an audited screening decision against the smoke deposit. The download
must have the active snapshot's SHA-256. If OFAC published in between, wait for the next hourly
refresh and download again:

```sh
curl -fsSL https://sanctionslistservice.ofac.treas.gov/api/download/SDN.XML -o SDN.XML
test "$(sha256sum SDN.XML | cut -d' ' -f1)" = "$(admin GET /v1/admin/reports/daily | jq -er '.sanctions_snapshot.sha256 | ascii_downcase')"
grep -oE '0x[0-9a-fA-F]{40}' SDN.XML | sort -u | head -20
```

**Checks:**

- All four entries are listed with their `source_ref`, and `sanctions_manual_entries` is `4`.
- Sentry shows no `TopupSanctionsRescreenFailed`, `TopupSanctionsListStale`, or
  `TopupSanctionsListVerifyFailed` for `production` since the additions.
- A successful hourly refresh after the additions shows in `topup_sanctions_list_refresh_total`
  (`unchanged` or `activated`).
- Compliance records the chosen address and its SDN entry `uid` as `SDN_EVM_ADDRESS`.

## 8. Smoke account, three live payments, and the SDN hit

This account is Phala's own and exists only to prove the production routes. Define the merchant
helpers:

```sh
prove_live_treasuries() {
  local merchant_key=$1 eth_safe=$2 base_safe=$3 prefix=$4 safe_address challenge_message safe_signature CHAIN_ID
  for CHAIN_ID in 1 8453; do
    safe_address="$eth_safe"; [ "$CHAIN_ID" = 8453 ] && safe_address="$base_safe"
    curl -fsS -X POST "$ORIGIN/v1/treasuries/challenge" \
      -H "Authorization: Bearer $merchant_key" -H 'content-type: application/json' \
      -d "{\"chain_id\":$CHAIN_ID,\"address\":\"$safe_address\"}" \
      > "${prefix}-treasury-challenge-$CHAIN_ID.json" || return 1
    challenge_message="$(jq -er '.message' "${prefix}-treasury-challenge-$CHAIN_ID.json")" || return 1
    printf '%s\n' "$challenge_message"
    read -r -p "Safe{Core} threshold signature of the unchanged message on chain $CHAIN_ID: " safe_signature || return 1
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
  local merchant_key=$1 url=$2
  curl -fsS -X POST "$ORIGIN/v1/webhook_endpoints" \
    -H "Authorization: Bearer $merchant_key" -H 'content-type: application/json' \
    -d "$(jq -cn --arg url "$url" '{url:$url,enabled_events:["*"]}')"
}

pin_live_webhook_keys() {
  local merchant_key=$1 account_id=$2 prefix=$3 nonce
  local report_data
  nonce="$(openssl rand -hex 32)" || return 1
  curl -fsS -H "Authorization: Bearer $merchant_key" "$ORIGIN/v1/attestation?nonce=$nonce" \
    > "${prefix}-public-attestation.json" || return 1
  jq '{quote: null, attestation: .tdx_quote}' "${prefix}-public-attestation.json" \
    | "$KIT_DIR/deploy/dstack-verifier.sh" > "${prefix}-public-verification.json" || return 1
  report_data="$(jq -er '.report_data' "${prefix}-public-attestation.json")" || return 1
  jq -e --arg app "$ATTESTED_APP_ID" --arg compose "$COMPOSE_HASH" \
    --arg report_data "$report_data" \
    '.details.tcb_status == "UpToDate" and .details.app_info.app_id == $app
     and .details.app_info.compose_hash == $compose
     and .details.report_data == $report_data + ("0" * 64)' \
    "${prefix}-public-verification.json" || return 1
  NONCE="$nonce" ACCOUNT_ID="$account_id" ATTESTATION_FILE="${prefix}-public-attestation.json" \
    uv run --project "$REPO_DIR/sdk/python" --locked python -c '
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

Create the smoke account with charges disabled and set Finance's `max_unfinalized_credit`:

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name:"Phala production smoke",contact:{name:"<name>",email:"<security email>"},due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},charges_enabled:false,reason:"production route smoke account"}')" > smoke-account.json)
export SMOKE_ACCOUNT_ID="$(jq -er '.id' smoke-account.json)"
export FINANCE_MAX_UNFINALIZED_CREDIT="<Finance-confirmed-integer-cents>"
(umask 077 && admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID" \
  "$(jq -cn --argjson cents "$FINANCE_MAX_UNFINALIZED_CREDIT" '{max_unfinalized_credit:$cents,reason:"Finance-approved pilot value"}')" \
  > smoke-account-limits.json)
```

With §§1.5–1.7, §6 and §7 passed, enable charges. The response holds the first live key; extract it
without printing it:

```sh
(umask 077 && admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID" \
  '{"charges_enabled":true,"reason":"attestation, contract, screening and Finance gates passed"}' \
  > smoke-live.json)
export SMOKE_MERCHANT_SECRET_KEY="$(jq -er '.api_keys[] | select(.livemode == true and .type == "secret") | .secret' smoke-live.json)"
shred -u smoke-account.json smoke-account-limits.json smoke-live.json
```

Prove the smoke Safes, accept exactly the three committed live assets, register the webhook
receiver, and pin its keys from the verified live attestation. The owners' signing steps are in
[integration §1.6](../../docs/integration.md#16-treasuries):

```sh
export ETH_TREASURY_SAFE="<ethereum-smoke-safe>" BASE_TREASURY_SAFE="<base-smoke-safe>"
prove_live_treasuries "$SMOKE_MERCHANT_SECRET_KEY" "$ETH_TREASURY_SAFE" "$BASE_TREASURY_SAFE" smoke &&
  configure_live_payment_settings "$SMOKE_MERCHANT_SECRET_KEY" &&
  register_live_webhook "$SMOKE_MERCHANT_SECRET_KEY" "<https smoke webhook receiver>" &&
  pin_live_webhook_keys "$SMOKE_MERCHANT_SECRET_KEY" "$SMOKE_ACCOUNT_ID" smoke || {
  echo "smoke live configuration failed; stop" >&2
  false
}
curl -fsS -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" "$ORIGIN/v1/config" | jq
```

Make three $5 payments, the committed `min_amount`, from a funded Foundry keystore. Ethereum USDC
and Base USDC submit a transaction-hash hint. Ethereum USDT sends none, which proves scanner
discovery. Each payment waits at most one hour for a credited deposit. Credit comes at the route's
confirmation within `max_unfinalized_credit`, otherwise at finality. On any failure, stop.
`PAYER_RPC_ETHEREUM` and `PAYER_RPC_BASE` are the operator's RPCs, not the service's keys:

```sh
# Copy the keystore password from the secret store into this private work directory first.
export PAYER_ACCOUNT="<foundry-keystore-account>" PAYER_PASSWORD_FILE="$LAUNCH_WORK_DIR/payer.password"
export PAYER_RPC_ETHEREUM="<operator ethereum rpc>" PAYER_RPC_BASE="<operator base rpc>"
wait_for_credit() {
  local quote_id=$1 deadline=$((SECONDS + 3600)) response
  while (( SECONDS < deadline )); do
    if response="$(curl -fsS "$ORIGIN/v1/quotes/$quote_id?expand[]=deposit" \
      -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY")" \
      && jq -e '.status == "complete" and .deposit.status == "credited"' <<<"$response" >/dev/null; then
      CREDITED_DEPOSIT_ID="$(jq -er '.deposit.id' <<<"$response")"
      return 0
    fi
    sleep 15
  done
  echo "quote $quote_id was not credited within 3600 seconds" >&2
  return 1
}

# smoke_payment LABEL CHAIN_ID ASSET TOKEN RPC_URL HINT: one $5 quote paid from the keystore; with
# HINT=hint its transaction hash is submitted to the quote. Sets CREDITED_DEPOSIT_ID.
smoke_payment() {
  local label=$1 chain_id=$2 asset=$3 token=$4 rpc_url=$5 hint=$6 quote quote_id address amount tx_hash
  quote="$(curl -fsS -X POST "$ORIGIN/v1/quotes" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" \
    -H 'content-type: application/json' -H "Idempotency-Key: production-smoke-$label-1" \
    -d "$(jq -cn --arg label "$label" --argjson chain_id "$chain_id" --arg asset "$asset" \
      '{client_reference_id:("production-smoke-" + $label),amount:500,currency:"usd",chain_id:$chain_id,asset:$asset}')")" || return 1
  quote_id="$(jq -er '.id' <<<"$quote")" && address="$(jq -er '.address' <<<"$quote")" &&
    amount="$(jq -er '.amount_atomic' <<<"$quote")" || return 1
  tx_hash="$(cast send --json --account "$PAYER_ACCOUNT" --password-file "$PAYER_PASSWORD_FILE" \
    --rpc-url "$rpc_url" "$token" 'transfer(address,uint256)' "$address" "$amount" | jq -er '.transactionHash')" || return 1
  if [[ "$hint" == hint ]]; then
    test "$(curl -fsS -o /dev/null -w '%{http_code}' -X POST "$ORIGIN/v1/quotes/$quote_id/transactions" \
      -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" -H 'content-type: application/json' \
      -d "$(jq -cn --arg hash "$tx_hash" '{transaction_hash:$hash}')")" = 202 || return 1
  fi
  wait_for_credit "$quote_id"
}

run_smoke_payments() {
  smoke_payment ethereum-usdc 1 usdc 0xA0b86991c6218b36c1d19d4a2e9eb0ce3606eb48 "$PAYER_RPC_ETHEREUM" hint || return 1
  export SMOKE_ETH_USDC_DEPOSIT_ID="$CREDITED_DEPOSIT_ID"
  smoke_payment ethereum-usdt 1 usdt 0xdAC17F958D2ee523a2206206994597C13D831ec7 "$PAYER_RPC_ETHEREUM" none || return 1
  export SMOKE_ETH_USDT_DEPOSIT_ID="$CREDITED_DEPOSIT_ID"
  smoke_payment base-usdc 8453 usdc 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913 "$PAYER_RPC_BASE" hint || return 1
  export SMOKE_BASE_USDC_DEPOSIT_ID="$CREDITED_DEPOSIT_ID"
}
run_smoke_payments || { echo "smoke payment sequence failed; stop" >&2; false; }
```

Prove screening refuses both a known SDN address and a supplement address, without moving funds. A
refund destination is screened before anything is created, so each request must fail with
`destination_sanctioned` and leave no refund:

```sh
export SDN_EVM_ADDRESS="<the §7 Compliance-recorded SDN address>"
sanctions_hit_smoke() {
  local address response
  for address in "$SDN_EVM_ADDRESS" 0xAE05eB856CB28c156d9722094F5eEfD5693Bc181; do
    response="$(curl -sS -X POST "$ORIGIN/v1/refunds" -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" \
      -H 'content-type: application/json' \
      -d "$(jq -cn --arg deposit "$SMOKE_ETH_USDC_DEPOSIT_ID" --arg address "$address" '{deposit:$deposit,destination_address:$address}')")" || return 1
    jq -e '.error.code == "destination_sanctioned"' <<<"$response" >/dev/null ||
      { echo "$address was not refused as sanctioned; stop" >&2; return 1; }
  done
  curl -fsS "$ORIGIN/v1/refunds?deposit=$SMOKE_ETH_USDC_DEPOSIT_ID" \
    -H "Authorization: Bearer $SMOKE_MERCHANT_SECRET_KEY" | jq -e '.data == []' >/dev/null
}
sanctions_hit_smoke || { echo "sanctions hit smoke failed; stop" >&2; false; }
```

Record the three deposits' state before the restore drill. The drill must return the same state:

```sh
SMOKE_DEPOSIT_IDS=("$SMOKE_ETH_USDC_DEPOSIT_ID" "$SMOKE_ETH_USDT_DEPOSIT_ID" "$SMOKE_BASE_USDC_DEPOSIT_ID")
for INDEX in "${!SMOKE_DEPOSIT_IDS[@]}"; do
  admin GET "/v1/admin/deposits/${SMOKE_DEPOSIT_IDS[$INDEX]}" > "smoke-deposit-$INDEX-before.json"
  jq -e --arg id "${SMOKE_DEPOSIT_IDS[$INDEX]}" '.id == $id' "smoke-deposit-$INDEX-before.json" >/dev/null
  jq '{id,status,amount,amount_atomic,amount_refunded,amount_refunded_atomic,amount_reversed,replaces,replaced_by,revision}' \
    "smoke-deposit-$INDEX-before.json" > "smoke-deposit-$INDEX-state.json"
done
```

**Checks:**

- Both Safes are active treasuries, and `GET /v1/config` lists Ethereum USDC/USDT and Base USDC.
- The pinned webhook keys come from a verified live attestation with the §6 app id and compose hash.
- All three quotes are `complete`, with deposits `credited`. Each credit was delivered to the
  receiver as a signed `deposit.credited` that verifies against the pinned keys.
- Both hint submissions answered `202`, and the USDT deposit was found without a hint.
- Both sanctions requests returned `400 destination_sanctioned`, and the deposit has no refund.
- The smoke used 3 of D and 2 Safe proof sets, within the pilot limits.

## 9. Production restore drill

Drill the production backup before Phala Cloud's charges. Follow
[RESTORE.md, Staging restore drill](../RESTORE.md#staging-restore-drill) and steps 1–5 of
[Restore](../RESTORE.md#restore), with production values. Use the §1.4 **Object Read only** token
only. `live_isolated` is a hard abort: run it before the instance exists, immediately after
creation, before each step, and at least every five minutes. On failure, delete the drill instance
by its `vm_uuid` and record the drill as aborted. The `RETURN` trap always deletes the throwaway
instance and the temporary env file. The restore-check report runs a reconciliation round twice;
count those RPC calls against the 150-call reserve:

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
      "$KIT_DIR/deploy/phala" cvms delete "$RESTORE_CVM_ID" --force ||
        echo "failed to delete restore instance $RESTORE_CVM_ID; delete it by vm_uuid before proceeding" >&2
      unset RESTORE_CVM_ID
    fi
    if [[ -n "${RESTORE_ENV_DIR:-}" && -e "$RESTORE_ENV_DIR/restore.env" ]]; then
      shred -u "$RESTORE_ENV_DIR/restore.env"
      rmdir "$RESTORE_ENV_DIR" || true
      unset RESTORE_ENV_DIR
    fi
  }
  trap restore_cleanup RETURN

  live_isolated || { restore_cleanup; return 1; }
  RESTORE_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" || return 1
  export RESTORE_STARTED_AT
  RESTORE_ENV_DIR="$(mktemp -d "$LAUNCH_WORK_DIR/restore-env.XXXXXX")" || return 1
  export RESTORE_ENV_DIR
  umask 077
  : > "$RESTORE_ENV_DIR/restore.env"
  ${EDITOR:-vi} "$RESTORE_ENV_DIR/restore.env" || return 1
  # Exactly RESTORE_AWS_ACCESS_KEY_ID, RESTORE_AWS_SECRET_ACCESS_KEY (read-only token), SENTRY_DSN,
  # TOPUP_RPC_ANKR_KEY, TOPUP_RPC_BLOCKPI_ETHEREUM_KEY and TOPUP_RPC_BLOCKPI_BASE_KEY.
  "$KIT_DIR/deploy/render.sh" --restore-check --images release/images.json --origin https://pending.invalid \
    "$ENV_DIR" > restore-check.yml || return 1
  "$KIT_DIR/deploy/preflight.sh" --env "$RESTORE_ENV_DIR/restore.env" --compose restore-check.yml \
    --environment-dir "$ENV_DIR" --restore-check --workspace "$PHALA_WORKSPACE" \
    --os-image dstack-0.5.12 --require-sentry || return 1

  # Restore step 1: create a new instance of the original app with its own read-only env.
  live_isolated || { restore_cleanup; return 1; }
  "$KIT_DIR/deploy/phala" instances add --app-id "$APP_ID" --compose-file restore-check.yml \
    --pre-launch-script "$KIT_DIR/deploy/phala-cloud-pre-launch.sh" --env-file "$RESTORE_ENV_DIR/restore.env" \
    --name phala-pay-production-restore --json > restore-instance.json || return 1
  RESTORE_CVM_ID="$(jq -er '.vm_uuid' restore-instance.json)" || return 1
  export RESTORE_CVM_ID

  # Restore step 2: fetch by vm_uuid, address the guest by its attested instance id, and verify.
  live_isolated || { restore_cleanup; return 1; }
  export PHALA_ATTESTATION_URL="https://cloud-api.phala.com/api/v1/cvms/$RESTORE_CVM_ID/attestation"
  curl -fsS -H "X-API-Key: $PHALA_CLOUD_API_KEY" "$PHALA_ATTESTATION_URL" \
    > restore-attestation.json || return 1
  "$KIT_DIR/deploy/phala" cvms get "$RESTORE_CVM_ID" --json > restore-cvm.json || return 1
  RESTORE_INSTANCE_ID="$("$KIT_DIR/deploy/phala-cvm.sh" instance-id restore-attestation.json)" || return 1
  export RESTORE_INSTANCE_ID
  RESTORE_GATEWAY_DOMAIN="$(jq -er '.gateway.base_domain' restore-cvm.json)" || return 1
  export RESTORE_GATEWAY_DOMAIN
  curl -fsS "https://${RESTORE_INSTANCE_ID}-8090.$RESTORE_GATEWAY_DOMAIN/prpc/Info" \
    > restore-info.json || return 1
  EXPECTED_OS_IMAGE_HASH="$(cat "$ENV_DIR/os-image-hash")" || return 1
  "$KIT_DIR/deploy/verify-attestation.sh" restore-attestation.json restore-info.json "$APP_ID" \
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
  RESTORE_REPORTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" || return 1
  export RESTORE_REPORTED_AT

  # Restore step 4: give the drill its own origin and rerun the restore-check variant.
  live_isolated || { restore_cleanup; return 1; }
  "$KIT_DIR/deploy/render.sh" --restore-check --images release/images.json --origin "$RESTORE_URL" \
    "$ENV_DIR" > restore-check.yml || return 1
  live_isolated || { restore_cleanup; return 1; }
  "$KIT_DIR/deploy/phala" deploy --json --cvm-id "$RESTORE_CVM_ID" --compose restore-check.yml \
    --pre-launch-script "$KIT_DIR/deploy/phala-cloud-pre-launch.sh" --no-public-logs --no-public-sysinfo --wait || return 1

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
  RESTORE_INSTANCE_ID="$("$KIT_DIR/deploy/phala-cvm.sh" instance-id restore-attestation-final.json)" || return 1
  export RESTORE_INSTANCE_ID
  curl -fsS "https://${RESTORE_INSTANCE_ID}-8090.$RESTORE_GATEWAY_DOMAIN/prpc/Info" \
    > restore-info-final.json || return 1
  "$KIT_DIR/deploy/verify-attestation.sh" restore-attestation-final.json restore-info-final.json "$APP_ID" \
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
    | "$KIT_DIR/deploy/dstack-verifier.sh" > restore-public-verification.json || return 1
  jq -e --arg app "$ATTESTED_APP_ID" --arg compose "$RESTORE_COMPOSE_HASH" \
    --arg report_data "$RESTORE_PUBLIC_REPORT_DATA" \
    '.details.tcb_status == "UpToDate" and .details.app_info.app_id == $app
     and .details.app_info.compose_hash == $compose
     and .details.report_data == $report_data + ("0" * 64)' \
    restore-public-verification.json || return 1
  NONCE="$NONCE" ACCOUNT_ID="$SMOKE_ACCOUNT_ID" ATTESTATION_FILE=restore-public-attestation.json \
    uv run --project "$REPO_DIR/sdk/python" --locked python -c '
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
  jq '{started_at: env.RESTORE_STARTED_AT, reported_at: env.RESTORE_REPORTED_AT, restore_check: .restore_check | {status, restored_heartbeat_at, latest_migration, row_counts}}' \
    restore-healthz-final.json > restore-drill-record.json || return 1
  return 0
}
restore_drill
```

After the function returns, confirm the drill instance is gone using the `vm_uuid` saved in
`restore-instance.json`: its API read must return **404**, not an authentication or network
failure. Also confirm production is still running, then revoke the read-only token in the R2
dashboard. Issue a new one for the next drill:

```sh
export DELETED_DRILL_CVM_ID="$(jq -er '.vm_uuid' restore-instance.json)"
export DRILL_CVM_URL="https://cloud-api.phala.com/api/v1/cvms/$DELETED_DRILL_CVM_ID"
test "$(curl -sS -o restore-deletion.json -w '%{http_code}' \
  -H "X-API-Key: $PHALA_CLOUD_API_KEY" \
  "$DRILL_CVM_URL")" = 404
"$KIT_DIR/deploy/phala" cvms get "$TOPUP_CVM_ID" --json | jq -e '.status == "running"'
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
```

If Phala Cloud retains a deleted CVM record, the 404 check fails safely. Confirm the saved
`vm_uuid` has a deleted status in Phala Cloud and record that evidence instead.

**Checks:**

- `restore_drill` returned 0 and live isolation held throughout.
- The final attestation matches the original app id and the compose hash of the `--origin
  "$RESTORE_URL"` render, with the pinned OS image.
- The report is `ok`, with reconciliation `complete`, and the service is frozen.
- All three smoke deposits keep their recorded state.
- `restore-drill-record.json` records the RPO (`restored_heartbeat_at` against
  `started_at`), the RTO (`reported_at` minus `started_at`), `latest_migration`, and row counts.
- The drill instance is deleted, the read-only token is revoked, and no read-write credential was
  ever sealed into the drill.

## 10. Onboard Phala Cloud and enable its charges last

Only after §§6–9 pass may Phala Cloud be onboarded. Open its account with charges disabled and set
Finance's `max_unfinalized_credit` and its live `max_open_amount_per_customer`, which must be at
least the largest approved quote (§1.6):

```sh
(umask 077 && admin POST /v1/admin/accounts "$(jq -cn '{name:"Phala Cloud",contact:{name:"<name>",email:"<security email>"},due_diligence:{reference:"<review reference>",reviewed_at:"<YYYY-MM-DD>",reviewed_by:"<reviewer>"},charges_enabled:false,reason:"production onboarding after smoke and restore"}')" > phala-cloud-account.json)
export PHALA_CLOUD_ACCOUNT_ID="$(jq -er '.id' phala-cloud-account.json)"
(umask 077 && admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID" \
  "$(jq -cn --argjson cents "$FINANCE_MAX_UNFINALIZED_CREDIT" --argjson customer 1000000 \
    '{max_unfinalized_credit:$cents,limits:{livemode:true,max_open_amount_per_customer:$customer},reason:"Finance-approved pilot value"}')" \
  > phala-cloud-account-limits.json)
jq -e '.limits.live.max_open_amount_per_customer == 1000000' phala-cloud-account-limits.json
```

With every gate passed, enable charges. The gates are the screening gates (§§6–8), Finance
(§1.6), the smoke payments (§8), the restore report (§9), and monitoring (§1.9, §6). Extract the
first live key without printing it:

```sh
(umask 077 && admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID" \
  '{"charges_enabled":true,"reason":"screening, Finance, smoke, restore and monitoring gates passed"}' \
  > phala-cloud-live.json)
export PHALA_CLOUD_MERCHANT_SECRET_KEY="$(jq -er '.api_keys[] | select(.livemode == true and .type == "secret") | .secret' phala-cloud-live.json)"
```

Prove Phala Cloud's Safes, accept exactly Ethereum USDC/USDT and Base USDC, register its live webhook
endpoint, and pin its keys from the verified attestation:

```sh
export PHALA_ETH_TREASURY_SAFE="<ethereum-phala-cloud-safe>" PHALA_BASE_TREASURY_SAFE="<base-phala-cloud-safe>"
prove_live_treasuries "$PHALA_CLOUD_MERCHANT_SECRET_KEY" "$PHALA_ETH_TREASURY_SAFE" "$PHALA_BASE_TREASURY_SAFE" phala-cloud &&
  configure_live_payment_settings "$PHALA_CLOUD_MERCHANT_SECRET_KEY" &&
  register_live_webhook "$PHALA_CLOUD_MERCHANT_SECRET_KEY" "<Phala Cloud's https webhook receiver>" &&
  pin_live_webhook_keys "$PHALA_CLOUD_MERCHANT_SECRET_KEY" "$PHALA_CLOUD_ACCOUNT_ID" phala-cloud || {
  echo "Phala Cloud live configuration failed; stop" >&2
  false
}
```

Hand the account id and live key to Phala Cloud's recorded contact through an encrypted channel.
Phala Cloud rolls the key at once and runs its servers with a restricted live key
([Operator onboarding, step 3](../README.md#operator-onboarding)). Pause the smoke account's
quotes, so it adds no load until the next smoke. Then shred the response files:

```sh
admin POST "/v1/admin/accounts/$SMOKE_ACCOUNT_ID/pause" '{"scopes":["quotes"],"reason":"smoke complete; idle until the next production smoke"}'
shred -u phala-cloud-account.json phala-cloud-account-limits.json phala-cloud-live.json
unset SMOKE_MERCHANT_SECRET_KEY PHALA_CLOUD_MERCHANT_SECRET_KEY PHALA_CLOUD_API_KEY
```

**Checks:** Phala Cloud's account has `charges_enabled: true`, Finance's `max_unfinalized_credit`,
and a live `max_open_amount_per_customer` of at least the largest approved quote. Both Safes are active treasuries, the three assets are accepted, and its
webhook keys are pinned. Phala Cloud confirms receipt, the key roll, and its first live payment
credited and delivered.

## 11. Pilot caps, monitoring, and stop actions

Sentry receives the service's alerts and Crons. The RPC, S, L, H, Q and quota rules in
[rpc-alerts.yaml](../rpc-alerts.yaml) are Prometheus rules over the admin-signed metrics. A
production CVM has no collector, and the admin key never leaves the operator, so the operator
evaluates them with `pilot_watch`. Run it in the first 72 hours at least every two staffed hours
and at each UTC day boundary, then at least twice a day. Also run it at once on any Sentry alert.
The owner accepts this manual cadence for the pilot. For each later watch session, use the same
private-directory/`umask 077` setup, derive a temporary `admin.pem` from the owner's stored seed,
and shred the local key and seed copy after the check (§13). A metrics collector with a metrics-only
key is the planned replacement and requires a reviewed implementation. Read the shared Ankr, staging Infura and production BlockPI
dashboards daily:

```sh
pilot_watch() {
  local stamp metrics daily status=0
  stamp="$(date -u +%Y%m%dT%H%M%SZ)" || return 1
  metrics="pilot-metrics-$stamp.txt" daily="pilot-daily-$stamp.json"
  admin GET /v1/admin/metrics > "$metrics" || return 1
  admin GET /v1/admin/reports/daily > "$daily" || return 1
  above() { metric_lines "$1" "$metrics" | awk -v max="$2" '{ s += $NF } END { exit !(s > max) }'; }
  any_at_least() { metric_lines "$1" "$metrics" | grep -F "$2" | awk -v min="$3" '$NF + 0 >= min { hit = 1 } END { exit !hit }'; }
  test "$(curl -sS -o /dev/null -w '%{http_code}' "$ORIGIN/healthz")" = 200 || { echo "STOP: /healthz is not 200"; status=1; }
  metric_lines topup_rpc_endpoint_ready "$metrics" | awk '$NF + 0 == 1 { ok++ } END { exit !(NR == 4 && ok == 4) }' ||
    { echo "RPC endpoint not ready: rpc-health.md"; status=1; }
  above topup_addresses_lagging 0 && { echo "RpcAddressBackfillLag: recheck within one hour; rpc-health.md if still lagging"; status=1; }
  above topup_finality_unresolved 1 && { echo "STOP S stock: pause the chain's quotes; deposit-reversed.md"; status=1; }
  above topup_finality_unresolved_entries_24h 1 && { echo "STOP S entries: pause the chain's quotes; deposit-reversed.md"; status=1; }
  above topup_confirmation_slow 1 && { echo "STOP L stock: pause the chain's quotes; rpc-health.md"; status=1; }
  above topup_confirmation_slow_entries_24h 1 && { echo "STOP L entries: pause the chain's quotes; rpc-health.md"; status=1; }
  above topup_confirmation_slow 0 && echo "L non-empty: recheck in ten minutes; provider lag if it stays (rpc-health.md)"
  any_at_least topup_coverage_lag_seconds '' 7200 && { echo "coverage lag over two hours: rpc-health.md"; status=1; }
  any_at_least topup_daily_budget_used 'name="hints"' 80 && echo "H exhausted until UTC midnight: hints fall back to scanning"
  any_at_least topup_daily_budget_used 'name="price:' 60 && echo "Q exhausted until UTC midnight: quotes return price_unavailable"
  any_at_least topup_issued_addresses '' 700 && { echo "address count at 70%: address-capacity.md"; status=1; }
  any_at_least topup_sanctions_list_verified_age_seconds '' 21600 && { echo "sanctions list stale: sanctions-list.md"; status=1; }
  jq '{generated_at, deposits_total: ([.routes[].deposits_by_state | to_entries[].value] | add), refunds_by_status: [.routes[].refunds_by_status], reconciliation_blocks, failing_webhook_endpoints, sanctions_snapshot: .sanctions_snapshot.verified_at}' "$daily"
  return "$status"
}
pilot_watch
```

**D tally:** D for a UTC day is that day's last `deposits_total` minus the previous day's last one,
plus recovery work. Record each day's figures for D, factory verifications and Safe proof sets,
with the shared Ankr seven-day run rate, staging Infura daily credits and production BlockPI
seven-day RU run rate. BlockPI payload surcharges are visible in its dashboard. Stop new load at each limit:

| Signal | Stop action | Runbook |
|---|---|---|
| S or L stock above 1, or S or L entries in rolling 24 h above 1 | Pause `quotes` on every route of the affected chain (below) and escalate. Keep verifying funded work and keep its reservations. Resume only when stock, entries and the day's work all fit | [Deposit reversed](deposit-reversed.md), [RPC health](rpc-health.md) |
| D reaches 46 in a UTC day, factory 60/day, or Safe 10/day | Pause `quotes` on all three routes until 00:00 UTC (5:00 PM PDT) and coordinate Phala Cloud's intake. Start no proof batch | [RPC operations](../RPC.md#monitoring-and-stop-actions) |
| Shared Ankr seven-day run rate ≥ 25,000 calls/day, staging Infura ≥ 1,500,000 credits/day, or production BlockPI seven-day run rate ≥ 1,250,000 RU/day | Add no merchant, route or test load. Investigate retry or range growth. Never add a second free account | [RPC health](rpc-health.md) |
| Staging Infura `402`, or production BlockPI quota/throughput exhaustion | Infura verify halts until UTC midnight; BlockPI verify waits for provider recovery. Preserve pending work and do not switch endpoints | [RPC health](rpc-health.md) |
| `TopupAddressCapacity` or 700 addresses on a chain (70%); 900 is critical | Plan the paid-provider or indexer upgrade, and stop growth before 1,000 | [Address capacity](address-capacity.md) |
| `422 refund_attachment_limit_exceeded` (R=2 attached pending, N=1 per 24 h) | Enforced by the service. Phala Cloud contacts the operator before another payout. Never cancel an attached refund to make room | [RPC operations](../RPC.md#pilot-limits-and-operating-modes) |
| `TopupRefundProgressAge`: attached refund pending over 1 h, or unseen by both endpoints for 24 h | The refund stays pending and reserved. Checks back off from 60 s to ten minutes after 30 min, then hourly after 24 h | [Pending refund transaction](rejected-funds-at-treasury.md#pending-refund-transaction-alert) |
| `TopupSanctionsListStale`, `TopupSanctionsListVerifyFailed`, `TopupSanctionsRescreenFailed` | Restore verification before the 24 h freshness limit, or pause `settlement` on the routes | [Sanctions list](sanctions-list.md) |
| H 80 or Q 60 exhausted | Automatic until UTC midnight. Review the demand, but never raise caps without a reviewed budget | [RPC health](rpc-health.md) |
| Any other Sentry alert or missed Crons check-in | Follow its runbook | [Alert index](README.md#alert-and-symptom-index) |

```sh
pause_chain_quotes() {
  local route
  case "$1" in
    1) set -- phala-cloud-ethereum-usdc-usd phala-cloud-ethereum-usdt-usd ;;
    8453) set -- phala-cloud-base-usdc-usd ;;
    *) echo "unknown production chain $1" >&2; return 1 ;;
  esac
  for route in "$@"; do
    admin POST "/v1/admin/routes/$route/pause" '{"scopes":["quotes"]}' || return 1
  done
}
```

Resume with the same body on `/v1/admin/routes/<route>/resume`. A pause does not stop transfers to
persistent addresses, so coordinate Phala Cloud's intake as well. External `/healthz` availability
has no Sentry Uptime monitor. A down CVM shows as missed Crons check-ins, and `pilot_watch` checks
the public origin.

**Checks:** each run's output and its saved files are filed with the daily tallies. Every stop
action taken is recorded with its alert, time (PT), and the resume decision.

## 12. Rollback

The rollback target is the **previously deployed production release recorded at launch** (§1.1),
with its own verified kit, image digests, configuration commit and deployment record. Never infer
it from a version number or from the current caller pin. Never delete the CVM or its app: the
backups are encrypted with the app's derived key.

- **Before Phala Cloud's charges (§§2–9):** leave every account without live charges or paused, fix
  the cause, and repeat the failed section. A configuration fix is a reviewed PR to `$ENV_DIR`
  followed by `dispatch_deploy upgrade`. To abandon the launch, stop the CVM
  (`"$KIT_DIR/deploy/phala" cvms stop "$TOPUP_CVM_ID"`), and keep the app, the bucket, and the DNS records
  until the owner decides.
- **After charges:** pause first. Pause `quotes` on the affected routes, or `quotes` and
  `settlement` on Phala Cloud's account
  (`admin POST "/v1/admin/accounts/$PHALA_CLOUD_ACCOUNT_ID/pause" '{"scopes":["quotes","settlement"],"reason":"<incident>"}'`).
  Follow [incident communication](incident-communication.md). Revert a bad setting with a PR and
  `dispatch_deploy upgrade` at the approved release. Never run down migrations, edit the database,
  or release reservations.
- **Release defect with a recorded prior deployment:** check the new release's changelog and
  [Rollback compatibility](../README.md#rollback-compatibility). Rehearse N → N-1 → N using that
  recorded target's own CLI and configuration. A reviewed PR restores `deploy-phala.yml`'s tag
  and commit and the retained configuration; this changes the shared staging/production pin.
  Approve the new main commit (§1.1), then `dispatch_deploy upgrade`. If the changelog requires
  restore, follow that procedure instead.
- **First production deployment:** there is no prior production rollback target. Keep charges
  paused and fix forward with an approved, staging-soaked release; never choose an older release
  that has not served production as a substitute.
- **Lost or corrupted database:** follow [RESTORE.md](../RESTORE.md) and the
  [restore runbook](restore.md). This launch's §9 drill is the rehearsal.

Retain this launch's verified kit, `release/images.json`, release tag/commit, `$ENV_DIR` commit
and both deployment records as the recovery point for the next upgrade.

**Checks:** after any rollback or fix, re-run §6 and `pilot_watch`. Lift pauses only when both pass.

## 13. Sign-off checklist

Record the evidence, the owner, and the time (PT) for each item. Launch is complete when every item
is checked.

- [ ] §1.1 `RELEASE_TAG` and `RELEASE_COMMIT` read from the caller at `APPROVED_COMMIT`;
      kit, images and pins verified; matching staging soak approved; prior production rollback
      target (or first deployment) recorded; OS hash recorded; launch work directory is outside
      the repository with `umask 077`.
- [ ] §1.2 Production Phala Cloud workspace and API key exist.
- [ ] §1.3 `production` Environment is `main`-only, with exactly three secrets and the variables;
      the admin and maintenance keys match `topup.yaml`.
- [ ] §1.4 `phala-pay-production` exists; the prefix was empty; read-write and Object Read only
      tokens are scoped to that bucket only.
- [ ] §1.5 Factories deployed, or existing deployments verified, on Ethereum and Base; reports
      kept; evidence PR updates CONTRACTS.md and removes `1`/`8453` from
      `PENDING_DEPLOYMENT_CHAINS`, merged before §2; final main commit approved.
- [ ] §1.6 Finance, Risk, and Operations signed off route amounts, `max_unfinalized_credit`,
      limits, and pilot caps for the deployed commit.
- [ ] §1.7 Four Safes verified against Finance's expectations.
- [ ] §1.8 Production Ankr read/BlockPI verify allocation PR merged; configuration, complete
      sealed mappings and preflight passed before §2. PR `<link>` and Operations owner `<name>`
      recorded; staging stays Ankr + Infura; resulting budget and unchanged D=46/20 caps accepted;
      production-attested comments updated before provisioning; both BlockPI chain keys ready.
- [ ] §1.9 Sentry scrubbing and alert rule set; synthetic alert received; owner accepts the
      pilot's external-monitoring risk and no Sentry Uptime monitor.
- [ ] §1.10 `pay-api.phala.com` records are DNS only with CAA compatible; owner's
      `pay.phala.com` JavaScript Detections decision and accepted CSP violations recorded.
- [ ] §1.11 Staging live SDN smoke passed; evidence and owner/Compliance sign-off recorded.
- [ ] §2 Provision run `<id>` has `headSha == APPROVED_COMMIT`; `TOPUP_CVM_ID` set; missing
      CVM/attestation documents fetched before DNS setup.
- [ ] §3 CNAME and TXT resolve publicly.
- [ ] §4 Online preflight passed with `rpc check` on all four endpoints; secrets sealed.
- [ ] §5 Acceptance upgrade run `<id>` passed with `headSha == APPROVED_COMMIT`; WAL
      archiving confirmed; unavailability recorded.
- [ ] §6 Attestation, OS hash `8db40cb2…`, ingress evidence, `/healthz`, endpoint readiness,
      OFAC snapshot, and Crons verified.
- [ ] §7 Four supplements added and audited; re-screen clean; owner/Compliance signed off the
      known SDN address and its use in a refused refund request; address and SDN uid recorded.
- [ ] §8 Three live payments credited and delivered (two hinted, one scanned); SDN and supplement
      hits refused.
- [ ] §9 Restore drill passed; RPO and RTO recorded; deletion confirmed by 404 or retained deleted status for
      `restore-instance.json`'s `vm_uuid`; production still running; read-only token revoked.
- [ ] §10 Phala Cloud onboarded, live, and confirmed; smoke account quotes paused.
- [ ] §11 `pilot_watch` cadence, daily tallies, and on-call owner agreed.
- [ ] §12 Approved release tag/commit, kit, images, configuration commit, and deployment records retained as the future
      N-1.
- [ ] Owner's final go-live approval.
- [ ] Private work copies shredded: `admin.pem`, `admin.seed`, `maintenance/production.pem`,
      `.env.production`, restore env, `payer.password`, admin request body, and smoke/Phala Cloud
      first-key response JSON files; transient secret environment variables unset.

After sign-off, keep the verified kit, images, configuration commit and non-secret evidence in
the owner's recovery store. Shred the local private copies below, including on an abandoned
launch once recovery work is complete; keep the original recovery keys in the secret store.
Restore env files are normally shredded by §9's cleanup; include any interrupted-drill copies
below. Configure the editor to avoid swap/backup files, and shred any private ones it created.
Repeat key/seed cleanup after each `pilot_watch`.

```sh
cleanup_launch_private_material() {
  local private_file
  for private_file in admin.pem admin.seed maintenance/production.pem .env.production payer.password \
    admin-body.json smoke-account.json smoke-account-limits.json smoke-live.json \
    phala-cloud-account.json phala-cloud-account-limits.json phala-cloud-live.json \
    restore-env.*/restore.env; do
    if [[ -f "$LAUNCH_WORK_DIR/$private_file" ]]; then
      shred -u -- "$LAUNCH_WORK_DIR/$private_file" || return 1
    fi
  done
  unset PHALA_CLOUD_API_KEY AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY SENTRY_DSN PRIVATE_KEY
  unset SMOKE_MERCHANT_SECRET_KEY PHALA_CLOUD_MERCHANT_SECRET_KEY
}
cleanup_launch_private_material
```
