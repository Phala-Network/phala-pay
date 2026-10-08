-- Expand-only: N-1 can keep inserting and updating deposits without this nullable column.
-- Claiming a deposit at the agreed finality checkpoint establishes its first due time.
-- Keep that anchor across rechecks, re-inclusion, restarts and binary rollback.
ALTER TABLE deposits ADD COLUMN finality_due_at timestamptz;
