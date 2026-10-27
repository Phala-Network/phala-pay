-- Breaking: quotes reference reusable customer networks. N-1 must restore before rollback.
ALTER TABLE quotes ADD COLUMN address_id uuid;
ALTER TABLE quotes ADD COLUMN window_start timestamptz;
ALTER TABLE quotes ADD COLUMN replaced_by uuid;
UPDATE quotes q SET address_id = a.id, window_start = CASE WHEN q.restore_id IS NOT NULL THEN q.expires_at
    ELSE date_trunc('second', q.created_at) END
FROM addresses a WHERE a.quote_id = q.id;
ALTER TABLE quotes ALTER COLUMN address_id SET NOT NULL;
ALTER TABLE quotes ALTER COLUMN window_start SET NOT NULL;
ALTER TABLE quotes ADD CONSTRAINT quotes_address_fkey
    FOREIGN KEY (address_id, account_id, livemode)
    REFERENCES addresses (id, account_id, livemode) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE quotes ADD CONSTRAINT quotes_replaced_by_fkey
    FOREIGN KEY (replaced_by, account_id, livemode)
    REFERENCES quotes (id, account_id, livemode) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE deposits ADD COLUMN quote_id uuid;
UPDATE deposits d SET quote_id = q.id
FROM addresses a JOIN quotes q ON q.id = a.quote_id
WHERE d.address_id = a.id AND (d.price_source = 'lock' OR q.consumed_by = d.id);
ALTER TABLE deposits ADD CONSTRAINT deposits_quote_fkey
    FOREIGN KEY (quote_id, account_id, livemode) REFERENCES quotes (id, account_id, livemode);
CREATE INDEX quotes_address_window_idx ON quotes (address_id, route, expires_at)
    WHERE consumed_by IS NULL;
CREATE INDEX deposits_quote_idx ON deposits (quote_id) WHERE quote_id IS NOT NULL;
