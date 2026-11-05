# Activities

`core/activity` keeps the tasks, calls, meetings and notes people plan and log, each optionally about one record in any module (a contact, a company, an invoice, an order). It invites guests to calls and meetings, records their answers, and reminds the assignee and the guests when one is due. Why it is in Core: [ADR 24](../../../docs/architecture/decisions/24-shared-activities-in-core.md).

## What this module owns

- Activities and their guests (`core_activities`, `core_activity_guests`).
- Repeating series: the rule (`core_activity_series`) and one activity per occurrence.
- One Activities list (`/activities`), shown as a table or as a calendar, one record page (`/activities/{id}`) and the Activities panel on record pages.
- Reminders: a per-tenant scheduled sweep, the bell's To do rows and the optional reminder email.
- Invitations: Send invites, the bell notice to colleagues, the invitation email worker, and the `core/activity.invitation_gates` registry a Domain contributes its contact rules to.
- The `core/activity.happened` event, record kind `core.activity.activity` and the agent operations.

Not here: syncing with outside calendars (Google, Outlook, iCalendar feeds), customer roll-ups, the customer timeline, lead conversion, sales cadences, telephony and routing stay in the Domain that owns them. Related records belong to their owners (`base/records`); people and accounts to `core/person` and `core/user`; delivery to `base/mail` and `base/notifications`.

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
| `location`, `join_url`, `meeting_type` | Meetings only: up to 255 characters; an `https://` or `http://` link up to 2,000; plain text up to 100 |
| `due_at`, `start_at` | Tasks, optional; `start_at` not after `due_at` |
| `call_outcome`, `description`, `internal_notes` | Plain text; internal notes are never sent to guests |
| `related_kind`, `related_id` | The `base/records` reference, both or neither |
| `shared` | Readers of the related record see it too. Default `false` |
| `assignee_user_id` | An active account of the tenant; the creator by default |
| `reminded_at` | When the sweep reminded the assignee; cleared when `starts_at`, `due_at` or the assignee changes |
| `series_id`, `series_index` | The repeating series it is an occurrence of, and its number there (1–200); both or neither |
| `created_by`, `created_at`, `updated_at`, `closed_at`, `deleted_at`, `version` | `closed_at` is set exactly while the status is a closing one |

