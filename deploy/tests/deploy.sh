#!/usr/bin/env bash
# deploy/deploy.sh, as the Release workflow publishes it, run --non-interactive against a release
# built from this commit and stub curl, gh, npm, docker, uvx, and Phala Cloud CLI:
# - the quick start, verified with the GitHub CLI, provisions the template with the sealed env,
#   which holds exactly the names given a value (no SENTRY_DSN), written under $XDG_RUNTIME_DIR, and
#   prints its gateway URL without waiting for an instance id or the attestation;
# - the custom domain, verified by SHA256SUMS, writes the environment directory with a generated
#   admin key (the release's own Python SDK), provisions the service variant, upgrades it to its
#   node's gateway without an env, and prints the DNS records, the TXT record's instance id from the
#   attestation's event log, once the attested app-compose carries the rendered compose and the
#   API's compose hash is its hash;
# - a custom domain whose CVM reports another compose hash than its own app-compose's, or attests
#   another update's compose, fails;
# - as Phala Cloud does, `cvms get` reports instance_id null;
# - a phala.toml in the caller's directory never reaches the CLI, which runs in an empty directory;
# - a run that created a CVM and then failed records it and prints its id, URL, and how to finish
#   it, and the rerun creates no other; so does a run whose CVM never attests its compose;
# - Ctrl-C mid-deploy prints the CVM and how to finish it, and shreds the sealed env file;
# - a name the workspace already has, or a CVM list it cannot read, refuses before any admin seed is
#   written; a release that does not match its SHA256SUMS, a refused
#   attestation, --strict without the GitHub CLI, an invalid instance name, or a secret the CLI
#   would read differently deploys nothing.
# Every run leaves no temporary directory and prints no secret. TOPUP names a local topup binary
# for preflight and the route-mode check (CI's build).
# The quick start's printed attestation command also keeps the OS pin after its kit is removed.
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
: "${TOPUP:?set TOPUP to a topup binary, for example target/debug/topup}"
export TOPUP
# Negative convergence cases use real short deadlines; happy paths settle in one call.
export ATTESTATION_WAIT_SECONDS=2 CVM_WAIT_SECONDS=5
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
version=v9.9.9 commit=$(git -C "$root" rev-parse HEAD)
# The Python SDK of this commit's version, as Python spells it.
sdk_version=$("$root/scripts/version.sh")
sdk=phala-pay==${sdk_version/-rc./rc}
fail() {
    echo "deploy: $*" >&2
    exit 1
}

# The release: its assets as release.yml writes them.
assets=$tmp/assets
"$root/deploy/build-kit.sh" "$version" "$assets" >/dev/null
jq -n '{"phala-pay": "ghcr.io/phala-network/phala-pay@sha256:\("1" * 64)",
    "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:\("2" * 64)",
    "phala-pay-reference-product": "ghcr.io/phala-network/phala-pay-reference-product@sha256:\("3" * 64)"}' \
    >"$assets/images.json"
"$root/deploy/render.sh" --template --images "$assets/images.json" \
    "$root/deploy/environments/phala-cloud-template/topup" >"$assets/phala-cloud-template.yml"
sed "s/^release=latest\$/release=$version/" "$root/deploy/deploy.sh" >"$assets/deploy.sh"
grep -qx "release=$version" "$assets/deploy.sh" || fail "the release's deploy.sh does not name it"
(cd "$assets" && sha256sum images.json "phala-pay-deploy-$version.tar.gz" phala-cloud-template.yml deploy.sh \
    >SHA256SUMS)
# The pinned Compose, fetched once before curl is stubbed.
PINNED_COMPOSE=$("$root/deploy/pinned-compose.sh")
export PINNED_COMPOSE

