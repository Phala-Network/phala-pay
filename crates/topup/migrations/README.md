# Database migrations

Migrations run as the trusted database owner: `topup migrate` reads `DATABASE_URL` and refuses to
run unless that login owns the database (or is a superuser), so the application login never
migrates. Migrations are additive: never edit an applied
migration, and never squash again once any environment holds data.

## Concurrent queue indexes

The `20261028000000`–`20261028000002` RPC queue index migrations use SQLx 0.9's first-line
`-- no-transaction` directive and one concurrent DDL statement per up/down file. Multiple
statements in one message would still open a PostgreSQL implicit transaction. The replacement
review index is valid before the old index is dropped. Normal Deploy/`topup migrate` retries
recover only matching invalid indexes owned by these pending migrations, under the existing migration
advisory lock. Matching valid builds retain their OID; unrelated definitions fail closed. The cutover's
outer transaction commits before these concurrent operations. See the
[plan evidence and CLI recovery tests](../../../docs/design/db-api-query-plans.md).

## Roles and privileges

The service runs through the login role configured by `DATABASE_URL`. That login role must be a
member of the migration-created `topup_app` NOLOGIN role. Table owners and PostgreSQL superusers
remain trusted migration/operations identities. `BEFORE UPDATE OR DELETE` triggers on
`transitions`, `audit`, and `reconciliation_findings` stay in place as defense in depth against
accidental owner-side mutation.

Default privileges grant `SELECT`, `INSERT`, `UPDATE`, and `DELETE` to `topup_app` on every table
the owner creates; no application table grants `TRUNCATE`. The migration narrows that grant:

| Tables | `topup_app` |
|---|---|
| `topup_migration_compatibility` (owner-written rollback ledger) | None |
| `transitions`, `audit`, `reconciliation_findings`, `heartbeat`, `events` | `SELECT`, `INSERT` (append-only) |
| `flushed`, `flush_failures` | `SELECT`, `INSERT` (finalized chain facts) |
| `reconciliation_blocks`, `deposit_address_client_secrets` | `SELECT`, `INSERT`, `DELETE` |
| `reconciliation_deposit_cursors`, `restores` | `SELECT`, `INSERT`, `UPDATE` |
| `restore_delivered_credits` | `SELECT`, `INSERT`; `UPDATE` of `discarded_at`, `discarded_by`, and `discard_reason` only |
| `restore_timeline` | `SELECT`, `UPDATE` |
| `restore_delivered_events`, `restore_deposit_tombstones` | `SELECT`, `INSERT` |
| `payment_settings_revisions` | `SELECT`, `INSERT` (append-only) |
| `payment_settings_cutover` | `SELECT`, `UPDATE` |
| `rpc_config_acceptances` | `SELECT`, `INSERT` (public digests) |
| `rpc_member_validations` | `SELECT`, `INSERT`; `UPDATE` of `validated_at` only |
| `rpc_chain_state` | `SELECT`, `INSERT`; `UPDATE` of `frozen`, `reason`, `awaiting_anchor` only |
| `rpc_watermarks` | `SELECT`, `INSERT`, `UPDATE`; epoch lowering is owner recovery only |
| `rpc_window_reviews` | `SELECT`, `INSERT`; `UPDATE` of `reviewed_at`, `reviewed_by`, `replayed_at` only |
| `rpc_role_bindings` | `SELECT`, `INSERT`; accepted chain roles are immutable |
| `rpc_reorg_ranges` | `SELECT`, `INSERT`; `UPDATE` of `replayed_through` only |
| `rpc_recoveries` | `SELECT`; recovery audit writes require the owner |
| `_sqlx_migrations` | `SELECT` |
| every other table | `SELECT`, `INSERT`, `UPDATE`, `DELETE` |

A migration adding a table that should not get the full operational grant must narrow it in the
same migration. `topup_app` also has `USAGE, SELECT` on `heartbeat_id_seq`. The database test
`application_role_privileges_match_the_documented_grants` checks every `public` table against its
list, so a new table fails it until it is listed there and, if narrowed, here.

