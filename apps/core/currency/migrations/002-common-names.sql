-- Other names people use for a currency (`RMB` for CNY, `RM` for MYR), most
-- common first. The ISO code stays the key and what is stored and sent; these
-- only help people find a currency and read its label. Filled by seed
-- `currencies-iso4217-v2`.
ALTER TABLE currencies ADD COLUMN common_names text[] NOT NULL DEFAULT '{}';