bin=$tmp/bin
mkdir -p "$bin"
cat >"$bin/curl" <<'STUB'
#!/usr/bin/env bash
# Serves the release's assets from STUB_ASSETS; nothing else.
output=/dev/stdout url=${*: -1}
while (($#)); do
    [[ "$1" != -o ]] || output=$2
    shift
done
[[ "$url" == https://github.com/Phala-Network/phala-pay/releases/download/v9.9.9/* ]] || exit 22
cp "$STUB_ASSETS/${url##*/}" "$output"
STUB
cat >"$bin/gh" <<'STUB'
#!/usr/bin/env bash
echo "gh $*" >>"$STUB_LOG"
case "$*" in
    --version) echo "gh version $STUB_GH_VERSION (2026-09-15)" ;;
    "auth status") ;;
    *contents/deploy/deadline.sh\?ref=v9.9.9) cat "$STUB_ROOT/deploy/deadline.sh" ;;
    *contents/deploy/verify-release.sh\?ref=v9.9.9) cat "$STUB_ROOT/deploy/verify-release.sh" ;;
    "api repos/Phala-Network/phala-pay/git/ref/tags/v9.9.9 "*) echo "commit $STUB_COMMIT" ;;
    "api repos/Phala-Network/phala-pay/compare/$STUB_COMMIT...main "*) echo ahead ;;
    "release download v9.9.9 "*) cp "$STUB_ASSETS"/* "${@: -2:1}" ;;
    "attestation verify "*) [[ "$3" != *"$STUB_REFUSE"* ]] ;;
    *) exit 1 ;;
esac
STUB
cat >"$bin/node" <<'STUB'
#!/bin/sh
echo 24
STUB
# npm ci --prefix DIR ...: the locked CLI, as the stub Phala Cloud CLI.
cat >"$bin/npm" <<'STUB'
#!/usr/bin/env bash
[[ "$1 $2" == "ci --prefix" ]] || exit 1
mkdir -p "$3/node_modules/.bin"
ln -s "$STUB_BIN/phala-cli" "$3/node_modules/.bin/phala"
STUB
cat >"$bin/docker" <<'STUB'
#!/bin/sh
case "$1" in info | pull) ;; *) exit 1 ;; esac
STUB
cat >"$bin/uvx" <<'STUB'
#!/usr/bin/env bash
# uvx --from phala-pay==VERSION topup-sdk keygen --keyid ID --seed-out FILE: the release's own SDK.
echo "uvx $*" >>"$STUB_LOG"
[[ "$*" == "--from $STUB_SDK topup-sdk keygen --keyid admin/v1 --seed-out "* ]] || exit 1
(umask 077 && echo "seed-never-printed" >"${@: -1}")
echo '{"keyid": "admin/v1", "public_key": "23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo="}'
STUB
# The Phala Cloud CLI: each deploy records its arguments and the env file it sends, and builds the
# app-compose (the compose and the pre-launch script), whose SHA-256 the CVM then reports as its
# compose hash; the CVM is on node prod5. As Phala Cloud's, `cvms get` reports no instance id; the
# attestation carries the app-compose, and its event log names the compose hash and the instance id,
# unless STUB_UNATTESTED. With STUB_FOREIGN, the second deploy is followed by another update, with
# another compose: the CVM reports its hash, and with STUB_FOREIGN=update also attests it. The
# workspace's CVMs are STUB_CVMS, or the CLI's answer is STUB_CVMS_JSON. With STUB_HANG, `cvms get`
# hangs. The real CLI reads phala.toml from its working directory; the stub records when one is
# there. Deploy number STUB_FAIL_DEPLOY fails.
cat >"$bin/phala-cli" <<'STUB'
#!/usr/bin/env bash
[[ ! -e phala.toml ]] || echo "phala.toml in $PWD" >>"$STUB_LOG"
case "$1" in
    whoami) echo "operator" ;;
    deploy)
        echo "phala $*" >>"$STUB_LOG"
        while (($#)); do
            case "$1" in
                -e) stat -c %a "$2" >"$STUB_STATE/env.mode"; cp "$2" "$STUB_STATE/env" ;;
                --compose) compose=$2 ;;
                --pre-launch-script) pre_launch=$2 ;;
            esac
            shift
        done
        deploys=$(($(cat "$STUB_STATE/deploys" 2>/dev/null || echo 0) + 1))
        [[ "$deploys" != "${STUB_FAIL_DEPLOY:-}" ]] || exit 1
        echo "$deploys" >"$STUB_STATE/deploys"
        # app-compose NAME COMPOSE: an app-compose and its hash, STUB_STATE/NAME.json and NAME.hash.
        app_compose() {
            jq -cjn --rawfile compose "$2" --rawfile pre_launch "$pre_launch" \
                '{manifest_version: 2, docker_compose_file: $compose, pre_launch_script: $pre_launch}' \
                >"$STUB_STATE/$1.json"
            sha256sum "$STUB_STATE/$1.json" | cut -d' ' -f1 >"$STUB_STATE/$1.hash"
        }
        app_compose app-compose "$compose"
        if [[ -n "${STUB_FOREIGN:-}" && "$deploys" == 2 ]]; then
            printf 'services: {}\n' >"$STUB_STATE/foreign.yml"
            app_compose foreign "$STUB_STATE/foreign.yml"
        fi
        echo "Provisioning CVM ..."
        echo '{"success": true, "vm_uuid": "cvm-0123", "app_id": "0xabcdef0123456789abcdef0123456789abcdef01"}'
        ;;
    cvms)
        echo "phala $*" >>"$STUB_LOG"
        case "$2" in
            list)
                if [[ -n "${STUB_CVMS_JSON:-}" ]]; then
                    printf '%s\n' "$STUB_CVMS_JSON"
                    exit 0
                fi
                jq -n --arg names "${STUB_CVMS:-}" '[$names | splits(" ") | select(. != "")] | {success: true,
                    page: 1, pageSize: 100, total: length, totalPages: (if length == 0 then 0 else 1 end),
                    items: map({appId: "0x\("b" * 40)", cvmName: ., status: "running", uptime: null})}'
                ;;
            get)
                if [[ -n "${STUB_HANG:-}" ]]; then
                    : >"$STUB_STATE/hanging"
                    exec "$STUB_SLEEP" 60
                fi
                reported=app-compose
                [[ ! -f "$STUB_STATE/foreign.hash" ]] || reported=foreign
                jq -n --arg hash "0x$(cat "$STUB_STATE/$reported.hash")" '{status: "running", in_progress: false,
                    instance_id: null, compose_hash: $hash, app_id: "0xabcdef0123456789abcdef0123456789abcdef01",
                    gateway: {base_domain: "dstack-pha-prod5.phala.network"}}'
                ;;
            attestation)
                attested=app-compose
                [[ "${STUB_FOREIGN:-}" != update ]] || attested=foreign
                hash=$(cat "$STUB_STATE/$attested.hash")
                [[ -z "${STUB_UNATTESTED:-}" ]] || hash=0
                jq -n --rawfile app_compose "$STUB_STATE/$attested.json" --arg hash "$hash" '{compose_file: $app_compose,
                    tcb_info: {event_log: [{event: "compose-hash", event_payload: $hash},
                        {event: "instance-id", event_payload: ("A" * 40)}]}}'
                ;;
            *) exit 1 ;;
        esac
        ;;
    *) exit 1 ;;
esac
STUB
# The waits poll without pausing; a hanging stub and the test itself still pause.
STUB_SLEEP=$(command -v sleep)
printf '#!/bin/sh\n' >"$bin/sleep"
# shred, recorded.
if command -v shred >/dev/null; then
    STUB_SHRED=$(command -v shred)
    cat >"$bin/shred" <<'STUB'
#!/usr/bin/env bash
echo "shred $*" >>"$STUB_LOG"
exec "$STUB_SHRED" "$@"
STUB
fi
export STUB_SLEEP STUB_SHRED
chmod +x "$bin"/*

secrets=(operator-key-id operator-secret-key operator-read-key operator-verify-key)
# deploy NAME [ENV...]: in a subshell, the release's deploy.sh, piped to bash as from
# pay.phala.com, with ENV set, from fresh stub state.
deploy() {
    local name=$1
    shift
    rm -rf "$tmp/state" && mkdir -p "$tmp/state" "$tmp/tmp" "$tmp/runtime"
    : >"$tmp/log"
    cd "$tmp"
    exec env PATH="$bin:$PATH" TMPDIR="$tmp/tmp" XDG_RUNTIME_DIR="$tmp/runtime" STUB_BIN="$bin" STUB_ASSETS="$assets" \
        STUB_ROOT="$root" STUB_COMMIT="$commit" STUB_LOG="$tmp/log" STUB_STATE="$tmp/state" STUB_SDK="$sdk" \
        STUB_GH_VERSION=2.101.0 STUB_REFUSE=never CVM_NAME="$name" DEPLOY_ENVIRONMENT=staging \
        WALG_S3_PREFIX=s3://operator-backups/"$name" AWS_ENDPOINT=https://objects.operator.test \
        AWS_ACCESS_KEY_ID="${secrets[0]}" AWS_SECRET_ACCESS_KEY="${secrets[1]}" TOPUP_RPC_ANKR_KEY="${secrets[2]}" TOPUP_RPC_INFURA_KEY="${secrets[3]}" "$@" \
        bash -s -- --non-interactive <"$assets/deploy.sh" >"$tmp/$name.out" 2>"$tmp/$name.err"
}
# run NAME [ENV...]: deploy, to its end.
run() {
    (deploy "$@")
}
# clean NAME: the run left no temporary file and printed no secret.
clean() {
    local left
    left=$(find "$tmp/tmp" "$tmp/runtime" -mindepth 1)
    [[ -z "$left" ]] || fail "$1 left $left"
    ! grep -qF -e "${secrets[0]}" -e "${secrets[1]}" -e seed-never-printed -e 'alpha#bravo' \
        "$tmp/$1.out" "$tmp/$1.err" || fail "$1 printed a secret"
}
# succeeds NAME [ENV...]: run, which must succeed, leave no temporary file, and print no secret.
succeeds() {
    run "$@" || { cat "$tmp/$1.err" >&2; fail "$1 failed"; }
    clean "$1"
}
# unfinished NAME [ENV...]: run, which must fail after creating CVM cvm-0123, leave no temporary
# file, print no secret, and print the CVM's id and how to finish or remove it.
unfinished() {
    ! run "$@" || fail "$1 passed"
    clean "$1"
    local line
    for line in "^Phala Pay v9.9.9: this run created CVM cvm-0123 and then failed" '^  CVM id    cvm-0123$' \
        '^To start over instead, delete CVM cvm-0123'; do
        grep -q -- "$line" "$tmp/$1.out" ||
            { cat "$tmp/$1.err" "$tmp/$1.out" >&2; fail "$1 did not print its CVM and how to recover it"; }
    done
}
deploys() {
    grep '^phala deploy' "$tmp/log" || true
}

# Quick start: the template, verified with the GitHub CLI, sealed at provision; a phala.toml in the
# caller's directory that would make the CLI update another CVM with another env is never read.
printf '%s\n' 'id = "another-cvm"' 'env_file = "other.env"' >"$tmp/phala.toml"
succeeds quick TOPUP_ADMIN_PUBLIC_KEY=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo= AWS_REGION=auto
[[ "$(grep -c '^gh attestation verify' "$tmp/log")" == 11 ]] || fail "quick did not verify 5 assets and provenance plus SBOM for 3 images"
[[ "$(deploys | wc -l)" == 1 ]] || fail "quick did not deploy exactly once"
for argument in "--name quick " "--image dstack-0.5.9 " "--no-dev-os" "--kms phala " "--instance-type tdx.medium " \
    "/kit/deploy/phala-cloud-pre-launch.sh " "-e $tmp/runtime/"; do
    deploys | grep -qF -- "$argument" || fail "quick did not deploy with $argument"
done
[[ "$(cat "$tmp/state/env.mode")" == 600 ]] || fail "quick's env file is not mode 0600"
diff <(cat "$tmp/state/env") - <<ENV || fail "quick sealed another env"
AWS_ACCESS_KEY_ID=${secrets[0]}
AWS_ENDPOINT=https://objects.operator.test
AWS_REGION=auto
AWS_SECRET_ACCESS_KEY=${secrets[1]}
TOPUP_ADMIN_PUBLIC_KEY=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=
TOPUP_RPC_ANKR_KEY=${secrets[2]}
TOPUP_RPC_INFURA_KEY=${secrets[3]}
WALG_S3_PREFIX=s3://operator-backups/quick
ENV
grep -qx '  URL       https://abcdef0123456789abcdef0123456789abcdef01.dstack-pha-prod5.phala.network' \
    "$tmp/quick.out" || fail "quick did not print the gateway URL"
grep -qx 'Phala Pay v9.9.9 is provisioned.' "$tmp/quick.out" || fail "quick did not print its summary"
grep -qF 'https://github.com/Phala-Network/phala-pay/blob/v9.9.9/docs/self-hosting.md#5-verify-the-attestation' \
    "$tmp/quick.out" || fail "quick did not link the release's guide"
# succeeds already proved the temporary kit was deleted. Execute the printed command with a fresh
# operator kit's verifier stub and check its arguments, so a reference to the deleted pin fails.
verification_command=$(sed -n 's/^      \(kit\/deploy\/verify-attestation\.sh .*\)$/\1/p' "$tmp/quick.out")
[[ -n "$verification_command" ]] || fail "quick did not print its attestation command"
mkdir -p "$tmp/operator/kit/deploy"
cat >"$tmp/operator/kit/deploy/verify-attestation.sh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@"
STUB
chmod +x "$tmp/operator/kit/deploy/verify-attestation.sh"
verification_arguments=$(cd "$tmp/operator" && bash -c "$verification_command") ||
    fail "quick's printed attestation command failed after cleanup"
expected_os_hash=$(<"$root/deploy/environments/phala-cloud-template/topup/os-image-hash")
diff <(printf '%s\n' "$verification_arguments") - <<ARGS || fail "quick's printed attestation command lost its OS pin after cleanup"
attestation.json
info.json
0xabcdef0123456789abcdef0123456789abcdef01
phala-cloud-template.yml
template
$expected_os_hash
ARGS
! grep -q '^phala cvms attestation' "$tmp/log" || fail "quick waited for the attestation"
! grep -q '^phala.toml in' "$tmp/log" || fail "quick ran the CLI where a phala.toml is"
[[ "$(cat "$tmp/quick.cvm-id")" == cvm-0123 ]] || fail "quick did not record its CVM"
[[ -z "${STUB_SHRED:-}" ]] || grep -q "^shred -u $tmp/runtime/.*/sealed\.env$" "$tmp/log" ||
    fail "quick did not shred the sealed env file"
