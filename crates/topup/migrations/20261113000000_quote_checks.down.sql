-- Preserve restart-safe rolling admission history through binary rollback/re-upgrade.
-- N-1 ignores this table; destructive rollback is deliberately unsupported.
SELECT 1;
