# Base Session

**Use it for:** the session store, session checks and signing sessions out.

**Not for:** cookies, tokens or a sessions table of your own.

`base/session.sessions` (`Sessions` in `src/lib.rs`) owns the durable session store: authentication of the opaque cookie token, the account and operator lists, termination commands, the termination broadcast, the session-ended notice seam, and the scheduled sweep. The account page is at `/settings/sessions`; the operator page is at `/system/sessions`, with a dashboard stats panel.

## Token at rest

**A session row stores only the SHA-256 of its cookie token, in lowercase hex (`session_id(token)`), never the token.** Whoever reads `sessions` (the SQL console, a backup, a replica, a slow-query log) cannot use a row to sign in.

- Methods that take a `token` hash it once: `check`, `check_portal`, `activity`, `activity_portal`, `hold`, `switch`, `describe`, `delete`, the session `revoke` keeps, and `announce_tenant_switch`.
- Methods that take an `id` take the stored digest, as listed rows carry it: `terminate`, `terminate_own`, `is_live`, `announce` and `Terminated::id`. The host hashes a socket's cookie once to match announcements.
- A module that writes a session row itself (Core User's sign-in and tenant switch) stores `session_id(token)`. The `sessions_id_digest` check (migration `009-token-digest`) refuses anything else; that migration also ended every earlier session.
- The session hash Tenancy seals, and SSO's `user_external_sessions.session_hash`, is the token's SHA-256, which is `decode(sessions.id, 'hex')`.
- The SQL console refuses a query that names `sessions` (`apps/base/audit/README.md`), and the Database Tables pages mask `id` and `csrf`.

## Guard boundary

`check(client, token)` rechecks absolute and idle expiry, the live tenant and
the Core-owned account verifier without writing session activity. Use it on
background timers, subscription callbacks, socket upgrades and service
revalidation. `activity(client, token)` performs the same checks and renews
only an eligible, still-existing session row. Use it only after accepting
authenticated HTTP navigation or a user event. Invalid events and failed
authorization do not renew activity. Renewals are throttled, and activity takes
a row lock while deciding whether renewal is due; a revocation between lookup
and renewal is refused. Expiry is enforced during authentication and never
depends on the cleanup job.

The idle lifetime and the renewal throttle are the operator settings
`session.lifetime_minutes` (default 120) and
`session.last_activity_touch_minutes` (default 5), declared in
`src/contributions.rs` and read on every check through `Settings::get_global`,
so a change applies to the next request. The throttle never exceeds half the
lifetime (`Policy::from_minutes`). The 24-hour absolute expiry set at sign-in
remains.

The live socket limits are operator settings here too, read by Web's live transport when a socket opens (`docs/live-pages.md`): `session.live.sockets_per_session` (default 16), `session.live.sockets_per_customer` across all that customer's sign-ins (default 16), `session.live.sockets_total` independently for staff and portal on each server (default 5,000 each), `session.live.messages_per_minute` (default 240) and the separate `session.live.chunks_per_minute` upload budget (default 6,000). Portal sockets cannot use staff capacity; every binary chunk is counted before storage or owner work.

## Impersonation

`sessions.impersonator_id` names the operator behind an impersonated session
(migration `002-impersonation`; Core User adds the scoped account key).
`check`/`activity` pass it to Tenancy, which re-proves it and seals it on the
Scope. `switch(sql, token, tenant, from, to)` moves a still-valid session from
one `Holder` to another inside the caller's transaction and rotates its CSRF
secret; a session that no longer holds `from` is `Conflict`. A switch is not
announced as a termination. `revoke` also ends the sessions a user
holds as an impersonating operator.

## Current company

`sessions.company_id` (migration `004-company`; Core Company adds the scoped key) is the company the session chose to work in, `NULL` for Tenancy to select the remembered or first valid desk. `check`/`activity` pass it and a one-way digest of the session credential to Tenancy, which keeps a company on the scope only while the account has a desk there and the identity owner accepts that session for the company's sign-in policy. During authorized impersonation, the viewed account must still have the desk, while the operator's already authenticated live session satisfies the workplace sign-in check without a target-provider session. `activity` forgets a choice Tenancy drops, so a removed desk or no-longer-valid company sign-in cannot come back unasked. An archived company stays a valid, read-only choice. Core User's desk-removal operation separately ends the person's sessions in its desk transaction. `choose_company(sql, scope, company)` writes the choice inside the caller's transaction (Core User's `choose_company`, which proves work access and company sign-in against the same session before writing and audits the switch); it names the session by the sealed scope's CSRF proof, never a token, and a session that ended is `Conflict`. `switch` does not carry the choice: entering an impersonation starts from the viewed account's remembered or first valid desk and parks the operator's choice in `sessions.held_company_id`, a choice made while impersonating belongs to that view alone, and leaving restores the operator's choice. `activity` forgets only the choice of the identity that holds the row, never the parked one. A company choice rotates nothing: the host's live sockets bind the company they were opened in and reload at their next event.