rm "$tmp/phala.toml"

# Custom domain: SHA256SUMS only (an older GitHub CLI), a generated admin key, the service variant.
succeeds custom STUB_GH_VERSION=2.100.0 DOMAIN=pay-api.operator.test \
    ENVIRONMENT_DIR="$tmp/custom/topup" ADMIN_SEED_FILE="$tmp/admin.seed"
grep -q "SHA256SUMS only" "$tmp/custom.err" || fail "custom did not say it checked SHA256SUMS only"
! grep -q '^gh attestation' "$tmp/log" || fail "custom used an older GitHub CLI"
[[ "$(stat -c %a "$tmp/admin.seed")" == 600 ]] || fail "custom's admin seed is not mode 0600"
grep -qx '  public_key: 23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo=' "$tmp/custom/topup/topup.yaml" ||
    fail "custom did not attest the generated admin key"
[[ "$(deploys | wc -l)" == 2 ]] || fail "custom did not provision and then set the gateway"
deploys | head -1 | grep -qF -- "--name custom " || fail "custom did not provision first"
deploys | tail -1 | grep -qF -- "--cvm-id cvm-0123 " || fail "custom did not upgrade the new CVM"
! deploys | tail -1 | grep -qF -- " -e " || fail "custom's upgrade sent an env"
[[ "$(cut -d= -f1 "$tmp/state/env" | tr '\n' ' ')" == "AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY TOPUP_RPC_ANKR_KEY TOPUP_RPC_INFURA_KEY " ]] ||
    fail "custom sealed other names than the ones it was given"
