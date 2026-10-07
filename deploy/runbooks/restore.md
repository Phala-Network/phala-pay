# Reconciliation after a restore

**Trigger:** the database was restored from backup ([RESTORE.md](../RESTORE.md)); merchants get
`503 service_restoring` with `Retry-After` on every request with an API key, reads included;
`admin GET /v1/admin/restore` shows `"frozen": true`.

**Why:** a restore brings back the database as of its **restore point** (the newest heartbeat in
it, at most the RPO before the loss), not the business as it was. Everything after the restore
point is lost, and some of it matters beyond the database:

- an API key the merchant revoked or rolled works again, secret or restricted;
- a treasury change the merchant canceled is pending again and would apply at its `effective_at`;
- a treasury whose crediting the merchant paused credits again (or a resume is undone);
- a webhook endpoint the merchant deleted receives events again;
- a deposit address given to a customer is unknown, so payments to it are not credited;
- a quote given to a customer is unknown (its address derives from its random id), so a payment
  to it is not found;
- an event the merchant received is gone: re-derived from the chain, a spot-priced deposit would
  be re-valued, and its `deposit.credited` carries the same event id with another `amount`, and
  its refunds and reversal would reference the re-valued amount.

So the service starts **frozen** after a restore (architecture §14): the admin API, `/healthz`, the
scanner, and the reconciler run; every merchant request with an API key answers
`503 service_restoring`, reads included, since a key revoked after the restore point is valid in
the restored database; nothing credits, settles, expires a quote, applies a treasury change,
verifies a refund, or delivers an event. The
freeze is a database row, so it holds from the restore-check instance through the upgrade to the
service compose, and a restore that booted straight into the service compose freezes it too (its
PostgreSQL timeline is new). Only `POST /v1/admin/restore/unfreeze` lifts it, once every chain is
rescanned.