## Step-up

The web proof of Base Step-up (`apps/base/step_up/README.md`) lives on the session row (migration `003-step-up`), so sign-out, termination and expiry end it, and `switch` clears it.

| Column | Meaning |
|---|---|
| `stepped_up_at` | The holder's last check here. The cap counts from it |
| `step_up_used_at` | The last protected action the window covered. The sliding window counts from it |
| `step_up_spent_at` | When an "ask every time" action used the last check |

- `step_up(sql, scope)` records a check inside the verifying owner's transaction (Core User's authenticator). It names the session by the sealed scope's CSRF proof, never a token.
- `step_up_window(client, scope, window, cap, extend)` reads, and with `extend` restarts, the window.
- `spend_fresh_step_up(client, scope, seconds)` lets one recent check cover one "ask every time" action.
- An impersonated session is never stepped up.
- `revoke` also ends the holder's agent step-up windows (`bilibi_step_up::end_agent_windows`).

## Listing

`/system/sessions` needs the platform operator identity and
`admin.system.session.list`; terminating needs `admin.system.session.manage`
too. The contributed system roles `auditor` and `system_viewer` list sessions.
The list is platform-global, like `count`, because the store is not
tenant-owned; it reads only metadata and never a payload. A session marked ended (below) is left out of both. Each row names its
user through `base/principal_directory`: a user in the operator's tenant shows
their name, one outside it shows `User <id>`, so no name crosses a tenant
boundary. The user column sorts by `user_id`. Search matches the IP address or
user agent recorded once at sign-in by `describe` (the host passes the proxy's
`X-Forwarded-For`/`X-Real-IP` and `User-Agent`; a direct connection records no
address). `sessions.signed_in_at` records admission time and has the same retention as the session row. `describe` also appends the sign-in (time, address, user agent and its device label) to `session_sign_ins`, keyed by tenant and user, which `previous_sign_in` reads for the latest sign-in before the current session; it survives sign-out, expiry and "sign out everywhere else", and the expiry sweep deletes rows older than 90 days. The UI says "No earlier sign-in on record" when none is found; the place of a sign-in is not resolved. Rows and termination requests are keyed by a digest of the session id (`Summary::key`), never by the bearer token.

The dashboard entry `base-dashboard-session-stats` (embed `session.dashboard-stats`,
slot `dashboard.sessions`) shows the open count, reading only while
`admin.system.session.list` is effective and otherwise an em dash.

## Your sessions

`/settings/sessions` lists only the signed-in person's active staff sessions. Each row shows the device label derived from the user agent, IP address, sign-in time, last activity and whether it is this device. There is no platform geo-IP lookup, so the page and account popup show the IP address and do not guess a city. The account popup shows the previous sign-in from `session_sign_ins` (time, device, IP address), or "No earlier sign-in on record".

An impersonating operator, agent bearer or system principal cannot open the page or read the previous-sign-in summary. A row command sends only `OwnSummary::key`; the mounted page keeps the stored id, and the mutation locks and deletes only a row with the sealed scope's tenant and user. The current session has no sign-out command. **Sign out everywhere else** excludes it.

## New-device sign-in email

**A staff sign-in from a device the account has not used in the last 90 days emails the account holder.** The history, judgement and daily claim are here (`src/sign_ins.rs`); Core User sends the email (`apps/core/user/src/sign_in_alert.rs`).

