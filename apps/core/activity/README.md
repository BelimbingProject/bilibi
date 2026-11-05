# Activities

`core/activity` keeps the tasks, calls and notes people plan and log, each optionally about one record in any module (a contact, a company, an invoice, an order), and reminds the assignee when one is due. Why it is in Core: [ADR 24](../../../docs/architecture/decisions/24-shared-activities-in-core.md).

## What this module owns

- Activities and their guests (`core_activities`, `core_activity_guests`).
- One Activities list (`/activities`), one record page (`/activities/{id}`) and the Activities panel on record pages.
- Reminders: a per-tenant scheduled sweep, the bell's To do rows and the optional reminder email.
- The `core/activity.happened` event, record kind `core.activity.activity` and the agent operations.

Not here: customer roll-ups, the customer timeline, lead conversion, sales cadences, telephony and routing stay in the Domain that owns them. Related records belong to their owners (`base/records`); people and accounts to `core/person` and `core/user`; delivery to `base/mail` and `base/notifications`.

## Who sees an activity

**An activity is private to the people taking part:** the creator, the assignee and account guests. Filing company, desk and a link to a record grant nothing.

| Reader | Sees it |
| --- | --- |
| Creator, assignee, account guest | Always |
| Anyone who may view the related record | Only when the activity is **shared** (`shared = true`, which needs a related record) |
| Holder of `core.activity.all.view` | Every activity of the tenant (oversight; no contributed role holds it) |

- The same rule answers every read: `get`, the list, panels, the agent operations, `base/records` pills and the bell. A shared activity counts when the related record's owner answers `Readable` for the reader (`RecordOwner::authorize`).
- The list without a record filter shows the activities you take part in. Shared activities on a record show on that record's panel and on `/activities?related=<kind>:<id>`, after `base/records.check` of the record for you.
- Reports (Grid table `activities`, capability `core.activity.view`) read only the activities the reader takes part in; the shared rule needs the owner's record check, which a Grid source cannot ask.
- Changes, Close and Delete need `core.activity.manage` and an activity you created or are assigned; with `core.activity.all.manage` as well, any activity you can see.
- An activity cannot be about another activity.

## Records

`core_activities`, tenant-scoped, soft-deleted, `version` for stale-save refusal.

| Field | Notes |
| --- | --- |
| `company_id` | Filing company: the creator's company. It grants no access. An account with no company cannot create activities |
| `kind` | `call`, `meeting`, `task`, `note`. Fixed in code |
| `subject` | 1–160 characters (`title` in the operations) |
| `status` | Calls and meetings: `scheduled`, `held`, `not_held`, `canceled`. Tasks: `not_started`, `in_progress`, `pending_input`, `completed`, `deferred`. Notes: none |
| `priority`, `direction` | Tasks (`high`, `medium`, `low`); calls (`inbound`, `outbound`) |
| `starts_at`, `ends_at` | Calls and meetings, required, `starts_at < ends_at`, UTC |
| `due_at`, `start_at` | Tasks, optional; `start_at` not after `due_at` |
| `call_outcome`, `description`, `internal_notes` | Plain text; internal notes are never sent to guests |
| `related_kind`, `related_id` | The `base/records` reference, both or neither |
| `shared` | Readers of the related record see it too. Default `false` |
| `assignee_user_id` | An active account of the tenant; the creator by default |
| `reminded_at` | When the sweep reminded the assignee; cleared when `starts_at` or `due_at` changes |
| `created_by`, `created_at`, `updated_at`, `closed_at`, `deleted_at`, `version` | `closed_at` is set exactly while the status is a closing one |

`core_activity_guests`: one row per guest. A guest is an account, a Core person or an email address; `role` is `creator`, `assignee`, `related` or `invited`.

## Facade (`core/activity.activities`, `Activities`)

Every write runs in the caller's `MutationTransaction` with its sealed scope and returns one audited `Change` per activity (`core.activity.activity`) and per guest row (`core.activity.guest`). Each write has a `preview_*` twin that checks the same rules with no write.

