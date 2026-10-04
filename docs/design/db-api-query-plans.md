# RPC queue query plans

Status: implemented in v0.8.0; query-plan and migration recovery evidence.

The database/API audit uses PostgreSQL 18.6 and SQLx 0.9.0. The reproducible test is
[`rpc_query_plans.rs`](../../crates/topup/tests/rpc_query_plans.rs). It creates an isolated migrated
database, inserts 120,000 review windows and 120,000 reorg ranges across four chains and three
epochs, and marks 90% complete. Half of the remaining observations have replay progress.
`ANALYZE` collects statistics; the test keeps the planner's default settings. It rolls back each
`EXPLAIN ANALYZE` update so the logical before/after data is identical.

## Results

The review query is `due_reviews` in `db/rpc.rs`: active epoch, unreviewed windows, ordered by
`COALESCE(replayed_at, created_at), from_block, id`, limited to 16. The replay query is the exact
`commit_head_window` update, with chain 1 and block range 1,000,000–3,000,000.

| Query | Before | After |
|---|---|---|
| Review | Bitmap heap scan through `rpc_window_reviews_pending`; 6,000 candidates, 4,000 removed by epoch filter; top-N sort | `rpc_window_reviews_due_idx` index scan; chain and epoch index conditions; 16 rows, no sort |
| Review execution / shared buffer hits | 6.510 ms / 1,962 | 0.125 ms / 20 |
| Reorg | Sequential scan of 120,000 rows, 119,666 removed | Bitmap index/heap scan through `rpc_reorg_ranges_pending_idx`, 334 matching rows |
| Reorg execution / scan buffer hits | 14.569 ms / 1,466 | 2.469 ms / 251 |

These are local warm-cache measurements, not a production latency guarantee. Update totals also
include writes and trigger/index maintenance. The regression test asserts index validity, the
absence of the redundant old review index, the review's sort-free scan, and the reorg index path;
it does not assert timings. The test prints full `EXPLAIN (ANALYZE, BUFFERS)` output.

The review index begins with `(chain_id, epoch)` and includes its full ordering expression, so
`LIMIT 16` stops the index walk early. Its `reviewed_at IS NULL` predicate excludes completed
history and its leading keys also serve pending review counts. The reorg index begins with
`(chain_id, epoch)` and its next-block expression, with the same incomplete-range predicate as
replay and pending-range counts. No additional count or watermark index is added: these leading
keys and the existing watermark primary key already cover them.

## Concurrent migration behavior

SQLx's official source at the commit published in the 0.9.0 crates verifies both required steps:

- [`sqlx-core/src/migrate/source.rs`](https://github.com/launchbadge/sqlx/blob/003b698e99e024f3621b8043a2426fde5b741171/sqlx-core/src/migrate/source.rs#L234)
  uses `sql.starts_with("-- no-transaction")` to set the migration's `no_tx` flag.
- [`sqlx-postgres/src/migrate.rs`](https://github.com/launchbadge/sqlx/blob/003b698e99e024f3621b8043a2426fde5b741171/sqlx-postgres/src/migrate.rs#L231)
  executes `no_tx` migrations directly on the connection rather than in an explicit transaction.

Each up/down file begins with that directive and contains exactly one concurrent create or drop.
PostgreSQL runs multiple commands sent in one simple-query message in an implicit transaction
([protocol documentation](https://www.postgresql.org/docs/18/protocol-flow.html#PROTOCOL-FLOW-MULTI-STATEMENT)),
so putting two concurrent statements into one nontransactional SQLx file still fails. The test
actually undoes the three migrations through SQLx to the pre-index version, captures the baseline,
then reapplies them and verifies that both indexes are valid. Existing applied migrations are
unchanged. The replacement review index is created before the old one is dropped; undo restores
the old index before removing the replacement.

## Deploy recovery

Concurrent index creation can leave an invalid index after cancellation or failure
([PostgreSQL documentation](https://www.postgresql.org/docs/18/sql-createindex.html#SQL-CREATEINDEX-CONCURRENTLY)).
DDL and SQLx's bookkeeping are not atomic. Operators rerun the normal Deploy migration service
(`topup migrate`); database shell access is not required.

The production entry point commits the payment-settings schema/backfill transaction through
`20261024000000` before running later migrations on a standalone connection. The cutover retains
its atomic schema/backfill validation and transaction advisory lock. The existing named migration advisory
lock covers the queue-index inspection, recovery and the original embedded migrations.
The same connection acquires the existing `payment-settings-migration` lock before any outer
transaction and retains it through all phases. Acquisition uses PostgreSQL's nonblocking
`pg_try_advisory_lock`, with a wait bounded by the pool's acquisition timeout. Each attempt ends
its snapshot before waiting; a blocking advisory-lock query can otherwise deadlock with an
expression index build waiting for old snapshots. SQLx's supported `set_locking(false)` disables
its nested lock, because the outer session lock already serializes normal migration commands.
The connection closes on cancellation and after migration, so no lock survives in the pool.

Recovery is limited to the two named queue indexes and their pending migration versions. It
requires the RPC epoch migration to be recorded, no recorded target/later or dirty migration,
and an object in `public` whose complete `pg_get_indexdef` matches the migration's table,
columns, expressions, predicate and index method. A mismatching object fails closed. A matching
invalid index is dropped concurrently, then the original migration rebuilds it. A matching valid
but unrecorded index retains its OID; after verification, `CREATE INDEX CONCURRENTLY IF NOT EXISTS`
finishes the migration's bookkeeping. The guard never drops a valid index and never silently
skips an invalid index. The old-index drop uses `IF EXISTS` so interruption after the drop and
before bookkeeping is also retryable. Other schemas and recorded indexes are untouched.

The CLI regression tests hold uncommitted writers on each queue table and invoke the actual
`topup migrate` binary with its production `lock_timeout = '30s'`. Session budgets override
connection URL timeout options; the tests use the normal migration entry point and budgets. Both
builds fail and leave
`indisvalid = false` with no successful migration record. After resolving the writer, another
normal CLI invocation reports cleanup, rebuilds the index with a new OID, records the migration,
and succeeds again on an idempotent retry. Separate tests verify that a valid completed build
keeps its OID and that valid or invalid unrelated definitions under a reserved name are refused
without mutation.

A same-name object with a different definition is not an interrupted build of these migrations.
Deploy stops with a specific recovery error rather than deleting that object. It requires a
reviewed correction to the unexpected schema, outside automatic queue-index recovery.

## Reproduce

Use the repository's database test setup with an isolated PostgreSQL 18 instance, then run:

```sh
CARGO_BUILD_JOBS=2 SQLX_OFFLINE=true CI=true cargo test --locked -p topup \
  --test rpc_query_plans -- --nocapture --test-threads=1
```

Set `OWNER_DATABASE_URL` and `DATABASE_URL` as described in
[CONTRIBUTING.md](../../CONTRIBUTING.md#rust-service). The harness removes its own database and
login role on completion.
