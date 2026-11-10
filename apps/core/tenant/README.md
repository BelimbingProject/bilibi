# Tenant

Create a customer's tenant and companies together, search the platform tenant directory, and suspend or reactivate a tenant.

For example, an operator can create **Acme** with **Acme Trading** as its primary company and **Acme Industrial** and **Acme Auto** as additional companies. The Tenants page and agent doors call the same contract. A failed write creates none of them.

## Operations

Every contract serves every channel through the operation registry.

| Key | Input | Permission |
| --- | --- | --- |
| `core.tenant.tenant.list` | Optional `search`, `page`, `page_size` (at most 100) | Operator standing and `admin.tenancy.tenant.list` |
| `core.tenant.tenant.get` | `tenant_id` | Same list permission |
| `core.tenant.tenant.create` | `name`, `primary_company`; optional `additional_companies`, `parent_id` | Operator standing and `admin.tenancy.tenant.create`, plus Base Step-up policy |
| `core.tenant.tenant.suspend` | `tenant_id`, `reason` (required, at most 500 characters) | Operator standing and `admin.tenancy.tenant.suspend`, plus Base Step-up policy |
| `core.tenant.tenant.reactivate` | `tenant_id`, `reason` (required, at most 500 characters) | Operator standing and `admin.tenancy.tenant.reactivate`, plus Base Step-up policy |

The create review shows normalized tenant input, parent facts, company names, generated company codes, status and the primary assignment. A tenant is always created active; to stop its access, suspend it afterwards with a reason. At most 99 additional companies may be supplied. All names obey their owning module's validation; matching code suggestions receive `_2`, `_3` and so on within 12 characters.

An agent runs the dry-run check, shows the review to its person, waits for explicit approval and commits the unchanged operations. If the commit answers `step_up_required`, retry that same write with the person's current 2FA code as `step_up_code`. The step-up key is the create operation key; its declaration is on the installation floor by default. Normal window and tenant policy rules apply.

### Suspend and reactivate

`suspend` and `reactivate` are reviewed writes. The review names the tenant, the status it moves to, the reason and what the move does; the commit rechecks everything on its own transaction. A tenant moves only between `active` and `suspended` ([Base Tenancy](../../base/tenancy/README.md#tenant-data-and-provisioning) owns the enum, the one transition function and the CHECK), the platform operator's own tenant cannot be suspended, and every move needs a reason that is kept in the audit trail and, while suspended, on the tenant. Suspending ends every session of the tenant in the same transaction (`Sessions::end_tenant`, audited per session, and every open page of the tenant is told after the commit); nobody can sign in, connect an agent or run a job as that tenant until it is reactivated. Reactivating restores sign-in and agent connections; ended sessions and jobs cancelled meanwhile are not restored. The tenant's data is kept. Both moves declare Base Step-up on the installation floor, like create.

## Browser

### Parent oversight consent

Administration → Tenancy → **Oversight** (`/tenancy/oversight`) shows the child's recorded parent, table levels, withheld/personal fields, expiry, status and latest reads. Editing requires `admin.tenancy.oversight.manage`; reading requires `.view`. Setup 2FA is offered before granting. Reviewed narrowing and **Revoke all access** remain available without enrollment. **Oversight received** (`/tenancy/oversight-received`) requires `tenancy.oversight.view` and shows only consenting direct children, never siblings or an unconsenting child's name.

The shared browser/agent contracts are `core.tenant.oversight.given`, `.received`, `.catalog` and reviewed `.update`. Update accepts `version`, `active`, optional `expires_at` and a bounded `tables` array of `{table, level, withheld, personal}`; levels are `aggregates` or `rows`. Every commit revalidates live catalog fields and consent version. Grant changes use the child's mutation boundary and audit; the sealed read proof grants no child write or record-page access.

Only fully classified sources enter the consent catalog ([Grid's classification contract](../../base/grid/README.md#parent-oversight)). Core Contacts, accounts, affiliations, addresses and companies are unavailable until their owners finish classification and required privacy/store contracts. An existing grant or forged personal opt-in cannot enable them, and masking/withholding unknown fields does not make a source eligible. The editor and report builder state this limitation instead of presenting unknown personal data as safe to share.

Administration → Tenancy → Tenants (`/tenancy/tenants`) keeps the list, sorting, parent picker and Add Tenant dialog. The dialog also collects the primary company and one additional company per line. The page calls the bounded directory contract through `PageContracts::read` and calls the create contract through `PageContracts::submit`. Its business write events are `create`, `confirm_suspend` and `confirm_reactivate`; `PageWrites` has no pending allowance. A row offers **Suspend** (an active tenant other than the platform operator's own) or **Reactivate** (a suspended one) as an icon button, only to an operator who holds that move's capability. Each opens a confirmation whose Reason is required; Escape and Cancel close it. The **Suspension** column shows when and why a tenant was suspended. Edit and provision-existing actions do not exist on this page.

The column picker offers every scalar tenant field the page shows under its dialog label: ID, Name, **Parent tenant**, Sub-tenants, Status and Suspension. Primary company and Additional companies are entered only when a tenant is created and no tenant view reads them back, so they are not columns. The default columns stay the same.

On a fresh disposable database, `BILIBI_TENANT_DEMO=disposable cargo run -p bilibi-tenant --example tenant-suspension-walkthrough` suspends and reactivates a fixture tenant through the reviewed operations and prints the refusals (blank reason, the operator's own tenant, an unknown tenant, a move the status does not allow), the ended session, the refused sign-in, Queue's two identity checks refusing and then admitting the tenant, and the audit rows.

## Identity and atomic writes

- [Base Tenancy](../../base/tenancy/README.md) owns tenant data and proves the person's live operator standing. Creation preserves today's operator-plus-capability permission; installation ownership is not required.
- Base Tenancy inserts the tenant in the write transaction and returns a sealed `CreatedTenant` receipt. It proves only this transaction's new tenant and original actor.
- [Core Company](../company/README.md) accepts that receipt to insert companies and the primary assignment. No request supplies a tenant to act as, and no scope is retargeted.
- Each owner rechecks authorization on the mutation transaction and returns every affected row as a `Change`. Tenant, companies, primary assignment, audit and plan consumption commit or roll back together.
- The actor remains the person. The committed action records the agent connection, and the shared trace links all mutations to it. Revoking the grant immediately ends further calls, including a previously reviewed plan.

Creating a tenant does not create accounts, switch the caller into that tenant or confer access to its business data. No general cross-tenant company operation is exposed. The ChatGPT plugin stays a thin adapter and discovers these contracts through the registry.

## Where to go next

See [operation contracts](../../base/agent_api/README.md), [Base Step-up](../../base/step_up/README.md) and [ADR 01](../../../docs/architecture/decisions/01-agent-first-operations.md).
