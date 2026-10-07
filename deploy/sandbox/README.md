# Integrator sandbox

The sandbox lets a merchant integrate before live mode
([architecture §12](../../docs/architecture.md#12-api-and-events)): Sepolia, a test token, a
test-mode account, and scripted late, under, over, rejected, and refused payment scenarios driven
through the reference product (`deploy/product`). The same scenarios run against a disposable
local stack (`make sandbox-local`) and, unchanged, against production test mode with a test key.
There is no separate integrator sandbox: integrators use their operator's production test mode
(design §9), and an operator's staging, if it runs one, stays internal pre-production.

Every step that deploys contracts, changes a CVM, or creates an account with an admin key is
marked **HUMAN-ONLY**; agents and CI never run them.

## Contents

| File | Purpose |
|---|---|
| `routes/sandbox-sepolia.template.yaml` | Capped test-mode route (chain id 11155111) for the local stack or an internal Sepolia sandbox; any test-mode account quotes on it. |
| `render-route.sh` | Renders the template from environment variables; refuses leftover placeholders. |
| `set-treasury.sh` | Proves a test EOA as an account's treasury on a chain (`POST /v1/treasuries/challenge`, `personal_sign`, `POST /v1/treasuries`). |
| `deploy-test-contracts.sh` | Deploys the test token (`MockERC20`, public `mint`), a second token for the unsupported-asset scenario, and `MockSanctionsOracle`. |
| `docker-compose.local.yml`, `run-local.sh` | Local stack (the attested compose with the `deploy/local` overlay, plus Anvil) and the end-to-end driver. |
| `scenarios/docker_restart.py` | The local `restart_command`: restarts the service container through the Docker API. |
| `scenarios/` | Scripted scenarios; `run.py` runs them against any configured stack. |

## Run everything locally

Requirements: Docker Compose, Foundry v1.8.3 (`forge`, `cast`), `jq`, `uv`, `curl`, and OpenSSL 3.

```sh
make sandbox-local                          # examples, then every scenario
deploy/sandbox/run-local.sh happy_path      # examples, then selected scenarios
```

The script builds the images, starts PostgreSQL, the dstack simulator, and an Anvil chain with
Sepolia's chain id (one-second blocks, `finalized` eight blocks behind), deploys the forwarder
factory and the sandbox test contracts, creates a throwaway admin key, renders and validates the
route with a 45-second rate-lock window, and starts the service. It then plays operator and
merchant: creates the sandbox account through `POST /v1/admin/accounts` (test mode only; the answer
carries its first test key), proves the Anvil owner as its test-mode treasury (`set-treasury.sh`),
and registers the reference product's webhook endpoint with that key. Last it runs the smoke check
`deploy/sandbox/smoke.py`, the reference product with one deposit driven through it, and then
`scenarios/run.py`. They run in a pinned uv/Python 3.14 container on the compose network, where the service
reaches the product endpoints as `http://product:8089`; this works even where a host firewall drops
traffic from containers to the host. `restart_mid_flow` runs last in its own container, the
only one given the Docker socket, which it uses to restart the local service
(`scenarios/docker_restart.py`). Prices come from
the live Chainlink, Binance, and Kraken endpoints, as in production. All containers, volumes,
and temporary files are removed on exit.

## Scenarios

Each scenario registers fresh workspaces, pays with the test token, and asserts the deposit state
from the product API, the verified webhooks, and the product ledger of the reference receiver.

| Scenario | Payment | Expected |
|---|---|---|
| `happy_path` | Exact quoted amount, then a second payment to the same address | The quote shows the payment (`payment.status` `seen`, or `recorded` once recorded at two blocks) with `matches_quote`; then `credited` at the quoted price with exactly the quoted credit, and the quote `complete`; the second payment `credited` at spot; the payer's view by `client_secret` shows the payment; `deposit.credited` delivered; one ledger credit each. |
| `late_payment` | Exact locked amount after `quote.expired` | `credited` at spot; lock stays `expired`. |
| `underpayment` | 97% of the locked amount (tolerance is 1%) | `credited` at spot below the quote; lock not consumed; cancel is refused (`400 quote_payment_received` while the quote is open). |
| `overpayment` | +0.5%, then +5% on a second lock | Within tolerance: lock price, exact quoted credit, the quote `complete`. Beyond: spot for the full amount, that lock not completed. |
| `unsupported_asset` | A token without a route to a quote's address | `rejected`, `deposit.rejected` reason `unsupported_asset`; the product never receives `deposit.credited`. |
| `product_refusal` | Payment for a suspended workspace | Deposit `credited`; the product holds it without a ledger credit (`account_suspended`) and requests its refund, which stays `pending` until the merchant pays it and marks it paid. |
| `restart_mid_flow` | The product fulfills the credit but its acknowledgement is lost, then the service restarts | The restarted service delivers the same `deposit.credited` again; exactly one ledger credit. Needs `restart_command`, so it is skipped against a deployed service. |

They cover, from the service side: a refusal is a hold and a refund request, delivery is retried
until the product acknowledges it, and a deposit is credited once.

## Obtaining test-mode credentials (integrators)

Integrators test in production test mode, with the same service, API, and attestation as live
mode; only the key selects the mode (design §9). Account creation is a human step on both sides.

1. Send the operator, through the agreed support channel: your company's name, a security contact
   (name and email), and what its due diligence asks for (done offline, design D8).
2. **HUMAN-ONLY, operator:** creates your account (`acct_…`) with `POST /v1/admin/accounts` and
   `charges_enabled: false`, as [Account credentials](../README.md#account-credentials) describes,
   and sends your contact, through an encrypted channel, the account id and its first test secret
   key (`ppay_sk_test_…`). Roll the key at once (`POST /v1/api_keys/{id}/roll {"expires_in": 3600}`,
   then revoke it with the new one), keep the new one in a mode-0600 file, and use it only for
   test mode; run production with a restricted key
   (`ppay_rk_test_…`) holding only the permissions it needs.
3. With the key, and no further help from the operator: pin your account's test-mode webhook keys
   from the attestation (docs/integration.md §5.3) and the forwarder factory and implementation
   (the deterministic addresses of [CONTRACTS.md](../CONTRACTS.md)); prove your testnet treasury (below); accept the test assets with
   `POST /v1/payment_settings` ([payment settings](../../docs/integration.md#19-payment-settings));
   then read the effective assets (`chain_id`, token `contract`, `quote_ttl_seconds`) from
   `GET /v1/config` and register your webhook receiver (`POST /v1/webhook_endpoints`,
   [webhook endpoints](../../docs/integration.md#511-webhook-endpoints-and-events)). New accounts
   accept nothing until their payment settings are configured.

Live mode is a later decision of the operator, on the same admin endpoint, which returns your first
live key. Production test mode needs a Sepolia route in the production deployment; ask the operator
for its name until `GET /v1/config` lists it.

Test tokens are free where the token's `mint(address,uint256)` is public, as on the sandbox's
`MockERC20` and the staging PHA token. You also need Sepolia ETH for gas from a public faucet.

## Deploying an internal Sepolia sandbox (operators)

Integrators never get a sandbox deployment of their own. An internal Sepolia sandbox, a CVM with the
sandbox-only contracts (a mintable test token, a second token for `unsupported_asset`, and
`MockSanctionsOracle`), is for the operator's own rehearsals.

1. **HUMAN-ONLY:** deploy the forwarder factory on Sepolia as
   [deploy/CONTRACTS.md](../CONTRACTS.md#sepolia) describes (it is deterministic: skip it if the
   factory already has code there), then
   the sandbox-only contracts, signed by a Foundry keystore account:

   ```sh
   cast wallet import sandbox-deployer --interactive   # once, with a funded throwaway key
   ETH_PASSWORD=/path/to/0600-password-file deploy/sandbox/deploy-test-contracts.sh \
     --rpc-url "$SEPOLIA_RPC_URL" --account sandbox-deployer
   ```

2. Render and validate the sandbox route. `PRODUCT_SLUG` only names the route
   (`sandbox-<slug>-tpha-usd`); routes are shared by every account of their mode, so one route
   serves every test-mode account of the sandbox:

   ```sh
   FORWARDER_FACTORY=0x... TEST_TOKEN=0x... \
     SANCTIONS_ORACLE=0x... PRODUCT_SLUG=acme \
     deploy/sandbox/render-route.sh > sandbox-acme.yaml
   docker run --rm -i "$PHALA_PAY_IMAGE" topup route validate /dev/stdin <sandbox-acme.yaml
   ```

3. Write the sandbox's environment directory: a copy of
   [deploy/environments/example/topup](../environments/example/topup) with the sandbox's values.
   Its `topup.yaml` holds its own `public_origin` (the sandbox's custom domain, for example
   `https://sandbox.topup.example`, with the DNS records of
   [Custom domain](../README.md#custom-domain)), its admin key, its sealed read/verify Sepolia endpoints as
   `provider-a` and `provider-b`, and the rendered route as its only `routes` item. Its
   `compose.yaml` holds its own backup prefix, and the domain as dstack-ingress's `DOMAIN`. Then
   render it as Deploy renders an Environment, with a release's `images.json` and the sandbox
   CVM's gateway (`$SANDBOX_GATEWAY_DOMAIN`, `gateway.<base domain>` of its node):

   ```sh
   deploy/render.sh --images images.json --gateway-domain "$SANDBOX_GATEWAY_DOMAIN" \
     sandbox-environment >sandbox-compose.yml
   ```

4. **HUMAN-ONLY:** deploy or update the sandbox CVM with `sandbox-compose.yml` exactly as the
   staging procedure in `deploy/README.md` describes, with a separate encrypted environment that
   holds only the sandbox's own secrets (the compose's sealed names). The admin API verifies
   `@target-uri` against the sandbox's `public_origin`, so a wrong value makes every admin request
   fail with `401`.
5. **HUMAN-ONLY, sandbox admin key holder:** create a test account with `POST /v1/admin/accounts`
   against the sandbox's `public_origin`, exactly as
   [Account credentials](../README.md#account-credentials) describes, with
   `charges_enabled: false`; the request is audited. Then run `deploy/sandbox/smoke.py` and the
   scenarios with its test key, as below.

## Running the scenarios against a deployed service

Write a configuration file; the fields are those of `ProductConfig` in
`deploy/product/reference_product/config.py`:

```json
{
  "service_url": "https://pay-api.example.com",
  "account": "acct_…",
  "api_key_file": "/home/me/acme-test.key",
  "factory": "0x...",
  "implementation": "0x...",
  "chains": [
    {
      "chain_id": 11155111,
      "name": "Sepolia",
      "rpc_url": "https://ethereum-sepolia-rpc.publicnode.com",
      "treasury": "0x...",
      "test_tokens": [{"symbol": "PHA", "address": "0x..."}]
    }
  ],
  "unsupported_token": "0x...",
  "listen_host": "127.0.0.1",
  "listen_port": 8089,
  "public_url": "https://acme.example/topup",
  "payer_account": "sandbox-payer"
}
```

- `service_url` is the operator's service URL (its `public_origin`), `account` your `acct_…`
  id, and `api_key_file` holds your test key.
- `chains` lists the networks the product takes payments on (the smoke example, the scenarios,
  and the deposit driver use the first; the driver's `--chain-id` picks another): each with its
  display `name`, a keyless public `rpc_url` (https; no key in the URL, since the product's
  config is published), and its mintable `test_tokens` (symbol and address; the first is the one
  paid with). `factory` and `implementation` are the same on every chain.
- Each chain's `treasury` is your account's test-mode treasury on it, which quotes need. Set it once
  through the API with an EIP-4361 proof (design D10): from a test EOA key,
  `deploy/sandbox/set-treasury.sh --api https://pay-api.example.com --key-file
  ~/acme-test.key --chain-id 11155111 --private-key 0x…` requests the challenge, signs it,
  and submits it; a Safe signs the challenge as a Safe message instead. Test-mode treasuries
  apply at once.
- `unsupported_token` is any Sepolia token the route does not accept (for `unsupported_asset`,
  which is skipped without it).
- `listen_host` and `listen_port` are where the reference endpoint listens; `public_url` is the
  HTTPS URL of the webhook endpoint you register (`POST /v1/webhook_endpoints`), forwarded to it by
  your tunnel or reverse proxy. The endpoint verifies signatures against `public_url`, never the
  incoming `Host` header.
- `payer_account` is a Foundry keystore account (`cast wallet import sandbox-payer --interactive`)
  holding a throwaway test key with Sepolia ETH; instead of it, `ETH_KEYSTORE` may name the
  keystore file. Export `ETH_PASSWORD` as the path of a mode-0600 file holding its keystore
  password (read as a password file, as Foundry does). `payer` (an unlocked address) is only for
  Anvil.
- Without a mode the reference product runs the product (`serve`) and one deposit (`deposit`) in
  one process; the two modes also run separately, as for staging (deploy/phala.md, "Staging
  reference product").
- Set `webhook_public_keys` (`whpk_…`, current first) after verifying the attestation quote; otherwise
  the product fetches `GET /v1/attestation` with its API key, checks only its binding to the
  nonce, account, mode, and keys, and warns.

Then run:

```sh
uv run --locked --project sdk/python python deploy/sandbox/smoke.py --config sandbox.json
PYTHONPATH=deploy/product uv run --locked --project sdk/python python -m reference_product --config sandbox.json
uv run --locked --project sdk/python python deploy/sandbox/scenarios/run.py --config sandbox.json
```

Sepolia deposits are credited about 30 seconds after paying (the route's default confirmation,
two blocks), but quote expiry and sweeps follow finality, about 15 minutes, so a full run still
takes a few hours; pass scenario names to run a subset. The late payment scenario waits at least
the route's quote window (`quote_ttl_seconds`; 120 seconds on the sandbox route).
`restart_mid_flow` is reported as `SKIP` without a `restart_command`.

Phala's staging credits its own reference-product CVM and stays internal, so the scenarios do not
run there; the deposit driver's options play their payments through that product instead
(deploy/phala.md, "Abnormal paths").
