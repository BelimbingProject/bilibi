## Oversight audit limits

See [Grid's oversight probe contract](../apps/base/grid/README.md#reads-inside-an-existing-transaction) for audit recovery and commit uncertainty.

## Exceptions

apps/base/grid/src/oversight.rs: get_detached: A replacement checkout gives the parent audit one bounded chance to commit after the audit connection is discarded before the parent commit is sent. The caller's transaction cannot preserve this evidence on rollback.
apps/web/examples/r1-reports.rs: get_detached: The disposable walkthrough holds independent transactions to block sources and audit writes while the Reports check runs.

## File length

apps/base/agent_api/src/lib.rs: keep: Connection disposal and rollback stay with the shared transactional read boundary that owns them.
apps/base/grid/src/oversight.rs: keep: The probe's read, audit and deadline handling share the oversight fan-out and evidence builders. Keeping those rules together makes the audit boundary traceable within Grid.
apps/base/grid/src/query.rs: keep: The oversight errors extend the shared query error type and belong in its existing display and database conversions.
apps/base/reports/src/lib.rs: keep: Transaction handling stays with the existing draft-check path used by standalone checks and saves.
apps/web/src/api.rs: keep: The new status and body belong in the existing Agent API error mapping.

## Upgrade notes for agents

Update exhaustive matches on Grid's `QueryError` for `AuditCommitUnknown` and `AuditSummaryFailed`, and on Database's `Error` for `UnrecoveredTransaction`. See the [Database connection recovery contract](../apps/base/database/README.md#transaction-continuations) for callers that own a pooled connection.
