# Activities

Tasks, calls, meetings and notes that people plan and log, each optionally about one record in any area (a contact, a company, an invoice, an order). Each has an assignee.

- **Find** activities with `core.activity.list`: open ones by default, `state: closed` for history, `mine: assignee` for yours, `related: {kind, id}` for one record's (a person's include those they are a guest of). Read one with `core.activity.get`, which adds its guests.
- **Point at a record** with `related: {kind, id}`. Find it first with `base.records.search` (people are kind `person`, companies `company`). An activity cannot be about another activity.
- **Create** a task with `core.activity.create_task` (`title`, optional `due_at`, `priority`), a call with `core.activity.log_call` (`starts_at`, `ends_at`, `status: held` for a call that took place or `planned`, `direction`), a meeting with `core.activity.create_meeting` (`starts_at`, `ends_at`, optional `location`, `join_url`, `meeting_type`), or a note with `core.activity.create_note`. The assignee is you unless you name `assignee_user_id`. You, the assignee and a related person become guests; `guests` adds colleagues (`user_id`), people (`person_id`) and email addresses.
- **Guests.** Add one with `core.activity.add_guest` and remove one with `core.activity.remove_guest` (the assignee stays until you reassign the activity). Read their ids with `core.activity.get`.
- **Invite** with `core.activity.send_invites`, for an open call or meeting still ahead: colleagues are told in the bell, everyone else is emailed the time, place, join link and description (never the internal notes). An installed Domain may refuse an address first (a consent check); that guest is not emailed.
- **Answers.** Record one with `core.activity.respond` (`accepted`, `declined`, `tentative` or `none`). You answer for yourself; recording someone else's needs the right to change the activity. A declined colleague is not reminded and is not counted as busy.
- **Who sees it.** Only you, the assignee and account guests, unless you set `shared: true`: then anyone who may view the related record sees it on that record and opens it. Leave private work (an HR note about a colleague) unshared. Sharing needs a related record.
- **Change** one with `core.activity.update`, its stored `version` and only the fields to change in `patch`. An empty string clears a field that may be empty.
- **Close** with `core.activity.close`: a call becomes held, a task completed. A note has nothing to close. **Delete** with `core.activity.delete`.
- **Reminders.** An open task with a due date, and a planned call or meeting, is reminded to its assignee in the bell's To do a few minutes before it is due (the tenant's setting), and by email when the tenant turns that on. A call's or meeting's account guests who have not declined see it in their To do too. Changing the due time asks for a new reminder; closing it removes it from To do.
- Times are date-times with an offset, stored in UTC. A held call or meeting has ended: logging, changing or closing one so that it is held is refused while its `ends_at` is in the future. Change `ends_at` first.
- You may change, close and delete activities you created or are assigned; with `core.activity.all.manage` as well, any activity you can see.
- Repeating activities and calendars are not available yet.
