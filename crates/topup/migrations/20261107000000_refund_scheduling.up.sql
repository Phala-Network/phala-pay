-- Durable event-driven refund verification; no pending reservation is released by migration.
ALTER TABLE refunds
    ADD COLUMN origin_checked boolean NOT NULL DEFAULT false,
    ADD COLUMN observed_block bigint CHECK (observed_block >= 0),
    ADD COLUMN verify_checked_at timestamptz,
    ADD COLUMN verification_attempts bigint NOT NULL DEFAULT 0 CHECK (verification_attempts >= 0),
    ADD COLUMN replacement_started_at timestamptz,
    ADD COLUMN replacement_attempts smallint NOT NULL DEFAULT 0 CHECK (replacement_attempts BETWEEN 0 AND 8),
    ADD COLUMN replacement_hash text CHECK (replacement_hash ~ '^0x[0-9a-f]{64}$'),
    ADD CONSTRAINT refunds_replacement_epoch_check
        CHECK ((replacement_hash IS NULL) = (replacement_started_at IS NULL)),
    ADD CONSTRAINT refunds_replacement_distinct_check
        CHECK (replacement_hash IS NULL OR (tx_hash IS NOT NULL AND replacement_hash <> tx_hash));
COMMENT ON COLUMN refunds.observed_block IS
    'Untrusted receipt inclusion hint. No further RPC until the published checkpoint reaches it.';
COMMENT ON COLUMN refunds.replacement_hash IS
    'One merchant-supplied replacement candidate; only dual finalized receipt and matching sender/nonce proof can release the original reservation.';
COMMENT ON COLUMN refunds.tx_from IS
    'Sender of the original attachment, persisted only when both providers agree; nonce changes alone never resolve a refund.';