grep -q 'GATEWAY_DOMAIN: gateway.dstack-pha-prod5.phala.network' "$tmp/custom/docker-compose.custom.yml" ||
    fail "custom's compose does not name the CVM's gateway"
grep -qx '  CNAME  pay-api.operator.test  gateway.dstack-pha-prod5.phala.network' "$tmp/custom.out" ||
    fail "custom did not print the CNAME record"
grep -qx "  TXT    _dstack-app-address.pay-api.operator.test  $(printf 'a%.0s' {1..40}):443" "$tmp/custom.out" ||
    fail "custom did not print the TXT record"
grep -qx "phala cvms attestation cvm-0123 --json" "$tmp/log" || fail "custom did not read the attestation"

# Fail closed: what does not verify or check out deploys nothing, leaves nothing, and prints no secret.
# refused NAME MESSAGE [ENV...]
refused() {
    local name=$1 message=$2
    shift 2
    ! run "$name" TOPUP_ADMIN_PUBLIC_KEY=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo= "$@" ||
        fail "$name deployed"
    grep -qF -- "$message" "$tmp/$name.err" || { cat "$tmp/$name.err" >&2; fail "$name failed for another reason"; }
    [[ -z "$(deploys)" ]] || fail "$name deployed"
    clean "$name"
}

