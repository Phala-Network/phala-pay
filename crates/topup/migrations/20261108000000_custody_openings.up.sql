-- Opening state is outside the address's inclusive discovery range. Evidence is immutable;
-- retry scheduling is kept separately so failed historical reads cannot monopolize new work.
CREATE TABLE custody_opening_balances (
    address_id uuid NOT NULL REFERENCES addresses(id),
    token text NOT NULL,
    start_block bigint NOT NULL CHECK (start_block > 0),
    block_hash text NOT NULL,
    amount_atomic numeric(78, 0) NOT NULL CHECK (amount_atomic >= 0),
    verified_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (address_id, token, start_block),
    CHECK (token ~ '^0x[0-9a-f]{40}$'),
    CHECK (block_hash ~ '^0x[0-9a-f]{64}$'),
    CHECK (amount_atomic = trunc(amount_atomic))
);
CREATE TABLE custody_opening_jobs (
    address_id uuid NOT NULL REFERENCES addresses(id),
    token text NOT NULL,
    start_block bigint NOT NULL CHECK (start_block > 0),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (address_id, token, start_block)
);
CREATE INDEX custody_opening_jobs_due_idx ON custody_opening_jobs (token, next_attempt_at);
GRANT SELECT, INSERT ON custody_opening_balances, custody_opening_jobs TO topup_app;
REVOKE UPDATE, DELETE ON custody_opening_balances FROM topup_app;
REVOKE DELETE ON custody_opening_jobs FROM topup_app;
REVOKE UPDATE ON custody_opening_jobs FROM topup_app;
GRANT UPDATE (attempts, next_attempt_at) ON custody_opening_jobs TO topup_app;
