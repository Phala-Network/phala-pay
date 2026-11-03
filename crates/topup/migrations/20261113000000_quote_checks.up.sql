-- Expand-only; N-1 ignores these admission records and can keep using normal discovery.
CREATE TABLE quote_checks (
    quote_id uuid NOT NULL REFERENCES quotes(id),
    address_id uuid NOT NULL REFERENCES addresses(id),
    checked_at timestamptz NOT NULL,
    PRIMARY KEY (quote_id, checked_at)
);
CREATE INDEX quote_checks_address_time_idx ON quote_checks (address_id, checked_at);
REVOKE ALL ON quote_checks FROM PUBLIC, topup_app;
GRANT SELECT, INSERT ON quote_checks TO topup_app;
