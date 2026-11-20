# Tenant watchers run after commit as one bounded principal that owners offer their own keys to

**Document Type:** Architecture decision record
**Status:** Proposed
**Agents:** claude/claude-opus-5-5-high (author)
**Scope:** How Base Watchers (`base/watchers`, #682) triggers, reads, acts and stays bounded without Base naming the Domains it acts in
**Last Updated:** 2026-11-16

ADR 28 decided that a shared server's tenants customise behaviour as data, with a watcher engine as the first trial. A watcher engine in Base must react to Domain events, read Domain records and call Domain write contracts, yet Base may name no Domain, and an automation must never act as the person who saved the record or gain authority nobody granted. Four seams were missing: a Base subscriber could not read events it does not list by name; a subscriber could not start a job of its own in the delivery transaction; a system principal's job could not call an operation contract (ADR 10); and a principal's bounds could hold only its declaring module's own keys (or a connector's, through `OptionalCapabilities`). This record decides how those seams open.

## Decision

1. **A watcher is a record with versions; one approved fixed version runs, with no AI.** Drafts (by a person or an agent, through the same `base.watchers.draft` contract) never run. Approval is a separate capability and re-runs every check as the approver, who must hold what the watcher does. A queued run keeps its pinned version and is refused, visibly, when the watcher changed before it acted.
2. **Triggers are owner events delivered by Base Events.** An owner offers an event of its own as a `Trigger`; the watchers subscriber reads the events its catalog names (`Subscribe::reads`), so Events applies its usual boot checks, ordering, receipts and replay. Base names no Domain event.
3. **The subscriber hands off; the watcher runs after commit.** In the delivery transaction the subscriber writes the queued runs and returns a `Handoff`: Events enqueues the subscriber module's own worker, as its own principal, in that transaction. The watcher never runs inside the trigger's save and never as the publisher's actor.
4. **One named principal, `watchers`, bounded by what owners offer.** Its declaration holds only its own keys. The owner of a capability may add it to a principal of a module it depends on (`OfferedCapabilities`); the owner, not Base, says "the watchers principal may read my cases and route them". Offers never grant: a tenant grants each key on Automation access, and a deny wins. Portal and platform keys cannot be offered.
5. **The job door admits a principal only where the contract's owner says so.** `PrincipalWrite` names the principals whose jobs may call one reviewed, job-door, no-step-up contract. The same schema, live capability, preview and commit apply. Watchers may act only through these contracts; there is no generic record write.
6. **Effects happen once per (tenant, version, event, action).** The receipt commits in the action's transaction; a retry finds it.
7. **Chains follow causes, not traces.** The runner names its run as the cause of its action's transaction (`Events::cause`, a transaction setting); Events stores the cause on each event recorded there. A run's depth is its cause's plus one; past the tenant's chain budget it is refused with a receipt. Approval and switching on reject cycles in the graph of active watchers' triggers and their actions' declared events, including the candidate. The causal-chain budget remains the backstop for chains that do not form a known cycle.
8. **Bounds are settings within fixed safety ceilings.** Operators can lower work, watcher-size, fact-read and retry budgets without a release; the [Watchers contract](../../../apps/base/watchers/README.md#limits) owns the settings and their enforcement.
9. **Some contracts are never automated.** Contracts that grant access, change identity or decide approvals (Authz, Authz Agent, Workflow, Step-up, Tenancy, Session, Core User, Tenant and Agent) and any contract that asks for step-up are refused at boot.

## Consequences

- A Domain makes a record automatable with four contributions and no Base change: a trigger, an action, a principal write and an offer (`apps/base/watchers/README.md`).
- Every Domain contract offered as an action must accept a principal scope (`user()` is `0`) and recheck its own watchers in its commit.
- A tenant administrator sees and grants the watchers principal's keys like any other principal's; until granted, runs are refused with `read_refused`, `forbidden` or `actor_refused`.
- Trace IDs stay what they were (one per originating request); the cause is a separate, narrower fact.
- ADR 10's watcher that a system principal's job cannot call contracts now has one exception: contracts whose owner admits that principal.

## Why not the alternatives

- **A per-trigger or per-action principal.** Several consents per watcher and no single bound to reason about; the tenant would grant pieces of one automation separately.
- **Base listing Domain capabilities in its own declaration** (`OptionalCapabilities` from Base). Base would name every Domain's keys.
- **A generic "record changed" hook in `MutationBoundary`.** It would run inside the save and expose fields the owner never offered.
- **Running the watcher inside the delivery.** The delivery runs as `domain-events`, not under the watchers principal's grants, and an action's contract needs its own audited transaction.
- **Counting runs per trace as the chain budget.** One import shares a trace across thousands of independent events; it would cut off unrelated work and miss nothing a cause does not catch.
- **Refusing every watcher that might start another.** Two-watcher chains are legitimate; the runtime budget bounds them with a visible receipt.
