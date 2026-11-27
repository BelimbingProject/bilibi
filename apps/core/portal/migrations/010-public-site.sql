-- ADR 34, slice 5: the portal site's address, title, logo and on/off are
-- the tenant's public site now (`base/public`, `public_sites`). The portal
-- keeps only its own option: whether people may ask for an account.
ALTER TABLE customer_portal_sites RENAME TO customer_portal_options;
ALTER TABLE customer_portal_options
 DROP COLUMN address,
 DROP COLUMN title,
 DROP COLUMN enabled,
 DROP COLUMN logo_asset_id;
