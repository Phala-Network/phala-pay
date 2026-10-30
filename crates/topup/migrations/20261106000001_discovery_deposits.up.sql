-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS deposits_discovery_activity_idx ON deposits (chain_id, block_time, address_id);