Work through the steps in order, with the runbook environment of the [README](README.md#environment)
and `BASE_URL` set to the instance's origin: `$RESTORE_URL` on the restore-check instance (after
[Restore](../RESTORE.md#restore) step 5), the Environment's `public_origin` once resumed. Every write below
but the discard of step 6 needs the freeze (`400 restore_not_frozen` otherwise), and each writes
`audit`. Steps 2 to 5 can run on
the restore-check instance, before the service resumes; do the security steps as early as possible.

## 1. Read the freeze and the restore point

```sh
admin GET /v1/admin/restore | tee restore-status.json | jq '{frozen, restore, rescan}'
```

`restore.restore_point` (Unix seconds) is where the lost window starts; `restore.detected_by` is
`restore_check` or `timeline`. Record both in the incident.

## 2. Ask every merchant for its records since the restore point

Through [incident communication](incident-communication.md), send each account's recorded contact
the restore point and ask for what it did or received after it, from its own records (its webhook
receiver's store, its database, or an `export_account` taken before the loss):

1. API keys it created, revoked, or rolled, secret and restricted: the key's `id` (`key_…`), or its
   prefix and last four characters (`ppay_sk_live_…abcd`, `ppay_rk_live_…abcd`);
2. the latest `treasury` object of each treasury it received an event about (`treasury.created`,
   `.updated`, `.canceled`), with its `status` and `crediting_paused_by`, and the delivery of each
   `treasury.updated` it received, as in 6 below;
3. webhook endpoints it deleted (`we_…`);
4. deposit addresses it received: `client_reference_id`, `address`, and `id` (`da_…`) or
   `version`, and a `client_secret` it holds for one;
5. quotes it created: the quote object (`id`, `client_reference_id`, `chain_id`, `asset`,
   `amount`, `amount_atomic`, `exchange_rate`, `address`, `created`, `expires_at`, `metadata`) and
   its `client_secret`, if it kept it;
6. the delivery of every `deposit.credited`, `deposit.rejected`, and `deposit.reversed` event it
   received, as its receiver got it: the raw body, byte for byte, with its `webhook-id`,
   `webhook-timestamp`, and `webhook-signature` headers. Only a delivery the service signed is
   imported, so a re-serialized body or a missing header cannot be used.

A merchant running the reference product exports items 2 and 4 to 6 with
`python -m reference_product fetch-restore-records --since <restore point>` (or
`export-restore-records` next to its ledger), each already the body of its request in
steps 3 to 5 without `reason`: `treasuries` (`treasuries/verify`), `treasury_applications`
(`treasuries/apply`), `deposit_addresses`, `quotes`, and `events` ([Staging reference
product](../phala.md#staging-reference-product)).

Tell it what it cannot produce is lost: a quote it has no record of is not found, so a payment to
it is not credited (the chain cannot name it: its address derives from its random id); a spot
deposit whose delivery it has no record of is re-valued, and its refunds and reversal reference the
re-valued amount; a quote whose `deposit.credited` it has no record of is credited at spot, since
its own record of the quote's price is not proof the service issued it
([design §13](../../docs/design/multi-tenant.md#13-operations)). Its API keys answer `503` until the unfreeze
(step 7).

Refunds it created or marked paid after the restore point are gone too: after the unfreeze it
creates them again and marks them paid with the same transaction. Their `deposit.refunded` carries
a new id, but the deposit's cumulative `amount_refunded` is the same, so the balance rule of the
[integration guide](../../docs/integration.md#the-balance-rule-and-event-ordering) takes nothing
back twice.

Keys and endpoints it created after the restore point are gone; it creates them again after the
unfreeze (a merchant left without a working key gets a recovery key,
[API key compromise](api-key-compromise.md#recovery-by-the-operator)).

## 3. Re-apply the security changes

**Required before the unfreeze.** While frozen no API key authenticates; the unfreeze lets every
key the restored database holds as valid read and write again, including one the merchant revoked
after the restore point. Revoke again every key the merchant revoked or rolled:

```sh
admin POST /v1/admin/restore/api_keys/revoke \
  '{"account":"acct_…","prefix":"ppay_sk_live_","last4":"abcd","reason":"INC-…: revoked at 10:02 PDT"}'
```

or with `"id":"key_…"` instead of `prefix` and `last4`. The answer is the key with
`"status": "revoked"`; after the unfreeze a request with it answers `401`. `400` names several keys
with the same prefix and last four (send the `id`), or `last_api_key` (issue a recovery key with
`revoke_existing`). A merchant that has not answered keeps every restored key valid at the
unfreeze: revoke on its behalf any key it reported compromised through other channels, or leave
the account paused until it answers.

Compare the treasuries with what the merchant received, cancel again what it canceled, and pause
or resume crediting again as it last did:

```sh
admin POST /v1/admin/restore/treasuries/verify \
  '{"account":"acct_…","livemode":true,"treasuries":[{"id":"trs_…","status":"canceled","chain_id":1,"address":"0x…","crediting_paused_by":["merchant"]}],"reapply":true,"reason":"INC-…"}' | jq
```

Each `result` is `matches`, `canceled` (canceled again now), `cancellation_lost` (without
`reapply`), `application_lost` (a change that applied after the restore point: restore it below),
`replacement_lost` (the treasury that change replaced, which the merchant received `replaced`,
reported only when the chain's pending change is sent in the same request as `active`, with the
same id and address, in any order: restoring the change replaces it, so it needs nothing of its
own), `missing` (proven after the
restore point: the merchant proves it again after the unfreeze), or `differs` (another chain,
address, or status: escalate). Send both treasuries of a lost change, each object as the
merchant's latest `treasury.updated` of it shows it; once the change is restored, both are
`matches`. Each `crediting` (when
`crediting_paused_by` was sent) is `matches`, `paused` or `resumed` (applied again now, announced
as `treasury.updated`), or `pause_lost` or `resume_lost` (without `reapply`). No treasury change
applies and nothing is credited while frozen, so none takes effect before this step. Only the
merchant's own pause is compared: re-apply the operator's own treasury pauses made after the
restore point from the incident record
(`admin POST "/v1/admin/accounts/$ACCOUNT/treasuries/$TREASURY_ID/pause" '{"reason":"…"}'`).

Restore each `application_lost` change now, before step 4: the deposit addresses and quotes issued
after it are derived over it, so they can be re-issued only once it is in force again. The evidence
is the merchant's delivery of the `treasury.updated` that announced it (its object `active`, its
`previous_attributes.status` `pending`), as its receiver got it:

```sh
admin POST /v1/admin/restore/treasuries/apply \
  '{"delivery":{"webhook_id":"evt_…","webhook_timestamp":"1790000005","webhook_signature":"v1a,…","body":"{\"id\":\"evt_…\",…}"},"reason":"INC-…"}' | jq '{applied, status: .treasury.status}'
```

Only a delivery the service signed is accepted (verified with the account's webhook keys, as in
step 5), of the restored treasury with the same chain and address. The change's own rules hold: it
was proven when it was submitted, it is still pending (cancel again first what the merchant
canceled, above), and its time-lock ended by the event's `created`. It is screened again when a
route of its chain can screen it; as the time-lock, it stays pending while its chain has no current
route (`400`). `400` if a sanctions list names it now (escalate; at the unfreeze its change is
canceled). When screening is unavailable, the screening the time-lock made before applying it
stands (the signed event attests it: a sanctioned change is canceled, never applied), and the daily
screening checks it again after the unfreeze. It applies at the event's `created`: the time-lock
recorded `applied_at` when the change applied, under its account's lock, and the event's `created`
is when that transaction started, at most seconds earlier and in whole seconds, so re-issue allows
for clock and rounding skew (step 4). The treasury
it replaced stays in force until then, and the account's deposit address networks on the chain
move to it. Its `treasury.updated` events are not sent again: the merchant received them when it
first applied, and new ones would carry the restore's time. The apply and its `restore.treasury_apply`
audit row commit together. `applied` is `false` when it is in force already. The merchant's unsigned `treasury`
object is never enough: a merchant without the delivery proves the treasury again after the
unfreeze, and the addresses and quotes issued over it meanwhile cannot be re-issued (escalate).

**A lost change that applied at once.** Any test-mode change, and a chain's first treasury in
either mode, applies when it is proven, with no pending state: the merchant received a
`treasury.created` with the object `active` (and a `treasury.updated` of the treasury it replaced,
`replaced`), never a pending change becoming `active`. When such a change applied after the restore
point, the restored database holds nothing of it: `treasuries/verify` answers `missing` for it and
`differs` for the treasury it replaced (received `replaced`, restored `active`), the merchant's
export has no `treasury_applications`, and `treasuries/apply` refuses each of its deliveries (`400`,
not the `treasury.updated` of a pending change becoming active). Record both results: that
`differs` needs no escalation of its own when the merchant's `missing` treasury of the same chain
is the one that replaced it. After the unfreeze the merchant proves the treasury again
(it applies at once, under a new `trs_` id) and pins it again. Deposit addresses and quotes issued
over it after the restore point cannot be re-issued (`400` in step 4), and payments to them are not
credited: escalate and settle them with the merchant. A live change of a chain that already has a
treasury is always time-locked for 48 hours, so with an RPO below that it is in the backup, pending,
and is restored with `treasuries/apply` as above.

Delete again every endpoint the merchant deleted, before deliveries resume:

```sh
admin POST /v1/admin/restore/webhook_endpoints/delete \
  '{"account":"acct_…","livemode":true,"id":"we_…","reason":"INC-…"}'
```

## 4. Re-issue the deposit addresses given out after the restore point

A deposit address's salt is derived from the account, mode, `client_reference_id`, and version
([design §5a](../../docs/design/multi-tenant.md#5a-deposit-addresses-d16)), so the service issues
the same address again from the merchant's record. A network's address is derived over the
treasury of its chain in force when it was issued, so restore lost treasury changes first (step 3):
the merchant's address is looked for over every treasury of the account in force since the restore
point (from 5 minutes before it, for the recorded application times). A restore without a restore
point (the restored database has no heartbeat) re-issues nothing: escalate.

```sh
admin POST /v1/admin/restore/deposit_addresses \
  '{"account":"acct_…","livemode":true,"client_reference_id":"team-42","address":"0x…","id":"da_…","reason":"INC-…"}' | jq
```

The answer's `deposit_address` has the merchant's `id` and its networks over the current
treasuries. Sent with its `address` or its `version` alone, each version the re-issue brings back
(the merchant's and the retired ones before it) also gets a network over every other treasury of
each chain in force since the restore point, superseded, still watched and credited, as a treasury
change leaves one: whichever of its addresses the merchant gave out is credited. The versions
between the restored latest one and it are issued retired, as the rotations left them. Each new network is backfilled from the
restored cursor, so the rescan credits payments made to it since. `400` means the address is not
the customer's over a treasury in force since the restore point: check the treasuries (step 3),
then the merchant's record. The chain alone cannot name these customers: a salt is a hash of the
`client_reference_id`.

One call brings back a version at most 32 past the customer's latest one (an address is looked for
that far too): each version between is issued with its networks in one transaction, so a larger
`version` is refused with `400` before anything is issued. Re-issue a customer further behind in
steps: `version` 32, 64, and so on (a version the customer has is returned as it is,
`reissued: false`), then the merchant's own address or version, with its `id` and
`client_secret`. Each step is its own audited re-issue.

A merchant can also re-register an address itself after the unfreeze: `POST /v1/deposit_addresses`
returns version 1 identically, and each `POST /v1/deposit_addresses/{id}/rotate` the next version.
Payments made to it meanwhile are credited once it is registered, but only from the chain's cursor
at that time; re-issue here so nothing is missed.

Re-issue every quote the merchant created after the restore point the same way. Its address salt
is derived from the account, `client_reference_id`, and `qt_` id ([architecture
§9](../../docs/architecture.md#9-quotes)), so only the quote's own address over a treasury of the
chain in force within 5 minutes of its `created` is accepted (every one is the merchant's own, and
the salt binds the `qt_` id). A quote the restored database holds, same id and account, is returned
as it is, `reissued: false`, whenever it was created; any other quote created more than 5 minutes
before the restore point is refused, since the restore did not lose it:

```sh
admin POST /v1/admin/restore/quotes \
  '{"account":"acct_…","livemode":true,"id":"qt_…","client_reference_id":"team-42","chain_id":1,"asset":"pha","amount":1000,"amount_atomic":"…","exchange_rate":"0.10000000","address":"0x…","created":1790000000,"expires_at":1790000900,"metadata":{},"client_secret":"qt_…_secret_…","reason":"INC-…"}' | jq
```

Only the address is checked: the terms are stored as the merchant recorded them, whatever the
route issues today, and never applied. The locked price is the merchant's record, not the
service's, so a payment to the quote is credited at spot, unless the merchant's delivered
`deposit.credited` for it (step 5) carries the quote's credit; and the quote's `expires_at` is the
restore's detection at the latest, so its payment page shows it expired instead of asking for a
payment at a price that is not honoured. Send the quote's `client_secret` when the merchant holds
it: only a secret the service issued for that `qt_` id to that account is accepted (its tags prove
both, so a secret copied from another merchant's payer page is refused), and it is kept, so the
payer's page reads the quote again; without it the quote is re-issued and its payment found all
the same, and a repeat with the secret, once the merchant finds it, adds it. `reissued` is `false`
when the quote exists already for the customer at the address. Its address is backfilled from the
restored cursor, so the rescan finds a payment made to it. `400` means the address is not the
quote's over a treasury in force around its `created` (check the treasuries, then the record), the
quote was created before the restore point, the
`client_secret` is
not the quote's or not the account's, or no route has the chain and asset. A quote nobody reports
stays lost.

A `qt_` or `da_` id is unique across accounts, and nothing but its secret proves which account the
service issued it to: a re-issue without one is the merchant's claim. `400` `id belongs to another
quote` (or `deposit address`) for a request whose `client_secret` was accepted means another
account or mode re-issued the id first, without its secret: this account's secret proves the id
is its own. The other claim cannot show this merchant's payers its address (this secret is refused
there), but it holds the id. Escalate: record both accounts and their records in the incident and
decide with both merchants; a payment to this merchant's quote address is forwarded only to its
treasury, but is not credited while the id is held elsewhere. Ask merchants for their secrets
before re-issuing without one.

The same `client_secret` field on the deposit address re-issue above (with its `id`) keeps a secret
the merchant holds for the address, accepted likewise only when the service issued it for that
`da_` id to that account. Without the secret, a re-issue without the `id` still brings the address
back, under a new `da_` id.

## 5. Import the events the merchant received

```sh
admin POST /v1/admin/restore/events '{"deliveries":[{"webhook_id":"evt_…","webhook_timestamp":"1790000005","webhook_signature":"v1a,…","body":"{\"id\":\"evt_…\",…}"}],"reason":"INC-…"}' | jq
```

Up to 100 deliveries per request, each as the merchant's receiver got it: `body` is the raw
request body as a string, byte for byte. Only what the service signed is imported: the
`webhook_signature` must verify over `webhook_id`, `webhook_timestamp`, and `body` with one of the
account's webhook keys in the event's mode (every version up to the restored current one, and the
next, for a roll lost with the restore). Each event is stored as the event it is, with no delivery:
when the rescan re-derives its deposit, the event is recorded already and nothing is sent again
with another body. The credit a `deposit.credited` or `deposit.reversed` carries is kept, and the
deposit is valued at it (amount, exchange rate, price source, valuation time), not re-valued, so its
refunds and reversal reference what the merchant was told. A `deposit.reversed` of a deposit that
was never valued (rejected, such as a token without a route) carries no credit and is imported all
the same. `imported`; `matches` (recorded already, same body); `mismatch` (recorded already with
another body, which is kept; record it). `400` names a delivery whose signature does not verify,
whose event is not a re-derived deposit event, whose `deposit.credited` has no valuation, whose id
is not the one its type and deposit derive, or whose deposit's id is not the one its
`receipt_log_index` and `revision` derive; nothing of that request is imported. `503` means the
webhook keys cannot be derived; retry.

A `deposit.reversed` also brings its deposit back, reversed, as the merchant received it, when the
restore lost it. Its `reversed_deposit` is `restored`; `recorded` (the ledger holds the deposit
already); `address_unknown` (its address is not issued in the event's account and mode: re-issue
it, step 4, and import the event again); or `rescanned` (see below). A deposit the
finality watch reversed because a re-included transaction put another transfer at its receipt
position ([architecture §7](../../docs/architecture.md#7-states-and-pump)) so keeps its revision
there, and the rescan records the transfer now at the position under its successor's id, with its
`replaces`, valued at the successor's delivered credit. Import every delivery before the service
resumes (step 6): a rescan that reaches the position first records the transfer under the reversed
deposit's id. The import then never restores the reversed deposit over it: its event is imported,
its `reversed_deposit` is `rescanned`, and `GET /v1/admin/restore` lists it as a `rescanned`
finding (step 8). Only a deposit the rescan recorded at the reversed deposit's revision or below is
such a conflict; one at a higher revision is its successor, whether the reversal named it or
not.

## 6. Resume and wait for the rescan

On a restore-check instance, [resume](../RESTORE.md#resume): the service boots frozen. The scanner
rescans each chain from its restored cursor and the reconciler runs; deposits are recorded, not
credited. Wait until every chain is rescanned:

```sh
admin GET /v1/admin/restore | jq '.rescan[] | {chain_id, restored_block, scanned_block, pending_backfills, blocked, complete}'
```

A chain is `complete` once it finalized past the moment the restore was detected with every
issued address backfilled. A chain `blocked` by reconciliation is left out: it credits nothing
until its block is lifted ([Chain frozen](chain-frozen.md)).

Once the rescan has recorded the deposits, check the imported events against them:

```sh
admin GET /v1/admin/restore | jq '.delivered_events.findings[] | select(.status == "contradicted")'
```

A `contradicted` finding is a deposit whose recorded transfer (account, mode, chain, transaction,
recipient, token, sender, or amount) is not the one the signed delivery names. The service never
delivers such an event, so escalate: the deposit is held, not valued or credited, and its confirm
step retries as an invariant violation until the operator decides. Once the chain is confirmed
right, discard the delivered credit, audited; the deposit is then valued from the chain as any
other, and the difference is settled with the merchant:

```sh
admin POST /v1/admin/restore/delivered_credits/discard '{"deposit":"dep_…","reason":"INC-…: chain shows …; settled with the merchant"}'
```

It works frozen or not, since the confirm step can also find a contradiction after the unfreeze.

## 7. Unfreeze

Only when steps 3 to 5 are done for every account that answered, every key a merchant revoked or
rolled after the restore point is revoked again (step 3), and every chain is `complete`. The
unfreeze is when merchant keys authenticate again:

```sh
admin POST /v1/admin/restore/unfreeze \
  '{"reason":"INC-…: reconciled, signed off by …","security_changes_reapplied":true,"deposit_addresses_reissued":true,"quotes_reissued":true,"delivered_events_imported":true}'
```

`400 restore_rescan_incomplete` means rescan or acceptance checks are incomplete. The reason and checklist are
recorded in the restore and in `audit`; crediting, settlement, quote expiry, treasury changes,
refund verification, and event delivery resume, and merchants' API keys work again, reads and
writes. Each checklist item is a step: `security_changes_reapplied` step 3,
`deposit_addresses_reissued` and `quotes_reissued` step 4, `delivered_events_imported` step 5.

Before unfreeze, the latest `restore.validation` audit entry for this restore must say `ok`.
A failed RPC or database check, an interrupted check, or absent validation evidence blocks it,
even when all rescans are complete. The existing API returns `400 restore_rescan_incomplete`
for either incomplete rescan or failed acceptance checks; inspect the restore report and audit
history to distinguish them. Rerun `restore-check` with service workers stopped after correcting
the fault. Every failed critical check makes both reconciliation and restore status `incomplete`.

An administrator may explicitly accept the critical-check risk only with an incident reason:

```sh
admin POST /v1/admin/restore/unfreeze \
  '{"reason":"override-critical-checks: INC-42: independent evidence reviewed by NAME; custody RPC unavailable","security_changes_reapplied":true,"deposit_addresses_reissued":true,"quotes_reissued":true,"delivered_events_imported":true}'
```

The exact `override-critical-checks:` prefix and a nonempty reason are required. This does not
bypass the rescan or checklist. It records `restore.critical_checks_override` with the admin actor,
restore id and supplied explanation, atomically with `restore.unfreeze`. Merchant and system
actors cannot override. Record the failed checks and independent evidence in the incident.
A drill must pass the normal checks; an override does not prove successful recovery.

RPO is the interval from the externally recorded failure instant to the newest replayed committed
transaction, at most 60 seconds including sampling and archive upload latency. RTO ends only
after unfreeze and a successful request using a merchant credential, at most 3600 seconds.
The read-only report is an intermediate checkpoint, not the end of downtime.

## 8. After the unfreeze

```sh
admin GET /v1/admin/restore | jq '.delivered_events'
```

Each finding is an imported event whose deposit the ledger does not hold as delivered:
`pending` until the rescan re-derives and values it; `contradicted` while its recorded transfer is
not the delivered one (step 6); `mismatch` when the ledger's token amount or credit differs from
what the merchant received. A delivered credit is carried into the ledger, so a `mismatch` follows
only a discarded contradiction or an event that carries no credit. The merchant keeps its
delivered credit and is never sent another; record each mismatch, both amounts, and the deposit in
the incident and settle it with the merchant. A `pending` finding that stays after the rescan is a
deposit the chain does not show: escalate. A deposit the finality watch reversed because another
transfer took its receipt position is brought back from its imported `deposit.reversed` (step 5),
so its successor is recorded under its own id. Only when that could not be done (the rescan
reached the position before the import: a `rescanned` finding) does the rescan record the final transfer under the reversed deposit's id: that deposit's imported
credit then contradicts the transfer it holds, so it is held, and the successor's events stay
`pending`. Record the two as one incident, settle what the merchant applied with it, then discard
the held deposit's delivered credit (step 6).

**Payment settings.** The restore held every account's payment settings in both modes
(docs/design/payment-settings.md §11): nothing is quoted or issued (`400
payment_settings_unconfirmed`), and a payment recorded meanwhile waits, unless its outcome was
delivered before (that outcome stands). No delivery proves a merchant's latest configuration, so
the operator cannot lift the hold: each merchant sends its complete configuration with
`POST /v1/payment_settings`, even unchanged, which binds its waiting payments. Ask each in the
second notice; `admin GET "/v1/admin/accounts/$ACCOUNT"` shows `status: held` until it does.

### Sanctioned delivered credit

`TopupDeliveredCreditSanctioned` names a deposit whose credit was delivered before the restore and
whose sender both endpoints now agree is sanctioned at the verified screening block. That is compliance, not commercial policy
(docs/design/payment-settings.md §11): the delivered credit stands, so no `deposit.rejected`
rewrites what the merchant applied. The service records the hit, and `GET
/v1/forwarders?sweepable` never offers that forwarder, so the funds stay in it. Escalate to
compliance with the deposit id, tell the merchant which credit is affected and why its forwarder is
excluded, and record the decision. Disagreement or unavailability instead holds and alerts:
no hit is recorded and the delivered credit is unchanged. The exception applies only to credit
already delivered before restore; every new credit requires two clear answers.
The service never sweeps or refunds a deposit with a recorded hit.

## Done when

`frozen` is `false`, every chain was `complete` at the unfreeze, no `delivered_events` finding is
`pending` or `contradicted`, each `mismatch`, `rescanned` finding, and discarded credit is recorded
and settled, and
every merchant confirmed that its keys, treasuries, endpoints, deposit addresses, and quotes are as
it left them and reconfirmed its payment settings in each mode it uses.
