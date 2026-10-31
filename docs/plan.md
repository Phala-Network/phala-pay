# Post-launch status

Phala's production instance has been live since October 23, 2026 PDT and runs v0.13.4 for
RedPill. Phala Cloud onboarding is planned; the instance takes no third-party merchants.
[Phala's instance](../deploy/phala.md#production) is the source for the current routes, RPC
providers, pilot caps, and deployment evidence. Production has three live stablecoin routes;
staging has four test routes. The deterministic factory is
[deployed and verified on Ethereum and Base](../deploy/CONTRACTS.md#mainnet), and screening uses
verified OFAC SDN snapshots with audited operator supplements.

The [production upgrade and restore evidence](../deploy/phala.md#launch-evidence) records the
v0.13.4 deployment, the passed v0.13.2 production-backup drill on October 29, and the passed
automated v0.13.4 drill on October 31. The automated drill does not replace a production-backup
drill on the current release or the owner's real-money acceptance.

## Gates before widening RedPill access

- Complete the owner's real-money acceptance payment and record the
  [production launch sign-off](../deploy/runbooks/production-launch.md#13-sign-off-checklist),
  including screening supplements, three-route smoke evidence, and a production restore drill
  on the current release. An upgrade's health check or a staging drill does not establish these.
- Decide admission before opening the organization allowlist. Each new quoting organization
  permanently consumes one of the configured historical addresses on each chain it uses. Keep a curated
  allowlist or require paid history and bound new crypto customers per day; plan the
  [capacity release](../deploy/runbooks/address-capacity.md#upgrade-paths) before 70% of the configured per-chain cap.
- Bound daily quote creation per organization and user. Production permits 600 fresh price
  snapshots per price chain per UTC day, shared by all customers; exhausted snapshots return
  `503 price_unavailable`. Confirm the account's open-quote and amount caps with `GET /v1/config`.
- Agree the [pilot monitoring cadence and stop actions](../deploy/runbooks/production-launch.md#11-pilot-caps-monitoring-and-stop-actions).
  Production has no metrics collector; D=280 deposits/day is operational, and pausing quotes
  does not stop transfers to existing addresses. The next upgrade adds persisted daily RPC/D
  alerts; v0.13.4 still needs manual quota checks. Verify generated alerts through the
  [admin alerts API](../deploy/runbooks/README.md#manage-alerts) and retain backup/restore evidence.
  Alert delivery is outside the service's ownership; operators manage generated alerts via that API.

## Phala Cloud and follow-ups

Phala Cloud still needs its merchant integration, account onboarding, treasury proofs, payment
settings, pinned webhook keys, and live acceptance. PHA price-lock support remains in the service
and staging; production PHA is disabled pending an Allowed independent price source.

The Zod and crypto-bigint security exceptions in [Trivy](../.github/security/trivy-ignore.yaml)
and [cargo-deny](../deny.toml) expire November 24, 2026 at 4 PM PST (November 25 at 00:00 UTC).
The SDK generator's minimatch exception expires November 29 at 7 PM PST (November 30 at 03:00 UTC).
Resolve or re-review each before expiry. Account closure (design PR 13) and the Phala Cloud
self-hosting template remain follow-ups, rather than prerequisites for the existing RedPill pilot.
