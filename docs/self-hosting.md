# Self-hosting Phala Pay

Phala Pay is open-source, self-hosted software. An operator runs its own instance in its own
dstack confidential VM (CVM) on Phala Cloud, for its own merchants, each an account (`acct_…`)
that the operator creates. Phala runs an instance only for Phala Cloud and offers no hosted
service to third parties; its staging instance and the demo at [pay.phala.com](https://pay.phala.com/)
are Phala's own, on Sepolia and Base Sepolia. Nothing in an instance depends on Phala's: you deploy
a verified [release](https://github.com/Phala-Network/phala-pay/releases) of the software from a
repository of your own that holds only your settings, to your own Phala Cloud workspace, serve your
own domain, and hold your own admin key. There is no fork to maintain and no image to build. The
only shared piece is the forwarder factory, a permissionless contract at one address on every
chain.

This guide is the order of the steps, from nothing to a credited test deposit and on to
operations. The linked documents hold the detailed procedures and reference. Steps marked **HUMAN-ONLY** change a registry, Phala Cloud, a CVM, a contract,
DNS, or a secret, and are run by a person from their own machine, never by CI or an agent.

| Role | Who | Reads |
|---|---|---|
| Operator | you: runs the instance, creates accounts, decides live access, handles incidents | this guide, [deploy/README.md](../deploy/README.md), [runbooks](../deploy/runbooks/README.md) |
| Merchant | each account: its keys, treasuries, webhook endpoints, sweeps, refunds | [integration.md](integration.md) |

An operator may also be a merchant of its own instance, as Phala is for Phala Cloud; it then does
the merchant's steps with the account's keys, never with the admin key.

## One-command deploy

There are two ways to deploy a release, both to your own Phala Cloud workspace, and a third is
coming:

- **One command**, from your machine, below: a testnet quick start, or an instance on your own
  domain.
- **Your environment repository** (sections 2 to 4): Deploy provisions and upgrades the instance
  from your committed settings. Every instance with merchants ends up here, since upgrades run
  through it.
- **[The Phala Cloud template](#the-phala-cloud-template)** (coming): the same testnet quick start
  in one click from Phala Cloud's console.

```sh
curl -fsSL https://pay.phala.com/deploy.sh | bash              # the latest release
curl -fsSL https://pay.phala.com/deploy/v0.9.2.sh | bash       # a given release
```

pay.phala.com redirects to the release's `deploy.sh` asset on GitHub, which deploys that release.
This is an HTTPS bootstrap, as with rustup, Deno, Bun, or Homebrew: you trust pay.phala.com, its
TLS, and Cloudflare, which publishes it, as well as GitHub and Phala Pay's releasers, to hand you
the script. The checks it then runs cannot vouch for the script itself, and without the GitHub CLI
its `SHA256SUMS` check catches a corrupted download, not a substituted release.

**High-assurance path.** Download the script from the release, verify its build provenance
against Phala Pay's repository, its Release workflow at the tag, and the tag's commit, read it, and
run it with `--strict`, which refuses to go on unless the GitHub CLI verifies the release's
provenance (as `PHALA_PAY_REQUIRE_ATTESTATION=1` does):

```sh
version=v0.9.2 repo=Phala-Network/phala-pay
gh release download "$version" -R "$repo" -p deploy.sh
gh attestation verify deploy.sh -R "$repo" --deny-self-hosted-runners \
  --source-digest "$(gh api "repos/$repo/commits/$version" --jq .sha)" \
  --cert-identity "https://github.com/$repo/.github/workflows/release.yml@refs/tags/$version"
bash deploy.sh --strict
```

It needs
`curl`, `tar`, `jq`, bash 4.4, Node.js 22 and npm, Docker (preflight checks the configuration in the
release's image), [uv](https://docs.astral.sh/uv/) or pipx if it is to generate the admin key (with
the release's own Python SDK, `phala-pay==<version>`), and your
Phala Cloud login (with a release's locked CLI, `kit/deploy/phala login`: [section 2](#2-your-environment-repository), step 1) or
`PHALA_CLOUD_API_KEY`, which it never stores. It:

1. downloads the release into a private temporary directory, removed on exit, and verifies it:
   with the GitHub CLI 2.101 or later, logged in, as [Verify a release](#verify-a-release) does;
   without it, against `SHA256SUMS` only, which checks the download but not its provenance;
2. asks for an instance name (5 to 63 letters, digits, and `-`, as Phala Cloud requires), the
   admin public key (or generates the keypair, the seed written
   mode 0600 only to the file you name and never printed), the backup location and its token, an
   optional Sentry DSN, and either nothing, for the **quick start** (the template variant at Phala
   Cloud's gateway domain, testnet routes, its admin key and backup location in the CVM's env), or
   a **custom domain** (the service variant, every setting attested, from an environment directory
   it writes from the template's routes, or one of yours with its keyed RPC providers' keys);
3. refuses an instance name your Phala Cloud workspace already has (before it generates an admin
   key), or a workspace CVM list it cannot read, renders the compose with the
   kit's `render.sh`, runs `preflight.sh --offline` and `check-route-modes.sh`, and provisions the
   CVM with the kit's locked Phala Cloud CLI, pre-launch script, `--image dstack-0.5.9 --no-dev-os`,
   and Phala Cloud's KMS, sealing the secrets you gave from a mode 0600 file, on tmpfs
   (`$XDG_RUNTIME_DIR`) where your session has one and otherwise in the temporary directory,
   shredded where `shred` exists and removed on every exit. It records the new CVM's id in the
   environment directory's `cvm-id` (the quick start: `NAME.cvm-id` in the directory you run it
   from), waits for the CVM to settle with its compose, and for a custom domain then sets the
   node's gateway, as Deploy does, and waits for the attestation of that compose, which must carry
   the compose it rendered and match the compose hash Phala Cloud reports, and whose event log names
   the instance id of the TXT record. The CLI runs in an empty directory of its own, so a
   `phala.toml` where you run the script is never read;
4. prints the CVM id, the URL, the DNS records of a custom domain, and how to verify the
   attestation (section 5). If a step after the CVM's creation fails or times out, or you press
   Ctrl-C, it still prints the CVM id, the URL, and how to finish or remove the CVM.

`--non-interactive` takes the inputs from the environment instead (the script's header lists
them). A secret is written as `NAME=VALUE`, which the Phala Cloud CLI reads as dotenv does, so a
value with a `#`, a quote, or surrounding whitespace is refused before anything is deployed. A run
that finds a recorded `cvm-id` creates no other CVM: it prints how to finish that one with Deploy,
or to delete it and the file to start over. That state is a file where you run the script, or in the
environment directory you choose: run it again from the same directory, with the same environment
directory. A run that cannot see the file still creates no duplicate, since it refuses a name the
workspace already has. A provision proves nothing about the instance's health: it is accepted once `/healthz`
answers and you have verified the attestation, and for a custom domain once the DNS records
resolve and Deploy's upgrade with the same release has passed (section 4, step 5). Commit the
custom domain's environment directory to your environment repository and set `TOPUP_CVM_ID` to the
CVM id to run that upgrade; then onboard your first account (section 6).

## 1. Prerequisites

- **A Phala Cloud workspace** with an API key for each GitHub Environment you deploy
  (`production`, and optionally `staging`), capacity for a `tdx.medium` CVM each, and a node that
  offers the OS image `dstack-0.5.9` ([deploy/README.md, "OS image"](../deploy/README.md#os-image)).
- **An S3-compatible object store** for the encrypted WAL-G backups: a bucket or prefix per
  Environment that the other Environment's credentials cannot reach, and a read-write token for it.
  The environment directory sets its endpoint, region, and addressing (R2: `auto` and path-style).
- **Two RPC providers per chain**, from different companies, over HTTPS: an endpoint serves one
  chain, so each chain of your routes has two of its own. Public gateways rate-limit; use paid
  plans for a mainnet. The chain must carry the canonical Multicall3
  ([deploy/contracts/multicall3.json](../deploy/contracts/multicall3.json)).
- **A domain in DNS you control** for the API, for example `pay-api.example.com`: a CNAME and a TXT
  record per instance, not proxied. It becomes your `public_origin` and your merchants' service
  URL, so choose one you will keep: changing it later changes every merchant's configuration.
- **A GitHub repository of your own**, private or public, for your environment directory and a
  deploy workflow (section 2).
- **On the operator's machine**: the [GitHub CLI](https://cli.github.com/) 2.101.0, the version
  Deploy pins (to download and verify releases), Node.js and npm (the kit's locked Phala Cloud CLI,
  `kit/deploy/phala`), Docker (the dstack verifier and
  `topup config check` run in it), `jq`, `curl`, OpenSSL, [uv](https://docs.astral.sh/uv/) (the
  Python SDK's key tools), the AWS CLI (to list backups), and Foundry v1.8.3 (`cast`, for
  preflight's chain checks, and `forge` if a chain needs the factory deployed).
- **Optional: a Sentry project** for errors, alerts, and Crons monitors. Without a DSN the
  service reports nothing, and you have only `/healthz`, the admin API, and the chain.

## 2. Your environment repository

**HUMAN-ONLY, owner of the repository.** It holds your settings and nothing else; the software
comes from a release.

1. **Verify a release** ([Verify a release](#verify-a-release)), the latest of the
   [releases](https://github.com/Phala-Network/phala-pay/releases) as `$version`, and extract its
   deploy kit next to your repository, with its locked Phala Cloud CLI:

   ```sh
   mkdir kit && tar -xzf "release/phala-pay-deploy-$version.tar.gz" -C kit --strip-components=1
   npm ci --prefix kit/deploy/tools --ignore-scripts
   ```

   The kit is `LICENSE`, `deploy/`, and `docs/` of the release: the attested stack, `render.sh`,
   the policy, preflight, the verifiers, the example environment, and this guide.
2. **Create the admin key** on the machine that will keep it, one per Environment, with the
   release's own Python SDK. The seed never leaves that machine; the printed `public_key` and the
   key id go into the environment directory's `topup.yaml` (step 4):

   ```sh
   uvx --from "phala-pay==${version#v}" topup-sdk keygen --keyid admin/production-v1 --seed-out ~/phala-pay/admin.seed
   ```

3. **Environments.** In your repository's Settings > Environments: `production` and, for a
   pre-production instance with test routes only, `staging` (the only names Deploy accepts).
   Deployment branches: `main` only. The only secret is `PHALA_CLOUD_API_KEY`, the workspace's
   API key: the Environment's secret, or a repository secret if your repository is in another
   organisation than Phala Pay's (step 5). Workflows run on `ubuntu-latest` unless the repository variable `CI_RUNNER` names a
   runner label.
4. **The environment directory**, per Environment and target: copy the kit's
   `deploy/environments/example/topup` to `<Environment>/topup/` in your repository, fill it in,
   and commit it by pull request. Preflight refuses the example's placeholders.

   | File | Settings |
   |---|---|
   | `topup.yaml` ([reference](configuration.md#the-configuration-file)) | `environment` (the Sentry environment), `public_origin` (`https://` + your domain), `admin_key` (step 2), `rpc` (each payment and price chain's independent read/verify endpoints, with `{key}` in place of a sealed key, [deploy/README.md, "RPC providers"](../deploy/README.md#rpc-providers)), and `routes` (section 3) |
   | `compose.yaml` | `WALG_S3_PREFIX` (`s3://BUCKET/PATH`, empty and used by no other app), `AWS_ENDPOINT`, `AWS_REGION`, `AWS_S3_FORCE_PATH_STYLE`, dstack-ingress's `DOMAIN` (your domain), and one `TOPUP_RPC_<ID>_KEY` line per keyed provider |

   The kit renders it and the release's image checks it, so you can check it before committing:

   ```sh
   kit/deploy/render.sh --images images.json --gateway-domain gateway.example.net production/topup >/dev/null
   docker run --rm -i "$(jq -r '."phala-pay"' images.json)" topup config check /dev/stdin <production/topup/topup.yaml
   ```

5. **The deploy workflow**, `.github/workflows/deploy.yml` in your repository. It calls the
   release's [Deploy](../.github/workflows/deploy.yml) at the release, which verifies the release
   and runs the kit's scripts on your environment directory ([deploy/README.md,
   "Deploy"](../deploy/README.md#deploy)). Pin it by the release's commit SHA, as GitHub
   recommends for third-party workflows (`gh api "repos/Phala-Network/phala-pay/commits/$version"
   --jq .sha`); Deploy refuses to run at any commit but `version`'s (called at the `version` tag,
   it peels the tag to that commit). The only secret it reads is `PHALA_CLOUD_API_KEY`, which it
   declares:

   - **In Phala Pay's organisation** (Phala-Network, or an organisation of its enterprise): pass
     `secrets: inherit`, with the key as the Environment's secret. A called workflow's Environment
     secret resolves empty unless the caller inherits secrets
     ([actions/runner#4453](https://github.com/actions/runner/issues/4453)).
   - **In another organisation**, where GitHub does not support `inherit`: pass the key from a
     repository secret, as below. The calling job cannot run in an Environment, so it cannot pass
     an Environment secret. This is weaker than step 3's Environment: any workflow on any branch
     can read a repository secret, so anyone who can push a branch can read the key, while an
     Environment secret reaches only jobs from its deployment branches. Restrict it: give write
     access to the repository only to those who may deploy, and give the key a workspace that
     holds only this instance. A repository secret serves both Environments; for a workspace per
     Environment, store one secret each and pass `${{ inputs.environment == 'production' &&
     secrets.PHALA_CLOUD_API_KEY_PRODUCTION || secrets.PHALA_CLOUD_API_KEY_STAGING }}`.

   ```yaml
   name: Deploy
   on:
     workflow_dispatch:
       inputs:
         environment:
           type: choice
           options: [staging, production]
           required: true
         mode:
           type: choice
           options: [provision, upgrade]
           required: true
   permissions:
     contents: read
     attestations: read
   jobs:
     deploy:
       uses: Phala-Network/phala-pay/.github/workflows/deploy.yml@<the release's commit SHA> # v0.9.2
       with:
         version: v0.9.2
         environment: ${{ inputs.environment }}
         mode: ${{ inputs.mode }}
         environment_dir: ${{ inputs.environment }}/topup
       # In Phala Pay's organisation, `secrets: inherit` instead.
       secrets:
         PHALA_CLOUD_API_KEY: ${{ secrets.PHALA_CLOUD_API_KEY }}
         TOPUP_MAINTENANCE_PRIVATE_KEY_PEM: ${{ secrets.TOPUP_MAINTENANCE_PRIVATE_KEY_PEM }}
         SENTRY_DSN: ${{ secrets.SENTRY_DSN }}
   ```

   The Environment's variables are only deployment state: `PHALA_WORKSPACE` (the display name of
   the API key's workspace) and `TOPUP_CVM_ID` (empty until the first provisioning). Why every
   other setting is committed and attested is in
   [deploy/README.md, "Attested settings"](../deploy/README.md#attested-settings). Another CI
   system, or none, can run the same steps: they are the kit's commands, in the order Deploy runs
   them.

Your repository needs no other workflow. The contract and restore checks run in Phala Pay's own
repository ([Verify contracts](../.github/workflows/verify-contracts.yml) daily, the local
[Restore drill](../.github/workflows/restore-drill.yml) weekly); run the kit's
`deploy/contracts/verify-deployment.sh` against your providers whenever you add a chain.

## 3. Routes and contracts

A route is one chain and token that accounts quote on and are paid through, in one mode. Routes
are committed and attested: a new route is a pull request to your environment repository and a
Deploy `upgrade`, never a runtime setting.

- **The routes of Phala's staging** are test routes on Sepolia and Base Sepolia
  ([deploy/phala.md, "Staging routes"](../deploy/phala.md#staging-routes) lists them, with their
  tokens and faucets). Any instance can copy them from its
  [topup.yaml](../deploy/environments/phala-network/staging/topup/topup.yaml) for a first
  noncommercial instance with `environment: staging` (or `testnet`, `local`, `sandbox`) and the
  existing price-source opt-ins. PHA routes cannot be copied into a production configuration.
- **Your own routes** are items of `topup.yaml`'s `routes`, written as route files are. The fields
  and their defaults are in [architecture §14](architecture.md#14-configuration-and-deployment),
  and [examples/phala-cloud-usdt.yaml](../examples/phala-cloud-usdt.yaml) is a production-eligible
  mainnet route template. [examples/phala-cloud-pha.yaml](../examples/phala-cloud-pha.yaml) is
  **staging/noncommercial only**: PHA has no second Allowed independent price source. See
  [price sources](configuration.md#price-sources). The kit's example environment ships only
  Chainlink-priced USDC/USDT test routes, valid under `environment: production`.
  `topup config check FILE` in the release's image checks the file (section 2), and
  `config show FILE` prints it resolved.
- **What Deploy refuses:** a live route on a test network, a test route on a mainnet, any live
  route in `staging`, and a chain that [check-route-modes.sh](../deploy/check-route-modes.sh) does
  not list. A chain is added there, and to [networks.json](../deploy/contracts/networks.json) for
  the contract scripts, by a pull request to Phala Pay and ships in its next release. `chain.sanctions_oracle` remains parsed for N-1 rollback but is deprecated; N screens against
  verified OFAC SDN snapshots across all EVM chains.
- **Its RPC providers.** Require explicit `rpc` read/verify pairs and
  distinct endpoint hosts. Configure each endpoint URL, `sealed_key` and measured `max_log_blocks`
  in the attested public configuration; keep credentials sealed under their explicit key names. See [RPC configuration](../deploy/README.md#rpc-providers) and
  [the RPC runbook](../deploy/RPC.md) for startup checks, outage recovery and migration.

- **The contracts.** The `ForwarderFactory` has no owner, no roles, and no admin, and is deployed
  deterministically through the Arachnid proxy at `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747`,
  with its implementation at `0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9`, on every chain. Reuse it;
  A dual-source agreed code mismatch freezes that chain; disagreement or an unavailable endpoint
  keeps it not-ready. The API and other chains continue. An audited lift requires a fresh passing
  dual-source check. Check a chain with the
  kit's read-only `deploy/contracts/verify-deployment.sh --rpc NETWORK/a=URL_A --rpc NETWORK/b=URL_B`
  (`NETWORK` from `networks.json`, the URLs with their keys), which compares it with the release's
  reference deployment.
  Only where it is missing, deploy it (**HUMAN-ONLY**, a funded throwaway EOA) as in
  [deploy/CONTRACTS.md](../deploy/CONTRACTS.md); if anyone deployed it first, the broadcast sends
  nothing. It is deployed on Sepolia and Base Sepolia; mainnet deployment remains in
  [Phala's production plan](plan.md#before-mainnet).

## 4. Release and provision

1. **Pick the release.** Your workflow's `uses: …@<commit> # v<version>` and `version` name it (section 2, step 5);
   its images are Phala's, public on GHCR, and each run verifies them again. Read its notes and
   [verify it](#verify-a-release) yourself once.
2. **Provision.** Run your Deploy workflow with `environment: production` and
   `mode: provision`. It runs preflight, creates the CVM with Phala Cloud's KMS, verifies the
   attested compose, and prints the CVM id, the DNS records, and the sealing commands in its
   summary. It proves nothing about the CVM's health: the CVM only has to boot, and shows `error`
   until it is sealed; step 5 is the acceptance step. Set `TOPUP_CVM_ID` to the CVM id; if a later step of the run fails, set it anyway and
   continue with `upgrade`, never provision twice
   ([deploy/README.md, "Deploy"](../deploy/README.md#deploy)).
3. **Seal the secrets** (**HUMAN-ONLY**). The CVM waits for them: PostgreSQL initializes only after
   it can list the empty backup prefix, so Phala Cloud shows the CVM as `error` until then. Write
   `.env.production` (mode 0600) with exactly the rendered compose's sealed names, which the
   provision summary lists: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `SENTRY_DSN` (may be
   empty), and each `TOPUP_RPC_<ID>_KEY` your directory declares. With the release's kit, your
   repository at the deployed commit, the rendered compose from the run's artifact, and
   `PHALA_CLOUD_API_KEY` exported:

   ```sh
   docker pull <the compose's phala-pay image>   # preflight --offline checks the configuration in it
   kit/deploy/preflight.sh --env .env.production --compose docker-compose.production.yml \
     --environment-dir production/topup --offline
   kit/deploy/phala envs update "$TOPUP_CVM_ID" -e .env.production
   ```

   [Sealing the secrets](../deploy/README.md#sealing-the-secrets) has the rules; never change a
   setting this way.
4. **Create the DNS records** (**HUMAN-ONLY**) the summary lists: a CNAME from your domain to
   the node's gateway and a TXT `_dstack-app-address.<domain>` naming the instance, not
   proxied. dstack-ingress in the CVM then obtains a Let's Encrypt certificate itself, through
   port 443 (`tls-alpn-01`); the CVM holds no DNS credentials
   ([Custom domain](../deploy/README.md#custom-domain)).
5. **Upgrade once with the same release**, the acceptance step. Run Deploy with `mode: upgrade`:
   it requires the CVM running, waits for `<public_origin>/healthz`, and verifies the attestation
   and the certificate evidence, retrying it for up to 10 minutes after the DNS change.
   Backups have started once a WAL segment younger than two minutes is listed:

   ```sh
   aws s3 ls "${WALG_S3_PREFIX%/}/wal_005/" --endpoint-url "$AWS_ENDPOINT" | tail -1
   ```

6. **Sentry and egress** (**HUMAN-ONLY**), if you use them: the scrubbing settings, an alert, and
   an Uptime monitor on `/healthz` ([One-time setup](../deploy/README.md#one-time-setup-human-only-repository-owner),
   step 5); and outbound traffic restricted to the RPC hosts, price sources, object store, Sentry,
   DNS, the Phala Cloud platform, and public addresses on 443 and 80 for webhooks
   ([Attestation, ingress, and egress](../deploy/README.md#attestation-ingress-and-egress)).

## 5. Verify the attestation

**HUMAN-ONLY, verifier**, before creating any account, with the deployed release's verified kit,
the run's rendered compose, and `PHALA_CLOUD_API_KEY` exported. Deploy already did this; doing it
yourself is what makes it evidence:

```sh
export CVM_ID=$TOPUP_CVM_ID
kit/deploy/phala cvms get "$CVM_ID" --json > cvm.json
kit/deploy/phala cvms attestation "$CVM_ID" --json > attestation.json
APP_ID=$(jq -er '.app_id' cvm.json) && GATEWAY_DOMAIN=$(jq -er '.gateway.base_domain' cvm.json)
curl -fsS "https://${APP_ID#0x}-8090.$GATEWAY_DOMAIN/prpc/Info" > info.json
EXPECTED_OS_IMAGE_HASH="$(cat production/topup/os-image-hash)"
kit/deploy/verify-attestation.sh attestation.json info.json "$APP_ID" docker-compose.production.yml service "$EXPECTED_OS_IMAGE_HASH"
kit/deploy/verify-ingress-evidence.sh "<your domain>" "$APP_ID"
```

The official dstack verifier checks the TDX quote, TCB, event log, and OS image; the script then
requires the app id, the compose hash of exactly the rendered compose, the sealed env names, and
the policy of [compose-policy.jq](../deploy/compose-policy.jq), under which `dstack-ingress` on 443
is the only published port ([deploy/README.md](../deploy/README.md#attestation-ingress-and-egress)). Give your merchants the
app id and the compose hash (`jq -j '.compose_file' attestation.json | sha256sum`): they check
their webhook keys' attestation against them ([integration.md §5.3](integration.md#53-pin-your-accounts-webhook-keys)),
and each upgrade changes the compose hash.

## 6. Onboard your first account

Accounts are created only by the operator, through the admin API; there is no signup. Due
diligence is done offline; the service records only its reference, date, and reviewer.

1. **Admin helper.** Convert the seed to the PEM the signer uses, once, and load the
   [runbooks' `admin` helper](../deploy/runbooks/README.md#environment), in the kit's directory,
   with `BASE_URL` set to exactly `topup.yaml`'s `public_origin` (or signatures answer `401`) and
   `ADMIN_KEY_ID` to its `admin_key.id` (`admin/production-v1`):

   ```sh
   (umask 077 && { printf '302e020100300506032b657004220420'; tr -d '\n' < admin.seed; } |
     xxd -r -p | openssl pkey -inform DER -out admin.pem)
   ```

2. **Create the account** (**HUMAN-ONLY**, admin key holder) with `charges_enabled: false`; the
   answer holds the `acct_…` id and its first secret test key, shown only there
   ([Operator onboarding](../deploy/README.md#operator-onboarding), step 2).
3. **Hand over the key** through an encrypted channel to the recorded contact, and delete the
   answer. The merchant rolls it at once.

Live mode (`charges_enabled: true`, which returns the first live key), the per-account exposure cap
`max_unfinalized_credit`, pauses, and recovery keys are later admin calls in the same section.

## 7. The merchant's side

Each merchant does these steps with its own secret key, against your `public_origin`; the
[integration guide](integration.md) is its reference, and [deploy/README.md, "Merchant
setup"](../deploy/README.md#merchant-setup) the summary.

1. **Keys**: roll the first key, keep secret keys offline for administration, and run servers with
   a restricted key ([integration.md §5.4](integration.md#54-manage-and-roll-keys)).
2. **Webhook keys**: fetch `GET /v1/attestation?nonce=…` with a key of the mode, verify it against
   the app id and compose hash you gave it, and pin the account's public webhook keys
   ([§5.3](integration.md#53-pin-your-accounts-webhook-keys)).
3. **Treasury proof**, per chain and mode: an EOA signs an EIP-4361 challenge
   (`pay.treasuries.set_eoa(…)` in the Python SDK, or
   [deploy/sandbox/set-treasury.sh](../deploy/sandbox/set-treasury.sh)); a Safe, deployed on the
   chain with the `CompatibilityFallbackHandler`, signs it as a Safe message
   ([§1.6](integration.md#16-treasuries)). A test-mode treasury applies at once; a later live change
   waits 48 hours.
4. **Payment settings**, per mode: the chains and assets the merchant accepts, with
   `POST /v1/payment_settings` ([§1.9](integration.md#19-payment-settings)). A new account accepts
   nothing, so quotes, deposit addresses, and payments are refused until it is configured;
   `GET /v1/config` then lists what is offered.
5. **Webhook endpoint**: `POST /v1/webhook_endpoints` with the receiver's HTTPS URL, then
   `POST /v1/webhook_endpoints/{id}/test` ([§5.11](integration.md#511-webhook-endpoints-and-events)).
6. **Pins**: the account id, the factory and implementation, and the treasury of each chain,
   configured in the merchant's server; the SDKs recompute every address from them.

## 8. A first credited test deposit

With the committed Sepolia route, the account's test key, a Sepolia treasury, test-mode payment
settings accepting the route's asset (section 7), and a public HTTPS
URL for the webhook receiver (a tunnel will do), the reference product, from a clone of Phala Pay at
the release's tag, runs a merchant backend and one deposit end to end: it creates a quote, pays it from a Foundry keystore holding Sepolia ETH
(minting the test token), and waits for the verified `deposit.credited` webhook, about 30 seconds
after the payment. Write the configuration and run it as in
[deploy/sandbox/README.md, "Running the scenarios against a deployed service"](../deploy/sandbox/README.md#running-the-scenarios-against-a-deployed-service),
with `service_url` set to your `public_origin`:

```sh
uv run --locked --project sdk/python python deploy/sandbox/smoke.py --config sandbox.json
PYTHONPATH=deploy/product uv run --locked --project sdk/python python -m reference_product --config sandbox.json
```

The deposit is then `credited` in `GET /v1/deposits`, final about 15 minutes later, and swept when
the merchant flushes its forwarders ([Sweeping](../deploy/README.md#sweeping)). The same
directory's scenarios play late, partial, rejected, and refunded payments.

## 9. Going live

1. A reviewed pull request to your environment repository adding the live route and its chain's
   providers to `topup.yaml`, with the factory verified on its chain; then Deploy `upgrade`
   ([Deploy](../deploy/README.md#deploy)).
2. Your own sign-off of the limits: each route's `merchant` defaults and bounds, each account's caps, and
   `max_unfinalized_credit` ([architecture §14](architecture.md#14-configuration-and-deployment)), and a
   passed restore drill (section 10).
3. `charges_enabled: true` for each account you enable; the merchant then proves a live
   treasury, configures its live payment settings, and follows the
   [go-live checklist](integration.md#44-go-live-checklist).

## 10. Backups and restore

The CVM backs itself up: PostgreSQL archives WAL every minute and WAL-G takes base backups into
`WALG_S3_PREFIX`, encrypted with a key derived in the CVM from the app id. The same app id
derives the same key and database passwords, so a restore needs no secret; a new app can never
read the old app's backups, so never delete the app, and give a new app a new prefix. The
`topup-backup` Crons monitor alerts when WAL-G has not uploaded for over two minutes
([Backup age](../deploy/runbooks/backup-age.md)).

- **Restore**: a new instance of the same app boots the read-only restore-check variant, which
  fetches the newest backup, replays WAL, and verifies the result; you then resume it as the
  service. The service starts frozen until you have reconciled with every merchant
  ([RESTORE.md](../deploy/RESTORE.md), [Reconciliation after a restore](../deploy/runbooks/restore.md),
  [the merchant notice](../deploy/README.md#after-a-restore-the-merchant-notice)).
- **Drills**: `make restore-drill` locally (and weekly in CI), and a drill against your own
  backups in a throwaway instance ([Staging restore drill](../deploy/RESTORE.md#staging-restore-drill)).

## 11. Upgrades and operations

- **Upgrades.** A new release is a pull request to your repository that changes the release in your
  workflow, in both places (the `uses:` commit and `version`). Review its notes and what it changes
  in the attested compose (`git diff OLD_TAG NEW_TAG -- deploy/` in a clone of Phala Pay, or render
  your directory with both kits and diff), [verify it](#verify-a-release), merge, and run Deploy
  `upgrade`. An upgrade sends only the compose, so the sealed secrets stay; rollback is an upgrade
  to an earlier release, and a schema is never rolled back ([Deploy](../deploy/README.md#deploy)).
  Tell merchants the new compose hash. The OS image is fixed; moving to dstack 0.6 changes every
  derived key ([OS image](../deploy/README.md#os-image)).
- **Payment settings upgrades.** Complete any unfinished payment-settings cutover on 0.9.x
  before upgrading. Migration and startup refuse an incomplete cutover with `payment settings
  cutover is incomplete; upgrade through 0.9.x first`. Migration also refuses databases predating
  the cutover before applying any migrations. New databases start recording immediately after
  migrations; historical `legacy` revisions still govern the deposits bound to them.
- **Operations.** A production CVM has no SSH, logs, or database access: you work through Sentry,
  the admin API (daily report, deposit view, pauses, metrics), and the chain. Every alert names its
  runbook ([runbooks](../deploy/runbooks/README.md#alert-and-symptom-index)); RPC usage and cost
  are in [Measuring RPC usage](../deploy/README.md#measuring-rpc-usage), and communication in
  [Incident communication](../deploy/runbooks/incident-communication.md).
- **Security.** Report vulnerabilities in the software as in [SECURITY.md](../SECURITY.md);
  incidents of your instance are yours to handle and disclose.
- **Local rehearsal**, from a clone of Phala Pay. `make up`, `make cvm-rehearsal` (a
  staging-shaped artifact against Anvil and the dstack simulator, through sealing, onboarding, and
  one credited deposit with a tx-hash hint and hermetic price fixtures) and `make sandbox-local`
  run the same compose without Phala Cloud
  ([Local verification](../deploy/README.md#local-verification)).

## Verify a release

[deploy/verify-release.sh](../deploy/verify-release.sh) is the verification Deploy runs on every
deployment; run the same program before you adopt a release. It downloads the release's assets
into a directory and stops at the first failure:

1. the tag's commit must be in Phala Pay's `main` history;
2. the assets must match `SHA256SUMS`;
3. every asset (`images.json`, the kit, `phala-cloud-template.yml`, `deploy.sh`) and every image `images.json`
   names must have a GitHub build provenance attestation signed by
   [release.yml](../.github/workflows/release.yml) at the tag, on a GitHub-hosted runner, for that
   commit (`gh attestation verify --source-digest`).

```sh
version=v0.9.2   # the release you adopt
for tool in verify-release.sh deadline.sh; do
  gh api -H 'Accept: application/vnd.github.raw' \
    "repos/Phala-Network/phala-pay/contents/deploy/$tool?ref=$version" >"$tool"
done
bash verify-release.sh "$version" release     # prints the release's commit
```

**Rebuild instead of trusting the build.** From a clone at the tag (`git clone --branch "$version"
--recurse-submodules https://github.com/Phala-Network/phala-pay.git`), with Buildx v0.37.1:

- `phala-pay` and `phala-pay-reference-product` are reproducible: `make verify-image` builds
  `phala-pay` twice on the release's pinned BuildKit, with the tag's commit time, and prints its
  manifest digest, which equals `images.json`'s
  (`DOCKERFILE=deploy/Dockerfile.reference-product deploy/verify-image.sh` for the other).
- `postgres-walg` is not reproducible (apt and dpkg timestamps): it has provenance only. Review
  its [Dockerfile](../deploy/Dockerfile.postgres-walg) at the tag.
- The kit's tar is `git archive` of the tag's `LICENSE`, `deploy/`, and `docs/`:
  `gzip -dc "phala-pay-deploy-$version.tar.gz"` equals
  `git archive --prefix="phala-pay-deploy-$version/" "$version" -- LICENSE deploy docs`.

## The Phala Cloud template

Phala Cloud's Phala Pay template, coming to its template gallery, is a one-click **testnet quick
start**: the release's `phala-cloud-template.yml`, rendered by
`deploy/render.sh --template` from
[deploy/environments/phala-cloud-template](../deploy/environments/phala-cloud-template/topup)
([deploy/README.md, "The Phala Cloud template variant"](../deploy/README.md#the-phala-cloud-template-variant)).
It serves Phala's staging routes at the app's own gateway domain, with no custom domain or
dstack-ingress. Its deploy form cannot put values in the attested compose, so five values come
from the CVM's env and are outside the attestation: the admin public key
(`TOPUP_ADMIN_PUBLIC_KEY`), the public origin's host (`DSTACK_APP_DOMAIN`, from Phala Cloud's
reviewed pre-launch script), and the backup location (`WALG_S3_PREFIX`, `AWS_ENDPOINT`,
`AWS_REGION`). Whoever controls the workspace can change them without changing the compose hash.
topup and postgres-walg parse each strictly at startup and refuse to start otherwise, and the
policy allows a runtime value in no other place. A template instance also has no restore-check
path: its restore guarantees would rest on those unattested values
([deploy/README.md](../deploy/README.md#the-phala-cloud-template-variant)). For an instance with
merchants, deploy as this guide describes, where every one of them is attested.

Allow outbound HTTPS to `sanctionslistservice.ofac.treas.gov` and
`wc2h-sls-prod-public-published.s3.us-gov-west-1.amazonaws.com`. A host or publication contract
change fails verification and eventually holds negative decisions. Before N-1 rollback, pause
settlement on routes affected by active manual sanctions entries, which N-1 cannot read.
See the [sanctions runbook](../deploy/runbooks/sanctions-list.md).
