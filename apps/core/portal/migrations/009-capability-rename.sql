-- The staff capability keys moved from admin.customer-access.* to
-- admin.portal.* when the module became Core Portal. Stored grants (system
-- role reconciliation, custom roles, principal allows and denies) still hold
-- the old keys, so rename them here: an old key grants nothing now.
UPDATE role_capabilities SET capability = 'admin.portal.view'
 WHERE capability = 'admin.customer-access.view';
UPDATE role_capabilities SET capability = 'admin.portal.manage'
 WHERE capability = 'admin.customer-access.manage';
UPDATE principal_capabilities SET capability = 'admin.portal.view'
 WHERE capability = 'admin.customer-access.view';
UPDATE principal_capabilities SET capability = 'admin.portal.manage'
 WHERE capability = 'admin.customer-access.manage';
UPDATE authz_system_grants SET capability = 'admin.portal.view'
 WHERE capability = 'admin.customer-access.view';
UPDATE authz_system_grants SET capability = 'admin.portal.manage'
 WHERE capability = 'admin.customer-access.manage';
