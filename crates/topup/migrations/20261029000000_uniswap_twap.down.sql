-- Binary rollback retains observations so a subsequent upgrade keeps the service's window.
-- Intentional no-op: N-1 does not use this expand-only table.
SELECT 1;