# A run that fails after the CVM's creation (here, setting its gateway) records it and prints its
# id, URL, and how to finish it; a rerun creates none, and says the same.
unfinished rerun STUB_GH_VERSION=2.100.0 DOMAIN=pay-api.rerun.test ENVIRONMENT_DIR="$tmp/rerun/topup" \
    TOPUP_ADMIN_PUBLIC_KEY=23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo= STUB_FAIL_DEPLOY=2
[[ "$(cat "$tmp/rerun/topup/cvm-id" 2>/dev/null)" == cvm-0123 ]] ||
    { cat "$tmp/rerun.err" >&2; fail "rerun's first run did not record its CVM"; }
grep -qx '  URL       https://pay-api.rerun.test' "$tmp/rerun.out" || fail "rerun's first run did not print its URL"
grep -qF "TOPUP_CVM_ID=cvm-0123, and run Deploy with mode upgrade" "$tmp/rerun.out" ||
    fail "rerun's first run did not say how to finish the CVM"
refused rerun "an earlier run created CVM cvm-0123 for this instance" STUB_GH_VERSION=2.100.0 \
    DOMAIN=pay-api.rerun.test ENVIRONMENT_DIR="$tmp/rerun/topup"
grep -qF "TOPUP_CVM_ID=cvm-0123, and run Deploy with mode upgrade" "$tmp/rerun.err" ||
    fail "rerun did not say how to finish the CVM"

