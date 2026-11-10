# Activities

`core/activity` keeps the tasks, calls, meetings and notes people plan and log, each optionally about one record in any module (a contact, a company, an invoice, an order). It invites guests to calls and meetings, records their answers, and reminds the assignee and the guests when one is due. Why it is in Core: [ADR 24](../../../docs/architecture/decisions/24-shared-activities-in-core.md).

## What this module owns

- Activities and their guests (`core_activities`, `core_activity_guests`).
- Repeating series: the rule (`core_activity_series`) and one activity per occurrence.
- One Activities list (`/activities`), shown as a table, a calendar or as the tasks in tiles by status, one record page (`/activities/{id}`) and the Activities panel on record pages.
- Reminders: a per-tenant scheduled sweep, each activity's own reminder offsets (else the creator's default, else the tenant's), the bell's To do rows and the optional reminder email to the assignee and to guests without an account.
- Invitations: Send invites, the bell notice to colleagues, the invitation email worker, and the `core/activity.invitation_gates` registry a Domain contributes its contact rules to.
- The calendar feed: one read-only iCalendar link per person, and its key ([Calendar feed](#calendar-feed)).
- The `core/activity.happened` event, record kind `core.activity.activity` and the agent operations.

Not here: pulling in outside calendars (Google, Outlook, iCalendar subscriptions); the feed below only publishes. Customer roll-ups, the customer timeline, lead conversion, sales cadences, telephony and routing stay in the Domain that owns them. Related records belong to their owners (`base/records`); people and accounts to `core/person` and `core/user`; delivery to `base/mail` and `base/notifications`.

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
| `reminded_at` | When the sweep last reminded; cleared, with the offsets already reminded at, when `starts_at`, `due_at` or the assignee changes |
| `reminder_minutes`, `reminder_email` | Its own reminders ([Reminders](#reminders)): up to five offsets in minutes before (0–10,080), `[]` for none, `NULL` for the default; whether each reminder is also emailed, `NULL` for the default |
| `series_id`, `series_index` | The repeating series it is an occurrence of, and its number there (1–200); both or neither |
| `created_by`, `created_at`, `updated_at`, `closed_at`, `deleted_at`, `version` | `closed_at` is set exactly while the status is a closing one |

`core_activity_series`: the rule a series was created with (frequency, `every`, weekly `weekdays`, monthly `month_day`, `until` or `occurrences`, the `work_calendar` whose non-working days were left out, and the `timezone` the time repeats in). It is never changed; see [Repeating series](#repeating-series).

`core_activity_guest_reminders`: one row per guest reminder email sent (guest, the time it was for, the offset), so a retried job never emails a guest twice.

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
| `delete_related(id, related)` | For the owner of `related` (Core Document's expiry reminder), after its own authorization: removes task `id` when it is still about `related`, whoever created it; a task already gone is left alone |
| `create_person_guest(id, guest_id, name)` | Creates a Core person from an email-address guest (`core/person` creation, under the actor's own right to create people) and makes the guest that person instead of the address, keeping their answer and invitation. Refused when a person already has the address |
| `reminder_defaults`, `set_reminder_defaults` | The reader's own default offsets and email choice ([Reminders](#reminders)) |
| `add_guest`, `remove_guest` | Invites an active account, a person the actor may view or an email address (at most 50 guests); removes anyone but the assignee (`AssigneeNotRemovable`). No version; the activity is locked |
| `respond` | Records `accepted`, `declined`, `tentative` or `none`. An account guest answers for themselves with `core.activity.view`; anyone else's answer needs the right to change the activity |
| `send_invites` | For an open call or meeting still ahead: stamps every guest but the sender `invited_at`, tells account guests in the bell, and queues the invitation email to the others (Invitations) |
| `conflicts(starts_at, ends_at, users, except)` | Other open calls and meetings those accounts take part in at that time (as assignee, or a guest who has not declined); another activity's title only for a reader who takes part in it. The record page draws it as **Free** or **Busy** beside each account guest (the title only when the reader may see it), before any invitation goes out |
| `feed_key`, `feed_keys`, `create_feed_key`, `revoke_feed_key`, `feed(token)` | The calendar feed key and its read ([Calendar feed](#calendar-feed)) |
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
- **Delete series** (`delete_series`) removes one occurrence and the later ones, or all of them, held and completed ones included. A repeating row on the Activities list has a **Delete series** action (following or all) that opens the record page's dialog and calls the same contract with the same refusals; "only this one" stays the row's own Delete on the record page.
- The rule itself never changes: to repeat differently, delete the later occurrences and create a new series.

## Calendar

**The Activities list shows as a calendar too** (`/activities?display=calendar`, the List, Calendar and Status tiles tabs): day, week, month, year and shared views of the activities the reader may see, through base/ui `Calendar`. It is the same list: search, kind, open or closed and "whose" filter both.

- Calls and meetings are drawn from start to end; a task at its due time, 30 minutes long. Notes and tasks without a due date are not on it. Colour is the kind (call, meeting, task), never a status.
- **Year.** Twelve small months; each day shows how many items it holds (`!` and red when one is overdue) and links to its day view. It reads the whole year, so a busy year asks for a narrower filter past 500 items.
- **Create from a free slot.** Click a free time (or drag down it; a tap on touch) and Add at this time asks for a meeting, a call or a task, then opens the same New dialog (`core.activity.create_*`) with the start and end filled in (a task falls due at the start; a call in the future is planned). A click, tap or Enter on an empty day uses 09:00 for an hour. A reader who may not create activities gets no slot events.
- **Overdue and soon.** An open call, meeting or task whose start or due time has passed is marked **Overdue** (red tint and the word); one starting or due inside the next 24 hours is **Due soon** (amber). The same marks show on the status tiles and the Status badge. The 24 hours is fixed (`IMMINENT_HOURS` in `web/mod.rs`); a tenant setting is the follow-up.
- The shared view puts each assignee of the activities shown in a column, the reader first; nobody else's private activities appear.
- Non-working days of the reader's company's default work calendar are marked (rest, off, holiday and its name).
- Dragging, resizing and the Move dialog call `core.activity.update` with the version drawn, so a stale or refused move leaves the item where it was and says why. Only open activities the reader may change can be moved; moving one to another person's column is refused (change Assigned to on its page). Moving a task moves its start by the same amount.
- It reads at most 501 activities for the range and asks for a narrower filter past 500.

### Status tiles

`/activities?display=board`: the tasks the reader may see as tiles in a column for each status (not started, in progress, pending input, completed, deferred), through base/ui `Board`. Search and "whose" filter it; the kind and open-or-closed filters are hidden, because the tiles are tasks and closed ones have their own columns. A tile shows its subject, an Overdue or Due soon mark, the due time and the assignee. Dragging a tile (or Move…) calls `core.activity.update` with the status and the tile's version, after checking the task is still in the column it was drawn in; a stale or refused move says why and leaves the tile. It reads the first 300 open tasks (by due time) and the latest 100 closed ones, and says when there are more.

## Calendar feed

**A person can publish their own calendar as a read-only iCalendar link** and subscribe to it from Google Calendar, Outlook or Apple Calendar. The feed is one more key to the same door, not a second set of rules.

- **Address.** `GET /feeds/activities/{key}` (a `PUBLIC_ENDPOINTS` contribution, so Web redacts the key from logs, sends no referrer and rate-limits each sender per minute: Base Integration setting `public_endpoints.sender_per_minute`). The key is the only credential.
- **Key.** 256 random bits as 64 hex characters, one per person and tenant. Only its SHA-256 is stored (`core_activity_feed_keys`); the key is shown once, when it is made, on **My settings › Calendar feed** (`/settings/calendar-feed`) and in the result of `core.activity.create_feed_key`.
- **Same rule as the page and the agent API.** Each read acts as the key's owner: it proves the account live, then reads `Activities::list` for calls and meetings between 30 days ago and a year ahead, and tasks due in that range (at most 501), the activities the owner takes part in (creator, assignee, or an account guest who has not declined; [Who sees an activity](#who-sees-an-activity)): theirs, never another person's, whoever holds the link. A holder of `core.activity.all.view` gets their own calendar too, not the tenant's. If the owner loses `core.activity.view` or is deactivated, the feed answers as for an unknown key.
- **What it shows.** Subject (tasks start with "Task: "), start and end, location, and the join link of a meeting. Never the description, internal notes, guests, related record, status or reminders.
- **Make, replace, revoke.** The person makes a key, replaces it (the old link stops working at once) or revokes it on that page, which also shows when it was made and last read (a usage stamp, at most once a minute). **Making or replacing a key asks for a fresh second factor** where the tenant's policy says so (Base Step-up operation `core.activity.feed_key.create`, on the floor), and only the browser may make one: `core.activity.create_feed_key` is a Web-only operation, so an AI connection or a review card never holds the key (the key would sit in model context and in a stored review outcome). The operations are `core.activity.feed_key` (read), `.create_feed_key` and `.revoke_feed_key` (an agent may revoke); creating, replacing and revoking are audited (`core.activity.feed_key`, no key or hash in the audit). A holder of `core.activity.feed.manage` (granted on purpose; no contributed role has it) also sees everyone's keys on that page and revokes any (`user_id`). Revoking deletes the row, so the link answers `404` from the next request.
- **One refusal.** An unknown, revoked, replaced or unusable key, and a request framed in an iframe, get the same bare `404`. Replies are never cached.

## Reminders

**A scheduled sweep runs every minute in each tenant that lets the scheduler work there** (`core/activity.reminders`, `Reach::EachTenant`, ADR 17). A tenant administrator allows it on Administration › Authorization › Automation access.

**Which offsets an activity reminds at**, first that is set:

1. The activity's own `reminder_minutes` (up to five offsets of 0–10,080 minutes before it starts or is due; `[]` is no reminder). Each occurrence of a series has its own; Change series copies one to the later occurrences.
2. The creator's default (`core.activity.default_reminder_minutes`, a personal setting), copied onto the activity when they create it. A person sets it with **Reminder defaults** on the Activities list; it changes no activity they already made.
3. The tenant setting `core.activity.reminder_minutes` (default 15), one reminder, read at each sweep.

**Whether it also emails** follows the same order: the activity's `reminder_email`, the creator's default (`core.activity.default_reminder_email`), then the tenant setting `core.activity.reminder_email` (default off). A note has no reminders.

1. The sweep finds open tasks with a due date and open calls and meetings with an offset that has come due and not been reminded at (`reminders_sent`).
2. In one audited transaction per 200 it stamps `reminded_at` and the offsets, and rings each assignee's bell after commit (`NotificationBoundary::todos_changed_for`), and the bells of a call's or meeting's account guests who have not declined. A run reminds at most 2,000; the next run takes the rest. An activity with several offsets due at once (the sweep was down) is reminded once.
3. When email is on for the activity, it then emails the assignee the kind, subject, time and a link to the activity. Mail is sent after commit; a refused message is logged and not retried, because the bell already holds the reminder.
4. For a call or meeting with email on, it queues `core/activity-guest-reminders` as the assignee (else the creator), who must still hold `core.activity.manage`. It runs as that person, not as the sweep's principal, because an invitation gate is asked for a sender (their desk, their right to ask) and the guest's `Reply-To` is that person; Queue re-proves the live account, desk and capability on every attempt, so it grants nothing the assignee lacks. The job emails each guest **without an account**, **sent the invitation** (`invite_sent_at`) and **not declined**: the guest's address, or a person's email as Core Person lets the assignee read it. It skips a closed activity and one moved since the reminder was due, asks every `core/activity.invitation_gates` gate for each guest exactly as the invitation worker does (one refusal skips the guest), sends one `base/mail` message (what, when in the filing company's zone and UTC, where, the join link; `Reply-To` the assignee; never the internal notes or a link into Bilibi), and records it in `core_activity_guest_reminders`. Delivery is outside any transaction: a crash between sending and recording sends that guest's reminder once more; a relay refusal retries the unreminded guests.

**Guest reminders are transactional mail, not marketing.** A guest receives one only about a meeting they were invited to, so Core asks no consent of its own and depends on no Domain. A Domain that keeps contact rules (a suppression, a consent) contributes an invitation gate, which then also decides these emails; with none contributed, every usable address is emailed.

**The bell's To do** lists the reader's reminded open activities, soonest first, up to five rows and a count: those assigned to them, and calls and meetings they are an account guest of and have not declined. A row opens the related record when the reader may view it, otherwise the activity. Closing, deleting or reassigning it removes it from those people's To do and refreshes their bells at once; declining or being removed takes it off that guest's. Moving its due time or reassigning it waits for new reminders. See all opens `/activities`.

There is no second reminder system for meetings. Each occurrence of a series is reminded on its own.

## Invitations

**Send invites tells colleagues in the bell and emails everyone else.** In the caller's transaction it stamps each guest but the sender `invited_at`, notifies the account guests (`core.activity.invitation`, linking the activity), and queues `core/activity-invites` (`{activity_id, invited_at}`), which runs as the sender under `core.activity.manage`.

For each guest without an account invited at that moment and not yet sent, the worker:

1. Takes the address, or the person's email as Core Person lets the sender read it. None when restricted, masked or retired: skipped, and flagged "No email address" on the page.
2. Asks every contributed invitation gate, each in its own transaction, whether that person or address may be emailed now. One refusal skips the guest.
3. Sends one `base/mail` message: who invites them, what, when (in the filing company's zone, named, and in UTC), where, the join link and the description, with `Reply-To` the sender's account address so the guest answers by replying. Never the internal notes, and no link into Bilibi.
4. Stamps `invite_sent_at`.

Delivery is outside any transaction: a crash between sending and stamping sends that guest's invitation once more. A relay refusal retries the unsent guests.

### Create a person from a guest

**An email-address guest with no Core person can become one.** On the activity page (and `core.activity.create_person_guest`) an address guest shows **Create person** when the reader may create people and no person has that address. The dialog asks the name; the person gets the address as their email. In one transaction the guest row points at the new person instead of the address, keeping its answer, invitation and reminders. Refused when a person already has the address (pick them instead) or the reader may not create people.

### Invitation gates (`core/activity.invitation_gates`)

A Domain that keeps contact rules (consent, suppressions) contributes an `InvitationGate`. Core never names the Domain. The same gates decide the guest reminder emails ([Reminders](#reminders)).

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

- Step-up: `core.activity.feed_key.create`.
- Capabilities: `core.activity.view`, `core.activity.manage`, `core.activity.all.view`, `core.activity.all.manage`, `core.activity.feed.manage` (revoke anyone's feed key). The `tenant_owner` role gets `view` and `manage` only.
- Settings (Settings › Activities, tenant): `core.activity.reminder_minutes`, `core.activity.reminder_email`. Personal (user scope, set with Reminder defaults on the Activities list): `core.activity.default_reminder_minutes`, `core.activity.default_reminder_email`.
- Schedule `core/activity.reminders` and its Queue worker `core/activity-reminders`; Queue workers `core/activity-invites` and `core/activity-guest-reminders`.
- Registry `core/activity.invitation_gates` (declared here; Domains contribute).
- Public endpoint and credential path `/feeds/activities/{token}` (service `core/activity.feed`); page `/settings/calendar-feed`, a tab of My settings.
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

Reads `core.activity.list` (with `from` and `to` a calendar range of up to 500, with `series_id` one series) and `core.activity.get` (with guests and their ids, and the series rule). Reviewed writes `core.activity.create_task`, `.log_call`, `.create_meeting` (each with an optional `repeat`), `.create_note`, `.update`, `.close`, `.add_guest`, `.create_person_guest`, `.remove_guest`, `.respond` (declares `view`: a guest answers for themselves), `.send_invites`, `.delete`, `.update_series` and `.delete_series` (`apply`: `following` or `all`), `.set_reminder_defaults`, `.create_feed_key` and `.revoke_feed_key`; and the reads `core.activity.reminder_defaults` and `core.activity.feed_key`. Guide `core.activity`. References are `{kind, id}`; reads add `label` and `path`. Pages and panels call the same write contracts.

## Migrations

`core-activity-001-activities`, `core-activity-002-invitations` (`invite_sent_at` on guests), `core-activity-003-series` (`core_activity_series`, `series_id` and `series_index`), `core-activity-004-reminders` (`reminder_minutes`, `reminder_email` and `reminders_sent` on activities, `core_activity_guest_reminders`), `core-activity-005-calendar-feeds` (`core_activity_feed_keys`, one row per person, removed with the account).

## How to try it

1. `./bilibi start`, sign in as `ai@agent.my`.
2. Open **Core › Activities** and choose **New task**: pick a related record, set a due time a few minutes ahead.
3. Allow the scheduler on Administration › Authorization › Automation access, and enable **Activity reminders** on System › Schedule.
4. When the reminder comes due, the bell's **To do** tab lists it; the row opens the related record.
5. Choose **New meeting**, set **Repeats** to Weekly on two days, **Ends** after 6 times, and save: the record page says how it repeats, **See all** lists the six, and **Change series** moves the later ones to another hour. The **Calendar** tab shows them on the week, and dragging one moves only it.
6. For a meeting, choose **New meeting**, set a time a few minutes ahead and save. On its page add a person or an email address as a guest, then **Send invites**: the email lands in the development mailbox (`.local/mailbox`) and a colleague guest gets the invitation in the bell, answers it on the page, and sees the reminder in To do.
7. Choose **Reminder defaults** on the Activities list, pick "1 hour and 10 minutes before" and email on, and save. A meeting you create now starts with them; on its page, **Reminders** and **Email reminders** change them in place. Add an email-address guest, **Send invites**, then **Create person** on that guest to turn the address into a person. Enable the schedule (the scheduler needs the two grants on Automation access, including the tenant directory one in the operator tenant): the next sweep reminds the assignee and emails the guest.
