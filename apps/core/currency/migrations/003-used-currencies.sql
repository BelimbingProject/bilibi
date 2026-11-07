-- A tenant no longer switches currencies on and off: a currency joins the
-- tenant's set the first time it is used (a base currency, a rate, or a new
-- record that `Currencies::check` accepts), and currency controls list those
-- first. The table says what it now holds.
ALTER TABLE currency_tenant_enabled RENAME TO currency_tenant_used;
ALTER TABLE currency_tenant_used RENAME COLUMN enabled_at TO first_used_at;
ALTER TABLE currency_tenant_used RENAME CONSTRAINT currency_tenant_enabled_pkey TO currency_tenant_used_pkey;
ALTER TABLE currency_tenant_used RENAME CONSTRAINT currency_tenant_enabled_id_key TO currency_tenant_used_id_key;
ALTER TABLE currency_tenant_used RENAME CONSTRAINT currency_tenant_enabled_tenant_id_fkey TO currency_tenant_used_tenant_id_fkey;
ALTER TABLE currency_tenant_used RENAME CONSTRAINT currency_tenant_enabled_code_fkey TO currency_tenant_used_code_fkey;
ALTER SEQUENCE currency_tenant_enabled_id_seq RENAME TO currency_tenant_used_id_seq;