`core_activity_series`: the rule a series was created with (frequency, `every`, weekly `weekdays`, monthly `month_day`, `until` or `occurrences`, the `work_calendar` whose non-working days were left out, and the `timezone` the time repeats in). It is never changed; see [Repeating series](#repeating-series).

`core_activity_guests`: one row per guest. A guest is an account, a Core person or an email address; `role` is `creator`, `assignee`, `related` or `invited`; `response` is `none`, `accepted`, `declined` or `tentative`; `invited_at` is when Send invites last included them and `invite_sent_at` when that invitation's email went out.

## Facade (`core/activity.activities`, `Activities`)

Every write runs in the caller's `MutationTransaction` with its sealed scope and returns one audited `Change` per activity (`core.activity.activity`) and per guest row (`core.activity.guest`). Each write has a `preview_*` twin that checks the same rules with no write.

| Method | Does |
| --- | --- |
| `create_task`, `log_call`, `create_meeting`, `create_note`, `create` | Creates; the creator and assignee become guests, a related person the `related` guest, and `guests` (accounts, people, addresses) are invited |
| `create_series` | Creates a repeating call, meeting or task: the rule and one activity per occurrence ([Repeating series](#repeating-series)) |
| `update_series`, `delete_series` | Change or delete one occurrence and the later ones (`Apply::Following`), or every live one (`Apply::All`) |
| `series` | The rule of the series an activity belongs to, in words and parts, and how many occurrences are live |
| `create_system_task` | A named principal with `core.activity.system.create` creates a task for an active originating account in an explicitly checked company, within the caller's transaction. The account is the creator and defaults as assignee. |
| `update` | Changes the fields a `Patch` names, fenced on `version`; reassigning keeps the old assignee as a guest |
| `close` | Calls and meetings to `held`, tasks to `completed` |
| `close_and_create_new` | Closes, and returns an unsaved copy with the same values and guests |
| `delete` | Soft delete, fenced on `version` |
| `add_guest`, `remove_guest` | Invites an active account, a person the actor may view or an email address (at most 50 guests); removes anyone but the assignee (`AssigneeNotRemovable`). No version; the activity is locked |
| `respond` | Records `accepted`, `declined`, `tentative` or `none`. An account guest answers for themselves with `core.activity.view`; anyone else's answer needs the right to change the activity |
| `send_invites` | For an open call or meeting still ahead: stamps every guest but the sender `invited_at`, tells account guests in the bell, and queues the invitation email to the others (Invitations) |
| `conflicts(starts_at, ends_at, users, except)` | Other open calls and meetings those accounts take part in at that time (as assignee, or a guest who has not declined); another activity's title only for a reader who takes part in it |
| `repoint(from, to)` | Moves every live activity related to `from` onto `to`, after `base/records.check` of both; used when a record is converted into another (a lead into a person) |
| `get`, `guests`, `guest_emails`, `list(Filter)`, `for_record`, `follow_up`, `may_create`, `may_edit`, `may_edit_all` | Reads under the rule above. `Filter::between` reads calls and meetings that overlap a range and tasks due in it (up to `CALENDAR_LIMIT`, 501); `Filter::series` one series' occurrences in order |
| `work_days(first, last)` | The dates as the reader's company's default work calendar resolves them; empty without one |

- A held call or meeting has ended: every write that leaves one held refuses an `ends_at` more than five minutes ahead.

Errors: `Invalid` with field errors, `Stale`, `NotFound`, `Forbidden`, `AlreadyClosed`, `AssigneeNotRemovable`.

## Repeating series

**A repeating call, meeting or task is one activity per occurrence.** Each has its own status, guests' answers and reminder, so a held Monday meeting and next Monday's planned one are two records. The rule is kept beside them.

- **Rule.** Daily, weekly (on chosen days; the start's own day when none is chosen) or monthly (the start's day of the month; a shorter month uses its last day), every 1–99 days, weeks or months. It ends on a last date or after a number of occurrences, never open-ended, and makes at most 200 (`MAX_OCCURRENCES`), all written in the creating transaction.
- **Off-pattern start.** A first date that is not on the pattern (a Tuesday for a series on Mondays) moves to the first date that is.
- **Wall-clock time.** The time repeats in the zone the creator gives (`timezone`; pages pass the reader's zone), so 09:00 stays 09:00 across a daylight-saving change.
- **Working days only.** Leaves out the dates the filing company's default work calendar (`core/work_calendar`, setting `work_calendar.default`) makes rest, off or holiday; the count counts only the dates kept. Refused without a default calendar, and it reaches at most two years ahead.
- Only a planned call or meeting and an open task with a due date repeat; a note and a held call do not. The guests, related record, assignee and sharing are the same on every occurrence.
- **One occurrence.** Every write on one occurrence (Update, Close, Delete, guests, Send invites) touches only it: that is how a series gets an exception. Send invites invites to that occurrence.
- **Change series** (`update_series`) copies the fields a patch names to one occurrence and the later ones, or to all. A new start, end or due time moves each open occurrence by the same wall-clock amount and gives it the same length; held and completed ones keep the times they happened at. Status changes one occurrence at a time. Each occurrence is checked first, so one refusal changes none.
- **Delete series** (`delete_series`) removes one occurrence and the later ones, or all of them, held and completed ones included.
- The rule itself never changes: to repeat differently, delete the later occurrences and create a new series.

## Calendar

**The Activities list shows as a calendar too** (`/activities?display=calendar`, the List and Calendar tabs): day, week, month and shared views of the activities the reader may see, through base/ui `Calendar`. It is the same list: search, kind, open or closed and "whose" filter both.

- Calls and meetings are drawn from start to end; a task at its due time, 30 minutes long. Notes and tasks without a due date are not on it. Colour is the kind (call, meeting, task), never a status.
- The shared view puts each assignee of the activities shown in a column, the reader first; nobody else's private activities appear.
- Non-working days of the reader's company's default work calendar are marked (rest, off, holiday and its name).
- Dragging, resizing and the Move dialog call `core.activity.update` with the version drawn, so a stale or refused move leaves the item where it was and says why. Only open activities the reader may change can be moved; moving one to another person's column is refused (change Assigned to on its page). Moving a task moves its start by the same amount.
- It reads at most 501 activities for the range and asks for a narrower filter past 500.

## Reminders

**A scheduled sweep runs every minute in each tenant that lets the scheduler work there** (`core/activity.reminders`, `Reach::EachTenant`, ADR 17). A tenant administrator allows it on Administration › Authorization › Automation access.

1. It finds open tasks with a due date and open calls and meetings whose time is within the tenant setting `core.activity.reminder_minutes` (default 15), and that have not been reminded.
2. In one audited transaction per 200 it sets `reminded_at` and rings each assignee's bell after commit (`NotificationBoundary::todos_changed_for`), and the bells of a call's or meeting's account guests who have not declined. A run reminds at most 2,000; the next run takes the rest.
3. When `core.activity.reminder_email` is on (default off), it then emails each assignee the kind, subject, time and a link to the activity. Mail is sent after commit; a refused message is logged and not retried, because the bell already holds the reminder.

**The bell's To do** lists the reader's reminded open activities, soonest first, up to five rows and a count: those assigned to them, and calls and meetings they are an account guest of and have not declined. A row opens the related record when the reader may view it, otherwise the activity. Closing, deleting or reassigning it removes it from those people's To do and refreshes their bells at once; declining or being removed takes it off that guest's. Moving its due time or reassigning it waits for a new reminder. See all opens `/activities`.

One activity has one reminder, at the tenant's lead time. There is no second reminder system for meetings. Each occurrence of a series is reminded on its own.

## Invitations

**Send invites tells colleagues in the bell and emails everyone else.** In the caller's transaction it stamps each guest but the sender `invited_at`, notifies the account guests (`core.activity.invitation`, linking the activity), and queues `core/activity-invites` (`{activity_id, invited_at}`), which runs as the sender under `core.activity.manage`.

For each guest without an account invited at that moment and not yet sent, the worker:

1. Takes the address, or the person's email as Core Person lets the sender read it. None when restricted, masked or retired: skipped, and flagged "No email address" on the page.
2. Asks every contributed invitation gate, each in its own transaction, whether that person or address may be emailed now. One refusal skips the guest.
3. Sends one `base/mail` message: who invites them, what, when (in the filing company's zone, named, and in UTC), where, the join link and the description, with `Reply-To` the sender's account address so the guest answers by replying. Never the internal notes, and no link into Bilibi.
4. Stamps `invite_sent_at`.

Delivery is outside any transaction: a crash between sending and stamping sends that guest's invitation once more. A relay refusal retries the unsent guests.

### Invitation gates (`core/activity.invitation_gates`)

A Domain that keeps contact rules (consent, suppressions) contributes an `InvitationGate`. Core never names the Domain.

| Field | Called | Answers |
| --- | --- | --- |
| `may_ask(client, scope)` | By Send invites when anyone is to be emailed | Whether the sender may ask this gate. `false` refuses Send invites (`Forbidden`) |
| `allow(tx, scope, invitee, key)` | By the worker, as the sender, per guest | Whether `Invitee { person, email }` may be emailed now. The gate may log in `tx`. `key` is the same for every retry of one invitation to one guest. `Invalid`, `Forbidden` or `NotFound` skips the guest; any other error retries the job |

With no gate contributed, every usable address is emailed.

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
- Schedule `core/activity.reminders` and its Queue worker `core/activity-reminders`; Queue worker `core/activity-invites`.
- Registry `core/activity.invitation_gates` (declared here; Domains contribute).
- Uses `core/work_calendar` (default calendar and resolved days) for "working days only" and the calendar's day marks.
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

Reads `core.activity.list` (with `from` and `to` a calendar range of up to 500, with `series_id` one series) and `core.activity.get` (with guests and their ids, and the series rule). Reviewed writes `core.activity.create_task`, `.log_call`, `.create_meeting` (each with an optional `repeat`), `.create_note`, `.update`, `.close`, `.add_guest`, `.remove_guest`, `.respond` (declares `view`: a guest answers for themselves), `.send_invites`, `.delete`, `.update_series` and `.delete_series` (`apply`: `following` or `all`). Guide `core.activity`. References are `{kind, id}`; reads add `label` and `path`. Pages and panels call the same write contracts.

## Migrations

`core-activity-001-activities`, `core-activity-002-invitations` (`invite_sent_at` on guests), `core-activity-003-series` (`core_activity_series`, `series_id` and `series_index`).

## How to try it

1. `./bilibi start`, sign in as `ai@agent.my`.
2. Open **Core › Activities** and choose **New task**: pick a related record, set a due time a few minutes ahead.
3. Allow the scheduler on Administration › Authorization › Automation access, and enable **Activity reminders** on System › Schedule.
4. When the reminder comes due, the bell's **To do** tab lists it; the row opens the related record.
5. Choose **New meeting**, set **Repeats** to Weekly on two days, **Ends** after 6 times, and save: the record page says how it repeats, **See all** lists the six, and **Change series** moves the later ones to another hour. The **Calendar** tab shows them on the week, and dragging one moves only it.
6. For a meeting, choose **New meeting**, set a time a few minutes ahead and save. On its page add a person or an email address as a guest, then **Send invites**: the email lands in the development mailbox (`.local/mailbox`) and a colleague guest gets the invitation in the bell, answers it on the page, and sees the reminder in To do.
