# Settings editor (P3)

**Use it for:** showing your settings on the System Settings page (give the setting `editor` metadata).

**Not for:** a settings page of your own.

`/system/settings`, titled **System Settings**, is the operator page for contributed settings. It is its own module because it needs Authz, and Authz depends on `base/settings`: storage stays below Authz, the screen sits above it.

## Tenant agent operations

The agent uses the caller's tenant and the [settings their owners delegate](../settings/README.md). The guide `base.settings_editor` walks through discovery, review and restore.

- `base.settings_editor.tenant.list` requires `base.settings.tenant.view`. It returns the delegated settings allowed by the caller's live field capabilities, their effective source (`tenant`, `global` or `default`), and whether the tenant has an override (`set_for_tenant`).
- `base.settings_editor.tenant.set` takes an exact key and a string value. The editor trims surrounding whitespace before parsing the string for the declared type. In the review card, choose an available non-secret setting before entering a value. Set creates an override even when the value equals the inherited value.
- `base.settings_editor.tenant.restore` takes an exact key and removes the tenant override so inheritance resumes. Both writes recheck `base.settings.tenant.manage` and the definition's field capability in the mutation transaction, and apply any owner-specific guard. Settings supplies validation and audit.

Encrypted values are always `null` in list, preview and result data. The list reports whether a secret is set. Agent Set refuses encrypted settings and directs the person to the operator's **System Settings** page with its existing checks. Agent Restore of an encrypted setting requires `base.settings_editor.tenant.secret.change` step-up.

This module contributes a **Tenant settings** [customisation section](../labels/README.md#what-can-be-changed-agents), so `base.labels.customisations.discover` lists every delegated setting with its source, declared limits and whether this tenant overrides it; a secret's value is restricted there too. Its rows on the read-only Customisations page are still to come (issue #797); it does not add a tenant settings screen.

Run the `tenant-settings` [walkthrough](../../../docs/runtime-parity.md) to exercise list, Set and Restore against two fixture tenants. It checks source reporting, mutation audit, secret masking and refusal of encrypted Set in the shared review card.

## Table

Settings appear in the shared `FlexTable`, with Setting, Value and Area columns. The company picker uses the same table; per-person settings do not use this module.

- Search matches a setting's name, key, description and current non-secret value as you type. Secret values are never searched or decrypted for the table.
- Area and Module filters follow search, then Settings scope, in the shared toolbar, wrapping below it on narrow screens. Options come only from settings the person may see in the selected scope; search and both filters combine before sorting and pagination.
- Click a heading to sort; click again to reverse. `ListState` keeps search, area, module, sort, company and pagination in the URL.
- Click a Value cell to open only its editor. Save or Enter saves; Cancel or Escape discards the draft. A successful save returns to reading. Validation and 2FA refusals stay beside the value.
- Protected values use the shared `StepUp` form, with setup guidance before enrollment and an inline `ProofInput` method choice and code afterwards.

## Fields

A field is every `base/settings.definitions` contribution with `editor` metadata, a concrete key (no wildcard) and the selected scope (global, tenant or company), shown as rows in the table. Declaring such a setting is all a module does to appear here. A field whose `editor.capability` the actor does not hold right now is withheld: it is not rendered and a forged submission naming it is ignored. When every field is withheld the page names the missing capabilities instead of claiming nothing was contributed.

Values are read with `Settings::resolve_masked`, so an encrypted value is never decrypted to draw the page; a stored secret shows the keep-current mask. Without a valid `BILIBI_SETTINGS_KEY` an encrypted field is read-only (it can still be cleared by Restore Defaults, which needs no key).

A module can contribute `ImageSetting` under `base/settings_editor.image-settings` with the same ID as a company-scoped setting. Its Value editor becomes a PNG/JPEG/WebP file input backed by Media, with Upload and Remove actions; real bytes are inspected and the stored setting is the Media asset ID. The contribution supplies the image noun.

A global image contribution with `staging` shows a standalone upload in Global scope for callers with the declared capability. Use a unique upload input name accepted by `UploadSpec`. It returns a checked Media asset ID for the owner's reviewed operation and leaves the setting untouched.

Saved overrides and runtime state live in `base_settings`; inherited module defaults have no stored row.

## Writes

Route access is `OperatorCapability(base.settings.global.manage)`. Every Save and Restore re-asks `authorize_operator` (logged) and rebuilds the field plan from live grants before acting; a refusal flashes and redraws, it never clears the page. A Save is planned in full, then applied in one `MutationTransaction` (shared mutation audit): a blank clears this scope's override, the mask keeps a secret, and a value equal to what the field already shows is not written. Messages say what happened ("1 setting updated, 1 override cleared."). Restore confirms through the shared dialog, which counts every changed setting in the current scope, including rows hidden by search or on other pages, and deletes only those that are still overridden. Changing the scope to Global from the URL or Back drops a pending restore.

A setting that a Base Step-up operation guards (`Operation::guards_setting`, declared `.always()`; for example the Software GitHub read token, `base.software.github_token.change`) asks for the holder's 2FA code every time before Save or Restore touches it. The row asks for a code and keeps a refused save open; while viewing as someone else it says the setting cannot be changed instead. Restore uses the host confirmation page. Other fields ask for nothing.

## Stored-secret reveal

"Show stored value" appears only for an actor holding `base.settings.secret.view` by name (`authz::explicitly_granted`; `grant_all` does not count) when the key is configured. Showing it is the Base Step-up operation `base.settings_editor.secret.reveal` ("Show a stored secret setting"), on the installation floor; it replaced the password prompt. When the session owes a check, the Value cell asks for the code inline and the person submits again; the authenticator locks repeated failures. Every attempt, including a forged request, records a retained `settings.secret.reveal` audit action without the value, and a reveal whose audit row cannot be written shows nothing. The value reaches the browser only as a `LiveBody.events` push to the `SecretStored` hook (`bilibi:secret:reveal`), which re-masks it after the definition's `reveal_duration_ms`.

A definition whose editor sets `NEVER_REVEALED` (`base/settings`) has no "Show stored value", and a request to reveal it is refused and recorded like a forged one.

## Tenant scope

The scope picker offers **This tenant** after Global, for everyone on the page: the defaults the actor's own tenant gives its companies, over the global ones. Issue 130 asks for tenant defaults (Search's `search.kinds` and `search.boost`).

