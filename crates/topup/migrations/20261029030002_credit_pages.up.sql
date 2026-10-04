-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS deposits_credit_page_idx ON deposits (id)
WHERE credit_minor IS NOT NULL AND price_scaled IS NOT NULL AND route IS NOT NULL AND route_version IS NOT NULL;
