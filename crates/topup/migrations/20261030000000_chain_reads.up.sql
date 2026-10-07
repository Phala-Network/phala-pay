-- Expand-only: N-1 continues reading and writing its existing columns and tables.
CREATE TABLE chain_checkpoints (
    chain_id bigint PRIMARY KEY CHECK (chain_id > 0),
    block_number bigint NOT NULL CHECK (block_number >= 0),
    block_hash text NOT NULL,
    block_time timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE chain_coverage (
    chain_id bigint PRIMARY KEY CHECK (chain_id > 0),
    through_block bigint NOT NULL CHECK (through_block >= 0),
    through_hash text NOT NULL,
    through_time timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE daily_budgets (
    day date NOT NULL,
    name text NOT NULL,
    used integer NOT NULL DEFAULT 0 CHECK (used >= 0),
    PRIMARY KEY (day, name)
);
ALTER TABLE addresses ADD COLUMN dual_covered_through bigint CHECK (dual_covered_through >= 0);
ALTER TABLE deposits ADD COLUMN dual_verified_at timestamptz;
ALTER TABLE quotes ADD COLUMN cancel_requested_at timestamptz;
REVOKE ALL ON chain_checkpoints, chain_coverage, daily_budgets FROM PUBLIC, topup_app;
GRANT SELECT, INSERT, UPDATE ON chain_checkpoints, chain_coverage, daily_budgets TO topup_app;

-- N-1 can lower created_block on reissue without knowing the new coverage marker.
-- Reset only evidence of the expanded range; all N-1 columns keep their existing semantics.
CREATE FUNCTION reset_dual_address_coverage() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.created_block < OLD.created_block THEN
        NEW.dual_covered_through := NULL;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER reset_dual_address_coverage BEFORE UPDATE OF created_block ON addresses
    FOR EACH ROW EXECUTE FUNCTION reset_dual_address_coverage();
