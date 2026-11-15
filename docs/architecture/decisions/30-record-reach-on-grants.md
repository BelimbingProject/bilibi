# Record reach is part of a role's grant, applied by the record's owner through Grid

**Document Type:** Architecture decision record
**Status:** Proposed
**Agents:** claude/claude-opus-5-5-high (author)
**Scope:** How a grant limits which records of a capability a person reads and changes: mine, my team's or all
**Last Updated:** 2026-11-15

## Context

**A capability opened every record of its kind in the tenant.** `cx.opportunities.view` reads every deal, so a salesperson cannot be limited to their own deals or their team's (#683). SuiteCRM answers this with All, Group and Owner levels on each action, and security groups: a row per record per group, inherited from the creator. Every list then pays for a sub-query, and a wrong group row cannot be seen. Core teams (#681) and team roles (#703) now give Bilibi a group every module can name.

## Decision

**A role gives a capability with a reach: `own`, `team` or `all`.** It is a column on the role's grant (`role_capabilities.reach`, migration `base-authz-017-grant-reach`); a team role carries the reach of its role's grants. A direct allow on an account carries one too (`principal_capabilities.reach`).

- **Opt in per capability.** A key can carry less than `all` only when its owner declares `CapabilityDefinition::with_reach`, after every read applies the reach and every write rechecks it, as company scope is opted into ([ADR 20](./20-company-role-assignments.md)).
- **The owner names the fields.** A Grid table declares its owner field (a deal's owner, a case's assignee) and optionally a team field (a case's queue team). There are no per-record group rows: who may see a record follows from fields the record already has.
- **One rule, applied by Grid.** Grid wraps the table's source in `Reach::predicate`, so lists, reports, projections, record names and search read the same rows. An owner's own SQL and write rechecks use the same predicate (`reach_proof`, `reach_admits`).
- **My team's** is: records I own, records owned by someone who shares a Core team with me, and records filed under one of my teams. It reads `authz_group_members`, Authz's own copy of team members, so Base never reads Core tables.
- **The widest grant wins**, as grants already union. Grant-all and a system principal's grant reach all.
- **A restriction on the user wins over the team.** A member sees what the team can see, unless restricted on the user: a direct deny removes the key, and a direct allow's reach is the account's reach for the key, capping what its roles and team roles give.
- **Reach only narrows.** It is applied on top of the owner's tenant and company filters, never instead of them ([ADR 19](./19-company-work-access-and-page-scope.md)). It is read live with each decision; nothing is cached.

**A grant gives no more reach than the actor holds.** Giving a role a key, assigning a role, giving a team a role or adding members to a team that carries roles, setting a provider's role bound, and a direct allow each need the actor to hold every key at least as far as it reaches. A system principal's grant needs `all`.

**Joining a team gives the member the team's reach.** Team management defines who shares "my team's" records; a membership change is not a grant and runs no reach check. To keep one member narrower, set a direct deny or a narrower direct allow on that user. Clearing that narrower allow widens the account again, so it needs the actor to hold the key as far as the account then reaches; clearing a deny checks no reach.

**Taking over an account needs `all`.** `holds_everything_of` (password, sign-in address, delete, impersonate) treats a key the actor reaches only partly as covering nobody else's grant of it: the account's own records are not the actor's.

**While impersonating,** a key that supports reach is held only when the operator reaches all of it, with the viewed account's reach (`denied_impersonation_reach`). One statement binds one reader, so the strict intersection of two readers' records cannot be expressed; refusing is the safe side.

## Rejected alternatives

- **Security groups per record (SuiteCRM):** a row per record per group costs every list a sub-query and hides mistakes, as the issue says.
- **Reach on the assignment instead of the grant:** one role often mixes keys that should reach differently (all products, own deals).
- **A separate capability per reach (`x.view.own`, `x.view.team`):** triples the vocabulary, and the widest-wins rule would be every owner's code instead of Authz's.
- **Reach for any key:** a key whose records have no owner field would silently show nothing or everything.

## Consequences

- A table read under a key with reach must declare an owner field, and a table without one must not name such a key; boot refuses either.
- A child table guarded by the same key (an opportunity's lines) must carry its parent's owner in its source, or it would show every row.
- A parent's oversight read of a child tenant skips a table the parent reaches only partly.
- Sub-teams do not roll up: a lead of "Malaysia sales" does not reach "KL Enterprise" records through reach alone.

## Status

Used by:

- Core Activity: `core.activity.all.view` and `core.activity.all.manage` reach by assignee (`apps/core/activity/README.md`, "Who sees an activity").
- Grid's reference tasks (`reference_tasks`, `grid.reference.task.view`), proved by `apps/web/examples/record-reach.rs`.

Planned:

- CX opportunities and cases opt in, a follow-up in the CX Domain.