| Method | Does |
| --- | --- |
| `create_task`, `log_call`, `create_note`, `create` | Creates; the creator and assignee become guests, a related person the `related` guest |
| `update` | Changes the fields a `Patch` names, fenced on `version`; reassigning keeps the old assignee as a guest |
| `close` | Calls and meetings to `held`, tasks to `completed` |
| `close_and_create_new` | Closes, and returns an unsaved copy with the same values and guests |
| `delete` | Soft delete, fenced on `version` |
| `repoint(from, to)` | Moves every live activity related to `from` onto `to`, after `base/records.check` of both; used when a record is converted into another (a lead into a person) |
| `get`, `guests`, `list(Filter)`, `for_record`, `follow_up`, `may_create`, `may_edit` | Reads under the rule above |

Errors: `Invalid` with field errors, `Stale`, `NotFound`, `Forbidden`, `AlreadyClosed`.

## Reminders

**A scheduled sweep runs every minute in each tenant that lets the scheduler work there** (`core/activity.reminders`, `Reach::EachTenant`, ADR 17). A tenant administrator allows it on Administration › Authorization › Automation access.

1. It finds open tasks with a due date and open calls and meetings whose time is within the tenant setting `core.activity.reminder_minutes` (default 15), and that have not been reminded.
2. In one audited transaction per 200 it sets `reminded_at` and rings each assignee's bell after commit (`NotificationBoundary::todos_changed_for`). A run reminds at most 2,000; the next run takes the rest.
3. When `core.activity.reminder_email` is on (default off), it then emails each assignee the kind, subject, time and a link to the activity. Mail is sent after commit; a refused message is logged and not retried, because the bell already holds the reminder.

**The bell's To do** lists the reader's reminded open activities, soonest first, up to five rows and a count. A row opens the related record when the reader may view it, otherwise the activity. Closing, deleting or reassigning it removes it from that person's To do; moving its due time waits for a new reminder. See all opens `/activities?mine=assignee`.

## Events

`core/activity.happened`, version 1, recorded in the write's transaction when a **shared** activity has happened: a held call or meeting, a completed task, a note. It is recorded once per change that makes it so: created that way, closed, or shared after it happened; and again by `repoint` for each moved one. Private activities are never announced.

| Payload | Meaning |
| --- | --- |
| `activity` | Activity id |
| `kind` | `call`, `meeting`, `task`, `note` |
| `happened` | `call.logged`, `meeting.held`, `task.completed`, `note.added` |
| `occurred_at` | The start of a call or meeting, when a task was completed, when a note was written (UTC, never ahead of now) |
| `related` | `{kind, id}` or `null` |
| `people` | Person ids of the person guests |
| `reason` | `happened`, or `repointed` with `from: {kind, id}` (the activity's `related` is now the target) |

The subject is `core.activity.activity:<id>` and the key `<id>:<version>`. The payload never carries the subject line or any text, because subscribers may show it to people who cannot open the activity.

## Contributions

- Capabilities: `core.activity.view`, `core.activity.manage`, `core.activity.all.view`, `core.activity.all.manage`. The `tenant_owner` role gets `view` and `manage` only.
- Settings (Settings › Activities, tenant): `core.activity.reminder_minutes`, `core.activity.reminder_email`.
- Schedule `core/activity.reminders` and its Queue worker `core/activity-reminders`.
- Record kind `core.activity.activity` (page `/activities/{id}`); holder table `activities`.
- Menu item Activities under Core.
- Panels:

| Embed | Context | Path the host passes |
| --- | --- | --- |
| `core/activity.company` | `Record { kind: "core/company" }` | `panel`, `company_id` |
| `core/activity.person` | `Record { kind: "core/person" }`: related to the person, or where they are a guest | `panel`, `person_id` |
| `core/activity.record` | `Record { kind: "*" }`, slot `record.activities` | `panel`, and the record's `base/records` `kind` and `id` |

Any record page shows the generic panel by placing `core/activity.record` with its own kind and id; the panel checks the record with `base/records.check` before it reads.

## Agent operations

Reads `core.activity.list` and `core.activity.get` (with guests). Reviewed writes `core.activity.create_task`, `.log_call`, `.create_note`, `.update`, `.close`, `.delete`. Guide `core.activity`. References are `{kind, id}`; reads add `label` and `path`. Pages and panels call the same write contracts.

## Migrations

`core-activity-001-activities`.

## How to try it

1. `./bilibi start`, sign in as `ai@agent.my`.
2. Open **Core › Activities** and choose **New task**: pick a related record, set a due time a few minutes ahead.
3. Allow the scheduler on Administration › Authorization › Automation access, and enable **Activity reminders** on System › Schedule.
4. When the reminder comes due, the bell's **To do** tab lists it; the row opens the related record.
