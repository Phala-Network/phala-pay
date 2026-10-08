-- Expand-only: N-1 ignores these defaults and nullable confirmation history columns.
-- Counters are reserved before network reads and never refunded or reset by a retry.
ALTER TABLE deposits ADD COLUMN confirm_head_checks integer NOT NULL DEFAULT 0;
ALTER TABLE deposits ADD COLUMN confirm_receipt_checks integer NOT NULL DEFAULT 0;
ALTER TABLE deposits ADD COLUMN confirm_deadline_at timestamptz;
-- First slow-lane entry is immutable history, independent of unresolved finality history.
ALTER TABLE deposits ADD COLUMN first_slow_at timestamptz;

-- Only complete, terminal dual evidence can be reused for valuation. Earlier final_at
-- markers alone are deliberately insufficient for a detected deposit that entered S.
CREATE FUNCTION confirmation_terminal_evidence(deposit uuid)
RETURNS jsonb LANGUAGE sql STABLE AS $$
    SELECT evidence -> 'chain_confirmation' FROM transitions
    WHERE deposit_id = deposit
      AND evidence -> 'chain_confirmation' -> 'terminal' = 'true'::jsonb
    ORDER BY created_at DESC, id DESC LIMIT 1
$$;

-- One eligibility rule shared by read admission, pump selection, and DB-derived S stock.
CREATE FUNCTION deposit_finality_pending(deposit deposits)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT deposit.state <> 'reversed' AND (
        (deposit.state = 'detected'
         AND (deposit.first_unresolved_at IS NOT NULL OR deposit.confirm_receipt_checks > 0)
         AND confirmation_terminal_evidence(deposit.id) IS NULL)
        OR (deposit.state <> 'detected' AND deposit.final_at IS NULL))
$$;
