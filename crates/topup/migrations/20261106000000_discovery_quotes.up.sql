-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS quotes_discovery_activity_idx ON quotes (created_at, address_id);
