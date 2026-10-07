-- Run against the verified pre-upgrade restore copy, never by editing live data.
-- All historical addresses count; closing or retiring an address does not free capacity.
BEGIN READ ONLY;
SELECT chain_id, count(*) AS issued_addresses, 1000 AS address_cap
FROM addresses GROUP BY chain_id ORDER BY chain_id;
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM addresses GROUP BY chain_id HAVING count(*) > 1000) THEN
        RAISE EXCEPTION 'address capacity exceeded: refuse upgrade; review paid providers or token-wide scanning/indexer';
    END IF;
END
$$;
ROLLBACK;
