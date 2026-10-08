# Deterministic contract deployment

The factory is permissionless and deterministic, so every Phala Pay instance on a chain uses the
same one: an operator reuses it wherever it is deployed and runs this runbook only on a chain where
it is missing ([self-hosting guide](../docs/self-hosting.md#3-routes-and-contracts)). No signing
key is stored in CI: every funding or broadcast command below is **HUMAN-ONLY**, run by a deployer
with their own key, and the Verify contracts workflow re-checks the deployment daily. Mainnet is
never deployed from automation.

The Arachnid deterministic deployment proxy is fixed at
`0x4e59b44847b379578588920cA78FbF26c0B4956C`. Its calldata is the plain 32-byte salt followed by
the complete init code. The upstream proxy itself is deployed by funding the one-time signer
`0x3fab184622dc19b6109349b94811493bf2a45362` and publishing the upstream signed legacy
transaction. These facts and the embedded transaction were checked on 2026-09-22 against upstream
commit `be3c5974db5028d502537209329ff2e730ed336c`.

The factory salt is fixed and unmodified:

```text
keccak256("phala-pay.ForwarderFactory.v2")
= 0x26f1d8427b0c2db52d02ee55402198e592a278fb8541ba4dfaefbd1ea7b09eee
```

`ForwarderFactory` has no constructor arguments, no roles, and no admin
([design D3](../docs/design/multi-tenant.md#d3-contracts)). Its init code is the build's creation
code, so the CREATE2 address depends only on the build and the salt: the factory is
`0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` and its implementation (the factory's first CREATE)
`0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9` on every chain (`local-test-vectors.json` records
both for the committed build). Anyone can deploy it; a treasury is chosen per forwarder, not per
factory, so a treasury change never needs a new factory.

## Prerequisites

- Foundry `1.8.3`, `jq`, and the repository dependencies.
- Two RPC providers for each chain used by post-deployment verification.
- A funded deployment EOA. Keep `PRIVATE_KEY` only in the operator's environment or secret manager.

`deploy/contracts/networks.json` maps each target network name to its chain id; Sepolia, Base
Sepolia, and mainnet are prefilled. Targets are written `NETWORK[/LABEL]=URL`; `NETWORK` selects
the expected chain id, which the RPC's `eth_chainId` must report, and the optional label
distinguishes providers in the report.

### Treasury Safe

Route files name no treasury: treasuries are the accounts' own, each proven per chain and mode
through the API by its merchant with an EIP-4361 (EOA) or EIP-1271 (Safe) signature, and every
forwarder commits to the treasury it pays
([design D10](../docs/design/multi-tenant.md#d10-treasury-proof-and-changes);
[Treasury change](runbooks/treasury-change.md)). The service never checks a Safe's configuration.
`verify-safe.sh` remains a check of one Safe: the `treasury` in
`deploy/contracts/safe-expectations.json`. In this repository that is Phala's finance Safe, which
Phala's finance proves as the treasury of Phala Cloud's account; an operator or merchant that wants
the same check of its own Safe records that Safe in that file of its own checkout. Run it before
that proof and whenever the Safe's owners change (the Verify contracts workflow runs it daily on Sepolia). On every target it checks: the RPC's
`eth_chainId` equals the committed chain id for the target network, the address has code (an EOA is rejected), the proxy
runtime code hash is approved, storage slot 0 and `masterCopy()` both equal the approved singleton,
the singleton's runtime code hash matches, owners match as a set, the threshold matches exactly,
the enabled modules (`getModulesPaginated`) match as a set, and the guard and fallback handler
storage slots of Safe v1.4.1 hold exactly the approved addresses. The singleton check matters
because every Safe proxy has the same runtime code; only slot 0 decides which implementation
answers `getOwners()` and `getThreshold()` and executes transactions. Modules, the guard, and the
fallback handler matter because a module can execute from the Safe without the owners'
signatures, a guard can block Safe transactions, and the fallback handler answers calls the Safe
does not implement itself.

```sh
deploy/contracts/verify-safe.sh \
  --rpc sepolia/a="$SEPOLIA_RPC_A" \
  --rpc sepolia/b="$SEPOLIA_RPC_B"
```

If any Safe check fails, the Safe's owners do not prove it as a treasury (or move the treasury off
it) until the Safe or the reviewed expectations are corrected.

## Reproducible build

`expected-codehashes.json` records the pinned compiler profile, canonical proxy hash, fixed salt,
and build artifact hashes. Runtime template hashes intentionally exclude immutable substitutions
(the factory's `implementation`, the implementation's `factory`); the deployment scripts create a
temporary local reference deployment to derive exact factory and implementation runtime hashes.
[contracts/reference.json](contracts/reference.json) is that reference deployment's manifest,
committed: the deterministic addresses, their runtime code hashes, and sample forwarder addresses.
`verify-deployment.sh` compares chains against it, so it needs only `cast` and `jq`, no Solidity
build, and runs from a release's deploy kit; `deploy/contracts/reference-manifest.sh --check`, in
CI and `make deploy-check`, fails unless a fresh build reproduces it. The codehash file
also records each immutable's 32-byte word offsets (`immutable_offsets`). At startup `topup run`
requires the route's addresses at exactly those offsets and, with them zeroed, the runtime template
hash; its compiled-in copies of these values are tested against this file. It also compares the
factory's `addressOf(treasury, sample salt)` with its own derivation.

```sh
export PATH="$HOME/.foundry/bin:$HOME/.cargo/bin:$PATH"
deploy/contracts/check-build.sh --check
deploy/contracts/reference-manifest.sh --check
make deploy-check
```

If an intentional contract or compiler-profile change is approved, regenerate and review the
fingerprints with `deploy/contracts/check-build.sh --write`, the local deployment vectors with
`deploy/contracts/test-determinism.sh --write`, and the reference deployment with
`deploy/contracts/reference-manifest.sh --write`. Never regenerate them merely to make a failed
deployment check pass. A contract change needs the independent review before mainnet
([architecture §4](../docs/architecture.md#4-contracts)).

## Canonical proxy

Check a target chain first:

```sh
deploy/contracts/deploy-proxy.sh --rpc-url "$RPC_URL"
```

If the proxy is absent, the script prints the required funding command. **HUMAN-ONLY:** fund the
one-time signer with exactly the upstream transaction gas budget, then publish the fixed signed
transaction:

```sh
cast send 0x3fab184622dc19b6109349b94811493bf2a45362 \
  --value 0.01ether \
  --rpc-url "$RPC_URL" \
  --private-key "$PRIVATE_KEY"
deploy/contracts/deploy-proxy.sh --rpc-url "$RPC_URL" --broadcast
```

The script refuses an existing proxy whose runtime code hash is not the canonical hash. Some EVM
chains reject the unprotected legacy transaction; such a chain is unsupported until the
architecture explicitly selects another deterministic deployer.

## Sepolia

The committed Sepolia routes (Phala's [staging routes](phala.md#staging-routes) on Sepolia, in
`deploy/environments/phala-network/staging/topup/topup.yaml`) use the #202 build's deterministic factory `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` and implementation
`0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9`, **deployed and verified on Sepolia**
(`verify-deployment.sh` passes on two providers; the Verify contracts workflow checks it daily).
`topup run` checks the factory's `implementation()` and both runtime code hashes at startup.
Circle's Sepolia USDC (`0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238`, `FiatTokenV2_2` behind
Circle's proxy) fits the factory's bounds: on a Sepolia fork (September 2026), a cold
`balanceOf` through the proxy used 9 750 of `BALANCE_OF_GAS` (30 000), and a forwarder's first
flush to a treasury holding no USDC used 37 822 of `FLUSH_GAS` (200 000). The
commands below are the record of that deployment and the procedure for any other test network
where the factory is missing (add the network to `deploy/contracts/networks.json`).

**HUMAN-ONLY**, with the deployer key in the environment only (the forge script reads
`PRIVATE_KEY`; it never appears in argv):

```sh
read -rsp "Deployer private key: " PRIVATE_KEY && printf '\n' && export PRIVATE_KEY

deploy/contracts/deploy-proxy.sh --rpc-url "$SEPOLIA_RPC_A"
deploy/contracts/deploy-factory.sh --rpc sepolia/a="$SEPOLIA_RPC_A" --dry-run
deploy/contracts/deploy-factory.sh --rpc sepolia/a="$SEPOLIA_RPC_A" --broadcast

deploy/contracts/verify-deployment.sh \
  --rpc sepolia/a="$SEPOLIA_RPC_A" \
  --rpc sepolia/b="$SEPOLIA_RPC_B" \
  > sepolia-contract-verification.json
jq -e '.passed == true' sepolia-contract-verification.json
```

The dry run and broadcast both hand `PRIVATE_KEY` to Foundry through the environment only:
`DeployFactory.s.sol` reads it with `vm.envUint("PRIVATE_KEY")` and passes it to
`vm.startBroadcast`, so the key is never a command-line argument (visible in the process list) and
never written to a repository file. The Foundry script prints the predicted factory and
implementation addresses before it sends. If the predicted factory already has code (anyone may
have deployed it), it accepts only the exact runtime hashes derived from the local build.

Verification writes its machine-readable report to stdout and live progress to stderr. Each
progress line identifies a provider by its one-based position in the `--rpc` arguments and a
fixed stage (chain id, individual code hashes, bindings, or numbered sample forwarders), with
whole-second elapsed time on completion and a provider total. A started stage without a completion
line identifies the call still in progress. Preflight forwards these diagnostics immediately.
Provider names, URLs, credentials, and raw RPC errors are omitted from diagnostics; target names
in the JSON report accept only letters, digits, underscores, and hyphens, up to 64 characters per
network or optional label. These diagnostics do not change RPC timeouts or request concurrency.

`verify-deployment.sh` checks, on every target, the chain id, the proxy, factory, and
implementation runtime code hashes of `reference.json`, `implementation()`, the implementation's
`factory()`, and `addressOf(treasury, salt)` for every sample forwarder of `reference.json`.

## Base Sepolia

The committed Base Sepolia routes ([staging routes](phala.md#staging-routes) on Base Sepolia, in the
same file) use the same deployment, **deployed on Base Sepolia (84532)
and verified**: `verify-deployment.sh --rpc base-sepolia/a=… --rpc base-sepolia/b=…` passes on both
staging providers (September 2026), and every runtime code hash equals Sepolia's. Preflight
re-checks it on every Deploy; the Verify contracts workflow checks Sepolia only.

| Contract | Address |
|---|---|
| `ForwarderFactory` | `0x45466D37587E6E46DC35eB96b74ba3D3b1E5b747` |
| forwarder implementation | `0x49F2F1F1a25269Ea0C6FF2AB1C7B09dCBE9c5bA9` |
| sanctions-oracle stand-in (`MockSanctionsOracle`; Base Sepolia has no Chainalysis oracle) | `0x8A0C93d85a05aD30741C193068abF2e5E16e7b35` |
| test PHA (`MockERC20`, 18 decimals, public `mint`) | `0x1a6F260377e42ead1418C7C1afDFD5DE371A9284` |
| a second `MockERC20`, the unsupported-token test contract | `0xC60dE2C49c2b546F968C68a51370250148C52e4b` |
| Circle's testnet USDC (`FiatTokenProxy` to `FiatTokenV2_2` `0xd74cc5d436923b8ba2c179b4bca2841d8a52c5b5`, 6 decimals) | `0x036CbD53842c5426634e7929541eC2318f3dCF7e` |

The chain carries the canonical proxy and Multicall3 at their committed code hashes, and the
mocks' code equals Sepolia's.

**Treasury on Base Sepolia.** The staging treasury is the Safe
`0x26430107887d4a691B340BdB887096B83E7a5844` on both Sepolia and Base Sepolia: the same SafeL2
v1.4.1 (singleton `0x29fcB43b46531BcA003ddC8FCB67FFE91900C762`), 1-of-1 with owner
`0xBfB672596209327979Dc7AB95960286D29e9Af1D`, and the `CompatibilityFallbackHandler`
`0xfd0732Dc9E303f09fCEf3a7388Ad10A83459Ec99` on each (read on both chains, September 2026).
**Never use `0x936c1991f8dA9a919fa11b557a3514719f5A4504` on Base Sepolia**, as a treasury or for
anything else. A Safe exists there at the address of Sepolia's staging finance Safe, but it is an
unusable copy: its only owner, `0x016a227d4eA58914D5b3EA0790B955BFEa55b737`, is a key that was
destroyed, so nothing it receives can ever leave. `safe-expectations.json` lists no Base Sepolia
network, so `verify-safe.sh` refuses a Base Sepolia target instead of checking that copy.

## Mainnet

Not deployed yet. Repeat only after Sepolia verification and the human release approval; if
anyone has deployed the factory first, the broadcast sends nothing. **HUMAN-ONLY:**

```sh
read -rsp "Deployment private key: " PRIVATE_KEY && printf '\n'
export PRIVATE_KEY

deploy/contracts/deploy-proxy.sh --rpc-url "$MAINNET_RPC_A"
deploy/contracts/deploy-factory.sh --rpc mainnet/a="$MAINNET_RPC_A" --dry-run
deploy/contracts/deploy-factory.sh --rpc mainnet/a="$MAINNET_RPC_A" --broadcast

deploy/contracts/verify-deployment.sh \
  --rpc mainnet/a="$MAINNET_RPC_A" \
  --rpc mainnet/b="$MAINNET_RPC_B" \
  > mainnet-contract-verification.json
jq -e '.passed == true' mainnet-contract-verification.json
```

Each report entry records the target, network, expected chain id, and the chain id the RPC
returned; a mismatch fails verification. Compare the Sepolia and mainnet JSON reports. Factory,
implementation, every sample forwarder, and runtime code hashes must be identical.

## Base mainnet

Production's Base route uses the same deterministic factory and implementation at chain 8453.
Deploy and verify it only after the Ethereum mainnet deployment has passed the same human release
approval. The network name is `base` in `deploy/contracts/networks.json`; the broadcast key stays
in the operator's environment and is never an argument or a repository value.

**HUMAN-ONLY:**

```sh
deploy/contracts/deploy-proxy.sh --rpc-url "$BASE_RPC_A"
deploy/contracts/deploy-factory.sh --rpc base/a="$BASE_RPC_A" --dry-run
deploy/contracts/deploy-factory.sh --rpc base/a="$BASE_RPC_A" --broadcast

deploy/contracts/verify-deployment.sh \
  --rpc base/a="$BASE_RPC_A" \
  --rpc base/b="$BASE_RPC_B" \
  > base-contract-verification.json
jq -e '.passed == true' base-contract-verification.json
```

Compare the Base report with the Ethereum mainnet report. The factory, implementation, sample
forwarders, and runtime code hashes must be identical; a mismatch blocks the production deploy.

## Route and compose update

A route file carries only `chain.forwarder_factory`; the implementation is derived as the
factory's first CREATE (`chain.implementation` is optional), and at startup `topup run` requires
the factory's `implementation()` to be it and both runtime code hashes to match the build. Put the
verified factory into the route in the environment's `topup.yaml` (in the operator's environment
repository, for example `production/topup/topup.yaml`), which the attested compose inlines.
Create a new route version; never mutate the contract tuple of an enabled version. Then check it
with the `images.json` and deploy kit (extracted to `kit/`) of the release to deploy:

```sh
docker run --rm -i "$(jq -r '."phala-pay"' images.json)" topup config check /dev/stdin \
  <production/topup/topup.yaml
# applies deploy/compose-policy.jq
kit/deploy/render.sh --images images.json --gateway-domain <gateway> production/topup \
  >/dev/null
```

Deploy the merged route with the Deploy workflow in mode `upgrade` (`deploy/README.md`, "Deploy"),
which verifies the attested read-back. A locally
rendered compose is only a review aid. Keep both contract verification reports with the deployment
record.

## Rollback and treasury changes

There is no contract rollback, upgrade, or setter. A failed or superseded deployment remains on
chain; correct the code and deploy a new factory under a new salt and route version, leaving
historical versions available for existing deposits. A treasury change needs no new factory and
no route change: treasuries are the accounts', set through the API
([Treasury change](runbooks/treasury-change.md)); new forwarders are derived for the new treasury,
and existing forwarders keep paying theirs. In Phala's instance, Phala's finance Safe is Phala
Cloud's account's treasury, verified here and proven through the API like any merchant's.
