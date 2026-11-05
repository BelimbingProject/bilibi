# Activities

Tasks, calls and notes that people plan and log, each optionally about one record in any area (a contact, a company, an invoice, an order). Each has an assignee.

- **Find** activities with `core.activity.list`: open ones by default, `state: closed` for history, `mine: assignee` for yours, `related: {kind, id}` for one record's (a person's include those they are a guest of). Read one with `core.activity.get`, which adds its guests.
- **Point at a record** with `related: {kind, id}`. Find it first with `base.records.search` (people are kind `person`, companies `company`). An activity cannot be about another activity.
- **Create** a task with `core.activity.create_task` (`title`, optional `due_at`, `priority`), a call with `core.activity.log_call` (`starts_at`, `ends_at`, `status: held` for a call that took place or `planned`, `direction`), or a note with `core.activity.create_note`. The assignee is you unless you name `assignee_user_id`. You, the assignee and a related person become guests.
- **Who sees it.** Only you, the assignee and account guests, unless you set `shared: true`: then anyone who may view the related record sees it on that record and opens it. Leave private work (an HR note about a colleague) unshared. Sharing needs a related record.
- **Change** one with `core.activity.update`, its stored `version` and only the fields to change in `patch`. An empty string clears a field that may be empty.
- **Close** with `core.activity.close`: a call becomes held, a task completed. A note has nothing to close. **Delete** with `core.activity.delete`.
- **Reminders.** An open task with a due date, and a planned call, is reminded to its assignee in the bell's To do a few minutes before it is due (the tenant's setting), and by email when the tenant turns that on. Changing the due time asks for a new reminder; closing it removes it from To do.
- Times are date-times with an offset, stored in UTC. A held call has ended: logging, changing or closing a call so that it is held is refused while its `ends_at` is in the future. Change `ends_at` first.
- You may change, close and delete activities you created or are assigned; with `core.activity.all.manage` as well, any activity you can see.
- Meetings, invitations and repeating activities are not available yet.
