-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS addresses_scope_page_idx ON addresses (account_id, livemode, id);