## Tenancy

Every tenant table (`customers`, `quotes`, `deposit_addresses`, `addresses`, `deposits`,
`refunds`, `api_keys`, `webhook_endpoints`, `events`, `idempotency_keys`, `account_limits`,
`retiring_webhook_keys`, `payment_settings_revisions`, `payment_settings_state`, and the
account-owned `treasuries`)
carries `account_id`, and the mode-bearing ones `livemode`. Merchant queries are built from a
server-side scope of both (`crate::tenancy::Scope`); the chain workers and the admin API act for the
platform and read across accounts. Composite foreign keys, `(parent_id, account_id, livemode)`
referencing a unique key of the parent, make a quote agree with its customer, an address with its
quote or deposit address, a deposit address with its customer, a deposit with its address and
customer, and a refund with its deposit, so no write can join
two accounts or two modes. `transitions`, `pending_transfers`, `flushed`, `flush_failures`, and
`webhook_deliveries` have no `account_id` and are reached only through their scoped parent.

The permissions each API key kind holds are in code (`crate::tenancy::Principal`, design D13),
beside the routes that require them; `20261021120000_atomic_idempotency` dropped the
`permissions` table that held them.

## Points the schema does not show on its own

- `accounts.public_id` is generated from `id`: `acct_` and its 32 hex digits.
- `addresses.treasury` is the forwarder's clone argument, the only address it can pay: the
  account's current treasury of the chain when the address was issued, kept for good.
- `addresses.created_block` defaults to zero, which makes the first scanner pass check the full
  chain history before setting `backfilled`. Quote creation sets it from the chain's committed
  cursor instead.
- `deposits.confirmations_at` is when the transfer reached the required confirmation and was
  recorded; `final_at` when both providers showed it at `finalized`.
- `events.data` is rendered in the transaction that changes its object, a snapshot of the object
  when the event happened, with `previous_attributes` on `*.updated` events; it is never
  re-rendered, so every endpoint, retry, resend, and read gets the same body.
- `pending_transfers` is display-only, written by the head scan and cleared by the finalized
  scanner's cursor advance. Nothing that affects money reads it.
- `quotes.metadata`, `deposits.metadata`, and `refunds.metadata` (`20261006080000_metadata`) are
  Stripe's metadata, checked by `metadata_is_valid(jsonb)`: an object of at most 50 strings with
  keys of 1 to 40 characters without `[` or `]` and values of 1 to 500 characters (an empty value
  unsets a key, so none is stored). A deposit is inserted with its quote's metadata. The same
  migration grants `deposits.write` to every holder of `quotes.write`.
- The heartbeat RPO target is the code constant `topup::heartbeat::RPO_SECONDS`, not a column.
- A `chain` reconciliation block written by the address-derivation or the per-forwarder custody
  check freezes that chain at runtime: pumps leave its deposits waiting, its scanner pauses, and
  quote creation answers `400 chain_frozen`; the service keeps serving other chains. No check
  writes the `address` scope since the flusher is gone. An operator lifts a block with the
  admin-signed
  `POST /v1/admin/reconciliation_blocks/{block_key}/lift` and a `reason`: it deletes the row and
  writes an `audit` row carrying the removed block in one transaction.

## Migration history

Each migration, in the order it applies.

