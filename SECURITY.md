# Security policy

## Supported versions

Phala Pay is pre-1.0. Fixes are made on `main` and ship in a new release, which carries the
service and all SDK packages at one version.

| Component | Supported |
|---|---|
| The service, contracts, and deployment files: the latest release (its `v<version>` tag, images, and deploy kit) and `main` | Yes. Operators deploy releases. |
| Earlier service releases | No. |
| `@phala/pay` (npm) and `phala-pay` (PyPI) | The latest release of each. |
| Earlier SDK releases | No. |

## Reporting a vulnerability

Report vulnerabilities through GitHub's
[private vulnerability reporting](https://github.com/Phala-Network/phala-pay/security/advisories/new)
for this repository
([how it works](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)).
If it is unavailable, email [support@phala.network](mailto:support@phala.network).

Do not open public issues, discussions, or pull requests for exploitable vulnerabilities. This
applies in particular to anything that could move or strand funds, credit a deposit that was not
paid, bypass attestation or admin authentication, or expose keys derived in the CVM.

A useful report names the affected component and commit or release, the steps to reproduce, and
the impact you expect.

## Scope

This policy covers the software in this repository. Each Phala Pay instance is run by its own
operator, who handles and discloses the incidents of that instance
([self-hosting guide](docs/self-hosting.md#11-upgrades-and-operations)).

## Trust boundary

What a deployment guarantees is what its attestation proves: the image digests and settings
measured into the CVM, verified as described in
[deploy/README.md](deploy/README.md#attestation-ingress-and-egress). Code or configuration in this
repository that is not measured into a deployment is not a guarantee of it.
