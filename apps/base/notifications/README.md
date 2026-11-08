# Notifications and to-dos

**Use it for:** telling people something happened (the bell) and putting work on their To do list.

**Not for:** a notifications table, inbox or bell of your own.

Other modules send notifications through `NotificationBoundary`; Core User stores and delivers them after commit.

Call `notify` inside the owner's mutation transaction with a validated `Notice` and explicit recipients. The boundary records each recipient once and wakes them only after commit. Core binds the persistence adapter through `bind` at boot.

`record` tells the acting account about its own write. For a named system principal it validates the title and records no self-notification, because that identity has no account or bell. Ordinary account writes retain their transactional self-notification. A system worker can still call `notify` for explicit people; empty, non-positive and ineligible recipients remain refused.

## To-do sources

The bell keeps actionable work separate from unread notifications.

An owner installs a `TodoSource` through `NotificationBoundary::register_todos` in its service factory. Its `read` receives the signed-in person's sealed scope and returns a bounded `TodoList`: live-authorized items, their count, whether that count is a lower bound, and a same-origin `see_all` link. The bell never reads an owner's tables or decides its permissions. Each source must recheck its own view permission and record eligibility; a denied source returns an empty list without a link.

Workflow supplies approvals using the same paged worklist read as `/workflow/approvals`. It advances past hidden candidates until the list ends, 100 eligible requests have been found or five pages have been read. The bell displays five rows and shows `N+` when the scan stopped before the end; hidden candidates never contribute. The full page retains its cursor for later requests.

A to-do has no read flag. Opening the bell, visiting a row and marking notifications read do not complete it; only the owner's normal action can remove it. The bell refreshes when opened, when switching tabs and on notification delivery. Approval rows open their request details, where the owner's normal decision is available. Each source supplies one See all link. A source that fails or returns a link that is not same-origin is logged and skipped, so the other sources and the notifications still show. A completed decision calls `todos_changed` inside its transaction to refresh the acting person's bell after commit, without creating a notification. Work that adds to or removes from other people's to-dos (Core Activity's reminder sweep, and closing, deleting or reassigning a reminded activity) calls `todos_changed_for(tx, recipients)` to refresh those people's bells after commit, again without a notification.
