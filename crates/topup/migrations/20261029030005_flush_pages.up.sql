-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS deposits_flush_page_idx ON deposits (id) WHERE state = 'credited' AND final_at IS NOT NULL;
