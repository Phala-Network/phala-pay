#!/usr/bin/env bash
# One-command deploy of a Phala Pay instance to the owner's own Phala Cloud workspace
# (docs/self-hosting.md, "One-command deploy"):
#
#   curl -fsSL https://pay.phala.com/deploy.sh | bash
#
# Each release publishes this script as its asset deploy.sh, set to deploy that release. The command
# above is an HTTPS bootstrap, like rustup's, Deno's, Bun's, or Homebrew's: it trusts pay.phala.com,
# its TLS and Cloudflare, GitHub, and Phala Pay's releasers to serve this script. For high
# assurance, verify the script's own attestation before running it (docs/self-hosting.md,
# "One-command deploy"). It downloads the release into a private temporary directory, removed on
# exit, and verifies it: with the GitHub CLI 2.101 or later, logged in, by the release's
# deploy/verify-release.sh (its commit, SHA256SUMS, and every build provenance attestation, as
# Deploy does); otherwise against SHA256SUMS only, which catches a corrupted download but proves no
# provenance. --strict, or PHALA_PAY_REQUIRE_ATTESTATION=1, refuses to go on without the GitHub CLI.
# It then runs only the verified kit's scripts, in Deploy's order (.github/workflows/deploy.yml):
# render.sh, preflight.sh --offline, check-route-modes.sh, and the kit's locked Phala Cloud CLI
# through phala-cvm.sh, with the kit's pre-launch script, OS image dstack-0.5.9, and Phala Cloud's
# KMS. Two variants:
# - quick start: the Phala Cloud template (deploy/README.md, "The Phala Cloud template variant"),
#   a testnet instance at its gateway domain, its admin key and backup location in the CVM's env;
# - custom domain: the service variant, every setting attested, from an environment directory that
#   it writes from the template's routes, or an existing one. It prints the DNS records to create.
#
# Unlike Deploy, it seals the secrets at provision, from the owner's machine: the env file, the
# sealed names given a value and no other, is written mode 0600 in a private directory under
# $XDG_RUNTIME_DIR (a tmpfs) where there is one, otherwise in the temporary directory, shredded
# (where shred exists) and removed on every exit; preflight refuses a value the CLI would read
# differently. No secret is printed or kept; a generated admin seed goes only to the file the owner
# names. Phala Cloud authentication is the owner's CLI login or PHALA_CLOUD_API_KEY, never stored.
# The CLI runs in an empty directory of its own, with absolute paths, so no phala.toml in the
# caller's directory can turn the new CVM into an update of another.
#
# One CVM per instance. The run's state is a file, in the directory it is run from or the chosen
# environment directory: once the CVM exists, its id (no secret) goes to the environment
# directory's cvm-id, or to CVM_NAME.cvm-id in the current directory for the quick start, and a
# later run that finds it creates nothing and says how to finish that CVM. A run from elsewhere
# cannot see that file, so before writing anything (an admin seed included) it also refuses a name
# the Phala Cloud workspace already has, as the CLI's names are unique in a workspace; an answer it
# cannot read as one complete page of CVMs is refused too. Once the CVM exists, every exit, Ctrl-C
# included, prints what is known of it (its id, app id, and URL) and, after a failure, how to
# finish or remove it. The quick start waits for the CVM to settle with its compose; a custom
# domain also for the attestation of the compose with the node's gateway, bound to this run's
# rendered compose as Deploy binds it (deploy/attested-compose.sh: the API's compose hash and the
# attested one must both be the hash of the app-compose carrying it), whose event log names the
# instance id the TXT record needs. A provision proves nothing about the CVM's health: /healthz and the attestation,
# and for a custom domain the certificate evidence, are the acceptance step.
#
# The inputs are prompted for on the terminal, or with --non-interactive read from the environment:
#   CVM_NAME                  the CVM's name: 5 to 63 letters, digits, and -, from a letter to a
#                             letter or digit, without --
#   DOMAIN                    the API's custom domain; empty or unset for the quick start
#   ENVIRONMENT_DIR           custom domain: the environment directory, written unless it exists
#                             (default ./CVM_NAME/topup)
#   TOPUP_ADMIN_PUBLIC_KEY    the admin public key; empty or unset to generate a keypair with the
#   ADMIN_SEED_FILE           release's Python SDK, phala-pay, its seed in this new file (default
#                             ./CVM_NAME-admin.seed)
#   WALG_S3_PREFIX AWS_ENDPOINT AWS_REGION (default auto)      the backup location
#   AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY                    its read-write token
#   DEPLOY_ENVIRONMENT        production (default) or staging; staging opts into Unclear prices
#   SENTRY_DSN                optional
#   TOPUP_RPC_<ID>_KEY        the key of each keyed RPC provider an existing directory declares
#
# Usage: deploy.sh [--non-interactive] [--strict]
set -euo pipefail

