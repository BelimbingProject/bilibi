# Teams

Teams give people who work together one shared record across Bilibi: a sales team, a support queue's agents, a production crew.

For example, create "KL Enterprise" with Aisyah as its lead, under the team "Malaysia sales". Add five sales staff. CX can later show "my team's deals", Factory can let a crew lead record the crew's time, and every Domain points at the same team.

## Shared facts

Core stores only these facts. Nothing else is in Core.

| Fact | Rule |
| --- | --- |
| Name | 1 to 200 characters; sibling teams have different names |
| Kind | A code of the tenant's `core.team.kind` pick-list. Core starts with "Team" (locked); a tenant adds its own words, such as crew, pod or squad |
| Parent team | Optional. No loops; teams nest at most 5 levels deep (`MAX_DEPTH`) |
| Company | Optional. One the manager works in (a desk there) |
| Leads | One or more members marked lead. A team always keeps at least one |
| Members | Active staff accounts, at most 5000 per team. A person may belong to several teams |

- **Stable id.** A Domain keeps its own facts about a team in its own tables (a crew's shift pattern, a queue's team), keyed by the team's id. Core never learns them.
- **Employment stays in People.** Supervisors, positions and employment are People's; a team is not a reporting line.

## Safe defaults

- **A team never widens company access.** Changing a team of a company needs work access to that company, and an archived company's teams cannot change. A team grants no capability; the later "mine, my team's or all" rule stays within the companies the reader already reaches ([ADR 19](../../../docs/architecture/decisions/19-company-work-access-and-page-scope.md)).
- **Sub-teams roll up to the leads above them.** `leads_above` returns the leads of every team above, nearest first, bounded by `MAX_DEPTH`. The record page shows them as "Also led by".
- **Nothing is cached.** Every read and decision reads membership live, so a change applies at the next check.
- **Past membership lives in the audit trail.** Each person's join, leave and lead change is one audit entry of kind `team_member` under the team's id, shown in the team's History.

## Public facade

Resolve `Arc<Teams>` from `bilibi_team::SERVICE` (`core/team.teams`). Every call takes the caller's sealed `Scope` and connection; writes take the caller's `MutationTransaction`.

| Method | Result |
| --- | --- |
| `get`, `list`, `names`, `choices` | Tenant-wide team facts under `teams.view`; lists are bounded |
| `members`, `leads`, `leads_above`, `member_ids`, `teams_of` | Membership reads under `teams.view` |
| `preview_create`, `create` | A team and its leads as its first members; `teams.manage` |
| `preview_update`, `update` | Name, kind, parent team or company, with the version read; `teams.manage` |
| `plan_members`, `change_members` | Add, remove, or turn a member's lead on or off; `teams.manage` |
| `may_change` | Whether the actor may change this team now |

**One place changes membership.** `plan_members` checks every membership change (a create's leads too) and `members::apply` writes it. A later rule, such as team roles, belongs there, so no door can skip it.

## Ports

Domains read teams through [Base Ports](../../base/ports/README.md), so they need no dependency on this module. Both answer under the caller's scope and `teams.view`.

| Port | Request | Answer |
| --- | --- | --- |
| `core/team.team` | `{"team_id": 7}` | `{"id","name","kind","parent_id","company_id","leads":[user ids],"leads_above":[user ids],"members":[user ids]}`; members are bounded by `MAX_MEMBERS` |
| `core/team.teams_of` | `{"user_id": 12}` | `{"teams":[{"team_id","name","kind","company_id","lead"}]}`, at most 500 |

A missing team or another tenant's is `Refused(NotFound)`; a caller without `teams.view` is `Refused(Forbidden)`.

## Pages

- **`/teams`** (Core menu, `teams.view`): search by name, filter by kind or by "Sub-teams of". "+ Team" opens the create dialog: name, kind, parent team, company and leads, chosen with the staff picker (`UserService::search_staff` under `teams.manage`).
- **`/teams/{id}`**: Details read first and edit in place (name, kind, parent team, company), with the leads and the leads above. **Members** lists leads first: "+ Add members" opens the staff picker; each row has a Lead on/off switch and a remove button; ticking several rows offers "Remove from team". **Sub-teams** lists up to 25 and links to the filtered list for more; "+ Sub-team" opens the same create dialog with the parent filled in. History is in the header.
- **Many keys, one operation.** "+ Add members", dropping staff from the Users list onto the Teams tile, the users list's "Add to team" (Non-local actions) and an AI agent all call `core.team.add_members`. A drop or "Add to team" opens the same review on `/teams`, where the person chooses the team.

## Agent operations

| Key | Capability |
| --- | --- |
| `core.team.list`, `core.team.get`, `core.team.members` | `teams.view` |
| `core.team.create`, `core.team.update` | `teams.manage` |
| `core.team.add_members`, `core.team.remove_members`, `core.team.set_lead` | `teams.manage` |

Writes are reviewed safe writes: an agent's plan shows a review card ("Add Aisyah and Ben to KL Enterprise") before commit, and the page calls the same contracts through `PageContracts`. The tenant owner role receives both capabilities; grants and direct denies follow Authz.

## Storage

`core_teams` and `core_team_members` (migration `core-team-001-teams`). Every statement binds the tenant. The `teams` Grid table and the `team` record kind (`/teams/{id}`) let lists and references name a team.

## Runtime check

Run `./bilibi start`, sign in, and use `/teams`: create a team with two leads, add members from the dialog and from the Users list's "Add to team", turn a lead off (the last lead's switch is off-limits), remove two members at once, add a sub-team, move a team under its own sub-team (refused), and read the team's History. Check the same with touch (coarse pointer) and at phone width.