- It lists the definitions with the tenant scope (Search's settings, Datetime's `localization.timezone`), notes "Inherited from global" or "the default", and labels the restore button "Clear overrides".
- Writes go under `Target::Tenant` as `settings.tenant.save`/`restore`, authorized by the route capability that every write already re-asks (`authorize_operator`); Settings binds the tenant from the scope, so no other tenant changes.
- The page sets the operator tenant's defaults. For delegated customer tenant settings, see [Tenant agent operations](#tenant-agent-operations).

## Company scope (K15)

The scope picker always offers Global and This tenant. Accounts with `base.settings.company.manage` also see the companies Tenancy's organisation directory (`PartyProofs::selectable_organisations`, bound by Core Company) says this actor may manage with that capability: their current workplace, or every company of the tenant with `admin.company.tenant-wide.manage`. Other desks do not widen company-record administration. The select never lists every company: it offers the picked company and at most 20 companies that its "Find a company" box matches (by name, legal name or code), kept in the address as `company_search`. Others see "You do not have permission to manage company settings."; an actor the directory names no company for is told no company is available.

A picked company is an id, never a proof: `switch_scope` reproves it (`organisation_target`) and every Save and Clear reproves it again plus the company's standing (`PartyProofs::organisation`). A denied, missing or cross-tenant company is refused with "You do not have permission to manage this company's settings."; an archived one with "This company is archived and read-only, so its settings were not changed." and is never offered by the picker. Under a company the page lists the definitions with the company scope (Datetime's `localization.timezone`, gated by `admin.company.update`), marks overrides "Set here", notes "Inherited from global/tenant/the default", writes under `Target::Company` as `settings.company.save`/`restore` and labels the restore button "Clear overrides". The K15 walkthrough (`apps/core/company/examples/k15-owner-binders.rs`) exercises the same seam.

## Manual walkthrough

On a disposable migrated database, install the walkthrough fixture (`cargo run -p bilibi-walkthrough-fixture`; tenant 1 is its operator), set `BILIBI_SETTINGS_KEY` to 64 hex characters, run the app and sign in as a tenant 1 account with `base.settings.global.manage` (an admin role with `grant_all`). Then:

1. Open **Administration → System → Settings**. Each Value cell says "Set here" or names the inherited source.
2. Open a number's Value cell and Save: one setting updated. Blank it and Save: one override cleared. Type `abc` into a number: "<Label>: must be a whole number", nothing stored.
3. Save two rows, press Restore Defaults: the dialog counts two changed settings; Restore clears them.
4. Grant the account `base.settings.secret.view` directly and store an encrypted editable setting (the walkthrough in `apps/web/examples` or any module's encrypted definition). "Show stored value" asks for 2FA; a refused check shows nothing, an accepted check shows the value briefly, and `base_audit_actions` holds one `settings.secret.reveal` row per attempt.
5. Deny one field's capability (`principal_capabilities ... is_allowed=false`) and reload: that row is gone; deny all and the page names them.
6. Pick **This tenant** and set Search's "Kinds of records" to `{"companies": false}`: the row says "Set here" and Global still inherits. The search box of a tenant 1 account finds no companies, while tenant 2's `manager@other.example.test`, granted `base.search.records.list`, still finds its own company. "Clear overrides" restores it.

A setting protected by the current 2FA policy uses the shared `bilibi_ui::StepUp` form. Before setup its control stays disabled beside a setup link; other settings can still be saved. Restore is replaced by the setup link when it would clear a protected override. Every write keeps its existing authorization and 2FA checks.
