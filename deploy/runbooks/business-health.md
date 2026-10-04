# Business progress and ingress certificate health

The independent business health monitor runs once per minute after restore unfreeze. Reads are
bounded to 20 seconds; failures raise `TopupBusinessProbeFailed` rather than implying health.
Read-only restore servers do not run this monitor.

| Alert | Trigger | Response |
|---|---|---|
| `TopupHeartbeatStale` | newest database heartbeat missing or at least 180 seconds old | Inspect `topup-backup` too. Other WAL writes do not prove heartbeat progress. Have the deployment owner inspect/restart the heartbeat service; investigate its exit and database credentials. |
| `TopupTreasuryProgressAge` | any uncanceled, unapplied treasury at least one hour past `effective_at` | Follow [Treasury change](treasury-change.md). Check route availability and screening providers. The 48-hour time-lock itself is excluded. |
| `TopupRefundProgressAge` | an attached refund transaction remains pending at least one hour after `paid_at` | Inspect the refund and transaction on both providers. Retry failures never reset this age. Unattached merchant refunds and sanctioned deposit holds are excluded. Follow [RPC health](rpc-health.md) for provider failures. |
| `TopupCertificateExpiry` (`severity:warning`) | served ingress leaf certificate has at most 14 days remaining | Have the deployment owner check ingress renewal, domain DNS, and ACME reachability. |
| `TopupCertificateExpiry` (`severity:critical`) | at most three days remaining | Escalate renewal immediately; prepare incident communication before expiry. |
| `TopupCertificateProbeFailed` | public HTTPS request, TLS validation, certificate extraction, or parsing fails | Check public ingress and DNS externally. An expired or untrusted certificate raises this alert too. |
| `TopupBusinessProbeFailed` | database business scan fails or exceeds 20 seconds | Investigate database availability/permissions and query cost. Other business measurements are unknown until the scan recovers. |

The certificate probe uses the configured public origin, validates TLS normally, follows no
redirects, and connects without an ambient proxy. It reads the actual peer certificate from
`GET /healthz`; HTTP local stacks skip it. HTTP status does not affect certificate expiry.
Sentry is the only alert channel. Severity is a grouping tag, so warning and critical issues
are distinct; configure the staging and production Sentry issue rules to notify on both.

For validation commands and detector tests see [Synthetic alert validation](README.md#synthetic-alert-validation).
