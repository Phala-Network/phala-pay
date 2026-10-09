-- Expand-only: N-1 ignores these defaults and nullable confirmation history columns.
-- Counters are reserved before network reads and never refunded or reset by a retry.
ALTER TABLE deposits ADD COLUMN confirm_head_checks integer NOT NULL DEFAULT 0;
ALTER TABLE deposits ADD COLUMN confirm_receipt_checks integer NOT NULL DEFAULT 0;
ALTER TABLE deposits ADD COLUMN confirm_deadline_at timestamptz;
-- First slow-lane entry is immutable history, independent of unresolved finality history.
ALTER TABLE deposits ADD COLUMN first_slow_at timestamptz;
-- Exact provenance of the proof that established the current final marker.
ALTER TABLE deposits ADD COLUMN confirmation_terminal_transition_id uuid REFERENCES transitions(id);

-- N-1 can update final_at without knowing the reference. Such a marker is legacy again;
-- a new writer establishes its replacement marker and reference together.
CREATE FUNCTION invalidate_confirmation_terminal_reference()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.final_at IS DISTINCT FROM OLD.final_at
       AND NEW.confirmation_terminal_transition_id IS NOT DISTINCT FROM OLD.confirmation_terminal_transition_id THEN
        NEW.confirmation_terminal_transition_id := NULL;
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER invalidate_confirmation_terminal_reference
BEFORE UPDATE OF final_at ON deposits FOR EACH ROW
EXECUTE FUNCTION invalidate_confirmation_terminal_reference();


-- Only complete, terminal dual evidence can be reused for valuation. Earlier final_at
-- markers alone are deliberately insufficient for a detected deposit that entered S.
CREATE FUNCTION confirmation_terminal_evidence(deposit uuid)
RETURNS jsonb LANGUAGE sql STABLE AS $$
    SELECT evidence -> 'chain_confirmation' FROM transitions
    JOIN deposits ON deposits.id = transitions.deposit_id
    WHERE deposit_id = deposit
      AND deposits.final_at IS NOT NULL
      AND deposits.confirmation_terminal_transition_id = transitions.id
      AND evidence -> 'confirmation_proof_version' = '1'::jsonb
      AND evidence -> 'chain_confirmation' -> 'terminal' = 'true'::jsonb
      AND (evidence #> '{chain_confirmation,receipts,0,Included,transfer}')
          ?& ARRAY['to','token','from','amount','tx_from','tx_nonce']
      AND (evidence #> '{chain_confirmation,receipts,1,Included,transfer}')
          ?& ARRAY['to','token','from','amount','tx_from','tx_nonce']
      AND (deposits.first_unresolved_at IS NULL
           OR transitions.created_at >= deposits.first_unresolved_at)
    ORDER BY transitions.created_at DESC, transitions.id DESC LIMIT 1
$$;

-- One eligibility rule shared by read admission, pump selection, and DB-derived S stock.
-- An old final marker has to be checked even before S entry; only a fresh terminal
-- proof processed after entry returns ownership to price-only retries.
CREATE FUNCTION deposit_finality_pending(deposit deposits)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT deposit.state <> 'reversed' AND (
        (deposit.state = 'detected'
         AND (deposit.first_unresolved_at IS NOT NULL OR deposit.confirm_receipt_checks > 0
              OR deposit.final_at IS NOT NULL)
         AND confirmation_terminal_evidence(deposit.id) IS NULL)
        OR (deposit.state <> 'detected' AND deposit.final_at IS NULL))
$$;