| Rule | Detail |
| --- | --- |
| Device | `client_name(user_agent)`: browser family and operating system, for example "Chrome on Windows". Not the version, not a fingerprint |
| New | `describe` returns `Some(NewDevice)` when the account's retained history already holds an earlier sign-in and none of them has the same device label |
| Not new | The first sign-in on record (nothing to compare), an impersonated session, and a portal customer's sign-in (the portal has no Your sessions page) |
| Limit | `claim_new_device_alert(tx, scope, device)` claims one email per account, device and UTC day in `session_sign_in_alerts` and audits it (`session_sign_in_alert`, action `user.new_device_sign_in_email`). A repeat sign-in is not new anyway, so the claim only matters for sign-ins that race |
| Timing | The web host calls `UserService::alert_new_device` after `describe`; it runs in the background, so a failure is logged and never delays or fails the sign-in |
| Content | Time, device and IP address, a link to `/settings/sessions` and "If this wasn't you, change your password". No secret, and no link that signs anything out by itself |

- Someone using the same browser and system as the account holder is not detected; the label is coarse on purpose, so a browser update does not raise an email.
- A device unused for 90 days counts as new again, because the history is pruned. After every row is pruned, the next sign-in counts as the first and sends nothing.
- There is no per-person setting to turn the email off yet.
- The expiry sweep deletes claims older than yesterday.

## Ending a session

