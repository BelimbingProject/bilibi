# Tenant customisation on shared servers

**Document Type:** Architecture decision record
**Status:** Accepted
**Agents:** claude/claude-opus-5-5-medium (author), amp/claude-opus-5.5 (custom-field rule inputs, #730)
**Scope:** What a tenant may change without a release, and where customer-supplied code may run
**Last Updated:** 2026-11-23

A shared Bilibi server runs one program for many tenants. A release is the right way to add a new capability, but too slow and too costly for one client's rule, label or view. Customer-supplied code inside that shared program would put every other tenant at risk. Custom fields (ADR 21, tenant-defined custom fields) already show the answer for fields: store the customisation as data. The owner agreed on 2026-11-14 to apply the same boundary to all tenant customisation.

**One codebase, two policies.** The same Platform, rule engine, settings, custom fields, labels and shared views run on every installation and behave the same way. An installation setting decides only whether customer code Extensions are allowed.

| Installation | What a customer can change |
| --- | --- |
| Shared server, many tenants in one program | Settings, custom fields, labels, shared views, watchers and approved JavaScript through the isolated worker in [ADR 34](./34-tenant-scripts-run-through-operation-contracts.md). |
| On-prem, one company per installation | The same data tools, plus code Extensions switched on by the installation setting. |
| One client on its own dedicated hosted server | Same as on-prem. Isolation from other customers decides, not where the server stands. It needs its own process, data credentials and secrets; a separate URL or schema inside a shared process does not count. |

**Only the installation owner sets the policy.** A tenant administrator cannot admit a compiled customer Extension for their tenant. The setting is checked wherever code enters: composition and release preparation, installer checks, startup validation, and any future plug-in upload. A page toggle alone is not enough. Switching an installation from on-prem to shared refuses while customer code is still in the composition: flipping a flag does not unload compiled Rust.

**The boundary is about customer code in a shared process, not about Extensions.** A shared server still runs the trusted Platform, Domains and operator-installed Extensions, such as reviewed connectors. A compiled Extension may read tenant settings and behave differently per tenant; that is ordinary configuration. What it must not do is carry hard-coded customer IDs or one customer's private branches.

**Code Extensions take no shortcut.** On-prem Extensions go through Bilibi operations, the caller's current permissions and audit, exactly like a data rule. They do not get a second write path. Native Rust shares the process, so the setting is a release policy, not a sandbox; Extensions keep the normal build, review and release path (ADR 02).

**Tenant JavaScript runs outside the application process.** On 2026-11-23 the owner approved QuickJS for tenant scripts on shared servers. Scripts are versioned tenant data. A disposable worker can request only the reads and actions approved for that version, through a host that checks the caller’s current permissions. The sandbox and its resource limits are part of the admission policy. [ADR 34](./34-tenant-scripts-run-through-operation-contracts.md)

## Consequences

- Shared-server clients get quick, tenant-local changes within the vocabulary Bilibi offers. A new trigger, action, record type or connector still needs a shared release.
- On-prem and dedicated-server clients keep full code customisation.
- Code admission is implemented through the [installation policy](../../../apps/base/system/README.md#installation-policy-srcinstallation_editorrs). Operators remain responsible for real process, credential and secret isolation; the setting does not create it.
- The watcher engine is built: [`base/watchers`](../../../apps/base/watchers/README.md), a structured watcher record with approved versions, run after commit as one bounded principal ([ADR 31](./31-tenant-watchers-run-after-commit-as-one-bounded-principal.md)). The internal operation caller admits a principal only where the contract's owner says so (`apps/base/agent_api/README.md`, "Queued jobs as callers"). See [Watcher conditions](../../../apps/base/watchers/README.md#what-a-watcher-is) for supported field paths and access checks.

## Why not the alternatives

- **Customer native plug-ins in the shared process.** One tenant's code could read or break every other tenant.
- **Plain Node or Python as a tenant sandbox.** Neither is a security boundary on its own.
- **Ban all Extensions on shared servers.** It would also ban reviewed product connectors that every tenant may configure as data.
- **Accept any script wrapped in JSON as "data".** It erases the boundary.
- **One AI prompt as the rule that runs.** AI may draft a rule and supply bounded observations; the approved fixed version runs without AI.
- **A separate rule engine per installation type.** Two engines drift; one engine with one policy setting does not.
