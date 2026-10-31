-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS deposits_observed_day_idx ON deposits (created_at);