`terminate(tx, id, current)` is the operator command: the caller's own session is refused before any row is read (`Termination::CurrentSession`), the delete is an audited mutation recording whose session ended and from where (the payload is marked redacted; the token is never written), and the termination is announced after the commit. `terminate_own` and `terminate_others` are the account commands: both re-bind every target to the sealed tenant and user, audit each ended session and keep the current session. `end_tenant(tx, tenant)` ends every session of one tenant inside the caller's transaction (Core Tenant's suspend operation, which holds the platform capability): each ended session is an audited change without its token, and every open page of it is told after the commit. `delete(tx, scope, token)` is logout, in the caller's transaction: the holder's own session, with no session audit row (the transaction's action row and the notice's event row remain). `revoke(tx, user, keep)` (`keep` the token of a session to leave open) runs inside an owner's mutation transaction (Core User's password change and reset), invokes every registered factor revocation hook and delegated-credential revoker, and returns the ended ids for `announce` after that commit, so a rolled-back credential change signs nobody out or disconnects an app. Each delegated revoker receives a `RevocationAccount` carrying a `CredentialRevocationReason` (password changed or reset, signed out, administrator action, or the neutral account changed) taken from the transaction's action; unknown actions map to account changed.

`subscribe_terminations()` is the broadcast the host consumes: every live
socket subscribes once (`apps/web/src/live.rs`) and, when its own session is
announced, sends the browser back through the page it holds and closes. The
page's guard then forwards to sign-in with the expired-session message. A
lagged subscriber may miss an announcement; the next guarded action or 15 s
refresh still refuses an ended session, so promptness, not correctness, depends
on the broadcast. `prune_expired` announces each session it removes, so what a
session held (an AI connection's switch) can be shown as ended. The broadcast
is in process and carries the stored session id; Domains use the durable notice
above instead. A session that
expired but is not yet pruned announces nothing; its holders re-check on their
own schedule.

## Session-ended notice

**Every ending records one durable notice per account and reason, in the ending transaction: the `core/user.session_ended` event on Base Events.** It names the tenant, the account and why; never the token, its digest or the session ID. A Domain subscribes to it by name (payload and versions: [Core User](../../core/user/README.md#session-ended-notice-coreusersession_ended)). Session decides when a notice is due (`src/ended.rs`); Core User records it through the `EndNotice` it binds at installation, because Session cannot depend on Base Events. Boot is refused without that binding.

| How the session ended | Reason | Recorded |
| --- | --- | --- |
| Logout (`delete`), portal logout, the portal replacing a browser's earlier session | `signed_out` | With the sign-out |
| The holder ends one or all of their other sessions (`/settings/sessions`) | `signed_out` | With the sign-out, one notice for all of them |
| The identity provider's back-channel sign-out (`revoke` under `user.sso.backchannel_logout`) | `signed_out` | With the sign-out |
| An operator terminates it (`terminate`) in the operator's own tenant | `terminated` | With the termination |
| A credential or account change ends it (`revoke`: password change or reset, desk removal, deprovisioning, erasure, a second-factor reset) | `terminated` | With the change |
| It lapsed: idle beyond the lifetime or past the absolute expiry | `expired` | By the expiry sweep, which deletes it |

- **Account.** The holder of the session; for an impersonated session, the impersonating operator, since the viewed account was never signed in.
- **One per account and reason.** Ending three of an account's sessions in one transaction is one notice with `sessions: 3`. An ended session that had already lapsed is reported `expired`.
- **Delayed to the sweep.** An operator ending another tenant's session, and an ending written as an impersonator (whose writes may not enqueue work), cannot record in that transaction. The row is marked ended instead (`ended_reason`, migration `008-ended-reason`): it stops authenticating at once and leaves the operator list, and the next sweep deletes it and records the notice in the session's own tenant.
- **No notice.** Suspending a tenant (`end_tenant`): nothing runs in a suspended tenant, including event delivery. A session moved into another membership of the same identity (Core User's tenant switch and `retire_replaced_session`): the tenant left hears nothing. A Domain's own heartbeat sweep still covers these and dropped connections.

Two reads answer "is anyone still here" without a session ID:

| Read | Answers |
| --- | --- |
| `signed_in(client, scope, account)` | Whether the account holds any live session in the scope's tenant: its own, or one where it is the impersonating operator. `scope` is a sealed `Scope` or the `SystemScope` an event subscriber receives (`TenantProof`). Another tenant's sessions never count |
| `stands(client, scope)` | Whether the session behind this sealed scope still stands: not signed out, terminated, moved or expired. A page's beat uses it to end itself before its socket closes. Answered from the scope's proof; returns only the bool |

## Cleanup

**Prune expired sessions** (`src/expiry.rs`, `base/session-expiry`, every five
minutes) deletes rows past the absolute expiry, idle beyond the lifetime
setting, or marked ended. Each tenant's rows are deleted in one write as the `session-expiry` system principal, which records the notices above; a tenant that fails keeps its rows for the next run while the others commit. Review and enable it at `/system/schedule` before expecting scheduled
cleanup or expiry notices; the enablement and delivery rules belong to Base Schedule.

The same sweep deletes `session_sign_ins` rows more than 90 days after the sign-in and `session_sign_in_alerts` claims after one to two days, for every tenant in one statement each. Until an operator enables the task, neither table is pruned, so enable it in production.

## Sign-in history and privacy

**A person's erasure does not delete their sign-in history; the 90-day sweep does.** Each record holds the sign-in time, device label, IP address and user agent. The new-device email claims (`session_sign_in_alerts`: account, device label, day) also stay through erasure, but the sweep deletes them after one to two days, far sooner than the history.

| Event | What happens to `session_sign_ins` | Why |
| --- | --- | --- |
| Personal-data export | `Sessions::sign_in_history` lists the person's records (time, device, address, user agent, and when the sweep deletes each) and any held new-device email claims in `core/user.accounts`, under each account | A person may see what we hold about them |
| Personal-data erasure | Records stay. `Sessions::sign_in_retained_until` lets Core User report them on the certificate as kept (`session.sign_in_history`) until the newest one's 90-day end | They are security evidence: they show where and when an account was used, which an investigation of account takeover needs after the owner is gone |
| 90 days after a sign-in | The expiry sweep deletes the record | The retention is short and fixed, so nothing is kept longer than the evidence is useful |

Records older than 90 days are never exported or reported as kept, even in the minutes before the sweep reaches them. Audit rows for sessions keep the IP address and user agent: that is unchanged and follows Audit's own retention and legal minimum (`apps/base/audit/README.md`).

## Try it

On a migrated disposable database, install the walkthrough fixture
(`cargo run -p bilibi-walkthrough-fixture`; it appoints tenant 1 as the
platform operator, which the production migration never does), run the app
and sign in as `admin@example.test`
(role `admin`, grant-all). Open `/dashboard` for the Sessions strip and
`/system/sessions` for the list. Sign in as another account in a second
browser profile, then terminate its row: that browser's open page is sent to
sign-in at once, and the row disappears. Your own row carries the Current badge
and no terminate control.

`Sessions::with_factors(step_up)` wires the factor registry; the production service factory does this. A standalone fixture that constructs `Sessions::new` must wire it before calling `revoke`, or revocation fails closed. Factor hooks retain enrollment and lockouts while clearing pending challenges and method-owned proof. A delegated-credential owner registers one `CredentialRevoker` during service installation; its hook receives the trusted account and caller's SQL transaction and returns every affected record as a `Change`. The [Step-up lifecycle contract](../step_up/README.md#factor-lifecycle) is authoritative for factors; `CredentialRevoker` is the account-wide reset seam for other credentials.
