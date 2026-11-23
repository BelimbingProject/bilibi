# Tenant scripts run through operation contracts

**Document Type:** Architecture decision record
**Status:** Accepted
**Agents:** codex (author)
**Scope:** Tenant scripts on shared and isolated installations
**Last Updated:** 2026-11-23

Bilibi stores tenant scripts as versioned JavaScript and runs them in a disposable QuickJS worker through the Rust binding rquickjs. This decision applies to shared SaaS, dedicated and on-prem installations.

## Context

A watcher reads “watch an event, if it matches, do something.” An ordinary operation step covers fixed inputs. A script step lets a tenant look up a permitted record, calculate an input or choose an action. The owner records the event in its save. After commit, the watcher evaluates its optional filter before starting a sandbox. Without a filter, it runs on every event of the selected kind.

## Decision

**Approve exactly what will run.** Store each tenant's source text, language, declared reads and operations, outbound permissions, examples, digest, drafter and approval evidence as versioned data. Approval requires a person with approval authority and every declared permission at the required reach. Edits create a new version. A watcher pins an approved script version and its operation bindings. Switching off, replacing that version or changing its bindings stops pending calls with a visible reason.

**Check who started it.** Every entry point checks a separate permission to run this particular script. Check it again when queued work starts. An event or schedule uses the tenant's named `watchers` principal and its live grants. A page button or CLI uses the signed-in person. An agent acting for a person also uses that person's permissions within the connector's authorization. In all cases the approved declaration further narrows access. A manager's approval gives a junior employee no additional record reach.

**Give the sandbox one way into Bilibi.** Run each invocation in an isolated, disposable worker with no database credentials, filesystem access, network access or tenant-supplied modules. A trusted host binds the tenant and caller. It accepts only declared calls to eligible operation contracts, including reads, and checks current grants, field restrictions and record reach on each call. Owners validate inputs and recheck writes at commit. Keep the watcher's existing subject binding and never-automated exclusions. Include every possible declared effect in watcher cycle checks and propagate the cause through each action.

**Review anything sent outside.** Mark outbound operations in the owner's contract metadata. Approval names the operation, the permitted destination and the data it may send. Recheck those terms against the actual arguments. A new recipient or broader payload needs fresh approval. Scripts never receive connector secrets.

**Bound the whole run.** Set ceilings for elapsed time, memory, calls, rows and output bytes. Count host calls and waiting time against the budget and terminate the worker when it expires. Keep the existing tenant and chain limits. The run log identifies the version, caller, cause, operations completed and reason for a skip, refusal or timeout. On a retry, use recorded effect identities to prevent duplicate writes or sends. The complete script plan is validated before any effect starts. Once execution begins, committed effects retain their receipts.

**Make examples executable.** Each version supplies example facts, author-typed lookup fixtures and expected ordered operation calls with their inputs. Fixtures are test data and are never captured from live records. Run the same script in preview with captured calls and no writes or deliveries. For example, an urgent case requests routing that case to the priority desk exactly once. A normal case requests no operation. Show expected and actual results at review and require all examples to pass before approval.

**Make recovery possible without AI.** AI drafts source and examples through the connector. A person reviews the exact version, permissions, destinations and previews before approving. The Customisations page lists scripts, their active versions and who approved them. A person can inspect, switch off or undo a change there. Restoring an earlier version rechecks its permissions and bindings. Undo changes future runs. Completed business actions retain their own correction history. [ADR 33](./33-pages-for-business-actions-and-customisations.md)

## Language choice

| | JavaScript | Lua |
| --- | --- | --- |
| Rust embedding and limits | QuickJS via rquickjs exposes heap and stack limits and an interrupt handler for deadlines. Supply a minimal context without a module loader or host I/O. | Lua 5.4 via mlua exposes a state memory limit and instruction hooks for deadlines. Start with an explicit library allowlist and accept source text only. The binding's built-in sandbox switch is specific to Luau. |
| Sandbox strength and size | A compact native engine with a broad language surface. Upstream QuickJS reports 210 KiB for one x86 hello-world build. | A small native interpreter designed for embedding, with fewer language facilities to expose. The linked Bilibi worker size and peak memory need measurement for either choice. |
| People and AI | Familiar to web developers. Object literals map naturally to operation inputs. We expect AI drafts to need less translation, an assumption to check against the examples. | Short scripts are easy to read. Reviewers and AI drafts must account for one-based arrays and Lua tables when exchanging JSON. |

QuickJS fits the JSON inputs that tenants and AI drafts already use. Lua would introduce table and array conversion rules for every reviewer. Both engines need a host deadline and process memory limit to cover native calls. [QuickJS manual](https://bellard.org/quickjs/quickjs.html) · [rquickjs runtime](https://docs.rs/rquickjs/latest/rquickjs/struct.Runtime.html) · [mlua state](https://docs.rs/mlua/latest/mlua/struct.Lua.html)

## Implementation

The implementation stores the JavaScript source and its SHA-256 digest in the watcher version. The optional filter runs before script facts are read. The worker receives declared event-record fields and owner-declared read-contract replies, then returns an ordered selection from approved operation and input pairs. Approval records permission for the watchers principal and person-initiated doors to run each exact script declaration; execution checks that permit and the caller's live `watchers.script.execute` grant. Only operations their owners explicitly designate as local can be selected by a script. Outbound operations remain unavailable to scripts in this slice. Setup packs refuse scripted watchers.

The tenant-wide `watchers.script.execute` grant applies to the named watcher principal for queued watcher or Schedule runs and to a person running examples or an approved script. The watcher's exact-version permission narrows it. `base.watchers.script.run` is the one person/job operation: the Watcher page, `/api/v1` (and therefore `bilibi-axi`), MCP delegated agents and scheduled Queue jobs all use it. It reads the target's declared facts as the caller, previews the complete selected plan before its first effect and commits every owner action in one transaction. Use the watcher page and Customisations history to inspect, switch off or restore an approved version; restore repeats current permission, binding and example checks. Independent script records, outbound terms and setup-pack transport remain future work. [Watchers contract](../../../apps/base/watchers/README.md#javascript-steps)

## Consequences

ADR 28 permits tenant JavaScript on shared servers only through this sandbox. Customer Extensions still follow the installation policy. ADR 31 continues to govern watcher delivery, permissions, effects and receipts. Before enabling scripts on shared servers, demonstrate isolation between tenants, termination at the limits, refusal after permission revocation and retry without duplicate effects. [ADR 28](./28-tenant-customisation-is-data-on-shared-servers.md) · [ADR 31](./31-tenant-watchers-run-after-commit-as-one-bounded-principal.md)
