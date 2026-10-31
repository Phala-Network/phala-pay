-- Endpoint ids and method names come only from the service's bounded transport labels.
CREATE TABLE rpc_daily_usage (
    day date NOT NULL,
    provider text NOT NULL CHECK (length(provider) BETWEEN 1 AND 40),
    chain_id bigint NOT NULL CHECK (chain_id > 0),
    method text NOT NULL CHECK (length(method) BETWEEN 1 AND 40),
    calls bigint NOT NULL CHECK (calls > 0),
    PRIMARY KEY (day, provider, chain_id, method)
);
REVOKE ALL ON rpc_daily_usage FROM PUBLIC, topup_app;
GRANT SELECT, INSERT, UPDATE ON rpc_daily_usage TO topup_app;

-- D counts first observation, including rejected rows; no full-history scan on each probe.
CREATE INDEX deposits_observed_day_idx ON deposits (created_at);
