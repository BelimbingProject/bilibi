-- A browser session keeps the authority that admitted it. Portable proof is
-- installation-owned; company SSO proof is confined to its tenant/provider.
ALTER TABLE sessions
 ADD COLUMN authentication_authority text NOT NULL,
 ADD COLUMN authentication_tenant_id bigint,
 ADD COLUMN authentication_provider_id bigint,
 ADD COLUMN authentication_company_id bigint,
 ADD CONSTRAINT sessions_authentication_authority_check CHECK (
  (authentication_authority='portable'
   AND authentication_tenant_id IS NULL
   AND authentication_provider_id IS NULL
   AND authentication_company_id IS NULL)
  OR
  (authentication_authority='tenant_sso'
   AND authentication_tenant_id IS NOT NULL
   AND authentication_tenant_id=tenant_id
   AND authentication_provider_id IS NOT NULL
   AND authentication_company_id IS NOT NULL)
 );
