-- Expand-only: N-1 can keep inserting and updating deposits without this nullable column.
-- Claiming a deposit at the agreed finality checkpoint establishes its first due time.
-- Keep that anchor across rechecks, re-inclusion, restarts and binary rollback.
ALTER TABLE deposits ADD COLUMN finality_due_at timestamptz;
-- Record the first unresolved check once, including RPC failures. Resolution never clears it.
ALTER TABLE deposits ADD COLUMN first_unresolved_at timestamptz;

-- One schedule for confirm and watcher; N-1 does not call this additive function.
CREATE FUNCTION finality_next_check_at(anchor timestamptz, checked_at timestamptz)
RETURNS timestamptz LANGUAGE sql IMMUTABLE AS $$
    SELECT checked_at + make_interval(secs => CASE
        WHEN checked_at < COALESCE(anchor, checked_at) + interval '10 minutes' THEN 60
        WHEN checked_at < COALESCE(anchor, checked_at) + interval '6 hours' THEN 600
        ELSE 3600
    END)
$$;
