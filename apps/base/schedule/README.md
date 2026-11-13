# Schedule definitions, review and occurrence claims

**Use it for:** recurring work at set times, once per installation or once in every tenant, run through Queue.

**Not for:** `tokio::time::interval`, sleep loops, cron crates or looping over tenants.

Schedule holds installation-global recurrence definitions, the operator's
decision about them, and the durable claim that hands exactly one Queue job to
each intended occurrence. Definitions, fingerprint review, suppression and the
cron/zone arithmetic are Q3; occurrence claiming, the Queue insertion, overlap
leases and run history are Q4; the operator board at `/system/schedule` and
history retention are Q5. Work itself runs on the Queue, never inline.

## Per-tenant reach (issue #138, ADR 17)

A definition declares `reach: Reach::Installation` (one job in the operator tenant) or `Reach::EachTenant` (one job in every tenant that has consented). Reach is part of the fingerprint, so changing it makes the operator's review stale. The full decision is [ADR 17](../../../docs/architecture/decisions/17-per-tenant-scheduled-work.md). A reference definition lives in `examples/schedule-walkthrough.rs`; production builds carry none.

The following describes the default `scheduler` execution identity. An owner that needs business permissions selects its own declared principal through [Owner business authority](#owner-business-authority); the same named grants, consent, Queue checks and tenant-run lifecycle apply to that selected identity.

**Contribute an each-tenant worker like any scheduled worker (see below), plus `bilibi_queue::CAPABILITY_POLICIES` = `bilibi_schedule::TENANT_POLICY` under the worker id.** Queue then decides each attempt on exactly one capability: the binding's `EXECUTE` in the operator tenant, `TENANT_WORK` for the `scheduler` principal and `TENANT_RUN` for a person in any other tenant. Boot refuses an each-tenant definition whose worker lacks the policy.

How one occurrence runs:

1. The scheduler claims the occurrence once, in the operator tenant, exactly as for an installation definition. The claimed job is the Platform worker `base/schedule-fan-out`, not the owner's. Operator **Run now** queues the same fan-out (as the `scheduler` principal, audited as the operator) and fans out to every tenant.
2. The fan-out walks the tenant directory (`Tenants::principal_page`, 200 at a time, in id order). It needs `scheduler` to hold `EXECUTE` and `admin.tenancy.tenant.list` in the operator tenant. The first attempt stores the highest tenant id it saw (`fan_out_through`); a retry walks the same cohort, so a tenant created later joins the next occurrence, never this one.
3. For each tenant it writes one `base_schedule_tenant_runs` row, unique on `(occurrence, tenant)`:
   * suspended tenant: `skipped` (`tenant_suspended`);
   * no consent: `not_granted`. A tenant administrator grants or withdraws scheduled work on Administration → Authorization → **Automation access** (`/authz/automation`), or through `base.authz_agent.automation_consent.set`. Both call Authz's existing `Grants::set_system` contract for `scheduler` and `TENANT_WORK`; the operator tenant uses `EXECUTE`. Declaration and provisioning grant nothing, and a denying grant wins;
   * previous run of this definition in this tenant still unfinished (`Overlap::Forbid`): `skipped` (`overlap`). Other tenants proceed;
   * otherwise `queued`, written in the same audited transaction, in that tenant, as the tenant's `scheduler` principal, as the job it links to (`Queue::enqueue_system`).
4. A row that already exists is left alone, so a retried or resumed fan-out never queues a second job for a tenant. Queue is still at least once, so the owner's effect must be idempotent.
5. The owner's handler runs through `Schedule::run` exactly as before. For an each-tenant definition it binds to that tenant's run row (job id and tenant), and `execution.tenant()` names the tenant; the owner writes with that tenant's scope. Before the first attempt starts, the same per-key check as for installation work applies (reviewed fingerprint, enabled, not paused); disabling or changing the definition therefore cancels every unstarted tenant job (`schedule_disabled`, `schedule_changed`, …). `started_at` is the durable start marker: a retry of a run that already started continues even if the definition is disabled meanwhile.
6. Revoking consent or suspending the tenant cancels that tenant's waiting job with `actor_refused` (Queue re-proves tenant, declaration and grant on every attempt). Every poll settles unfinished tenant runs from Queue's state: `completed` is `succeeded`, a cancelled or discarded job is `failed` with Queue's failure code, a missing job is `failed` (`linked_job_missing`). No replacement job is ever created.

**One board, two audiences.** `/system/schedule` is the only Schedule page. An operator (`admin.system.schedule.view`) sees every definition; an each-tenant row's **Result** cell reads the latest fan-out on its second line ("2 of 4 tenants succeeded · 1 skipped · 1 not granted"), and History filters by tenant ID and names the tenant. A tenant administrator (`admin.schedule.tenant.view`) sees the same page narrowed to each-tenant definitions and their own tenant's runs; the Result cell says plainly when the tenant has not allowed the task ("Not granted here: an administrator must allow this task to run here.") and has no Settings tab or platform health. Another tenant's rows are never read: `Schedule::tasks` and `Schedule::run_page` pick the audience from the caller's capabilities on every call and a tenant caller's tenant filter is always its own.

**Tenant Run now.** A tenant administrator with `admin.schedule.tenant.execute` (`TENANT_RUN`) runs one each-tenant definition now for their own tenant (`Schedule::run_now_for_tenant`). It queues the owner's worker as that person, through the same review, pause and per-tenant overlap checks, and records a manual occurrence plus the tenant's run row in one audited transaction. It confers no scheduler grant, so it works even when the tenant has not consented to scheduled runs. The operator tenant is refused: its operators use the board's Run now, which fans out to every tenant. Even when the definition allows overlap, a tenant has one manual run of a task at a time: Run now is refused (`overlap`) while that tenant's previous manual run of the same key is unfinished, so repeated clicks cannot queue a job each.

Missed minutes still coalesce once at the claim, so write each-tenant work as "find what is due now". Firing at a tenant's local wall-clock time, per-tenant cron or pause, and per-company fan-out are out of scope: run hourly and let the worker compare the tenant's own settings.

Owners contribute a `Definition` to `base/schedule.definitions` with
`Contribution::new`; the contribution id is the durable key
(`^[a-z0-9][a-z0-9._/-]{0,254}$`) and the provenance module is the owner. A
definition names a five-field cron, an IANA zone, a task name, a Queue `worker`
id, bounded arguments, an overlap policy and the one misfire policy
(`Coalesce`). The worker must be a `base/queue.workers` entry owned by the same
module and must accept the arguments; boot refuses unknown zones, macros
(`@daily`), seconds/year fields, `L`/`W`/`#`/`?`, expressions that can never
match a date (`0 0 31 2 *`), unbounded arguments, the reserved
`__bilibi_schedule__` argument and an owner route that is not an internal path.
A restricted day-of-month and a restricted weekday must both match.

## Scheduled workers: use `worker::validate` and `Schedule::run`

The claim enqueues the definition's arguments plus bounded claim metadata under
`RESERVED_ARGUMENT`. A scheduled worker therefore must:

* validate through `bilibi_schedule::worker::validate(args, mine)`, so the
  owner's validator sees only its own arguments and the metadata is kept. Boot
  refuses a definition whose worker drops that metadata, so a hand-rolled
  validator cannot get as far as a refused claim;
* bind its handler (`base/queue.handlers`) with `capability: EXECUTE`, and for
  an each-tenant definition also contribute `TENANT_POLICY` (above);
* run its `Handler::perform` body through `Schedule::run(execution, args, |args|
  ...)`. Before the owner's code starts, the definition must still
  exist with the same fingerprint, be enabled and unsuppressed, and the durable
  occurrence (or, for an each-tenant definition, the tenant's run row) must be
  bound to this job; otherwise the
  occurrence is failed and the job cancelled with `schedule_removed`,
  `schedule_changed`, `schedule_unreviewed`, `schedule_disabled`,
  `schedule_suppressed` or `invalid_schedule_occurrence`. Pausing, disabling or
  materially changing a definition therefore cancels work that has not begun.
  Each attempt is a `base_schedule_runs` row; a job without metadata is
  cancelled with `missing_schedule_metadata`.

The owner still makes its business effect idempotent: Queue is at least once,
and the occurrence claim prevents duplicate enqueue, not duplicate effects after
a worker failure.

## Who runs the work

### Owner business authority

An each-tenant definition can select a dedicated, owner-declared system principal instead of `scheduler`. Contribute `ExecutionPrincipal { principal: "<owner-worker>" }` to `EXECUTION_PRINCIPALS` under the definition's id, from the same module. Declare that principal in the module's Authz `VocabularyContribution`, with `EXECUTE`, `TENANT_WORK` and the exact business capabilities its ordinary facade needs. Use a separate principal when work needs a different authority bound. This is an optional contribution; existing definitions keep their current identity and fingerprint.

Boot refuses a selection for a missing, foreign or installation-only definition, a worker not bound to `EXECUTE`, an undeclared principal, a principal declared by another module, a bound missing either schedule key, or a principal with standing grants. The principal name and its sorted, complete declared capability bound enter the definition's review fingerprint. Changing either needs a fresh operator review before new work starts; the Enable confirmation shows the execution name and permission bound. Deploying the declaration or enabling Schedule grants no capabilities.

The fan-out still runs as `scheduler`, with only its existing operator `EXECUTE` and tenant-directory grants. The owner's job runs as its selected principal in each target tenant. That tenant must explicitly grant **that principal** `TENANT_WORK` (the operator tenant grants it `EXECUTE`) through Authz's `Grants::set_system` / reviewed `set_system_in` contract. A grant to `scheduler` does not consent to another principal's work. Grant its business capabilities separately through the same owner contract; its declared bound is the maximum possible grant, never an automatic allow. The tenant board reads consent for each definition's actual execution principal. Tenant Run now continues to run as the person asking, under that person's own business permissions.

Administrators review these named grants on the shared **Automation access** page (`/authz/automation`) or through Authz Agent's installed system-principal read/reviewed-set operations. The [Authz Agent contract](../authz_agent/README.md) owns the review, stale-rule fence and grant authority; a business owner adds no grant editor of its own.

The handler still contributes `TENANT_POLICY`, enters `Schedule::run`, and takes the real sealed scope from `Execution::scope()`. Use `Execution::begin` for the lease fence and call the ordinary owner facade with that scope, retaining the fence while a standalone facade opens its short transaction. Queue re-proves the selected principal, tenant and schedule grant on every attempt and at the fence. The facade rechecks its business capabilities in its own mutation transaction; direct denies, live grant/declaration removal, tenant suspension, field restrictions, Workflow subject authorization and locked-record rules still apply. A principal has no staff account, desk or approval vote. Do not reconstruct a staff scope, change tenants on a scope, or introduce a worker-only business bypass.

Every attempt also checks that its captured principal is still the definition's selected identity. An owner-principal job whose reviewed definition or timing version changed is cancelled with `schedule_changed`, including a retry that had already started; a fresh reviewed occurrence must start the new version. Disabling an otherwise unchanged definition retains the established started-run retry rule. A person's manual run continues to use their own ordinary authority.

### Disposable owner proof

`apps/web/examples/scheduled-owner-reference.rs` links two each-tenant definitions and one owner principal only into its private executable. On a fresh disposable database, run `bilibi-web migrate` and then `BILIBI_ENV=development bilibi-web seed-production` before the example. That install seed stores the Dev instance role, which turns ordinary step-up enforcement off (base/step_up README). The example installs only the walkthrough fixture, whose administrators enroll no second factor: without that seed every grant command below fails with `StepUpRequired { reason: "not_enrolled" }`. Set `BILIBI_DISPOSABLE_SCHEDULE_DEMO=yes`, `DATABASE_URL`, `BIND_ADDR` and an exactly matching `APP_ORIGIN`. Run the executable with `grant-fan-out 1` to give only the scheduler its infrastructure grants, then `serve` and visit `/system/schedule`. Review its execution identity and bound in Enable, then enable it. Before owner grants it records `not_granted` without an owner job.

Run the executable with `catalog` for the public boot-contract proof without a database. It checks missing and foreign definitions, undeclared and foreign principals, installation reach, missing schedule keys and standing grants. A reordered bound retains the review fingerprint; a widened one changes it.

After the explicit owner grants, `notifications 1` proves the Notifications boundary's principal self-record is a no-op, an account's self-record persists exactly once, rollback leaves no row, invalid titles and explicit recipients remain refused, and a principal can still notify a real account explicitly. The same boundary is called by Company's ordinary facade during the scheduled effect.

Create and archive a non-primary fixture company through the normal Company page, then run `locked-owner TENANT_ID COMPANY_ID` after granting that principal the business keys. It proves the granted principal's normal Company write is still refused for the archived record, with its value and version unchanged.

The example also contributes **Scheduled locked owner reference**, targeting company `101` (the first new company in a fresh walkthrough). After creating and archiving that company in tenant 2, review and enable this separate definition. Its real automatic tenant-2 job carries the same sealed principal and lease fence, reaches the ordinary owner facade, and is cancelled with `owner_refused` without changing the archived row. Tenant 1 has no such company and cannot reach tenant 2's record. No additional principal capability is declared for this case.

Run the same executable with `grant 1` and `grant 2` to give the fixture administrators' explicit grants through `Grants`; the operator also grants the scheduler its two fan-out keys. The worker fences the real Queue execution and calls Company's ordinary facade to set the tenant primary company's legal name to its job reference. Inspect `/companies/1` and `/companies/100` in their respective fixture tenants, together with the Schedule history and audit attribution `System · scheduled-owner-reference`.

`revoke-business 2` removes only the owner update grant. The next scheduled job still passes consent but the ordinary owner permission check refuses it (`owner_refused`), leaving the company unchanged. `deny-business 2` proves an explicit deny; `revoke-consent 2` prevents subsequent owner jobs; `consent-list 2` reads Authz Agent's Automation access operations and asserts the shared `scheduler` list omits both owner definitions while the owner principal's consent key, and its current grant, appear under installed system principals; `invalid-grant 1` asserts that granting a key outside the declared bound is refused. To exercise waiting-job and retry reproof, change grants or suspend a fixture tenant before its next Queue attempt; Queue cancels refused identities with `actor_refused`. These commands and browser actions belong only to the disposable fixture runtime; production carries no reference definition or automatic grants.

For a deterministic retry reproof, retain a discarded real owner job in the operator tenant. After `revoke-consent 1` removes that principal's `EXECUTE`, `retry 1 JOB_ID` uses Queue's audited `retry_for` command; it authorizes the fixture administrator and accepts only an owner job visible in this definition's own tenant history. Queue retains the captured system identity and cancels it with `actor_refused` on its next attempt. The administrator's own authority never replaces that captured identity.

### Installation and default tenant work

A recurrence has no user behind it. For `Reach::Installation`, the scheduler
enqueues as the named system principal `scheduler` (`PRINCIPAL`) in the
platform operator's tenant (`platform_operator`); without an operator nothing
is claimed. Declaration grants nothing: an operator must grant the principal `EXECUTE` in that tenant
(`Grants::set_system(admin, "scheduler", EXECUTE, Some(true))`, capability
`admin.authz.system-principal.grant`). Queue re-proves the principal, its
declaration and that grant on every attempt (`Queue::enqueue_system`,
`Execution::identity`); an ungranted principal's jobs cancel with
`actor_refused`. Run-now (`Schedule::run_now`) queues a `manual` occurrence as
the operator who asked, after `authorize_operator(EXECUTE)`, so that job runs as
that user and its run row names them.

## Claims, coalescing and overlap

`Scheduler` polls every 15 s in each app process (`bilibi-web run` starts it
beside the Queue executor; `Scheduler::poll(now)` takes the clock so any moment
can be replayed). Each poll first settles unfinished occurrences whose job
Queue already reports terminal (`completed` is `succeeded`, cancelled,
discarded or missing is `failed`, and the lease is released), then for every
definition computes `previous(now)` and claims it when it is newer than the
key's watermark (the latest intended instant ever claimed). Downtime
**coalesces**: only the latest missed occurrence is considered, the instants in
between are never claimed. Both passes of a DST fold are real occurrences; a
gap's missing minute is dropped.

`Schedule::claim` is one audited transaction under `lock(key)`: eligibility,
reconciliation of that key, the `(key, intended_at, trigger)` uniqueness check,
the forbid-overlap check, the Queue job (`base/queue.job`) and the occurrence
row (`base/schedule.occurrence`) commit or roll back together, so two app
processes polling the same instant produce exactly one occurrence and one job;
the loser sees `AlreadyClaimed`. `Overlap::Forbid` sets `overlap_key` on the
unfinished occurrence (a partial unique index: one active lease per key); a
later occurrence is refused with `Overlap` and recorded once as a `skipped` run
until the earlier job succeeds, is cancelled or exhausts its retries. Refusals
write nothing.

`base_schedule_runs` is best-effort operational evidence (running, succeeded,
failed, skipped; trigger and who asked for a manual run); recording failure is
logged and never changes the owner's outcome. An attempt whose process died or whose end could not be written would stay `running` for ever, so every reconcile pass (`occurrence::reconcile`, each poll) turns an attempt row that nothing runs any more into `failed` with the excerpt `abandoned`: a later attempt of the same job exists, or its occurrence (a tenant's run: that tenant's run row) is already settled. A pass walks the unfinished occurrences and tenant runs oldest first in batches of 300, past the ones whose job is still alive (up to 40 batches), so a backlog of waiting runs does not keep a later run whose job ended from settling. Its reads are indexed (`base-schedule-005-reconcile-indexes`), and the watermark is read with one index probe per key. `Schedule::runs` reads it
redacted with `admin.system.schedule.view`.

## Review and suppression

Every definition is `Unreviewed` until an operator reviews its **fingerprint**
(`Registered::fingerprint`): SHA-256 over key, name, expression, zone, owner,
task name, worker, canonicalised arguments, overlap and misfire, plus any selected execution principal and its declared bound. Changing any of
those in code makes a stored review stale, so a redeploy can never silently run
changed work; the owner route is display-only and not part of it. Review state
is `Unreviewed`, `Enabled` or `Disabled` for the current fingerprint.
Suppression is separate: a row in `base_schedule_suppressions` means paused and
deleting it means resumed. Rows are keyed by text, not foreign keys, so a removed
or renamed definition leaves its row as orphan evidence and a new key can never
inherit a decision.

`Schedule::review`, `suppress` and `resume` take the sealed `Scope`, require the
persistent operator identity plus `admin.system.schedule.manage`
(`authorize_operator`), and write the state and its Audit evidence in one
`MutationTransaction` under a per-key advisory lock (`schedule::lock`). A
repeated `suppress`/`resume` changes nothing and records nothing. `tasks` needs
`admin.system.schedule.view` and returns redacted facts (no arguments).
`admin.system.schedule.execute` gates run-now.

`Schedule::eligibility` is checked on a claim and again, under the same lock, when a job first starts. Any database error is an `Err`, never `Eligible`.

## Changing a task's time

Operators with Schedule manage permission can select a fixed-time task's schedule, choose a time, and save it in the zone shown beside the picker. For example, a daily task at 03:17 UTC opens at 11:17 in Asia/Kuala_Lumpur; saving 09:30 stores `30 9 * * *` with `Asia/Kuala_Lumpur`.

- The picker uses the account's top-bar clock: company, browser-local, or UTC. The browser reports its IANA zone before local editing becomes available. Changing the top-bar clock while editing leaves the open draft and its displayed zone together.
- The saved zone stays with the setting; daylight saving follows that zone. Gaps skip the missing occurrence and folds can run twice, using the recurrence rules below.
- Only schedules with one fixed hour and minute have a picker. Intervals and multiple daily times keep their cadence. Calendar rules convert with the clock where one cron can represent them; a month-boundary conversion that cannot be represented keeps the saved-zone read view.
- **Reset to code default** asks for confirmation and restores the contribution's time and zone. Saving and resetting preserve the task's enabled/disabled and paused state; they do not enable an unreviewed task.

`Schedule::set_time` and `reset_time` require the sealed operator scope and recheck `MANAGE` inside `MutationBoundary`, under the same per-key lock used by claims. The installation-wide `base_schedule_timings` row holds the cron, IANA zone, contributed fingerprint and revision; Audit records the operator, old value and new value as `schedule.time.changed` or `schedule.time.reset`. The submitted timing version refuses stale edits. These writes use the board's existing declared pending allowance until Schedule has operation contracts.

A new contributed fingerprint ignores both the old override and its review. A timing revision also changes queued-job metadata, so work queued at an earlier time is cancelled before the owner runs. Reset retains a revision tombstone, preventing old default-time jobs from becoming valid again. Dispatch and diagnostics use the effective recurrence; a changed setting starts with future occurrences rather than immediately catching up an occurrence from before the change. `Task.expression` and `Task.timezone` remain the code defaults; `clock_expression`, `clock_timezone`, `timing_version` and `time_overridden` describe the effective setting.

## Time

`cron` knows only civil minutes. `recurrence` is the only place a zone matters,
and every function takes the instant as an argument:

* **Fold** (clocks repeat): both instants are real. `previous(now)` is the latest
  that has happened, so a poll before the repeat gets the first and a poll after
  it gets the second; `next(now)` is the earliest still strictly ahead.
* **Gap** (clocks skip): the occurrence does not exist and is dropped, not
  shifted. `next` resumes from the first wall time after the gap; `previous`
  searches before it.
* `previous` follows the exact rule. `next` is display only and also considers the repeat that is still ahead while the first pass of a fold is running.

The board's Schedule column reads each cron in plain words in the viewer's top-bar zone, converted at the zone's offset now ("Every day at 11:17 PM EDT"). The words name that zone, because a daylight-saving change moves a fixed clock time. A cadence with no clock time ("Every 5 minutes") has no zone to name. When no single clock time reads right in the viewer's zone, the words stay in the saved zone and say so ("Every day at 3:17 AM (UTC; shown in saved time zone)"). Read-only schedules keep the exact saved cron and zone in the tooltip. The words do not change dispatch: the effective cron and saved zone decide when the task runs, and Next run is the real next instant in the viewer's zone.

Replay any moment with `cargo run -p bilibi-schedule --example schedule-walkthrough -- dst [key instant]`
(no database). The other modes use a disposable database
(`BILIBI_DISPOSABLE_SCHEDULE_DEMO=yes`, `DATABASE_URL`, after `bilibi-web migrate`
and `BILIBI_ENV=development bilibi-web seed-production`, which stores the Dev instance
role: without it the grant steps fail with `StepUpRequired { reason: "not_enrolled" }`;
they install the walkthrough fixture); they link a private worker, handler and five definitions that
production does not have:

```sh
export BILIBI_DISPOSABLE_SCHEDULE_DEMO=yes
cargo run -p bilibi-schedule --example schedule-walkthrough -- timing    # clock settings, stale work, DST, authorization, audit
cargo run -p bilibi-schedule --example schedule-walkthrough -- review    # Q3: review, suppression, audit
cargo run -p bilibi-schedule --example schedule-walkthrough -- prepare   # operator tenant, enable, grant scheduler
cargo run -p bilibi-schedule --example schedule-walkthrough -- coalesce  # one claim after five missed hours
cargo run -p bilibi-schedule --example schedule-walkthrough -- race 2026-06-03T10:00:00Z   # two processes, one row
cargo run -p bilibi-schedule --example schedule-walkthrough -- overlap   # lease refuses, skipped run, cancel releases
cargo run -p bilibi-schedule --example schedule-walkthrough -- execute 2026-06-04T12:00:10Z # scheduler/user/stale review
cargo run -p bilibi-schedule --example schedule-walkthrough -- worker    # live, real clock; run it in two terminals
cargo run -p bilibi-schedule --example schedule-walkthrough -- inspect
```

`worker` claims the every-minute definitions and executes them; a second
`worker` process (set `SCHEDULE_DEMO_KEEP=1` so it does not reset the first)
shows that each minute is claimed by exactly one of them, and the 75-second
task's next minute is refused for overlap and recorded as skipped while it runs.

## Operator board (`/system/schedule`)

The board is `src/web.rs`, a framed live
page behind `AnyOf(admin.system.schedule.view, admin.schedule.tenant.view)` with a System →
Schedule menu entry (the operator and tenant audiences above). Its read model is `src/board.rs`: tasks with their latest
run (filtered and sorted in memory over the bounded definition
list), an exact run-history page filtered, ordered and paged in PostgreSQL
(`Schedule::run_page`; the sort is an enum, never a client identifier), and
diagnostics that report the local scheduler loop's heartbeat, Queue and run
recorder reads and due work independently (`Unavailable`/`Unknown`, never a
healthy default). The URL uses these names (`tab`, `task_search`,
`task_status`, `task_sort`, `task_dir`, `run_search`, `run_status`,
`start_date`, `end_date`, `run_sort`, `run_dir`, `page`, `page_size`).

`view` reads, `execute` queues Run now, `manage` enables, disables, pauses,
resumes and changes retention. The page shows controls from the actor's
effective capabilities, but every command first re-asks
`authorize_operator` for its capability and the Schedule API checks it again
inside the transaction; a refusal flashes "You do not have permission for that
action." and redraws the controls without clearing the page. Enable, Pause and
Disable confirm through the shared `ConfirmDialog`, which holds the task it
describes; the confirmation acts on that held task, never on a key the client
sends. History dates bound calendar days in the zone the Started column shows: company, browser-local or UTC according to the saved display mode. The shared `BrowserTimeZone` hook reports the local zone and display-mode changes; the display policy validates it and UTC is the fallback.

The board's tables use independent shared `ListColumns`/`FlexTable` arrangements: show/hide and reorder columns, row-height steps, Compact/Normal and Reset. History alone has pagination (25/50/100 rows); tasks remain unpaged. Facts a cell shows on a second line (overlap and misfire policy, last runtime, run runtime and trigger) are optional columns through `ListColumns::with_record_fields`; the default columns do not change.

## Retention and contributed housekeeping

`schedule.history.keep_days` is a global integer setting (default 90, 0–3650,
zero keeps everything), changed from the board's Settings tab
(`board::set_keep_days`, operator + `manage`, audited as
`schedule.retention.changed`). The daily `base/schedule.occurrence-retention`
task (03:37 UTC, forbid overlap) deletes finished occurrences and finished run
rows older than that; unfinished claims and each key's latest scheduled claim
(the watermark) are never deleted. The same daily task prunes both.

`base/schedule.queue-retention` (03:47 UTC, forbid overlap) deletes finished
Queue jobs older than the global setting `queue.jobs.keep_days` (default 30,
0–3650, zero keeps everything, changed on System Settings). Schedule
contributes it because Queue is upstream of Schedule; what is deleted and kept
is Queue's rule (`Queue::prune`, `apps/base/queue/README.md`).

Base Session contributes `base/session-expiry` (every five minutes), which
deletes sessions past their absolute or idle expiry (`Sessions::prune_expired`).
Both tasks follow the scheduled-worker contract above (`worker::validate`,
`Schedule::run`, a handler using the typed Schedule capability policy). Like
every definition they are unreviewed until an operator enables them on the
board, and installation-reach tasks run only after `scheduler` is granted
`EXECUTE` in the operator tenant. Copy
`src/retention.rs` or `apps/base/session/src/expiry.rs` for a new housekeeping
task rather than starting a Tokio interval of your own.
