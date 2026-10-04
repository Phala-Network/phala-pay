-- Additive, replay-safe work cursors. Previous binaries ignore these tables.
CREATE TABLE scan_address_sweeps (
    chain_id bigint NOT NULL CHECK (chain_id >= 0),
    lane text NOT NULL CHECK (lane IN ('finalized', 'head', 'missing')),
    epoch bigint NOT NULL,
    anchor bigint NOT NULL CHECK (anchor >= 0),
    from_block bigint NOT NULL CHECK (from_block >= 0),
    through_block bigint NOT NULL CHECK (through_block >= 0),
    block_time timestamptz,
    horizon bigint,
    last_id uuid NOT NULL,
    PRIMARY KEY (chain_id, lane)
);
CREATE TABLE reconciliation_work_cursors (
    check_name text NOT NULL,
    chain_id bigint NOT NULL,
    last_id uuid,
    PRIMARY KEY (check_name, chain_id)
);
