# RPC health

**Trigger:** `TopupRpcGroupUnavailable` after one minute without a serving A or B candidate,
`TopupRpcChainFrozen`, `TopupRpcMemberQuarantined`, `TopupRpcMemberCooldown`,
`TopupRpcQuotaPressure`, `TopupRpcUnclassifiedError`, `TopupRpcAnchorUnavailable`, `TopupRpcRecoveryUnavailable`, or
`TopupRpcMetricsRefreshFailed`. These are Sentry events, grouped only by configured chain,
group, member and failure class. The client suppresses repeats of an issue for ten minutes.

**Impact:** unavailable independent evidence holds credit and cursor progress pending. A fork
freeze requires audited recovery; an endpoint repair cannot unfreeze the chain.

## First steps

1. Read the Sentry grouping tags. Fetch `admin GET /v1/admin/metrics` and
   `admin GET /v1/admin/reports/daily` through the signed admin helper in the
   [runbook index](README.md#environment). There is no deployed Prometheus collector.
2. Compare group eligibility, member quarantine, failure classes, budget wait, accepted heads,
   chain epoch and replay backlog. Do not infer zero usage from an absent or stale snapshot.
   Recovery failures appear as "RPC member recovery probe failed" warn logs on the first and every tenth consecutive failure per member.
3. For a fork freeze follow [Chain frozen](chain-frozen.md). For a stopped scanner follow
   [Scanner lag](scanner-lag.md); for divergent providers follow
   [Provider disagreement](provider-disagreement.md).
4. Repair a credential or endpoint through an attested configuration upgrade. Check the sealed
   credential assignment and provider account quota; never share a company between A and B or
   bypass independent verification. Unknown RPC errors require a reviewed classification rule.
5. Confirm complete recovery probes readmit candidates, pending replay drains and reconciliation
   succeeds. Do not manually advance watermarks or treat one provider's answer as agreement.

## Metrics refresh failure

`TopupRpcMetricsRefreshFailed` means the durable snapshot could not be refreshed. Its last
successful gauges and `topup_rpc_metrics_refreshed_at_seconds` remain available; they are stale,
not proof that the current chain is healthy. Before the first success these gauges are absent.
HTTP request and RPC dispatch counters continue independently. The recovery worker continues
probing and retries collection on subsequent passes. Check other Sentry database errors and
health monitors; repair the database through the existing recovery procedure if necessary.
Never restart only to make counters look fresh.
