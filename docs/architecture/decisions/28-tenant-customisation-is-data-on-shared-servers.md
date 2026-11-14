# Tenant customisation is data on shared servers; customer code needs its own server

**Document Type:** Architecture decision record
**Status:** Accepted
**Agents:** claude/claude-opus-5-5-medium (author)
**Scope:** What a tenant may change without a release, and where customer-supplied code may run
**Last Updated:** 2026-11-14

A shared Bilibi server runs one program for many tenants. A release is the right way to add a new capability, but too slow and too costly for one client's rule, label or view. Customer-supplied code inside that shared program would put every other tenant at risk. Custom fields (ADR 21, tenant-defined custom fields) already show the answer for fields: store the customisation as data. The owner agreed on 2026-11-14 to apply the same boundary to all tenant customisation.

**One codebase, two policies.** The same Platform, rule engine, settings, custom fields, labels and shared views run on every installation and behave the same way. An installation setting decides only whether customer code Extensions are allowed.

| Installation | What a customer can change |
| --- | --- |
| Shared server, many tenants in one program | Data only: settings, custom fields, labels, shared views and rules. A locked-down formula language may calculate and choose among approved actions. No customer-supplied general code (Python, JavaScript, Wasm and so on). |
| On-prem, one company per installation | The same data tools, plus code Extensions switched on by the installation setting. |
| One client on its own dedicated hosted server | Same as on-prem. Isolation from other customers decides, not where the server stands. It needs its own process, data credentials and secrets; a separate URL or schema inside a shared process does not count. |

**Only the installation owner sets the policy.** A tenant administrator cannot switch on code for their tenant. The setting is checked wherever code enters: composition and release preparation, installer checks, startup validation, and any future plug-in upload. A page toggle alone is not enough. Switching an installation from on-prem to shared refuses while customer code is still in the composition: flipping a flag does not unload compiled Rust.

**The boundary is about customer code in a shared process, not about Extensions.** A shared server still runs the trusted Platform, Domains and operator-installed Extensions, such as reviewed connectors. A compiled Extension may read tenant settings and behave differently per tenant; that is ordinary configuration. What it must not do is carry hard-coded customer IDs or one customer's private branches.

**Code Extensions take no shortcut.** On-prem Extensions go through Bilibi operations, the caller's current permissions and audit, exactly like a data rule. They do not get a second write path. Native Rust shares the process, so the setting is a release policy, not a sandbox; Extensions keep the normal build, review and release path (ADR 02).

**A locked-down formula counts as data.** It is a small program in the computer-science sense, so it counts as data only while Bilibi controls its vocabulary, its authority and its maximum work. A formula is pure: it reads permitted facts and returns a result. A separate, bounded list of owner-approved actions does the work, through the trusted executor and the normal operation contract. The formula is never the authority to act. Start with a structured rule record; add text formulas only when real rules need them. A language qualifies only if it passes these tests with the chosen engine and real Bilibi contracts:

| Case | Required result |
| --- | --- |
| Compare a known field (`priority == urgent`, a decimal) | Accept after checking the field ID and type |
| Add or subtract amounts | Exact arithmetic with explicit currency or unit; never floating point for money |
| Choose one allowed action with approved parameters | Accept as an action request; the executor calls the normal contract |
| Supply a tenant, principal, operation name or record target | Refuse, or resolve through a fixed, scoped binding |
| Call an action from inside the formula, or thousands of times | Refuse: no side effects during evaluation, a capped number of actions after it |
| Nested scans or a huge regular expression | Refuse above measured size and cost limits |
| Import, `eval`, recursion, files or HTTP | Refuse |
| Rule B triggers Rule A forever | Stop with a causal-chain budget and a visible failure; reject obvious cycles before activation |
| Read a masked value and leak it through the result | Refuse the read at the source |
| An action that changes permissions or skips an approval | Refuse automatic execution |

A JSON document that embeds a script or an unlimited action loop is code, whatever its file format.

## Consequences

- Shared-server clients get quick, tenant-local changes within the vocabulary Bilibi offers. A new trigger, action, record type or connector still needs a shared release.
- On-prem and dedicated-server clients keep full code customisation.
- Operators must enforce code admission and real isolation; the installation setting does not exist yet.
- The rule engine is follow-up work: [#682](https://github.com/BelimbingProject/bilibi/issues/682) (wave 3). It holds the design, limits and runtime proof. When it lands, this record moves to **Accepted; implemented** and the owner READMEs it touches (custom fields, the internal operation caller) are updated in the same change.

## Why not the alternatives

- **Customer native plug-ins in the shared process.** One tenant's code could read or break every other tenant.
- **Plain Node or Python as a tenant sandbox.** Neither is a security boundary on its own.
- **Ban all Extensions on shared servers.** It would also ban reviewed product connectors that every tenant may configure as data.
- **Accept any script wrapped in JSON as "data".** It erases the boundary.
- **One AI prompt as the rule that runs.** AI may draft a rule and supply bounded observations; the approved fixed version runs without AI.
- **A separate rule engine per installation type.** Two engines drift; one engine with one policy setting does not.
