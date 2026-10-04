-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS deposits_custody_page_idx ON deposits (address_id, asset_contract, block_number);
