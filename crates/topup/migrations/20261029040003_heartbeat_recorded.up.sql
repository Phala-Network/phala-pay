-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS heartbeat_recorded_at_idx ON heartbeat (recorded_at);
