-- Expand-only forward repair; the N-1 schema and migration checksums are preserved.
-- N-1 reissue writes do not know the new marker. Rebase their negative-evidence cursor
-- atomically when the history to scan expands, even while N-1 is running.
CREATE OR REPLACE FUNCTION reset_dual_address_coverage() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.created_block < OLD.created_block THEN
        NEW.dual_covered_through := NULL;
        UPDATE cursors
        SET scanned_block = GREATEST(NEW.created_block - 1, 0), scanned_block_time = NULL
        WHERE chain_id = NEW.chain_id AND scanned_block >= NEW.created_block;
    END IF;
    RETURN NEW;
END;
$$;

-- A restored/previously upgraded database may already have a cursor above dual coverage.
-- A NULL timestamp cannot authorize a negative decision before N reads its boundary again.
UPDATE cursors c
SET scanned_block = limits.boundary, scanned_block_time = NULL
FROM (
    SELECT coverage.chain_id,
        LEAST(coverage.through_block, COALESCE(min(GREATEST(
            COALESCE(a.dual_covered_through, GREATEST(a.created_block - 1, 0)),
            GREATEST(a.created_block - 1, 0))) FILTER (WHERE a.id IS NOT NULL), coverage.through_block)) AS boundary
    FROM chain_coverage coverage
    LEFT JOIN addresses a ON a.chain_id = coverage.chain_id
    GROUP BY coverage.chain_id, coverage.through_block
) limits
WHERE c.chain_id = limits.chain_id AND c.scanned_block > limits.boundary;
