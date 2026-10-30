-- Run against a verified restore copy, using `topup config show` from the target release/kit.
-- Pass that validated public JSON as psql -v config=...; no live database edits.
-- All historical addresses count; closing or retiring an address does not free capacity.
BEGIN READ ONLY;
SELECT set_config('topup.capacity_config', :'config', true) IS NOT NULL AS target_configuration_loaded;
WITH caps AS (
    SELECT * FROM jsonb_to_recordset(current_setting('topup.capacity_config')::jsonb->'rpc')
        AS c(chain_id bigint, max_issued_addresses bigint)
), counts AS (
    SELECT chain_id, count(*) AS issued_addresses FROM addresses GROUP BY chain_id
)
SELECT coalesce(caps.chain_id, counts.chain_id) AS chain_id,
    coalesce(issued_addresses, 0) AS issued_addresses, max_issued_addresses AS address_cap
FROM caps FULL JOIN counts USING (chain_id) ORDER BY chain_id;
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM jsonb_to_recordset(current_setting('topup.capacity_config')::jsonb->'rpc')
            AS c(chain_id bigint, max_issued_addresses bigint)
    ) OR EXISTS (
        SELECT 1 FROM (
            SELECT chain_id, count(*) AS issued_addresses FROM addresses GROUP BY chain_id
        ) counts FULL JOIN jsonb_to_recordset(current_setting('topup.capacity_config')::jsonb->'rpc')
            AS caps(chain_id bigint, max_issued_addresses bigint) USING (chain_id)
        WHERE max_issued_addresses IS NULL OR max_issued_addresses NOT BETWEEN 1 AND 1000000
            OR issued_addresses > max_issued_addresses
    ) THEN
        RAISE EXCEPTION 'address capacity exceeded or missing target chain cap: refuse upgrade/rollback; review target configuration and provider budget';
    END IF;
END
$$;
ROLLBACK;
