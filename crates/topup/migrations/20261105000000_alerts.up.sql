CREATE TABLE alerts (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    grouping_key text NOT NULL,
    severity text NOT NULL,
    tags jsonb NOT NULL CHECK (jsonb_typeof(tags) = 'object'),
    runbook text NOT NULL,
    first_seen_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL,
    resolved_at timestamptz,
    acknowledged_at timestamptz,
    acknowledged_by text,
    silenced_until timestamptz,
    CHECK (last_seen_at >= first_seen_at),
    CHECK ((acknowledged_at IS NULL) = (acknowledged_by IS NULL))
);
CREATE UNIQUE INDEX alerts_active_group_idx ON alerts (name, grouping_key)
    WHERE resolved_at IS NULL;
CREATE INDEX alerts_created_idx ON alerts (first_seen_at DESC, id DESC);
CREATE INDEX alerts_resolved_idx ON alerts (resolved_at) WHERE resolved_at IS NOT NULL;
GRANT SELECT, INSERT, UPDATE, DELETE ON alerts TO topup_app;