# A custom domain whose CVM never attests its compose times out, and still prints the CVM and how to
# finish it.
unfinished unattested-cvm STUB_GH_VERSION=2.100.0 DOMAIN=pay-api.unattested.test \
    ENVIRONMENT_DIR="$tmp/unattested-cvm/topup" TOPUP_ADMIN_PUBLIC_KEY=23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo= \
    STUB_UNATTESTED=1
grep -qF "the attestation reports compose hash '0', not the deployed " "$tmp/unattested-cvm.err" ||
    fail "unattested-cvm failed for another reason"

# The gateway upgrade is bound to this run's compose: a CVM that reports another update's compose
# hash fails, whether it attests its own app-compose (whose hash is not the reported one) or the
# other update's (which does not carry this run's compose).
unfinished foreign-hash STUB_GH_VERSION=2.100.0 DOMAIN=pay-api.foreign.test \
    ENVIRONMENT_DIR="$tmp/foreign-hash/topup" TOPUP_ADMIN_PUBLIC_KEY=23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo= \
    STUB_FOREIGN=hash
grep -qF "the attestation reports compose hash '$(cat "$tmp/state/app-compose.hash")', not the deployed $(cat "$tmp/state/foreign.hash")" \
    "$tmp/foreign-hash.err" || { cat "$tmp/foreign-hash.err" >&2; fail "foreign-hash failed for another reason"; }
