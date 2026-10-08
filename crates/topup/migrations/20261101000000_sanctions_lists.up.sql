-- Expand-only: N-1 ignores these tables; retain them on binary rollback.
CREATE TABLE IF NOT EXISTS sanctions_list_snapshots (
    id uuid PRIMARY KEY,
    source text NOT NULL CHECK (source = 'ofac_sdn'),
    publish_date date NOT NULL,
    sha256 bytea NOT NULL CHECK (octet_length(sha256) = 32),
    record_count bigint NOT NULL CHECK (record_count > 0),
    address_count bigint NOT NULL CHECK (address_count >= 0),
    fetched_at timestamptz NOT NULL,
    activated_at timestamptz,
    verified_at timestamptz NOT NULL,
    UNIQUE (source, sha256)
);
-- The latest activation is active; preserve historical activation evidence.
CREATE INDEX IF NOT EXISTS sanctions_list_latest
    ON sanctions_list_snapshots (source, activated_at DESC, id DESC);
CREATE TABLE IF NOT EXISTS sanctions_list_addresses (
    snapshot_id uuid NOT NULL REFERENCES sanctions_list_snapshots(id),
    sdn_uid bigint NOT NULL,
    id_type text NOT NULL,
    raw_value text NOT NULL,
    evm_address bytea CHECK (octet_length(evm_address) = 20),
    PRIMARY KEY (snapshot_id, sdn_uid, id_type, raw_value)
);
CREATE INDEX IF NOT EXISTS sanctions_list_evm_address
    ON sanctions_list_addresses (evm_address, snapshot_id);
CREATE TABLE IF NOT EXISTS sanctions_manual_entries (
    evm_address bytea PRIMARY KEY CHECK (octet_length(evm_address) = 20),
    reason text NOT NULL CHECK (length(reason) BETWEEN 1 AND 1000),
    source_ref text NOT NULL CHECK (length(source_ref) BETWEEN 1 AND 1000),
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    removed_by text,
    removed_at timestamptz,
    CHECK ((removed_by IS NULL) = (removed_at IS NULL))
);
REVOKE ALL ON sanctions_list_snapshots, sanctions_list_addresses, sanctions_manual_entries
    FROM PUBLIC, topup_app;
GRANT SELECT, INSERT, UPDATE ON sanctions_list_snapshots, sanctions_manual_entries TO topup_app;
GRANT SELECT, INSERT ON sanctions_list_addresses TO topup_app;