repository=Phala-Network/phala-pay
# The release this script deploys; the Release workflow sets its tag in the published asset.
release=latest
# The Python SDK whose topup-sdk keygen generates an admin key: the release's own version, which
# scripts/version.sh sets. A pre-release publishes no SDK, so its run needs TOPUP_ADMIN_PUBLIC_KEY.
sdk=phala-pay==0.10.0rc3

say() {
    printf '%s\n' "$*" >&2
}
die() {
    say "deploy.sh: $*"
    exit 1
}
# ask NAME PROMPT [DEFAULT]: NAME from the terminal, or with --non-interactive from the
# environment; DEFAULT when that is empty, and required without a DEFAULT. ask_secret does not
# echo the answer.
ask() {
    local name=$1 prompt=$2 value=""
    if ((non_interactive)); then
        value=${!name:-}
    elif [[ -n "${secret:-}" ]]; then
        read -rs -p "$prompt: " value </dev/tty
        say ""
    else
        read -r -p "$prompt${3:+ [$3]}: " value </dev/tty
    fi
    [[ -n "$value" || $# == 3 ]] || die "$name is required"
    [[ "$value" != *[[:cntrl:]]* ]] || die "$name must be one line"
    printf -v "$name" '%s' "${value:-${3:-}}"
}
ask_secret() {
    secret=1 ask "$@"
}
# check NAME ERE DESCRIPTION
check() {
    [[ "${!1}" =~ $2 ]] || die "$1 must be $3"
}
yaml_string() {
    jq -rn --arg value "$1" '$value | tojson'
}
# The Phala Cloud CLI reads phala.toml (a CVM id, an env file) from its working directory: it runs
# in an empty one.
phala_cli() {
    (cd "$work/cli" && "$@")
}
phala_cvm() {
    phala_cli "$kit/deploy/phala-cvm.sh" "$@"
}

# recover CVM [ID]: how to finish or remove the instance's CVM, named CVM (its id or its name), with
# id ID when known.
recover() {
    if [[ "$variant" == service ]]; then
        echo "Finish it with Deploy, which renders the node's gateway, keeps the sealed env, and prints the"
        echo "DNS records: commit $ENVIRONMENT_DIR to your environment repository, set the Environment"
        echo "variable TOPUP_CVM_ID=${2:-"<its id: kit/deploy/phala cvms get $1>"}, and run Deploy with mode upgrade"
        echo "at $release ($docs#4-release-and-provision, step 5)."
    else
        echo "See it in Phala Cloud's dashboard, or with the release's locked CLI"
        echo "($docs#2-your-environment-repository, step 1): kit/deploy/phala cvms get $1."
        echo "The quick start has no upgrade path."
    fi
    echo "To start over instead, delete CVM $1 in Phala Cloud$([[ ! -f "$state" ]] || echo ", then $state")."
}

# summary STATUS: once the CVM exists, on every exit, what is known of it; after a failure, how to
# finish or remove it.
summary() {
    if (($1 == 0)); then
        printf '\nPhala Pay %s is provisioned.\n\n' "$release"
    else
        printf '\nPhala Pay %s: this run created CVM %s and then failed; the CVM is not finished.\n\n' \
            "$release" "$cvm_id"
    fi
    printf '  %-9s %s\n' "CVM id" "$cvm_id" "App id" "$app_id"
    [[ -z "$url" ]] || printf '  %-9s %s\n' URL "$url"
    [[ -z "$record" ]] || printf '  %-9s %s%s\n' Compose "$record" "$([[ "$variant" == service ]] || echo " (the release's)")"
    if (($1 != 0)); then
        echo
        recover "$cvm_id" "$cvm_id"
        return
    fi
    if [[ "$variant" == service ]]; then
        cat <<SUMMARY

Create these DNS records, not proxied (Cloudflare: DNS only); dstack-ingress then obtains its
certificate through port 443:

  CNAME  $DOMAIN  $gateway
  TXT    _dstack-app-address.$DOMAIN  $instance_id:443

$env_dir holds this instance's settings and no secret. Commit it to your
environment repository and set TOPUP_CVM_ID=$cvm_id to upgrade the instance with Deploy:
$docs#2-your-environment-repository
SUMMARY
    fi
    cat <<SUMMARY

The provision proves nothing about the instance's health. It is accepted once
  - $answers, and
  - you have verified its attestation with the release's verified kit
    ($docs#5-verify-the-attestation):
      kit/deploy/verify-attestation.sh attestation.json info.json $app_id $record $variant

Next, create a merchant account with the admin key (BASE_URL=$url):
$docs#6-onboard-your-first-account
SUMMARY
}

# finish: on every exit, the summary of a CVM this run created, on the script's own stdout (fd 3:
# an exit inside a redirected function keeps that redirection); the sealed env file shredded where
# shred exists, and every temporary file removed.
finish() {
    local status=$?
    if [[ -n "$sealed" ]]; then
        [[ ! -f "$sealed/sealed.env" ]] || ! command -v shred >/dev/null || shred -u "$sealed/sealed.env" || true
        rm -rf "$sealed"
    fi
    [[ -z "$work" ]] || rm -rf "$work"
    [[ -z "$cvm_id" ]] || summary "$status" >&3
}

# All of it in a function, so that bash has read the whole script before any command can read the
# rest of a piped script from stdin.
main() {
    non_interactive=0 strict=0
    # fd 3: the script's stdout, for the summary (finish).
    exec 3>&1
    # What finish and summary read: the temporary directories, and what is known of the new CVM.
    work="" sealed="" env_dir="" cvm_id="" app_id="" url="" gateway="" instance_id="" record="" answers=""
    [[ "${PHALA_PAY_REQUIRE_ATTESTATION:-}" != 1 ]] || strict=1
    local argument
    for argument in "$@"; do
        case "$argument" in
            --non-interactive) non_interactive=1 ;;
            --strict) strict=1 ;;
            *) die "usage: deploy.sh [--non-interactive] [--strict]" ;;
        esac
    done
    ((non_interactive)) || { : </dev/tty; } 2>/dev/null ||
        die "no terminal to prompt on: run with --non-interactive and the inputs in the environment"

    local deployment_environment=${DEPLOY_ENVIRONMENT:-production}
    check deployment_environment '^(production|staging)$' "production or staging"

    say "== prerequisites"
    # The kit's scripts need bash 4.4 (empty arrays under set -u); `bash` is the same one on PATH.
    ((BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1] >= 404)) || die "bash 4.4 or later is required (macOS: brew install bash)"
    local tool timeout_cli=timeout
    if ! command -v timeout >/dev/null; then
        timeout_cli=gtimeout
        command -v gtimeout >/dev/null || die "GNU timeout is required (macOS: brew install coreutils)"
    fi
    for tool in curl tar jq node npm docker; do
        command -v "$tool" >/dev/null || die "$tool is required"
    done
    (($(node -p 'process.versions.node.split(".")[0]') >= 22)) || die "Node.js 22 or later is required"
    docker info >/dev/null 2>&1 || die "Docker must be running: preflight checks the configuration in the release's image"
    if [[ "$release" == latest ]]; then
        release=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repository/releases/latest")
        release=${release##*/}
    fi
    check release '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$' "v<semver>"
    # The guide of the release deployed.
    docs=https://github.com/$repository/blob/$release/docs/self-hosting.md
    local provenance="" major minor
    if command -v gh >/dev/null; then
        IFS=. read -r major minor _ <<<"$(gh --version | sed -n '1s/^gh version \([0-9.]*\).*/\1/p')"
        if ((major * 1000 + minor >= 2101)) && gh auth status >/dev/null 2>&1; then
            provenance=gh
        fi
    fi
    if [[ -z "$provenance" ]]; then
        ((!strict)) || die "--strict needs the GitHub CLI 2.101 or later, logged in (gh auth login)"
        say "note: without the GitHub CLI 2.101 or later, logged in, the release is checked against"
        say "      its SHA256SUMS only, not its build provenance; the full check: $docs#verify-a-release"
    fi

    say "== Phala Pay $release: settings"
    ask CVM_NAME "Instance name (the CVM's name: 5 to 63 letters, digits, and -)"
    # The locked CLI's rule for a CVM name.
    ((${#CVM_NAME} >= 5 && ${#CVM_NAME} <= 63)) || die "CVM_NAME must be 5 to 63 characters"
    [[ "$CVM_NAME" =~ ^[A-Za-z]([A-Za-z0-9-]*[A-Za-z0-9])?$ && "$CVM_NAME" != *--* ]] ||
        die "CVM_NAME must be letters, digits, and -, from a letter to a letter or digit, without --"
    say "Quick start: a testnet instance at Phala Cloud's gateway domain, its admin key and backup"
    say "location unattested. Custom domain: every setting attested, for an instance with merchants."
    ask DOMAIN "Custom domain for the API, e.g. pay-api.example.com (empty for the quick start)" ""
    variant=template
    local existing=0
    if [[ -n "$DOMAIN" ]]; then
        variant=service
        check DOMAIN '^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$' "a lowercase DNS name"
        ask ENVIRONMENT_DIR "Environment directory (written unless it exists)" "$PWD/$CVM_NAME/topup"
        if [[ -f "$ENVIRONMENT_DIR/topup.yaml" ]]; then
            existing=1
            say "using the settings of $ENVIRONMENT_DIR"
        elif [[ -e "$ENVIRONMENT_DIR" ]]; then
            die "$ENVIRONMENT_DIR exists without a topup.yaml"
        fi
    fi
    # One CVM per instance: the run that created it recorded its id in the environment directory, or
    # for the quick start in the directory it ran from, and a later run creates none.
    state=$PWD/$CVM_NAME.cvm-id
    [[ "$variant" == template ]] || state=$ENVIRONMENT_DIR/cvm-id
    if [[ -f "$state" ]]; then
        local recorded
        recorded=$(<"$state")
        say "deploy.sh: an earlier run created CVM $recorded for this instance ($state); this run creates none."
        recover "$recorded" "$recorded" >&2
        exit 1
    fi

    say "== download and verify the release"
    trap finish EXIT
    trap 'exit 130' INT TERM
    work=$(mktemp -d "${TMPDIR:-/tmp}/phala-pay-deploy.XXXXXX")
    work=$(CDPATH='' cd -- "$work" && pwd)
    # The sealed env file's private directory: on tmpfs where the session has one, so no secret
    # reaches a disk.
    if [[ -n "${XDG_RUNTIME_DIR:-}" && -d "$XDG_RUNTIME_DIR" && -w "$XDG_RUNTIME_DIR" ]]; then
        sealed=$(mktemp -d "$XDG_RUNTIME_DIR/phala-pay-deploy.XXXXXX")
    else
        sealed=$(mktemp -d "$work/sealed.XXXXXX")
    fi
    local assets=$work/release
    kit=$work/kit
    mkdir -p "$assets" "$kit" "$work/cli"
    if [[ "$provenance" == gh ]]; then
        for tool in verify-release.sh deadline.sh; do
            "$timeout_cli" --foreground --kill-after=2 60 gh api -H 'Accept: application/vnd.github.raw' \
                "repos/$repository/contents/deploy/$tool?ref=$release" >"$work/$tool" ||
                die "could not download $release verification helper $tool"
        done
        bash "$work/verify-release.sh" "$release" "$assets" >/dev/null </dev/null ||
            die "the release $release did not verify"
    else
        local download=https://github.com/$repository/releases/download/$release asset
        curl -fsSL --retry 3 -o "$assets/SHA256SUMS" "$download/SHA256SUMS"
        while read -r _ asset; do
            [[ "$asset" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "SHA256SUMS names an unexpected file"
            curl -fsSL --retry 3 -o "$assets/$asset" "$download/$asset" </dev/null
        done <"$assets/SHA256SUMS"
        (cd "$assets" && if command -v sha256sum >/dev/null; then sha256sum --quiet -c SHA256SUMS; else
            shasum -a 256 --quiet -c SHA256SUMS; fi) || die "the release $release does not match its SHA256SUMS"
        say "the assets match SHA256SUMS"
    fi
    local images=$assets/images.json
    tar -xzf "$assets/phala-pay-deploy-$release.tar.gz" -C "$kit" --strip-components=1
    npm ci --prefix "$kit/deploy/tools" --ignore-scripts --no-audit --no-fund --loglevel=error </dev/null >&2
    phala_cli "$kit/deploy/phala" whoami </dev/null >&2 ||
        die "log in to Phala Cloud with the release's locked CLI (kit/deploy/phala login:" \
            "$docs#2-your-environment-repository, step 1), or export PHALA_CLOUD_API_KEY"
    # A CVM's name is unique in its workspace: one there already, perhaps from a run elsewhere whose
    # recorded id this run cannot see, is refused rather than duplicated, before anything is written
    # (an admin seed included). The locked CLI's answer (cli/src/commands/cvms/list, CLI 1.1.22) must
    # be one complete page: page 1 of 1 (or of 0, for no match), every match on it (total fits the page
    # and is the items' count), each item a name and an app id. Anything else is refused, never read
    # as "no such name"; that includes a match the CLI leaves out (an app without a current CVM).
    local existing_cvms
    existing_cvms=$(phala_cli "$kit/deploy/phala" cvms list --search "$CVM_NAME" --page-size 100 --json </dev/null) ||
        die "could not list the workspace's CVMs"
    jq -e 'def count: type == "number" and . >= 0 and . == floor;
        type == "object" and .success == true
        and all(.page, .pageSize, .total, .totalPages; count) and .page == 1 and .pageSize >= 1
        and .total <= .pageSize and (.totalPages == 1 or (.total == 0 and .totalPages == 0))
        and (.items | type) == "array" and (.items | length) == .total
        and all(.items[]; type == "object" and (.cvmName | type) == "string" and (.appId | type) == "string")' \
        <<<"$existing_cvms" >/dev/null 2>&1 ||
        die "could not check that no CVM is named $CVM_NAME: the workspace may have an app matching it" \
            "without a current CVM (or more than one page of matches); inspect it in Phala Cloud, or choose" \
            "another instance name"
    if jq -e --arg name "$CVM_NAME" 'any(.items[]; .cvmName | ascii_downcase == ($name | ascii_downcase))' \
        <<<"$existing_cvms" >/dev/null; then
        say "deploy.sh: the Phala Cloud workspace already has a CVM named $CVM_NAME; this run creates none."
        say "If an earlier run of deploy.sh created it, that run recorded its id where it ran (the quick"
        say "start's CVM_NAME.cvm-id in its current directory, a custom domain's cvm-id in the environment"
        say "directory)."
        recover "$CVM_NAME" >&2
        say "Otherwise choose another instance name."
        exit 1
    fi

    say "== admin key and backup location"
    if ((!existing)); then
        ask TOPUP_ADMIN_PUBLIC_KEY "Admin public key (empty to generate a keypair here)" ""
        if [[ -z "$TOPUP_ADMIN_PUBLIC_KEY" ]]; then
            ask ADMIN_SEED_FILE "New file for the admin seed" "$PWD/$CVM_NAME-admin.seed"
            local keygen
            if command -v uvx >/dev/null; then
                keygen=(uvx --from "$sdk" topup-sdk)
            elif command -v pipx >/dev/null; then
                keygen=(pipx run --spec "$sdk" topup-sdk)
            else
                die "generating the admin key needs uv or pipx (the Python SDK's topup-sdk keygen)"
            fi
            # keygen writes the seed mode 0600, refuses an existing file, and prints the public key.
            TOPUP_ADMIN_PUBLIC_KEY=$("${keygen[@]}" keygen --keyid admin/v1 --seed-out "$ADMIN_SEED_FILE" \
                </dev/null | jq -er '.public_key') || die "topup-sdk keygen failed"
            say "the admin seed is in $ADMIN_SEED_FILE (mode 0600): keep it offline, it is the admin key"
        fi
        check TOPUP_ADMIN_PUBLIC_KEY '^[A-Za-z0-9+/]{43}=$' "a base64 ed25519 public key"
        ask WALG_S3_PREFIX "Backup location s3://BUCKET/PATH (a new, empty prefix for this instance only)"
        check WALG_S3_PREFIX '^s3://[a-z0-9][a-z0-9.-]{1,61}[a-z0-9](/[A-Za-z0-9._~/-]*)?$' "s3://BUCKET[/PATH]"
        ask AWS_ENDPOINT "Object store endpoint, e.g. https://ACCOUNT.r2.cloudflarestorage.com"
        check AWS_ENDPOINT '^https://[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:[0-9]{1,5})?/?$' "an https origin"
        ask AWS_REGION "Object store region" auto
        check AWS_REGION '^[a-z0-9]+(-[a-z0-9]+)*$' "a region name such as auto or us-east-1"
    fi

    say "== render"
    local render
    if [[ "$variant" == template ]]; then
        env_dir=$kit/deploy/environments/phala-cloud-template/topup
        render=(--template)
    else
        env_dir=$ENVIRONMENT_DIR
        # A new CVM's gateway is known only once it is on a node: rendered again then, as by Deploy.
        render=(--gateway-domain gateway.pending.invalid)
        if ((!existing)); then
            mkdir -p "$env_dir"
            {
                echo "# Written by deploy.sh $release: the kit's phala-cloud-template routes and keyless"
                echo "# providers (Phala's staging), served at this domain (docs/configuration.md)."
                echo "environment: testnet"
                echo "public_origin: https://$DOMAIN"
                echo "admin_key:"
                echo "  id: admin/v1"
                echo "  public_key: $TOPUP_ADMIN_PUBLIC_KEY"
                sed -n '/^rpc_companies:/,$p' "$kit/deploy/environments/phala-cloud-template/topup/topup.yaml"
            } >"$env_dir/topup.yaml"
            cat >"$env_dir/compose.yaml" <<YAML
# Written by deploy.sh $release, as deploy/environments/example/topup/compose.yaml describes.
services:
  postgres:
    environment: &walg
      WALG_S3_PREFIX: $(yaml_string "$WALG_S3_PREFIX")
      AWS_ENDPOINT: $(yaml_string "$AWS_ENDPOINT")
      AWS_REGION: $(yaml_string "$AWS_REGION")
      AWS_S3_FORCE_PATH_STYLE: "true"
  backup:
    environment: *walg
  dstack-ingress:
    environment:
      DOMAIN: $DOMAIN
YAML
            say "wrote $env_dir: this instance's settings, no secret"
        fi
    fi
    local compose=$work/docker-compose.yml pinned_compose
    "$kit/deploy/render.sh" "${render[@]}" --images "$images" "$env_dir" >"$compose"
    pinned_compose=$("$kit/deploy/pinned-compose.sh")
    if [[ "$variant" == service ]]; then
        [[ "$("$pinned_compose" -f "$compose" config --no-interpolate --format json |
            jq -r '.services["dstack-ingress"].environment.DOMAIN')" == "$DOMAIN" ]] ||
            die "$env_dir does not serve $DOMAIN"
    fi

    say "== secrets (never printed)"
    # The compose's sealed names that have a value, which become the CVM's allowed_envs: an optional
    # one left empty is not sent, so it is unset. The template's DSTACK_APP_DOMAIN comes from the
    # pre-launch script.
    local env_file=$sealed/sealed.env name
    (umask 077 && : >"$env_file")
    for name in $("$pinned_compose" -f "$compose" config --variables | awk 'NR > 1 && NF > 0 { print $1 }' | sort); do
        case "$name" in
            DSTACK_APP_DOMAIN) continue ;;
            TOPUP_ADMIN_PUBLIC_KEY | WALG_S3_PREFIX | AWS_ENDPOINT | AWS_REGION) ;;
            SENTRY_DSN) ask_secret "$name" "Sentry DSN (optional)" "" ;;
            TOPUP_RPC_*_KEY) ask_secret "$name" "$name, its provider's API key (empty for a keyless URL)" "" ;;
            *) ask_secret "$name" "$name" ;;
        esac
        [[ -z "${!name}" ]] || printf '%s=%s\n' "$name" "${!name}" >>"$env_file"
    done

    say "== preflight (offline)"
    local preflight=("$kit/deploy/preflight.sh" --env "$env_file" --compose "$compose" --environment-dir "$env_dir"
        --offline)
    [[ "$variant" == service ]] || preflight+=(--template)
    docker pull --quiet "$(jq -er '."phala-pay"' "$images")" </dev/null >/dev/null
    "${preflight[@]}" </dev/null >&2
    "$kit/deploy/check-route-modes.sh" "$deployment_environment" "$compose" </dev/null >&2

    say "== deploy"
    local update=(--compose "$compose" --pre-launch-script "$kit/deploy/phala-cloud-pre-launch.sh"
        --no-public-logs --no-public-sysinfo)
    phala_cvm deploy "$work/deploy.json" --name "$CVM_NAME" "${update[@]}" -e "$env_file" \
        --instance-type tdx.medium --fs ext4 --kms phala --image dstack-0.5.9 --no-dev-os \
        --public-tcbinfo --secure-time </dev/null
    cvm_id=$(jq -er '.vm_uuid' "$work/deploy.json")
    printf '%s\n' "$cvm_id" >"$state"
    app_id=$(jq -er '.app_id' "$work/deploy.json")
    say "created CVM $cvm_id (app $app_id), recorded in $state; waiting for it to settle"
    [[ "$variant" == template ]] || url=https://$DOMAIN
    phala_cvm wait --unsealed "$cvm_id" </dev/null >"$work/cvm.json"
    local previous deployed
    if [[ "$variant" == service ]]; then
        # The node's gateway is attested configuration: upgrade the new CVM to the compose rendered
        # with it. No -e, so the sealed env stays.
        gateway=$(phala_cvm gateway-domain "$work/cvm.json")
        "$kit/deploy/render.sh" --gateway-domain "$gateway" --images "$images" "$env_dir" >"$compose"
        "${preflight[@]}" </dev/null >&2
        previous=$(jq -er '.compose_hash | ascii_downcase | ltrimstr("0x")' "$work/cvm.json")
        phala_cvm deploy "$work/deploy-gateway.json" --cvm-id "$cvm_id" "${update[@]}" </dev/null
        phala_cvm wait --unsealed "$cvm_id" "$previous" </dev/null >"$work/cvm.json"
        # The TXT record names the instance: the attestation of this compose, which the CVM booted,
        # carries its id in the event log (`cvms get` reports none).
        deployed=$(jq -er '.compose_hash | ascii_downcase | ltrimstr("0x")' "$work/cvm.json")
        say "waiting for the CVM to boot compose $deployed and attest it"
        phala_cvm attestation "$cvm_id" "$deployed" </dev/null >"$work/attestation.json"
        # Bound to this run's compose, as Deploy's verify-attestation.sh binds it: the attested
        # app-compose carries the rendered compose and the kit's pre-launch script, its hash is the
        # event log's, and the API's new compose hash must be that one, not any other update's.
        local attested
        mkdir "$work/attested"
        attested=$("$kit/deploy/attested-compose.sh" "$work/attestation.json" "$compose" "$work/attested") ||
            die "CVM $cvm_id attests another compose than this run's"
        [[ "$attested" == "$deployed" ]] ||
            die "CVM $cvm_id reports compose hash $deployed, not this run's attested $attested"
        instance_id=$(phala_cvm instance-id "$work/attestation.json")
        record=$(dirname -- "$env_dir")/docker-compose.$CVM_NAME.yml
        cp "$compose" "$record"
        answers="the DNS records resolve and $url/healthz answers: Deploy's upgrade with this
    release checks that, the attested compose, and the certificate evidence"
    else
        # The quick start is served at its gateway domain: no instance id to wait for.
        url=$(jq -er '"https://\(.app_id | ltrimstr("0x")).\(.gateway.base_domain)"' "$work/cvm.json")
        record=phala-cloud-template.yml answers="$url/healthz answers"
    fi
}

main "$@"
