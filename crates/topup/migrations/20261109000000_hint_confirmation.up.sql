-- Expand-only: N-1 ignores the optional confirmation handoff. It never establishes final_at.
ALTER TABLE deposits ADD COLUMN hint_confirmation_evidence jsonb;

CREATE FUNCTION invalidate_hint_confirmation_evidence()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF ROW(NEW.chain_id,NEW.address_id,NEW.tx_hash,NEW.receipt_log_index,NEW.log_index,NEW.block_number,
           NEW.block_hash,NEW.block_time,NEW.asset_contract,NEW.from_address,NEW.amount_atomic,
           NEW.tx_from,NEW.tx_nonce)
       IS DISTINCT FROM
       ROW(OLD.chain_id,OLD.address_id,OLD.tx_hash,OLD.receipt_log_index,OLD.log_index,OLD.block_number,
           OLD.block_hash,OLD.block_time,OLD.asset_contract,OLD.from_address,OLD.amount_atomic,
           OLD.tx_from,OLD.tx_nonce)
       OR NEW.state <> 'detected'
       OR NEW.first_unresolved_at IS DISTINCT FROM OLD.first_unresolved_at THEN
        NEW.hint_confirmation_evidence := NULL;
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER invalidate_hint_confirmation_evidence BEFORE UPDATE ON deposits
FOR EACH ROW EXECUTE FUNCTION invalidate_hint_confirmation_evidence();
