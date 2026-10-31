-- Old binaries and restored historical quotes leave NULL, conservatively retaining exposure.
ALTER TABLE quotes ADD COLUMN pricing_mode text
    CHECK (pricing_mode IN ('stablecoin', 'spot'));

CREATE FUNCTION preserve_quote_pricing_mode() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.pricing_mode IS DISTINCT FROM OLD.pricing_mode THEN
        RAISE EXCEPTION 'quote pricing mode is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER quotes_pricing_mode_immutable BEFORE UPDATE ON quotes
    FOR EACH ROW EXECUTE FUNCTION preserve_quote_pricing_mode();
