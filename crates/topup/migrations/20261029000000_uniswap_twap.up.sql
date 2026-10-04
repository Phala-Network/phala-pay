-- Expand-only: the previous binary ignores this independent observation table.
-- No payment rows, existing constraints or defaults change. Keep history on binary rollback.
CREATE TABLE price_twap_observations (
    policy text NOT NULL,
    block_number bigint NOT NULL CHECK (block_number >= 0),
    block_timestamp bigint NOT NULL CHECK (block_timestamp >= 0),
    sample jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (policy, block_number),
    UNIQUE (policy, block_timestamp)
);
-- New, empty table: a transactional index is bounded and needs no concurrent build.
CREATE INDEX price_twap_observations_window ON price_twap_observations (policy, block_timestamp DESC);
REVOKE ALL ON price_twap_observations FROM PUBLIC, topup_app;
GRANT SELECT, INSERT ON price_twap_observations TO topup_app;
