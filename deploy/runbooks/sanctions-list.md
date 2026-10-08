# Sanctions list verification and operator entries

Applies to `TopupSanctionsListStale`, `TopupSanctionsListVerifyFailed`, and
`TopupRefundDestinationSanctioned`. Treasury and restored-credit hits also follow
[treasury-change](treasury-change.md) and [restore](restore.md).

## Stale or failed verification

The service fetches the official OFAC SDN publication hash hourly, immediately on startup.
No active snapshot, a negative answer older than `sanctions.max_staleness` (default 24h), or
failed list reads yields `sanctions_inconclusive` and holds the decision. An active-list hit
still denies when stale. The stale alert starts at 6h; hash, XML, record-count or publication-date
validation failures alert immediately. Failed verification never advances `verified_at` or
activates a partial snapshot. The hourly Sentry Cron is `topup-sanctions-refresh`.

1. Read the admin-signed `/v1/admin/reports/daily` and `/v1/admin/metrics`. Record the active
   snapshot id, publication date, SHA-256 and verification time. Inspect refresh counters by
   result (`unchanged`, `activated`, `hash_mismatch`, `parse_error`, `fetch_error`) and the
   `TopupDepositStateAgeExceeded` alert for `confirmed` deposits.
2. Open the [OFAC SLS page](https://sanctionslistservice.ofac.treas.gov/) and compare its
   SDN.XML SHA-256 with the recorded snapshot. OFAC's [hash page](https://ofac.treasury.gov/specially-designated-nationals-list-sdn-list/hash-values-for-ofac-sanctions-list-files)
   describes the published hashes. Check `Publish_Date` and `Record_Count` against the file.
   Record evidence; never manually mark a snapshot verified or bypass a failed hash check.
3. Check DNS, HTTPS/TLS and outbound access to `sanctionslistservice.ofac.treas.gov` and
   `wc2h-sls-prod-public-published.s3.us-gov-west-1.amazonaws.com`. Host changes fail closed;
   review the official redirect and ship a reviewed source update. The PublicationPreview
   endpoint is SLS's UI backend and has no stable documented API guarantee. A contract failure
   holds once stale; do not fall back to TLS-only acceptance. Contact `O_F_A_C@treasury.gov`
   when necessary.
4. Pause settlement on affected routes if verification cannot be restored before the freshness
   limit, if operators cannot establish the source's integrity, or if a hit needs review. Use
   the signed admin pause API described in [runbook environment](README.md#environment). Keep funds restricted until the
   compliance owner clears the incident. Resume only after a verified refresh and review.

## Manual entries

Only the operator's admin signing key can add or remove supplements. Every request is signed
using the existing HTTP-message-signature flow and writes an append-only audit row in the same
transaction as the mutation. Never write entries directly with SQL in production.

- Add: `POST /v1/admin/sanctions/manual/add`.
- Remove: `POST /v1/admin/sanctions/manual/remove`.
- Body for both: `{"address":"0x…","reason":"reviewed designation","source_ref":"UK entity reference"}`.
  Use a full 20-byte EVM address, a nonempty reason and source reference, each at most 1000 bytes.
- An entry applies across all EVM chains immediately on the next decision. Removal is recorded
  with actor and timestamp; prior actions remain in audit. Re-adding records a new audited action.
  Removal does not clear a hit from the OFAC snapshot or an existing treasury settlement pause.
- Review the designation source, audit and affected routes before removing an entry. Record the
  compliance owner's decision; broader entity, KYC and KYT obligations remain the operator's.

## Hits and evidence

Deposit hits reject with `sanctioned` and `deposit.rejected`; refunds use the existing refusal
path. A hit on a current treasury pauses its account's quotes and settlement; a pending treasury
change is canceled, including before its time-lock expires after snapshot activation. Pending
refund destinations are re-screened and any hit is audited and alerted for operator review;
the merchant controls refunds, and an attached transaction still follows the existing ledger
verification path. A hit on credit delivered before restore preserves the credit and blocks
sweeping that forwarder. Follow the relevant compliance runbook; do not refund sanctioned funds.

Screening evidence includes snapshot id, SHA-256, publication date, verification time,
`manual_hit` and `screened_at`. All digital-currency identifiers and historical snapshots are
retained. There is no clear-result cache; every decision uses local database reads.

## N-1 rollback

The expand-only tables remain in place and N-1 ignores them. N-1 resumes the deprecated
Chainalysis oracle, whose list is known to be stale, and may re-screen held deposits with that
oracle. A successful rollback drill proves binary/schema compatibility, not current sanctions
coverage. **If active manual entries exist, pause settlement for affected routes before rollback**:
N-1 cannot see them. Retain the pause until N is restored and verified screening is healthy.
Keep the previous configuration with the parsed `chain.sanctions_oracle`; N+1 removes it.
N-1's strict configuration schema cannot parse the new top-level `sanctions` section, so use the
retained previous configuration when rolling back.

Run the disposable local [rollback drill](../local/rollback-drill.sh) with the verified published
N-1 image digest. The N worker uses a loopback-only SLS fixture in the test-support build; the
production build has fixed official endpoints. The CVM rehearsal resolves those URLs to a local
TLS fixture using only its disposable Compose network and certificate. CI and the drill never
fetch real OFAC data.
Staging's later live smoke check must verify a snapshot and a known SDN hit, then clean up its
artifacts; this implementation does not deploy or perform that check.