! grep -q '^  TXT' "$tmp/foreign-hash.out" || fail "foreign-hash printed DNS records"
unfinished foreign-update STUB_GH_VERSION=2.100.0 DOMAIN=pay-api.foreign.test \
    ENVIRONMENT_DIR="$tmp/foreign-update/topup" TOPUP_ADMIN_PUBLIC_KEY=23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo= \
    STUB_FOREIGN=update
for line in "CVM cvm-0123 attests another compose than this run's" \
    "attested docker_compose_file differs from the prepared compose"; do
    grep -qF "$line" "$tmp/foreign-update.err" || { cat "$tmp/foreign-update.err" >&2; fail "foreign-update failed for another reason"; }
done
! grep -q '^  TXT' "$tmp/foreign-update.out" || fail "foreign-update printed DNS records"

# Ctrl-C mid-deploy, while the redirected wait runs: the summary still reaches the script's own
# stdout, the sealed env file is shredded (where shred exists) and removed, and no secret is printed.
set -m
(deploy interrupted STUB_GH_VERSION=2.100.0 DOMAIN=pay-api.interrupted.test \
    ENVIRONMENT_DIR="$tmp/interrupted/topup" TOPUP_ADMIN_PUBLIC_KEY=23Y9wEJMOTySGV3UXmcTFnQsbigA9/cYTvmqdQxzmdo= \
    STUB_HANG=1) &
interrupted=$!
set +m
for _ in $(seq 600); do
    [[ ! -e "$tmp/state/hanging" ]] || break
    "$STUB_SLEEP" 0.1
done
[[ -e "$tmp/state/hanging" ]] || { kill -KILL -- "-$interrupted"; fail "interrupted never reached the wait"; }
kill -INT -- "-$interrupted"
status=0
wait "$interrupted" || status=$?
[[ "$status" == 130 ]] || { cat "$tmp/interrupted.err" >&2; fail "interrupted exited $status, not 130"; }
clean interrupted
for line in "^Phala Pay v9.9.9: this run created CVM cvm-0123 and then failed" '^  CVM id    cvm-0123$' \
    '^  URL       https://pay-api.interrupted.test$' "TOPUP_CVM_ID=cvm-0123, and run Deploy with mode upgrade"; do
    grep -q -- "$line" "$tmp/interrupted.out" ||
        { cat "$tmp/interrupted.err" "$tmp/interrupted.out" >&2; fail "interrupted did not print its CVM ($line)"; }
done
[[ -z "${STUB_SHRED:-}" ]] || grep -qx "shred -u $tmp/runtime/phala-pay-deploy\.[A-Za-z0-9]*/sealed\.env" "$tmp/log" ||
    fail "interrupted did not shred the sealed env file"

# A name the workspace already has, from a run elsewhere or another CVM, is not created again, and
# refused before an admin seed is generated.
refused taken "the Phala Cloud workspace already has a CVM named taken-name" CVM_NAME=taken-name \
    STUB_CVMS="taken-name-2 Taken-Name" TOPUP_ADMIN_PUBLIC_KEY= ADMIN_SEED_FILE="$tmp/taken.seed"
