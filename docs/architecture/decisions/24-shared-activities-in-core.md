# ADR 24: Shared activities in Core, private by default, reminded in the same module

**Document Type:** Architecture Decision Record
**Status:** Accepted; implemented
**Agents:** claude-code/opus-5.5-high (author)
**Scope:** Tasks, calls, notes and their reminders in `apps/core/activity`; customer workflows stay in CX
**Last Updated:** 2026-11-05 (meetings and invitations)

## Context

**An invoice collector and a purchasing officer both need "call this person by Friday".** Finance, People, Supply Chain, Commerce and Factory all plan and log follow-ups about their own records. The activity record was built in CX (`cx/activities`), so any other Domain would need CX installed, a forbidden Domain-to-Domain edge, or a second activity table.

The CX version had two problems for shared use. Everyone in the filing company could read every activity, so a private HR note about an employee was visible company-wide. And it called `cx/timeline` directly in every write, so it could not move to Core as it was.

## Decision

**Use `apps/core/activity`, owner `core/activity`, Rust crate `bilibi-activity`, menu label "Activities" under Core.** The singular noun matches `company`, `person`, `document` and `product`.

### Core facts

| Fact | Why every installation needs it |
| --- | --- |
| Task, call, meeting or note, with subject, status, times, priority, direction and outcome | Plan or log a follow-up in the same shape everywhere |
| One related record through `base/records` | Find the follow-up from the invoice, order or person it is about |
| Assignee and guests (accounts, people, addresses) | Say who does it and who takes part |
| Shared flag | Let readers of the related record see it, by choice |
| Reminder state | Tell the assignee when it is due |

Core supplies one list, one record page, panels on the company and person pages, a generic panel any record page can place, and one set of operations. Meetings, guests' answers and invitations use the same record (see "Meetings and invitations" below); recurrence and calendars come later.

### Private by default

**An activity is seen by its creator, its assignee and its account guests.** When it is marked shared, people who may view the related record see it too, through the record owner's own read check (`RecordOwner::authorize`). Filing company, desk and the link itself grant nothing. One rule serves every door: pages, panels, agent operations, record pills, the bell and Reports. A tenant-wide oversight read (`core.activity.all.view`) exists, but no contributed role holds it.

### Reminders in the same module

**The scheduled job lives with the record.** `core/activity` contributes an each-tenant schedule (ADR 17) that marks due activities reminded, rings each assignee's bell through a To do source (`base/notifications`) and, when the tenant turns it on, emails them (`base/mail`). The To do row opens the related record. Base gained one small call, `NotificationBoundary::todos_changed_for`, to wake other people's bells after commit.

### Core never calls CX

Core records `core/activity.happened` (`base/events`) when a shared activity has happened, or when `repoint` moves one. CX's customer timeline subscribes and projects its own lines. Private activities are never announced, so a timeline cannot reveal them.

| Owner | Responsibility |
| --- | --- |
| Core Activity | Records, visibility, list, record page, panels, reminders, operations, the event |
| Base | Records references, events, schedule and queue, notifications, mail |
| CX | Customer roll-ups, the customer timeline projection, lead conversion (it calls `repoint`), sales cadences, telephony, routing |
| Other Domains | Their own state machines (approvals, production operations, training obligations). A follow-up may point at them; it does not replace them |

### Meetings and invitations

**Meetings, guests' answers and Send invites are Core too**, added 2026-11-05 when CX's meeting slice was folded in: a purchasing officer meets a supplier as a sales rep meets a customer. Colleagues are told in the bell; people and addresses get an email from a queue worker that runs as the sender.

**Core never asks a Domain directly whether it may email someone.** It declares a registry, `core/activity.invitation_gates`; a Domain that keeps contact rules contributes a gate (CX asks its consent register). Every gate must allow an address before it is emailed. With no gate, Core emails any usable address.

**One reminder per activity, as before.** The sweep that reminds the assignee also puts a call or meeting on its account guests' To do, unless they declined. There is no second reminder table.

## Rejected alternatives

- **Activities in CX:** every other Domain would need CX, or its own activity table.
- **Activities in Base:** a business record with its own screens is not a mechanism.
- **Keep company-wide visibility:** a link to an employee or a shared filing company would expose private staff work.
- **A reminder service in Base:** the sweep needs the activity's own rules (due time, assignee, closed state); one owner keeps them together. Base already gives the schedule, queue, bell and mail.
- **Core calls the timeline through a port:** the projection can wait, so an event is the right seam, and it keeps Core free of any Domain.
- **Move the CX module unchanged:** it carried timeline subjects and the company-wide rule.
- **Meetings and invitations in CX on top of Core:** two reminder sweeps and two guest tables for one record.
- **Core calls `cx/consent` through a port:** a port is named in its provider's namespace, so Core would name CX. A registry Core declares keeps the Domain unnamed.
- **Several reminders per activity with their own offsets:** a second reminder system beside the tenant's lead time; it can come later on the same `reminded_at` model if people need it.

## Consequences

`cx/activities` hands over the records, pages, panels, operations and capabilities; it keeps the timeline projection as a `core/activity.happened` subscriber. Capability, operation, route, table and record-kind names change from `cx.activities.*` to `core.activity.*` in the same change, with no aliases. Under the no-production-users rule (valid before 2027-01-01) the CX table is dropped in a new CX migration and disposable databases are recreated; nothing is copied across.

Reminders depend on each tenant allowing the scheduler (Automation access). Without it no reminder is sent, and the To do stays empty.

## Prior art

The 2026-11-05 CX promotion scout compared Salesforce, Dynamics 365, Odoo, SAP and HubSpot: each shares one activity record across business objects (Salesforce tasks and events, Dataverse activity tables, Odoo `mail.activity.mixin`) while customer journeys and contact-centre work stay in their own products.