`20261004000000_multi_tenant` is the base schema: docs/design/multi-tenant.md §14 on top of
docs/architecture.md §6. The pre-tenancy history (`20260922000000_initial_schema` through
`20261003000000_fast_credit`) was squashed into it without a data migration, because no
environment holding that history is kept: Phala's staging was reset for it
([deploy/phala.md, "Staging reset"](../../../deploy/phala.md#staging-reset-human-only)) and
production was never deployed. It runs only on an empty database; on a database that still holds the old history the
migrator refuses to start, because the applied versions are missing from the binary.

`20261005040000_chain_sourced_sweeps` (design PR 4) removes the operator flusher: `flushes`,
`flush_exclusions`, `deposits.flush_id`, the `flush` pause scope, and the treasury-inflow totals
of `reconciliation_custody_cursors`. It recreates `flushed` as the chain-sourced record of design
§14, adds `flush_failures` and `addresses.deployed_block`, and indexes credited deposits by
address for the sweep linkage.

`20261006000000_operator_onboarding` applies the 2026-09-28 amendment of the design (operator
onboarding, API only; design PR 5): it drops what `20261004000000` created for the dashboard
(`users`, `identities`, `passkeys`, `recovery_codes`, `memberships`, `invitations`, `sessions`,
the `role:*` grants, `accounts.business_profile`, `country`, `tos_acceptance`, `live_access`, and
the `user` audit actor), drops `request_signing_keys` and the per-object `quotes.idempotency_key`
and `refunds.idempotency_key`, and adds `accounts.contact`, `accounts.due_diligence`,
`events.actor`, and `api_keys.created_by` as the creating key id or `admin`.

`20261006080000_metadata` adds Stripe's metadata to quotes, deposits, and refunds, and grants
`deposits.write` to every holder of `quotes.write`
([schema notes](#points-the-schema-does-not-show-on-its-own)).

`20261007000000_deposit_addresses` restores deposit addresses (docs/design/multi-tenant.md §5a):
`deposit_addresses`, `addresses.deposit_address_id` with `addresses.quote_id` now nullable and
exactly one of them set, `account_limits.max_active_deposit_addresses`, and the
`deposit_addresses.read` and `.write` permissions. Its down migration refuses to run once a
deposit address exists.

`20261009000000_merchant_refunds` (design PR 9) replaces the operator refund workflow with the
merchant's two-step flow (design D5): statuses `pending`, `succeeded`, `failed`, `canceled`;
`chain_id`, `destination_address` (was `to_address`), `log_index`, and `failure_reason`; one
refund per transfer log (`refunds_transfer_unique`, over pending and succeeded refunds). It drops
`requested_by`, `approved_by`, `route`, `tx_version`, `confirmed_at`, the natural-key replay index,
and `refund_payment_claims`. Existing rows map `confirmed` to `succeeded` and every other status to
`pending`.

`20261010000000_one_deposit_address` applies the owner's 2026-09-28 decision of one deposit
address per customer across all chains and assets: it drops `deposit_addresses.chain_id`, `asset`,
and `route`, makes the version and the active address unique per customer, lets a deposit address
own one `addresses` row per chain (at most one without `superseded_at`, the current network), and
adds `addresses.superseded_at` for a network replaced after a treasury change, which stays watched
and credited. It refuses to run, and so does its down migration, while any deposit address exists
(none was issued in the per-chain shape). It is numbered after `20261009000000` so that it applies
after the refunds migration on a database that already has it.

`20261011000000_webhook_keys` (design PR 6) adds per-account, per-mode webhook keys (design D11):
`retiring_webhook_keys` keeps a rolled key version signing beside the new one until its
`expires_at`, and `accounts.webhook_key_version` is checked to hold a positive integer per mode.
No secret is stored: every key is derived from dstack at `settlement/{acct}/{live|test}/v{n}`.

`20261012000000_treasuries` (design PR 7) puts the `treasuries` table of `20261004000000` to use
(design D10): it adds `livemode`, `kind` (`eoa` or `contract`), and the lifecycle `applied_at` and
`replaced_at` (with `canceled_at` and its `cancellation_reason`, `requested` or `sanctioned`: pending,
current, replaced, canceled), indexes current treasuries by `screened_at` for the daily
re-screening, makes the proof and
`created_by` columns required, and allows one pending change and one current treasury per account,
mode, and chain. `treasury_challenges` holds the single-use EIP-4361 nonces, bound to the account,
mode, chain, and address, with the message issued. It indexes deposit address networks by account,
mode, and chain for the replacement a treasury change makes, and allows `treasury` event objects.
It refuses to run, and so does its down migration, while any treasury exists.

`20261013000000_webhook_endpoints` (design PR 8, numbered after design PR 7's
`20261012000000_treasuries`) lets merchants manage their endpoints:
`webhook_endpoints.description`, `metadata` (Stripe's, `metadata_is_valid`), and `deleted_at` (a
deleted endpoint is kept for the notice of its deletion), `disabled_reason` limited to `gone` (a failing
endpoint is never disabled), and one to 100 `enabled_events`. `webhook_deliveries` gains
`failed_at` (delivery stopped: a `410`, or the endpoint disabled or deleted), and `url` (an
endpoint's notice of its own change, sent to its previous URL whatever its status), with the
pending index per endpoint. `events.object_type` admits `webhook_endpoint` beside PR 7's
`treasury`, and
`events_scope_type_created_idx` serves `GET /v1/events?type=`. Its down migration discards
endpoint events and deleted endpoints.

`20261014000000_api_vocabulary` (design PR 10) adds `accounts.self_paused_scopes`, the merchant's
own `quotes` pause through `POST /v1/account/pause` (design §12), kept apart from the operator's
`paused_scopes` so a merchant's resume never lifts an operator pause;
`deposit_address_client_secrets`, the SHA-256 of the newest client secrets of each deposit address,
which read its public view (scoped through `deposit_addresses`, never updated); `flushed.id`, a
generated UUID of each event's identity that is the sweep's API id; and the `forwarders.read`
permission of `GET /v1/forwarders`. Its down migration folds a merchant's own
pause into `paused_scopes`.

`20261015000000_api_conformance` (the design's "API conformance" amendment) makes events
snapshots written once: `events.request_id` and `idempotency_key` record the API request behind an
event (`NULL` for the service's workers), a `NOT VALID` check requires `data.object` on every new
row, and `topup_app` loses `UPDATE` and `DELETE` on `events`. It renames the treasury events
`account.treasury.pending|updated|canceled` to `treasury.created|updated|canceled`, in stored
events and in endpoints' `enabled_events`, adds `webhook_endpoints.last_attempt_at` and
`last_attempt_status` (the endpoint's delivery health), and indexes undelivered deliveries by event
for `GET /v1/events?delivery_success=false`. Its down migration restores the grants and names.

`20261016200000_funds_direction` (the design's "launch hardening" amendment) issues restricted
keys: it removes `account.write` and `endpoints.write` from `key:restricted` in `permissions`
(restricted keys never manage keys, treasuries, webhook endpoints, webhook keys, or account
settings) and checks that `api_keys.permissions` is a JSON array. It adds
`treasuries.crediting_paused_by` (`merchant`, `operator`), the per-treasury crediting pause that
holds deposits to forwarders over the address, and `events.signing_key_version`, the retired
webhook key version that signs a key roll's `account.updated` whenever it is delivered. Its down
migration revokes restricted keys.

`20261017000000_ledger_correctness` applies the ledger correctness amendment of the design: it
renames `refunds.log_index` to `receipt_log_index`, the paying log's position in its receipt (a
pending refund's named log, block-wide until now, is cleared so any matching log pays it), adds
`refunds.paid_at` and the transaction's `tx_from` and `tx_nonce` kept when first read, and the
`transaction_dropped` and `transaction_not_found` failure reasons; adds
`deposits.finality_check_at`, the finality watch's per-deposit recheck time, with the unfinal
index extended to `(chain_id, block_number, id)`; and adds `accounts.max_unfinalized_credit`
(cents, default 100 000) with an index of credited deposits not final. Its down migration returns
refunds failed as dropped or never seen to `pending`.

`20261018000000_restore_mode` adds restore mode (architecture §14, design §13):
`restore_timeline` holds the PostgreSQL timeline the service last acknowledged (first the one the
migration runs on), so `topup run` finds a restore by a newer timeline; `restores` holds each
detected restore, and the one without `unfrozen_at` is the freeze (at most one, a partial unique
index), with the restore point and each chain's restored cursor; `restore_delivered_events` marks
the events imported from merchants' records as delivered after the restore point. Its down
migration drops the three tables and leaves imported events in `events`.

`20261019000000_account_limits` reads the open-quote caps from `account_limits` (design §12)
instead of the route file: `max_open_quotes`, `max_open_minor_account`, and
`max_open_minor_customer` become nullable, a null keeping the mode's default as
`max_active_deposit_addresses` already did, and `quotes_open_exposure_idx` indexes the open
reserved quotes per account and mode for the exposure check. Its down migration writes the
defaults into null caps before restoring `NOT NULL`.

`20261020000000_resumable_backfill` adds `addresses.backfilled_through`, the last block through
which the scanner committed an address's one-time backfill (architecture §8): a failed pass, a
restart, or a provider refusal resumes the backfill after it instead of reading the address's whole
range again, so a backfill longer than one provider budget still completes and the chain's
finalized cursor moves on. Its down migration drops the column; an unfinished backfill then restarts
from the address's creation block.

`20261021030000_claimable_deposits` narrows `deposits_claimable_idx` to `detected` and
`confirmed` deposits, the only ones the pump claims: a credited deposit waits for a finalized
`Flushed` event, which the scanner and the finality watch apply, so credited deposits that are
never swept no longer sit at the head of every claim's scan. Transitions the pump wrote for
credited deposits before (`flush_not_confirmed` waits) are kept: `transitions` is append-only.
Its down migration restores the index over every non-terminal state.

`20261021120000_atomic_idempotency` makes idempotent requests atomic (architecture §12): it adds
`idempotency_keys.owner`, the request that holds a key, a fresh id per claim that a takeover
replaces, so only the owner saves a response, in the transaction of the request's changes. It also
drops `permissions`: a secret key holds every permission and a restricted key all but
`api_keys.write`, `treasury.write`, `endpoints.write`, and `account.write`, now in code. Its down
migration recreates `permissions` with those grants and drops `owner`.

`20261021130000_deposit_revisions` lets a receipt position take a new deposit after its deposit was
reversed because another transfer is final there (architecture §7): `deposits.revision` counts the
deposits recorded at the position before a row (0 for every existing one, whose id is unchanged),
`deposits_chain_event_unique` becomes `(chain_id, tx_hash, receipt_log_index, revision)`, and the
partial unique index `deposits_chain_event_live_unique` keeps at most one deposit that is not
reversed per position. Its down migration refuses to run once a deposit with a revision above 0
exists.

`20261021160000_restore_correctness` keeps a restore from changing what merchants were told
(architecture §14, design §13): `restore_delivered_credits` holds the credit of each deposit in a
signature-verified delivered `deposit.credited` or `deposit.reversed` imported after a restore,
which the confirm step values the re-derived deposit at (a deposit whose transfer contradicts it is
held until the operator discards it, `discarded_at`); `quotes.restore_id` marks a quote re-issued
from the merchant's record, whose locked price is never applied. Its down migration drops both;
run it only while no re-issued quote is open.

`20261023000000_current_invariants` makes the invariants the service assumes of its rows
constraints, so a row that breaks one fails the migration instead of a worker: a deposit carries
its transaction's `tx_from` and `tx_nonce` unless it is a reversed deposit restored from a delivered
event, every event's `data` holds its `object`, and a deposit's snapshot carries its
`receipt_log_index`, `revision`, `block_hash`, and `block_time`. Its down migration drops them.

`20261026000000_rpc_groups` adds accepted public configurations, reviewed member genesis
evidence, persistent head/cursor anchors, chain freeze/recovery epochs, immutable numeric
window selectors with independent review markers, and owner-written recovery audits. Existing
0.6 height-only cursors require A/B hash agreement before runtime progress. Recovery preserves
old evidence and repairs derived address/backfill progress under the exclusive writer lock; see
[the RPC runbook](../../../deploy/RPC.md).
