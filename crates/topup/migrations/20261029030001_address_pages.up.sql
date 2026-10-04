-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS addresses_chain_page_idx ON addresses (chain_id, id);
