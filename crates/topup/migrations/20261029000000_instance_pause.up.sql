-- Instance-wide request admission uses the pause mechanism, separately from business pauses.
-- A lease survives a CVM restart and expires even if the deployment runner disappears.
CREATE TABLE instance_pause (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    paused_scopes text[] NOT NULL CHECK (paused_scopes <@ ARRAY['mutations']::text[]),
    owner text NOT NULL,
    expires_at timestamptz NOT NULL
);
INSERT INTO instance_pause (paused_scopes, owner, expires_at) VALUES ('{}', '', now());
GRANT SELECT, UPDATE ON instance_pause TO topup_app;
REVOKE INSERT, DELETE ON instance_pause FROM topup_app;