grep -qF "kit/deploy/phala cvms get taken-name" "$tmp/taken.err" || fail "taken did not say how to find the CVM"
[[ ! -e "$tmp/taken.seed" ]] || fail "taken wrote an admin seed"
! grep -q '^uvx' "$tmp/log" || fail "taken generated an admin key"
# Neither is a CVM list it cannot read as one complete page (page 1 of 1, or of 0 for no match;
# total on the page and the items' count; each item a name and an app id): an answer is refused,
# never taken for "no such name", such as total 1 with no items.
page='"success": true, "page": 1, "pageSize": 100, "total": 1, "totalPages": 1'
item='"appId": "0xbb", "status": "running", "uptime": null'
# The refusal says what may be behind it and what to do.
unreadable="deploy.sh: could not check that no CVM is named unreadable-list: the workspace may have an app"
unreadable+=" matching it without a current CVM (or more than one page of matches); inspect it in Phala Cloud,"
unreadable+=" or choose another instance name"
for answer in 'not JSON' '[]' '{"success": true}' "{$page}" "{$page, \"items\": {}}" \
    "{$page, \"items\": [{$item}]}" "{$page, \"items\": [{$item, \"cvmName\": 7}]}" \
    "{$page, \"items\": [\"listed-name\"]}" "{$page, \"items\": [{$item, \"cvmName\": \"other\", \"appId\": null}]}" \
    "{${page/\"totalPages\": 1/\"totalPages\": 2}, \"items\": []}" \
    "{${page/\"totalPages\": 1/\"totalPages\": \"1\"}, \"items\": []}" \
    "{${page/\"total\": 1/\"total\": 0}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{$page, \"items\": []}" \
    "{${page/\"total\": 1/\"total\": 2}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{${page/\"totalPages\": 1/\"totalPages\": 0}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{${page/\"total\": 1, \"totalPages\": 1/\"total\": 0, \"totalPages\": 2}, \"items\": []}" \
    "{${page/\"total\": 1/\"total\": 101}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{${page/\"pageSize\": 100/\"pageSize\": 0}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{${page/\"page\": 1/\"page\": 2}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{${page/\"total\": 1/\"total\": 1.5}, \"items\": [{$item, \"cvmName\": \"other\"}]}" \
    "{${page/\"success\": true/\"success\": false}, \"items\": []}"; do
    refused unreadable-list "$unreadable" STUB_CVMS_JSON="$answer" \
        TOPUP_ADMIN_PUBLIC_KEY= ADMIN_SEED_FILE="$tmp/unreadable-list.seed"
    [[ ! -e "$tmp/unreadable-list.seed" ]] || fail "unreadable-list generated an admin key for $answer"
done

refused price-license "production refuses staging-only price licensing opt-in" DEPLOY_ENVIRONMENT=production \
    TOPUP_ADMIN_PUBLIC_KEY=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=
refused short-name "CVM_NAME must be 5 to 63 characters" CVM_NAME=abcd
refused bad--name "CVM_NAME must be letters, digits, and -"
refused 1st-name "CVM_NAME must be letters, digits, and -"
refused strict "--strict needs the GitHub CLI" STUB_GH_VERSION=2.100.0 PHALA_PAY_REQUIRE_ATTESTATION=1
refused hash-secret "AWS_SECRET_ACCESS_KEY has a #, a quote, or surrounding whitespace" \
    AWS_SECRET_ACCESS_KEY='alpha#bravo'
cp "$assets/images.json" "$tmp/images.json"
echo '{}' >"$assets/images.json"
refused tampered "does not match its SHA256SUMS" STUB_GH_VERSION=2.100.0
cp "$tmp/images.json" "$assets/images.json"
refused unattested "did not verify" STUB_REFUSE=deploy.sh
echo "one-command deploy test passed"
