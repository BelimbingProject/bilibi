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
| Roles | Optional. Authorization roles the team gives its members (below) |

- **Stable id.** A Domain keeps its own facts about a team in its own tables (a crew's shift pattern, a queue's team), keyed by the team's id. Core never learns them.
- **Employment stays in People.** Supervisors, positions and employment are People's; a team is not a reporting line.

## Safe defaults

- **A team never widens company access.** Reading or changing a team of a company needs work access to that company, and an archived company's teams cannot change. A team without a company is visible to every reader with `teams.view`. A team the reader may not see answers `NotFound`, never `Forbidden`, so it reveals nothing: not its name, company, people or sub-teams. Leads rolled up from a team above that the reader cannot see are not shown. A team whose parent is hidden reads, and is returned from a save, with no parent id or name; renaming it keeps the stored parent and the duplicate-name check does not answer for hidden siblings. A hidden team cannot be chosen as a parent (the same "Choose a team from the list" refusal as a missing one). The filter runs in SQL, so paging and counts stay correct ([ADR 19](../../../docs/architecture/decisions/19-company-work-access-and-page-scope.md)).
- **A team's roles are grants.** See [Team roles](#team-roles). The later "mine, my team's or all" rule ([#683](https://github.com/BelimbingProject/bilibi/issues/683)) stays within the companies the reader already reaches.
- **Sub-teams roll up to the leads above them.** `leads_above` returns the leads of every team above, nearest first, bounded by `MAX_DEPTH`. The record page shows them as "Also led by".
- **Nothing is cached.** Every read and decision reads membership live, so a change applies at the next check.
- **Past membership lives in the audit trail.** Each person's join, leave and lead change is one audit entry of kind `team_member` under the team's id, shown in the team's History.

## Team roles

Members hold the team's roles while they are in the team. Core owns the team; Authz owns the permission.

- **Giving a team a role is a grant.** It needs `teams.manage` for the team, `admin.authz.role.update`, every permission the role gives (the subset rule) and step-up (`base.authz.principal_role.set`). Taking it away needs the same, minus the subset rule.
- **Never a full-access role.** A team cannot carry `core_admin` or another grant-all role, nor an engagement role; give those to a person.
- **Adding a member to a team that gives roles is a grant too.** It runs the same checks for every role the team gives, so a team manager cannot hand a role they could not give. Holding a role through the team counts as holding it, as for any grant. Removing a member only narrows: a team manager can always do it.
- **Company scope.** A team with a company gives its roles in that company only ([ADR 20](../../../docs/architecture/decisions/20-company-role-assignments.md)): a role with tenant-wide permissions is refused, and moving a team that gives roles to another company is a grant there.
- **Live, never cached.** Joining, leaving, a role given or taken away, a rename or a move apply at the next permission check. A direct deny still wins.
- **Explainable.** The account's Roles & Permissions card says "Via team Line 2 crew" (or only "Via a team in a company you do not work in" when the reader may not see it), and the decision log says `group_role_capability (via team Line 2 crew)`.
- **Hidden stays hidden.** A team the reader may not see shows no roles.
- **Boundary.** `members::apply`, `update` and `change_role` call Authz's `Grants::set_group_members_in`, `sync_group_in` and `set_group_role_in` inside the team's own `MutationTransaction`; Authz keeps the team's name, company, roles and members in its own tables (`apps/base/authz/README.md`, "Group roles") and never reads Core's. Roles do not pass down to sub-teams.

## Public facade

Resolve `Arc<Teams>` from `bilibi_team::SERVICE` (`core/team.teams`). Every call takes the caller's sealed `Scope` and connection; writes take the caller's `MutationTransaction`.

| Method | Result |
| --- | --- |
| `get`, `list`, `names`, `choices` | Team facts under `teams.view`, for the teams the reader may see; lists are bounded |
| `members`, `leads`, `leads_above`, `member_ids`, `teams_of` | Membership reads under `teams.view`, for the teams the reader may see |
| `preview_create`, `create` | A team and its leads as its first members; `teams.manage` |
| `preview_update`, `update` | Name, kind, parent team or company, with the version read; `teams.manage` |
| `plan_members`, `change_members` | Add, remove, or turn a member's lead on or off; `teams.manage`, and the grant checks above for an addition to a team that gives roles |
| `roles`, `role_choices` | The roles a team gives, and the roles the actor may give it now |
| `plan_role`, `change_role` | Give a team a role or take it away; the grant checks above |
| `may_change` | Whether the actor may change this team now |

