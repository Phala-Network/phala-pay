-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS refunds_scope_created_idx ON refunds (account_id, livemode, created_at, id);
