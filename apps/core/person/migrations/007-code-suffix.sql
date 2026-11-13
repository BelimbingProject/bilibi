-- The default code after `base_N` is the next number of the same length
-- (#591). Codes of one length and base sort as their numbers do in the C
-- collation, so this index finds the highest taken in one probe, however
-- many people share the base.
CREATE INDEX persons_code_suffix ON persons (tenant_id, length(code), (code COLLATE "C"));
