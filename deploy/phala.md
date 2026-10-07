# Phala's instance

Phala runs an instance only for Phala Cloud and offers no hosted service to others. Its
`staging` Environment (`https://pay-api-staging.phala.com`, on Sepolia and Base Sepolia) also runs
a reference product whose API serves the live demo on Phala's website,
[pay.phala.com](https://pay.phala.com/). This page records that setup and Phala's own policies.
Another operator needs none of it, and can run the reference product the same way for its own
rehearsals. The generic procedures are in the [deployment reference](README.md).

## API admission limits

API-key authentication that cannot acquire a database slot within 250 ms returns
`503 unavailable` with `Retry-After: 1`. See
[Service configuration](../docs/configuration.md#api-admission-limits) for the authentication
gate and protection model.

## Onboarding policy

Phala's instance onboards only Phala's own accounts, as in
[Operator onboarding](README.md#operator-onboarding): Phala Cloud's, which collects Phala Cloud's
own revenue, and the staging reference product's. It takes no third-party merchants, so live mode
(`charges_enabled`) is a decision about Phala Cloud alone. Phala, as the operator of this
instance, is responsible for its compliance, as every operator is for its own (architecture §15,
"Compliance").

## Staging routes

Staging serves six test-mode routes, three on Sepolia and three on Base Sepolia, all on the
deterministic factory ([Contracts](README.md#contracts)); any test key quotes on all of them, and
`GET /v1/config` lists each chain's assets:

| Route | Token | Pricing | Test tokens |
|---|---|---|---|
| `phala-cloud-sepolia-pha-usd` ([config](environments/phala-network/staging/topup/topup.yaml)) | test PHA `0x8F40e7E99678F44c88158f049E62817580ab113B` (`MockERC20`, 18 decimals) | volatile: Kraken `PHAUSD`, checked against Binance `PHAUSDT` × mainnet USDT/USD | `mint(address,uint256)` is public |
| `phala-cloud-sepolia-usdc-usd` ([config](environments/phala-network/staging/topup/topup.yaml)) | Circle's testnet USDC `0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238` ([Circle's list](https://developers.circle.com/stablecoins/usdc-contract-addresses), 6 decimals) | stablecoin: 1.00 while every fresh USDC/USD rate is within 1% | [Circle's faucet](https://faucet.circle.com) (Ethereum Sepolia) |
| `phala-cloud-sepolia-usdt-usd` ([config](environments/phala-network/staging/topup/topup.yaml)) | Aave's testnet USDT `0xaA8E23Fb1079EA71e0a56F48a2aA51851D8433D0` ([Aave's address book](https://github.com/bgd-labs/aave-address-book), 6 decimals) | stablecoin: 1.00 while every fresh USDT/USD rate is within 1% | Aave's faucet contract `0xC959483DBa39aa9E78757139af0e9a2EDEb3f42D`: `mint(token, to, amount)` is public, up to 10 000 per call |
| `phala-cloud-base-sepolia-pha-usd` ([config](environments/phala-network/staging/topup/topup.yaml)) | test PHA `0x1a6F260377e42ead1418C7C1afDFD5DE371A9284` (the same `MockERC20`, 18 decimals) | as on Sepolia | `mint(address,uint256)` is public |
| `phala-cloud-base-sepolia-usdc-usd` ([config](environments/phala-network/staging/topup/topup.yaml)) | Circle's testnet USDC `0x036CbD53842c5426634e7929541eC2318f3dCF7e` ([Circle's list](https://developers.circle.com/stablecoins/usdc-contract-addresses), 6 decimals) | as on Sepolia | [Circle's faucet](https://faucet.circle.com) (Base Sepolia) |
| `phala-cloud-base-sepolia-usdt-usd` ([config](environments/phala-network/staging/topup/topup.yaml)) | Aave's testnet USDT `0x0a215D8ba66387DCA84B284D18c3B4ec3de6E54a` ([Aave's address book](https://github.com/bgd-labs/aave-address-book), 6 decimals) | as on Sepolia | Aave's faucet contract `0xD9145b5F45Ad4519c7ACcD6E0A4A82e83bB8A6Dc`: `mint(token, to, amount)` is public, up to 1 000 000 per call, once an hour per recipient |

| Chain | `confirmations` | RPC providers | Sanctions oracle (a `MockSanctionsOracle`) |
|---|---|---|---|
| Sepolia (11155111) | `2`, Ethereum L1's default | `provider-a`, `provider-b` | `0x28A73f8235d966244210D9c49E34EDdA4fF9e1f6` |
| Base Sepolia (84532) | `3`, the OP-stack default: the payment's block and two more on the sequencer's unsafe head, credited about 7 s after paying (architecture §8) | `base-sepolia-a` `https://base-sepolia.gateway.tenderly.co`, `base-sepolia-b` `https://base-sepolia-rpc.publicnode.com`, both keyless | `0x8A0C93d85a05aD30741C193068abF2e5E16e7b35` |

Tether publishes no testnet USDT, so the USDT routes take Aave's, the testnet USDT of Aave's
markets on both chains. Aave's app no longer lists its Sepolia market, but both faucet contracts
stay public, and the demo's mint button calls them from the visitor's wallet. It is a plain ERC-20
whose `transfer` returns `true`; Tether's no-return `transfer` is covered by the contract tests and
the demo's end-to-end test. [examples/phala-cloud-usdt.yaml](../examples/phala-cloud-usdt.yaml) is
the mainnet template, and Tether's fee switch and blacklist are handled as
[USDT fee switch and blacklist](runbooks/usdt-issuer-controls.md) says. USDC moves one or two
transfers a block on both chains, so its routes set `backstop: addresses`, which puts each whole chain, PHA included, on transfer requests by recipient (architecture §8); the
RPC cost is unchanged while staging has fewer than 1 000 addresses ([Measuring RPC
usage](README.md#measuring-rpc-usage)). The head loop polls once per block on both chains: every
12 s on Sepolia, and every 2 s on Base Sepolia, whose route credits at a depth, so Base Sepolia's
provider A takes about six times Sepolia's head polls and per-block log requests (about 47 500
`eth_blockNumber` and 43 200 `eth_getLogs` a day, half a request a second each), and its other
calls are unchanged. `--head-poll-interval-s` overrides the interval for every chain.
Routes are attested config: adding or changing one is a PR and an `upgrade` of `topup` with
Deploy Phala's instance, never a reset.

### RPC providers

Staging's independent groups are configured in
[topup.yaml](environments/phala-network/staging/topup/topup.yaml)
([RPC providers](README.md#rpc-providers)). Every member is public and keyless; backups have
priority 10 and separate synthetic key budgets. Account budgets are shared across chains.

| Chain | Group | Primary | Backup |
|---|---|---|---|
| Sepolia | A (`provider-a`) | Tenderly (`https://sepolia.gateway.tenderly.co`) | Sentio (`https://sepolia.rpc.sentio.xyz`) |
| Sepolia | B (`provider-b`) | PublicNode (`https://ethereum-sepolia-rpc.publicnode.com`) | ethPandaOps (`https://rpc.sepolia.ethpandaops.io`) |
| Base Sepolia | A (`base-sepolia-a`) | Tenderly (`https://base-sepolia.gateway.tenderly.co`) | Sentio (`https://base-sepolia.rpc.sentio.xyz`) |
| Base Sepolia | B (`base-sepolia-b`) | PublicNode (`https://base-sepolia-rpc.publicnode.com`) | None (PublicNode singleton) |

The branch image's real `topup rpc check` verified the retained backups on 2026-10-03 PDT: chain id,
genesis agreement, canonical Multicall3, deployed factory/implementation, token/oracle code and
calls, latest/safe/finalized heads, receipts and recent logs. Both Sentio A backups also passed
address-less Transfer logs over an unsplit 2 000-block window. No route changed; the Phala Cloud
template's routes remain byte-identical.

Reviewed operator evidence for the exact configured endpoints:

| Operator | Endpoint evidence | Operator evidence |
|---|---|---|
| Tenderly | [Sepolia gateway](https://sepolia.gateway.tenderly.co), [Base Sepolia gateway](https://base-sepolia.gateway.tenderly.co) | [Tenderly supported networks](https://docs.tenderly.co/node-rpc/rpc-reference) |
| Allnodes (PublicNode) | [PublicNode's official endpoint list](https://www.publicnode.com/) lists `ethereum-sepolia-rpc.publicnode.com` and `base-sepolia-rpc.publicnode.com` | [PublicNode terms](https://www.publicnode.com/terms) name Allnodes Inc. as the service operator |
| Sentio | [Official Sepolia chain page](https://app.sentio.xyz/chain/sepolia) lists `sepolia.rpc.sentio.xyz`; [official Base Sepolia chain page](https://app.sentio.xyz/chain/base-sepolia) lists `base-sepolia.rpc.sentio.xyz` (both RPC URLs redirect to these pages on GET) | [Sentio's terms](https://www.sentio.xyz/terms/) name Sentio XYZ Inc. |
| Ethereum Foundation (ethPandaOps) | [ethPandaOps's official Sepolia page](https://sepolia.ethpandaops.io/) lists `rpc.sepolia.ethpandaops.io` | [Official ethPandaOps repository credits](https://github.com/ethpandaops/ethereum-package#credits) identify the Ethereum Foundation contributors |

The registry's domain suffixes and PSL registrable domains are distinct: `tenderly.co`,
`publicnode.com`, `sentio.xyz` and `ethpandaops.io`. Each company appears in only one group on
each chain. The PSL check prevents domain aliases; it is **not a substitute for endpoint
operator evidence or proof of upstream independence**. Pocket was removed: its
[official Supplier server documentation](https://docs.pocket.network/services/servers/) says requests are served by
third-party Suppliers, whose independence from PublicNode/Tenderly cannot be established.
Base Sepolia B remains a PublicNode singleton: Coinbase's official `https://sepolia.base.org`,
listed by [Base's network documentation](https://docs.base.org/get-started/connect-to-base),
failed the real branch-image probe at genesis (pruned history). It is not an eligible backup.

Other candidates were rejected by actual probes: dRPC Sepolia requires a paid plan; Ankr
requires a key; Coinbase and dRPC Base Sepolia prune genesis history; 1RPC cannot serve the
required log window. OnFinality repeatedly throttled complete probes even at one request per
second. Pocket Sepolia intermittently failed genesis reads (pruned history); ethPandaOps
passed three consecutive complete probes and replaces it. These rejected endpoints were not
added as usable backups.

Debug loops reproduced v0.7.0's failure in the second `eth_getBlockByNumber("finalized")`
probe: load-balanced gateways can return a lower finalized height a few hundred milliseconds
after the first answer (`stale`). Acceptance now uses one finalized snapshot for its numeric
capability checks. Runtime and readmission still enforce persisted floors and canonical hashes;
stale or mismatched evidence is never retried into acceptance. Same-height hashes in the
snapshot, persisted anchors and subsequent reads must agree. Bounded transient probe retries
also tolerate the separately observed `eth_call` oracle capability failures classified as
`throttled` on Base Sepolia. See [the RPC runbook](RPC.md#configuration-and-acceptance).

Mainnet needs paid providers from two different companies.

## Staging reset (HUMAN-ONLY)

A reset replaces staging with a fresh provision on an empty backup prefix, with every account
created again, instead of an upgrade, for a release that cannot read the old rows. Staging was last
reset at the v0.3.0 cutover ([#256](https://github.com/Phala-Network/phala-pay/pull/256)): its
schema requires every deposit's transaction origin and every deposit event's receipt position,
revision, and block (`20261023000000_current_invariants`), and its reference product creates its
ledger fresh. Nothing on staging is live, so no funds or merchants are affected; the new app id
derives new webhook keys (`whpk_…`) for every account. Every step is HUMAN-ONLY, by the staging
owner, except the workflow runs, which the owner dispatches; agents and CI run none of them. Run them from a checkout of `main` with `PHALA_CLOUD_API_KEY` of the `staging` Environment
exported and the release's verified kit in `kit/`:

```sh
version=v0.9.0   # the release deploy-phala.yml pins
bash deploy/verify-release.sh "$version" release
mkdir kit && tar -xzf "release/phala-pay-deploy-$version.tar.gz" -C kit --strip-components=1
npm ci --prefix kit/deploy/tools --ignore-scripts
```

Before the reset, a pull request points staging's
[compose.yaml](environments/phala-network/staging/topup/compose.yaml) at a new, empty prefix
(v0.3.0's: `s3://crypto-topup-test/staging-v030`); from then until the reset, never upgrade the old
staging CVM from `main`. The factory
`0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` and its implementation
`0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9` are deployed and verified on Sepolia and Base Sepolia
([Contracts](README.md#contracts)), and the staging Safe `0x26430107887d4a691B340BdB887096B83E7a5844`
(SafeL2 v1.4.1, 1-of-1, `CompatibilityFallbackHandler` v1.4.1) is the same address on both. Never
use `0x936c…4504` on Base Sepolia: a copy exists there whose owner key is destroyed.

1. **Record the old CVMs, stop them, and clear their variables.** Deploy refuses to provision
   while a CVM id variable is set. It names each new CVM after its run (`phala-pay-staging-<run
   id>`), so the stopped CVMs stay beside the new ones as the rollback until step 9:

   ```sh
   repo=(-R Phala-Network/phala-pay)
   OLD_TOPUP_CVM_ID=$(gh variable get TOPUP_CVM_ID --env staging "${repo[@]}")
   OLD_PRODUCT_CVM_ID=$(gh variable get STAGING_PRODUCT_CVM_ID --env staging "${repo[@]}")
   echo "$OLD_TOPUP_CVM_ID $OLD_PRODUCT_CVM_ID"   # keep these for step 9
   kit/deploy/phala cvms stop "$OLD_TOPUP_CVM_ID"
   kit/deploy/phala cvms stop "$OLD_PRODUCT_CVM_ID"
   gh variable delete TOPUP_CVM_ID --env staging "${repo[@]}"
   gh variable delete STAGING_PRODUCT_CVM_ID --env staging "${repo[@]}"
   ```

2. **Check that the new prefix is empty.** PostgreSQL initializes a cluster only on a prefix
   that provably holds no backup ([RESTORE.md](RESTORE.md#bootstrap-from-backup)); this lists
   nothing:

   ```sh
   aws s3 ls "s3://crypto-topup-test/<new prefix>/" --recursive \
     --endpoint-url https://1d694c298092ffa09c793cbca4587812.r2.cloudflarestorage.com
   ```

3. **Provision the service**, then record its CVM id from the run summary. The new CVM is unsealed
   and shows `error` in Phala Cloud until step 4: PostgreSQL refuses to start without the storage
   credentials, so app-compose fails, by design. If a run fails after its summary lists a CVM id,
   continue with that CVM as [Deploy](README.md#deploy) step 2 says; never provision twice:

   ```sh
   gh workflow run deploy-phala.yml "${repo[@]}" --ref main -f environment=staging -f target=topup -f mode=provision
   gh variable set TOPUP_CVM_ID --env staging --body "<the summary's CVM id>" "${repo[@]}"
   ```

4. **Reseal the service's secrets** with the commands the summary prints
   ([Sealing the secrets](README.md#sealing-the-secrets)): `.env.staging` (mode 0600) holds exactly
   the rendered compose's sealed names, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and
   `SENTRY_DSN`. Staging's providers are keyless, so it seals no `TOPUP_RPC_<ID>_KEY`, and the old
   CVM's two empty provider names are not sealed again.
5. **Switch DNS** for `pay-api-staging.phala.com` to the records the summary lists: the CNAME to
   the new node's gateway and the `_dstack-app-address` TXT to the new instance, DNS only
   ([Custom domain](README.md#custom-domain)).
6. **Upgrade once and verify**, the acceptance step (the provision proved nothing about health).
   Dispatch the same run with `-f mode=upgrade`: it requires the CVM running, waits for `/healthz`,
   and verifies the attestation and the certificate evidence. Then verify the
   attestation from your machine ([Attestation](README.md#attestation-ingress-and-egress)), and
   check that `GET /v1/config` with any test key lists the Sepolia and Base Sepolia assets.
7. **Re-onboard the accounts** ([Operator onboarding](README.md#operator-onboarding), steps 1–3,
   `charges_enabled: false`): the reference product's first, then each internal merchant's (Phala
   Cloud's staging backend), and send each contact its `acct_…` and key. Set up the reference
   product's account ([Staging reference product](#staging-reference-product), setup steps 1–3),
   which proves the staging Safe again as the account's treasury on each chain: the owners sign
   the new instance's challenge as a Safe message, verified by EIP-1271
   ([Treasury setup](README.md#treasury-setup)), since the new database holds no treasury, and
   configures its payment settings (setup step 3), since a new account accepts nothing.
   A pull request sets that new `acct_…` as `account` in
   [product/config.json](environments/phala-network/staging/product/config.json); merge it.
8. **Provision, reseal, and switch the reference product** (setup step 4): dispatch with
   `-f target=product -f mode=provision` (the new CVM's volume starts the product's ledger empty),
   `gh variable set STAGING_PRODUCT_CVM_ID --env staging`, seal `.env.product` with the new
   account's restricted key as `PRODUCT_API_KEY`, as the summary prints, switch the DNS records of
   `pay-demo-api.phala.com`, dispatch `-f target=product -f mode=upgrade`, and register its webhook
   endpoint. Run one deposit of each collection method (setup step 5) and one
   [sweep](README.md#sweeping) from the Safe; confirm `swept` and the daily report.
9. **Delete the old CVMs by their recorded ids** (never by name or app id), once the new service
   has run clean for a day. Their app is gone with them, so the old prefix can no longer be
   restored and may be deleted too:

   ```sh
   kit/deploy/phala cvms delete "$OLD_TOPUP_CVM_ID" --force
   kit/deploy/phala cvms delete "$OLD_PRODUCT_CVM_ID" --force
   ```

## Staging reference product

The product runs Starlette under a single supervised Granian worker with one runtime thread.
Granian 2.8.4's `backpressure=32` bounds **worker-accepted connections**, including incomplete
headers: its [accept loop](https://github.com/emmett-framework/granian/blob/v2.8.4/src/workers.rs#L1037)
acquires a semaphore permit **before** `accept()`, and its
[HTTP/1 connection handler](https://github.com/emmett-framework/granian/blob/v2.8.4/src/workers.rs#L670)
releases that permit only after the connection ends. This is distinct from the limit of 16
synchronous application handlers. Keep-alive is disabled. Accepted connections have a five-second
**total** header-read deadline and a 16 KiB header buffer; trickling bytes does not reset the timer.

The configured listen backlog is **128**, Granian 2.8.4's minimum, subject to the OS limit.
Connections beyond the worker's 32 permits wait in the kernel accept queue; TCP handshakes can
succeed there without Granian accepting a socket. The header deadline starts when Granian accepts
the connection, **not** while it is queued. On Linux the completed queue can hold backlog + 1;
further connection attempts can time out or be refused. The SYN queue is a separate OS resource.
There is no promise that only 32 clients can establish TCP connections or that queued clients
close within five seconds. The admission cap bounds worker sockets and header buffers, while the
finite OS queue absorbs excess connections without allocating more application resources. Header
deadlines release occupied permits even under continuous trickling. These limits bound resources,
but sustained saturation can still delay legitimate clients; they do not guarantee availability
against an unlimited connection flood.

Bodies are limited to 1 MiB; application requests have a 30-second deadline. SDK and demo outbound
HTTP exchanges share a 25-second handler budget, including response reads and pagination, and
disable automatic SDK retries so `Retry-After` cannot extend shutdown. A timed-out synchronous
handler retains its admission slot until it finishes.

On SIGTERM, the worker drains synchronous work before closing clients. Granian kills a worker
that cannot drain within 35 seconds, leaving margin inside Compose's 45-second stop grace period.
As with any forced process termination, a remote mutation may have completed without a response;
retry mutations with the same idempotency key. The sandbox's in-process server uses the same HTTP
limits but has no process supervisor; production must use the `serve` command.

Staging's reference product is a merchant like any other, with its own account, and a second CVM
running [product/reference_product](product/reference_product): `serve` mode is the webhook
receiver that applies every `deposit.*` snapshot by the balance rule (a deposit nets to
`amount − amount_refunded − amount_reversed` while `credited` or `reversed`; its tests are in
`product/tests`), an account API, and the API of the website's live demo, with a SQLite ledger;
`deposit` mode, run
from an operator's machine, plays a customer and signs the product's account API with a separate
driver key (`driver/v1`, the product's own authentication, not Phala Pay's).

- **Its key.** The sealed env holds only `PRODUCT_API_KEY`, the account's **restricted** test key
  (`ppay_rk_test_…`) with exactly the permissions it uses (a write grant includes its resource's
  read). It needs no `account.write`, `api_keys.*`, `treasury.*`, `endpoints.*`, or `events.read`:
  the webhook endpoint and the treasury are set once with the secret key, and the product sends no
  transaction.

  | Permission | What the product calls |
  |---|---|
  | `account.read` | `GET /v1/attestation` (pins the account's webhook keys; trust strip), `GET /v1/config` (the tokens the demo offers) |
  | `quotes.write` | `POST /v1/quotes`, `GET /v1/quotes/{id}` |
  | `deposit_addresses.write` | `POST /v1/deposit_addresses`, `GET /v1/deposit_addresses/{id}` |
  | `deposits.read` | `GET /v1/deposits`, `GET /v1/deposits/{id}` |
  | `refunds.write` | `POST /v1/refunds`, `POST /v1/refunds/{id}/mark_paid\|cancel`, `GET /v1/refunds` |
  | `sweeps.read` | `GET /v1/balance`, `GET /v1/sweeps` |
  | `forwarders.read` | `GET /v1/forwarders?sweepable=` (the flush the merchant signs) |

  Its preflight ([product/preflight.sh](product/preflight.sh)) accepts only a test key, restricted
  or secret. The account's secret key stays with the staging owner, offline.
- **Its pins.** The attested product config names its `account` (`acct_…`), the forwarder factory
  and implementation (the same on every chain), and, for each of its `chains` (Sepolia and Base
  Sepolia), the account's treasury there, from which the SDK recomputes every quote and
  deposit address before the product shows it; the placeholder `acct_000…` fails the online
  preflight until a PR sets the real id. It pins its account's test-mode webhook keys from the
  authenticated attestation at its `service_url`, fetched with `PRODUCT_API_KEY`, at startup or on the
  first webhook when the key is sealed later (until then it answers `503`, and topup retries).
- **Attested settings.** Every value is committed in its environment directory,
  [environments/phala-network/staging/product](environments/phala-network/staging/product):
  `config.json` holds `service_url` (staging topup's `public_origin`), `public_url`
  (`https://pay-demo-api.phala.com`, its [custom domain](README.md#custom-domain)), and
  `driver_public_key`. The overlay holds dstack-ingress's `DOMAIN`, and the CVM's gateway is
  Deploy's input. The config also commits each chain's `rpc_url`, a keyless public RPC (publicnode's; the product seals no RPC key,
  and its preflight refuses a keyed URL and checks online that each reports its chain and that the
  chain's treasury is a contract), its test tokens, `bonus_bps` (the demo merchant's own +10% on
  credits paid in PHA, a promotion, not a Phala Pay feature), and `web_origin`,
  `https://pay.phala.com`, the only origin the demo's API allows.
- **Its restore records.** Its ledger keeps what a service restore asks merchants for
  ([restore runbook](runbooks/restore.md) step 2): each verified delivery once per `webhook-id`,
  in the webhook inbox, as received (the raw body bytes and the `webhook-id`,
  `webhook-timestamp`, and `webhook-signature` headers) in the transaction that applies it; and
  each quote and deposit address it creates, as the service returned it, `client_secret`
  included. A client secret is a capability: the ledger file is its owner's alone (mode 0600),
  and nothing logs one. The export is the bodies of the operator's
  `POST /v1/admin/restore/treasuries/verify` (each treasury's latest object among its `treasury.*`
  deliveries), `/treasuries/apply` (each signed `treasury.updated` of a pending change becoming
  `active`), `/deposit_addresses`, `/quotes`, and `/events` requests (treasuries and events in
  batches of 100, deliveries in the order of their deposits' positions and revisions), without
  `reason`. `--since` takes the restore point (Unix seconds, `GET /v1/admin/restore`'s
  `restore.restore_point`) and keeps every record the service created or the product last
  recorded from five minutes before it on; `--output` writes a new mode-0600 file instead of
  stdout. On the staging CVM, the owner fetches it from its machine through the product's account
  API (`GET /accounts/restore-records?since=`), signed with the driver key of setup step 1, with
  `driver.json` of step 5 and no other secret:

  ```sh
  PYTHONPATH=deploy/product uv run --locked --project sdk/python python -m reference_product \
    fetch-restore-records --config driver.json --driver-seed-file ~/staging/driver.seed \
    --since <restore point> --output records.json
  ```

  Next to the ledger file (a local stack), `export-restore-records --config FILE` prints the same
  from the ledger, opened read-only. The controlled
  [restore drill](RESTORE.md#local-and-ci-drills) runs this receiver and imports what it fetches.
- **Its networks.** The page offers a configured chain only once the service serves assets there
  (`GET /v1/config`), so a new route appears with no product change.
- **Its custom domain.** The same pinned dstack-ingress as topup's
  ([Custom domain](README.md#custom-domain)) terminates TLS for the overlay's `DOMAIN` (Phala's:
  `pay-demo-api.phala.com`) in the product's compose and forwards to `product:8089`, so the demo's
  API, the product's webhook endpoint, and its account API are at `https://pay-demo-api.phala.com`.
  Phala's website, `pay.phala.com`, is not a CVM's: Cloudflare serves it ([Website](#website)).

The product serves the JSON API of the live demo on the public **Phala Pay website**
([pay.phala.com](https://pay.phala.com/), [product/web](product/web), served by Cloudflare:
[Website](#website)) at `<public_url>/api/`
([reference_product/demo.py](product/reference_product/demo.py)); it serves no page. The page
calls it cross-origin: the API answers CORS preflights and sends
`Access-Control-Allow-Origin: https://pay.phala.com` with `Access-Control-Allow-Credentials: true`
and `Vary: Origin` on every `/api/` response, errors included, and nothing of CORS to any other
origin; `/webhooks`, `/healthz`, and `/accounts` have no CORS. The page is a short headline, the
live demo, and the key properties. The demo
sets the product beside its backend (stacked on narrow screens): first, what the customer sees (a cloud console's
billing page, framed as the merchant's app); then what the merchant's backend sees (the
payment's live event stream, then tabs for payments, refunds, sweeps, API requests and webhooks,
and the attestation). The billing page has both ways to collect a payment: a **quote** (a locked price and an exact amount, paid through
`@phala/pay`'s `<Checkout expectedAddress>`) and the visitor's single **deposit address** (every
token on every network, any amount credited at spot, its payments read by the browser with the
address's `client_secret`). An order id set as `metadata` arrives in the `deposit.credited` event.
A timeline built only from real data (chain block times, the service's objects read with the
product key, this product's verified webhooks and ledger rows) shows each payment received, credited
(at two confirmations), final, and reversed if it is; the ledger follows the balance rule. Refunds
follow the merchant flow: declare (a final deposit), pay from the treasury of the deposit's
address, `mark_paid`, verified at finality; on staging that treasury is the finance Safe, so a
visitor who pays from their own wallet sees the verification fail (`sender_mismatch`). Sweeps are
the merchant's: the page shows the unswept balance, the `flush` call and the Safe Transaction
Builder batch the SDK builds, and the finalized sweeps; the product holds no wallet key. Each
browser gets a random demo account in a cookie of the API's origin (`HttpOnly; Secure;
SameSite=Lax; Path=/`, host-only: the two origins are same-site under `phala.com`, so the page's
credentialed requests carry it); quote creation is rate-limited per account (3 a minute, 20 a day)
and overall (30 a minute), POSTs must be JSON, and the page carries a strict CSP. Test PHA is
minted by the visitor's own wallet on the selected network (`mint` is public on the staging tokens);
test USDC comes from Circle's faucet, and gas from each testnet's public faucets. With `sdk/js` built, `cd product/web && pnpm run e2e` runs the whole flow on
Anvil, with the real factory at its deterministic address, against a stand-in service
([product/web/e2e/fake_service.py](product/web/e2e/fake_service.py)): it builds the page against
the local product and serves it from its own origin under the CSP of `public/_headers`, so the
demo runs cross-origin, with CORS and the cookie, as in production.

Setup, in order, during the [staging reset](#staging-reset-human-only)'s steps 7 and 8 (each step
**HUMAN-ONLY** unless it is a workflow run):

1. On the owner's machine (mode-0600 files, never committed), create the driver key, and commit its
   printed `public_key` as the product config's `driver_public_key` by PR:

   ```sh
   cd sdk/python
   uv run --locked topup-sdk keygen --keyid driver/v1 --seed-out ~/staging/driver.seed
   ```

2. **As the product's merchant**, with the account's first secret test key from
   [onboarding](README.md#operator-onboarding): roll it, then create the product's restricted key and keep
   its `secret` for step 4:

   ```sh
   curl -fsS "https://pay-api-staging.phala.com/v1/api_keys" -H "Authorization: Bearer $SECRET_KEY" \
     -H 'content-type: application/json' -d '{"name": "reference product", "type": "restricted",
     "permissions": ["account.read", "quotes.write", "deposit_addresses.write", "deposits.read",
     "refunds.write", "sweeps.read", "forwarders.read"]}'
   ```

3. **Treasury Safe owners**: set the account's treasury on each of its chains to the staging Safe
   ([Treasury setup](README.md#treasury-setup), Safe message; in test mode it applies at once).
   Then, with the secret key, accept every chain and asset the product offers in test mode
   ([Payment settings](README.md#payment-settings)); the product's restricted key can read them but
   not change them:

   ```sh
   curl -fsS "https://pay-api-staging.phala.com/v1/payment_settings" \
     -H "Authorization: Bearer $SECRET_KEY" -H 'content-type: application/json' -d '{"chains": [
       {"chain_id": 11155111, "assets": [{"asset": "pha"}, {"asset": "usdc"}, {"asset": "usdt"}]},
       {"chain_id": 84532, "assets": [{"asset": "pha"}, {"asset": "usdc"}, {"asset": "usdt"}]}]}'
   ```

   `GET /v1/config` with `PRODUCT_API_KEY` then lists the six assets. Open a PR
   setting the product config's `account` in
   [config.json](environments/phala-network/staging/product/config.json) to the new `acct_…` id,
   and merge it.
4. Run Deploy Phala's instance (`staging`, target `product`, `provision`), set `STAGING_PRODUCT_CVM_ID`, create the
   [DNS records](README.md#custom-domain) for `pay-demo-api.phala.com` the summary lists, seal `.env.product`
   holding `PRODUCT_API_KEY=<ppay_rk_test_…>` with the two commands it prints, and run it with
   `upgrade`, which waits for `https://pay-demo-api.phala.com/healthz` and verifies
   the certificate evidence. Then, with the secret key, register the product's endpoint:
   `POST /v1/webhook_endpoints {"url": "<public_url>/webhooks", "enabled_events": ["*"]}`
   and `POST /v1/webhook_endpoints/{id}/test`.
5. Run a deposit. The payer is a Foundry keystore with a throwaway key and some testnet ETH; the
   test PHA token is a `MockERC20` with a public `mint`, so the driver mints the quoted amount and
   pays it. `driver.json` holds the `ProductConfig` fields: `service_url` (topup's origin),
   `account`, `factory`, `implementation`, `chains` (as in the product config: each with its
   `chain_id`, `name`, `rpc_url`, `treasury`, and `test_tokens`), and `public_url` (the product
   URL). The driver pays on the first chain, with its first test token; `--chain-id 84532` pays on
   Base Sepolia instead.

   ```sh
   export ETH_KEYSTORE=~/.foundry/keystores/staging-payer ETH_PASSWORD=~/staging/payer.password
   PYTHONPATH=deploy/product uv run --locked --project sdk/python python -m reference_product deposit \
     --config driver.json --driver-seed-file ~/staging/driver.seed \
     --amount-minor <cents>
   ```

   The driver recomputes the quote's address before paying and exits 0 once the product has
   recorded exactly one credit and the verified `deposit.credited` webhook (about 30 seconds after
   paying, at the route's two confirmations). `--min-atomic` refuses a quote below that many atomic
   units and prints the `--amount-minor` needed; the quote must also fit the 500000-cent
   per-deposit and per-account caps (PHA below about $0.24). `--until swept` also waits until the
   merchant [sweeps](README.md#sweeping) the forwarder and the sweep is finalized. The deposit address is
   exercised from the demo page: pay any amount of test PHA to it.

`make cvm-rehearsal` runs this product CVM locally, with one deposit.

### Abnormal paths

The driver also plays the sandbox scenarios' abnormal payments against staging, each in a fresh
workspace, and checks the deposit state, the verified webhooks, and the product ledger
(architecture §7, §9, §15):

| Path | Options | Expected |
|---|---|---|
| underpayment | `--pay-bps 9700` | `credited` at spot for what arrived, then `swept`; the lock later expires |
| after the quote window | `--pay-after-expiry` | `quote.expired`, then `credited` at spot and `swept` |
| unsupported token | `--token T --until rejected` | once final, at the next reconciliation round, `rejected(unsupported_asset)`; the tokens stay in the forwarder; `TopupUnsupportedInflows` |
| refund | `--pay-bps N --until refunded --refund-to A` | a payment of N/10000 of the quote above `max_deposit_atomic` (200000 test PHA): `rejected(out_of_bounds)`, swept; once the deposit is final the driver requests a refund and waits while the treasury Safe's owners, as the merchant, pay it from the treasury of the deposit's address and attaches the transaction with `POST /v1/refunds/{id}/mark_paid`, until `succeeded` and one `deposit.refunded` |

Each row adds its options to the step-5 driver command: `T` is the Sepolia unsupported test token
`0x287E3577c66866a3F5Cb7a8Dac6761EB43608392`, `A` an address the staging owner controls, and the refund
row needs `--timeout 43200`. A mismatch between the expected and the observed outcome exits
non-zero with the reason.

## Website

`pay.phala.com` is the static build of [product/web](product/web), served by the Cloudflare Worker
`phala-pay-web` with static assets only (no Worker script) and deployed by **Cloudflare Workers
Builds**, connected to this repository, with Cloudflare's [`cf` CLI](https://github.com/cloudflare/cf)
pinned in [product/web/package.json](product/web/package.json). `main` deploys to production and
every other branch to its own [Worker Preview](https://developers.cloudflare.com/workers/previews/).
Each build runs once and its deploy only uploads it. Its dashboard build settings: root directory
`deploy/product/web` for both; for production the build command `npm run build:cloudflare` and the
deploy command `npm run deploy`; for the Previews Base the build command
`npm run build:cloudflare:preview` and the deploy command `npm run deploy:preview`. A branch's
Preview copies the Previews Base when it is created, so changing the Base leaves existing Previews
on their old settings until each is edited too.

- **Build.** `build:cloudflare` builds `@phala/pay-react` and its workspace dependencies
  (`@phala/pay`) in dependency order, then the page, each from its own lockfile with
  `npx -y pnpm@12.6.0`, on the Node of
  `product/web/.node-version` (24, as CI). The Cloudflare Vite plugin writes the page as cf's
  Build Output, in `product/web/.cloudflare/output`. The page's API origin is fixed at build time:
  `VITE_DEMO_API_ORIGIN` in `product/web/.env.production`, `https://pay-demo-api.phala.com`.
- **Production.** `deploy` runs `cf deploy --prebuilt`, which uploads that Build Output and
  deploys it. CI checks the config and the Build Output with `cf deploy --prebuilt --dry-run`.
- **Previews.** `build:cloudflare:preview` runs `build:cloudflare` as a Preview build
  (`CLOUDFLARE_PREVIEW_BUILD=true`, which `cf previews deploy --prebuilt` requires; the Build
  Output then records `isPreview` and leaves out the custom domain), and `deploy:preview` runs
  `cf previews deploy --prebuilt`, which uploads it and creates or updates the Preview named after
  the branch (`WORKERS_CI_BRANCH`, set by Workers Builds). Its Preview URL,
  `https://<branch slug>-phala-pay-web.phala-dev.workers.dev`, always serves the branch's latest
  build; each deployment also has its own URL. Its demo API calls are refused by CORS by design:
  the API allows only `https://pay.phala.com`.
- **[cloudflare.config.ts](product/web/cloudflare.config.ts).** The Worker's name and
  compatibility date; any path but the page and its assets is a real `404`
  (`notFoundHandling: "none"`); the custom domain `pay.phala.com`, for production only (cf rejects
  custom domains in a Preview, so the config leaves them out when `isPreview`); no `workers.dev`
  copy of the production site (`workersDev: false`); and Preview URLs on (`previewUrls: true`) for
  production as well as Previews: a branch's Preview gets `workers.dev` URLs only when the Worker
  itself has Preview URLs enabled (`cf previews deploy` returns none otherwise). The cost is a
  `workers.dev` URL per production version, which serves the same public page with `noindex`.
- **[public/_headers](product/web/public/_headers).** Cloudflare's static-assets headers, served in
  production and in every Preview: the page's CSP (`connect-src` names only the demo API and
  `pay-api-staging.phala.com`, whose public quote and deposit address views the SDK components
  read), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `no-cache` for the
  page, a year's immutable caching for the content-hashed `/assets/*`, a day's caching for the
  fixed-name icons, manifest, link preview image, `robots.txt`, and `sitemap.xml`, and
  `X-Robots-Tag: noindex` on `workers.dev`. `vite preview` serves the Build Output in the Workers
  runtime with these headers, as the end-to-end tests do.
