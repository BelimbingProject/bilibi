# Base Authz Agent

**Use it for:** the operation contracts that change grants and roles, shared by pages and agents.

**Not for:** changing grants or roles from your own module.

This module gives pages and agents the same reviewed doors for authorization changes.

- Authz sits below Agent API, so these adapters live in a module above both, as Audit Views sits above Audit.

| Operation | Capability | `Grants` call | Step-up operation(s) |
|---|---|---|---|
| `base.authz_agent.role.list` (read) | `admin.authz.role.list` | `roles::list` | — |
| `base.authz_agent.system_principal.list` (read) | `admin.authz.system-principal.list` | installed vocabulary and `effective_system` | — |
| `base.authz_agent.system_principal.set` | list, then grant for allow/deny or revoke for clear | `set_system_in` | `base.authz.system_principal.set` |
| `base.authz_agent.automation_consent.list` (read) | `admin.authz.system-principal.list` | `effective_system` | — |
| `base.authz_agent.automation_consent.set` | `admin.authz.system-principal.list`, then grant or revoke permission inside `Grants` | `set_system_in` | `base.authz.system_principal.set` |
| `base.authz_agent.principal_role.set` | `admin.authz.role.update` | `assign_role_with_in` | `base.authz.principal_role.set`, plus `base.authz.grant_all_role.assign` when giving a full-access role |
| `base.authz_agent.grant_all_role.assign` | `admin.authz.role.update` | `assign_role_with_in` (full-access roles only) | `base.authz.principal_role.set`, `base.authz.grant_all_role.assign` |
| `base.authz_agent.principal_capability.set` | `admin.authz.role.update` | `set_direct_in` | `base.authz.principal_capability.set` |
| `base.authz_agent.role_capability.set` | `admin.authz.role.update` | `set_role_capability_in` | `base.authz.role_capability.set` |

## Automation access

Administration → Authorization → **Automation access** (`/authz/automation`) lists every shared schedule on the default `scheduler` identity and every event subscription that wants to act for the current organisation. Those schedules share one consent record; a schedule with its own execution principal is not listed there and is consented through [Installed system principals](#installed-system-principals) ([Schedule owner business authority](../schedule/README.md#owner-business-authority)). Event subscriptions share another.

Grant and Withdraw call `base.authz_agent.automation_consent.set`, the same reviewed operation an agent calls. Authz checks the live administrator, verifies they hold the access they are granting, asks for 2FA under the existing policy, and records the change in Audit. Withdrawing scheduled work refuses its next authorization check. Withdrawing Events leaves deliveries waiting until access is granted again.

## Installed system principals

The same Automation access page lists every installed system principal/capability pair, its stored rule, effective access and standing grant. This administration door grants nothing when a module is installed or a tenant is provisioned.

Read `base.authz_agent.system_principal.list`, then review `base.authz_agent.system_principal.set` with `principal`, `capability`, `rule` and `expected_rule` (the stored rule returned by the read). Both rule fields use `allow`, `deny` or `clear`. The browser previews and confirms the same operation with unchanged input.

- **Allow** needs the existing grant capability and the capability being granted without a company bound.
- **Deny** needs the existing grant capability, matching Authz's stored-rule contract; a deny wins over standing access.
- **Clear** needs the existing revoke capability and removes either stored rule. A standing grant becomes effective again when its deny is cleared.
- Only an installed principal's declared capabilities are accepted. The live administrator is proved in preview and again by Authz at commit; Authz retains step-up and audited mutation ownership.
- A changed stored rule refuses the review. Commit compares `expected_rule` while holding Authz's tenant grant lock, including when no grant row exists. Cancel and read/review again after a conflict.

## An agent never exceeds its person

- The plan service authorizes each operation's capability.
- `Grants` re-proves it inside the write.
- The account page's escalation guard holds here as well, at review and again at commit:
  - a role or capability is grantable only by an actor who holds everything it grants;
  - a full-access role is grantable only by a full-access actor.
- System roles' capabilities are refused: reconciliation owns them.
- Impersonation is never agent reach (`apps/base/agent_api/README.md`).

## Two-factor

Every write is a step-up operation declared by Authz:

- A commit may answer `step_up_required`. The agent asks its person for a code and commits the same plan again with `step_up_code`.
- The review lists the step-up keys under `step_up`.
- Policy, windows and audit: `apps/base/step_up/README.md`.

## Assignment bounds

`base.authz_agent.principal_role.set` accepts an optional `assignment` object with `company_id`, inclusive UTC `valid_from`/`valid_until` dates and `purpose`. An omitted start means effective now; an omitted end means no expiry, including on engagement roles. Company scope is inside `assignment`, so it does not select the acting account's workplace. The preview names the exact assignment and its previous bounds. Commit repeats the owner's company, date, role-policy and escalation checks in the write transaction. Omitting company targets the unscoped assignment; removing Company A's assignment leaves Company B's grant intact. A role requiring an engagement refuses missing company or purpose in preview and commit. Company-scoped roles may contain only owner-opted-in company capabilities. Full-access roles can have dates, but cannot have company scope.

Direct grants and custom-role capability changes require authority without a company bound. Holding a capability within one company cannot be used to grant it across the tenant. Grant administration never adds a desk.

## Running proof

Use an empty disposable migrated database, run `seed-production` in Dev, then `BILIBI_PRINCIPAL_PROOF=disposable cargo run -p bilibi-web --example system-principal-grants` alongside the private running app at its exact `APP_ORIGIN`. The example installs the walkthrough fixture and prints reviewed grant/deny/clear results, undeclared bounds, insufficient authority, direct deny, stale review, tenant isolation, live step-up refusal, audit counts and Queue's refusal after withdrawal. It retains a denying scanner rule for browser inspection; clear it through Automation access when finished. No credentials or codes are printed.