**One place changes membership.** `plan_members` checks every membership change (a create's leads too) and `members::apply` writes it, Authz's members of the team included, so no door can skip the team-role grant check.

## Ports

Domains read teams through [Base Ports](../../base/ports/README.md), so they need no dependency on this module. Both answer under the caller's scope and `teams.view`, and only for teams the caller may see.

| Port | Request | Answer |
| --- | --- | --- |
| `core/team.team` | `{"team_id": 7}` | `{"id","name","kind","parent_id","company_id","leads":[user ids],"leads_above":[user ids],"members":[user ids]}`; members are bounded by `MAX_MEMBERS` |
| `core/team.teams_of` | `{"user_id": 12}` | `{"teams":[{"team_id","name","kind","company_id","lead"}]}`, at most 500 |

A missing team or another tenant's is `Refused(NotFound)`; a caller without `teams.view` is `Refused(Forbidden)`.

## Pages

- **`/teams`** (Core menu, `teams.view`): search by name, filter by kind or by "Sub-teams of". "+ Team" opens the create dialog: name, kind, parent team, company and leads, chosen with the staff picker (`UserService::search_staff` under `teams.manage`).
- **`/teams/{id}`**: Details read first and edit in place (name, kind, parent team, company), with the leads, the leads above and **Roles**: each role the team gives, with a remove button, and "+ Add role", which offers only the roles the reader may give. Taking a role away asks first. **Members** lists leads first: "+ Add members" opens the staff picker; each row has a Lead on/off switch and a remove button; ticking several rows offers "Remove from team". **Sub-teams** lists up to 25 and links to the filtered list for more; "+ Sub-team" opens the same create dialog with the parent filled in. History is in the header.
- **Many keys, one operation.** "+ Add members", dropping staff from the Users list onto the Teams tile, the users list's "Add to team" (Non-local actions) and an AI agent all call `core.team.add_members`. A drop or "Add to team" opens the same review on `/teams`, where the person chooses the team.

## Agent operations

| Key | Capability |
| --- | --- |
| `core.team.list`, `core.team.get`, `core.team.members` | `teams.view` |
| `core.team.create`, `core.team.update` | `teams.manage` |
| `core.team.add_members`, `core.team.remove_members`, `core.team.set_lead` | `teams.manage` |
| `core.team.add_role`, `core.team.remove_role` | `teams.manage`, plus Authz's grant checks |

Writes are reviewed safe writes: an agent's plan shows a review card ("Add Aisyah and Ben to KL Enterprise", "Give Line 2 crew the role Line operator") before commit, and the page calls the same contracts through `PageContracts`. `core.team.add_role`, `core.team.remove_role`, `core.team.add_members` and `core.team.update` declare step-up `base.authz.principal_role.set`; `core.team.get` returns the team's roles. The tenant owner role receives both capabilities; grants and direct denies follow Authz.

## Storage

`core_teams` and `core_team_members` (migration `core-team-001-teams`). A team's roles and Authz's copy of its members live in Authz's `authz_groups`, `authz_group_roles` and `authz_group_members` (source `team`). Every statement binds the tenant. The `teams` Grid table and the `team` record kind (`/teams/{id}`) let lists and references name a team; both show only the teams the reader may see (the Grid source reads the reader's desks).

## Runtime check

Run `./bilibi start`, sign in, and use `/teams`: give a team a role and take it away (Roles row), create a team with two leads, add members from the dialog and from the Users list's "Add to team", turn a lead off (the last lead's switch is off-limits), remove two members at once, add a sub-team, move a team under its own sub-team (refused), and read the team's History. Check the same with touch (coarse pointer) and at phone width.

`scripts/run-walkthroughs.sh team-roles` runs `examples/team-roles-walkthrough.rs`: a full-access role and a role whose permissions the team manager lacks are refused, a grant asks for step-up outside development, members hold the role at the next check and lose it on leaving, a direct deny wins, a company team gives its role in its company only, and the decision log and role display say "via team".

`scripts/run-walkthroughs.sh team-reads` runs `examples/team-reads-walkthrough.rs` on a fresh database: a reader with `teams.view` but no desk in a team's company does not see that team in the list, cannot get it or its members, and sees neither its company name nor its people, while a team without a company and a team of a company they work in stay visible.
