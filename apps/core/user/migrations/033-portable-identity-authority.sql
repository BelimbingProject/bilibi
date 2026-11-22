-- Issue 787: staff credentials belong to the installation and never to a
-- customer membership. Development databases are disposable, so this is a
-- clean schema break with no conversion path.
DO $$
BEGIN
 IF EXISTS (SELECT 1 FROM users) THEN
  RAISE EXCEPTION 'core-user-033-portable-identity-authority requires a fresh database';
 END IF;
END $$;

ALTER TABLE user_identities DROP CONSTRAINT user_identities_home_account_fkey;
ALTER TABLE user_identities DROP CONSTRAINT user_identities_audience_check;
ALTER TABLE user_identities DROP CONSTRAINT user_identities_home_tenant_id_home_user_id_key;
ALTER TABLE user_identities DROP COLUMN home_tenant_id, DROP COLUMN home_user_id;
ALTER TABLE user_identities ADD CONSTRAINT user_identities_audience_check CHECK (
 (kind='staff' AND tenant_id IS NULL) OR
 (kind='customer' AND tenant_id IS NOT NULL)
);

ALTER TABLE user_sign_ins
 ADD COLUMN authentication_authority text NOT NULL DEFAULT 'portable',
 ADD COLUMN authentication_tenant_id bigint,
 ADD COLUMN authentication_provider_id bigint,
 ADD COLUMN authentication_company_id bigint,
 ADD CONSTRAINT user_sign_ins_authentication_authority_check CHECK (
  (authentication_authority='portable'
   AND authentication_tenant_id IS NULL
   AND authentication_provider_id IS NULL
   AND authentication_company_id IS NULL)
  OR
  (authentication_authority='tenant_sso'
   AND authentication_tenant_id=tenant_id
   AND authentication_provider_id IS NOT NULL
   AND authentication_company_id IS NOT NULL)
 );
