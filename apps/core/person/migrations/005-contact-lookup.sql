-- Comparison only: never rewrite personal/work display values. Consent's
-- international policy accepts +, 00 or country-coded digits, 6..15 digits;
-- a national number's country is deliberately not inferred.
CREATE FUNCTION core_person_phone_key(value text) RETURNS text
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    WITH compact AS (SELECT translate(btrim(value), ' -().', '') AS value),
    digits AS (SELECT CASE
        WHEN left(value,1)='+' THEN substr(value,2)
        WHEN left(value,2)='00' THEN substr(value,3)
        ELSE value END AS value FROM compact)
    SELECT CASE WHEN value COLLATE "C" ~ '^[1-9][0-9]{5,14}$' THEN '+' || value END FROM digits
$$;

CREATE INDEX persons_phone_key ON persons(tenant_id, core_person_phone_key(phone), id);
CREATE INDEX persons_uncomparable_phone ON persons(tenant_id)
    WHERE phone IS NOT NULL AND core_person_phone_key(phone) IS NULL;
