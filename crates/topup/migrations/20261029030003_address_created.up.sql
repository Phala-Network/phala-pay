-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS addresses_chain_created_idx ON addresses (chain_id, created_block);
