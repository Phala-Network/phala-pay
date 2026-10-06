-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS quotes_scope_created_idx ON quotes (account_id, livemode, created_at, id);
