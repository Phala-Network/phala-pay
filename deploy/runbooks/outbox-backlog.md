# Outbox backlog

**Trigger:** the `topup-outbox-test` or `topup-outbox-live` monitor missing its check-ins, `outbox delivery claim failed` or
`outbox delivery failed` issues, a non-zero `credited_undelivered` or any `failing_webhook_endpoints` in the daily report
(`GET /v1/admin/reports/daily`), or a merchant reporting that webhooks or credits stopped.

**Impact:** `deposit.credited` is how the merchant learns it owes a credit, so an undelivered one
means a user not credited yet. Deposit states stay authoritative and deliveries retry with backoff
(capped at 1 h) until delivered, by themselves: a failing endpoint is never disabled (owner
decision, design §11), and is probed about once an hour, one delivery at a time. Only the
receiver's `410 Gone` or the merchant disables an endpoint. One merchant's receiver or every
merchant. There is no email channel: the recorded contact is how the operator reaches a merchant
whose endpoint keeps failing.

## First steps

1. Read the error of `outbox delivery claim failed` or `outbox delivery failed` in Sentry: a
   database error stops every delivery; a receiver's failures do not raise an issue.
2. The daily report's `failing_webhook_endpoints` names each failing endpoint, its account,
   `pending_deliveries`, `oldest_pending_at`, and `last_attempt_status` (`null`: no response,
   such as a timeout or a URL the egress proxy refused). With the merchant, from its recorded
   contact, check its webhook endpoints (`GET /v1/webhook_endpoints`: `status`,
   `disabled_reason`, and the same `pending_deliveries`, `oldest_pending_at`, `last_attempt`),
   TLS, and signature verification. Report `credited_undelivered` and
   `credited_undelivered_max_age_seconds` per route. The merchant credits only from signed
   events, so it cannot catch up by fetching state alone; `GET /v1/events?delivery_success=false`
   lists what its endpoints have not received.
3. For a missing event, the admin deposit view lists each deposit's `events`: `id` (the
   `webhook-id`), `type`, and `delivered_at` (`null` while undelivered); the merchant sees
   the same event with `pending_webhooks` in `GET /v1/events`. Delivery attempts are not
   observable in production.

## Decide

- Receiver down or answering `5xx`, however long: tell the merchant through its recorded contact;
  once it fixes the receiver, the next probe succeeds and the backlog drains by itself.
- Endpoint disabled (by the merchant, or after it answered `410 Gone`), or an event lost by a
  receiver that accepted it: the merchant re-enables the endpoint (`POST
  /v1/webhook_endpoints/{id} {"disabled": false}`) and resends each missed event with its own key
  (`POST /v1/events/{id}/resend {"webhook_endpoint"}`, docs/integration.md §5.11), same id and
  payload. The operator has no replay: it does not act on a merchant's webhooks (design §2).
- A URL whose host resolves to a private, CGNAT, or metadata address is refused by the egress proxy
  ([webhook egress](../README.md#webhook-egress)) and fails like an unreachable one: the merchant
  must use a public address.

- Receiver rejects signatures (`4xx`): the merchant re-pins its account's webhook public keys for
  the mode from `GET /v1/attestation` with its own key; after a webhook key roll the previous key
  keeps signing for the roll's overlap (at least 48 hours in live mode).
- Monitor silent with no error: the delivery worker stopped. **HUMAN-ONLY:** restart the CVM
  (`deploy/phala cvms restart "$TOPUP_CVM_ID"`).

## Done when

`topup-outbox-<mode>` checks in again and the merchant receives new events; receivers deduplicate
by webhook id.

## Proactive delivery alerts

`topup-outbox-live` and `topup-outbox-test` prove successful database polling, not merchant
fulfilment. The independent business monitor checks each mode once per minute, even when the
worker is retrying, cooling down endpoints, or cannot claim anything:

- `TopupOutboxBacklog`: at least 1,000 eligible pending endpoint deliveries, or the oldest at
  least 24 hours old. Disabled/deleted endpoints are excluded except their pending notices.
- `TopupOutboxStalled`: pending backlog with no persisted successful delivery for 15 minutes.
  A new backlog and a process restart get a full 15-minute grace period. Success in one mode
  cannot mask a stall in the other. Age/count alerts remain active across restarts.
- `TopupOutboxInternalFailure`: signing/rendering/key failures, or failure to connect through
  the configured egress proxy. `component` identifies the safe failure code. These are retried
  without penalizing the merchant endpoint. Check dstack signing and smokescreen availability.

A healthy endpoint can mask a different endpoint's failure in the mode-wide success signal;
the oldest-pending alert still catches that partial stall. Check the daily report's failing
endpoints. See [Synthetic alert validation](README.md#synthetic-alert-validation) before rollout.
