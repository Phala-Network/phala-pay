-- Finalized dual-log evidence has no receipt-local identity. Keep exact block aggregates for
-- custody, including tokens that acquire a route later; samples are operational, not accounting.
CREATE TABLE ignored_transfer_totals (
    id uuid PRIMARY KEY,
    chain_id bigint NOT NULL CHECK (chain_id > 0),
    address_id uuid NOT NULL REFERENCES addresses(id),
    token text NOT NULL CHECK (token ~ '^0x[0-9a-f]{40}$'),
    block_number bigint NOT NULL CHECK (block_number >= 0),
    block_hash text NOT NULL CHECK (block_hash ~ '^0x[0-9a-f]{64}$'),
    amount_atomic numeric NOT NULL CHECK (amount_atomic >= 0 AND amount_atomic = trunc(amount_atomic)),
    event_count bigint NOT NULL CHECK (event_count > 0),
    evidence_hash text NOT NULL,
    observed_day date NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (address_id, token, block_hash)
);
CREATE INDEX ignored_transfer_custody_idx ON ignored_transfer_totals (chain_id, token, block_number, address_id);
CREATE INDEX ignored_transfer_page_idx ON ignored_transfer_totals (created_at, id);
CREATE TABLE ignored_transfer_samples (
    total_id uuid NOT NULL REFERENCES ignored_transfer_totals(id),
    chain_id bigint NOT NULL,
    address_id uuid NOT NULL REFERENCES addresses(id),
    token text NOT NULL,
    observed_day date NOT NULL,
    block_hash text NOT NULL,
    log_index bigint NOT NULL CHECK (log_index >= 0),
    tx_hash text NOT NULL CHECK (tx_hash ~ '^0x[0-9a-f]{64}$'),
    from_address text NOT NULL CHECK (from_address ~ '^0x[0-9a-f]{40}$'),
    amount_atomic numeric(78,0) NOT NULL CHECK (amount_atomic >= 0),
    reason text NOT NULL CHECK (reason IN ('unsupported_asset','below_route_minimum','zero_amount','previously_ignored')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, block_hash, log_index)
);
CREATE INDEX ignored_transfer_sample_total_idx ON ignored_transfer_samples (total_id, log_index);
CREATE INDEX ignored_transfer_sample_limit_idx ON ignored_transfer_samples (address_id, token, observed_day);
CREATE INDEX ignored_transfer_sample_retention_idx ON ignored_transfer_samples (created_at);
CREATE TABLE ignored_transfer_daily (
    chain_id bigint NOT NULL,
    address_id uuid NOT NULL REFERENCES addresses(id),
    day date NOT NULL,
    event_count bigint NOT NULL CHECK (event_count > 0),
    PRIMARY KEY (chain_id, day, address_id)
);
REVOKE UPDATE, DELETE ON ignored_transfer_totals FROM topup_app;
REVOKE UPDATE ON ignored_transfer_samples FROM topup_app;
GRANT SELECT, INSERT ON ignored_transfer_totals TO topup_app;
GRANT SELECT, INSERT, DELETE ON ignored_transfer_samples TO topup_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ignored_transfer_daily TO topup_app;
