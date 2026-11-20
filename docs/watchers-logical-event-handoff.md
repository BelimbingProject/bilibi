# Logical event identity: companion handoff

The mounted walkthrough contains executable amendment and approval regression checks; runtime execution belongs to the Test phase.

## Companion bindings

The trigger identity contract is in [Base Watchers](../apps/base/watchers/README.md#offering-a-trigger-and-an-action).

| Companion | File | Trigger binding | Existing immutable identity |
| --- | --- | --- | --- |
| [Commerce Sales](https://github.com/BelimbingProject/bilibi-dom-commerce/pull/16) | `sales/src/follow_up.rs` | `logical_event: None` | One compact event per order creation or transition; the invoice-consumer events remain multipart |
| [Supply Chain PR 28](https://github.com/BelimbingProject/bilibi-dom-supply-chain/pull/28) | `purchasing/src/follow_up.rs` | `logical_event: Some("/snapshot_key")` | `purchasing/src/order_events.rs` records one snapshot key across the approval's line parts |
| [People PR 77](https://github.com/BelimbingProject/bilibi-dom-people/pull/77) | `claims/src/follow_up.rs` | `logical_event: None` | One recorded event per approval: its immutable Events ID identifies the occurrence; `claims/src/workflow.rs` publishes no multipart approval |

The original multipart bindings were inspected at Commerce `06a6596`, Supply Chain `ecc65c0`, and People `8487775`; Commerce now offers compact single-occurrence events. Purchasing also supplies `fixtures/walkthrough/watchers-source-hook.sh`, which invokes real amendment and independent approval facades. No companion source is copied into Platform.

## Running the proof

Mount the companion branches, discover modules, and run each companion's `fixtures/walkthrough/watchers-followup.sh` against its own fresh disposable database. Supply Chain's script sets `BILIBI_WATCHERS_SOURCE_HELPER` to the absolute hook path; the Platform walkthrough requires it and fails if either helper invocation fails. Pin companions to the final Platform head before delivery.

The Supply Chain path holds the other existing event-delivery jobs under row locks, allowing only the first approval part to deliver and complete for both watchers. It stops its worker and confirms part two has neither delivered nor evaluated, invokes `amend-header`, releases the jobs, and processes part two. Assertions require one task and one immutable identity per watcher across two evaluated revisions.

With the worker stopped again, the proof invokes `reapprove` twice and requires both snapshots' complete declared parts to remain undelivered and unevaluated (including retained line generations). It then processes both snapshots at the latest source state and requires two distinct tasks and identities per watcher at one evaluated revision, with all parts of each snapshot returning the same task. Aggregate counts include the first approval's tasks. Later manual writes re-read the live source revision.

Every processing stage waits for all expected deliveries, successful terminal runs and completed Queue jobs before stopping its worker. The one-shot completion fault still requires a completed retry with at least two attempts and unchanged task/effect counts. The replay check remains explicitly a no-op. Independent watchers, manual requests, privacy, stale-version and foreign-recipient refusals, and creation after deletion remain covered.

These checks are implemented; this review does not claim they have run. The Test phase executes the mounted walkthroughs and records their results.

## Required companion changes

Commerce Sales, Supply Chain Purchasing and People Claims each need a companion PR linked from the Platform PR. For each module:

- Replace the `bilibi_rules` Cargo dependency and Rust imports with `bilibi_watchers`, and change the descriptor dependency from `base/rules` to `base/watchers`.
- Register triggers and actions through `bilibi_watchers::{TRIGGERS, ACTIONS}`. Offer capabilities and admit job writes for the `watchers` principal in `OFFERED_CAPABILITIES` and `PRINCIPAL_WRITES`.
- Set `approval_capabilities: &[bilibi_activity::MANAGE]` in the follow-up `Action` initializer. Follow the [Activities registration contract](../apps/core/activity/README.md#follow-up-operations-offered-by-record-owners) and the [Watchers action contract](../apps/base/watchers/README.md#offering-a-trigger-and-an-action).
- Update the walkthrough script to `fixtures/walkthrough/watchers-followup.sh` and invoke the `watchers-domains-walkthrough` Platform example. Update the Supply Chain source hook and environment variable to `watchers-source-hook.sh` and `BILIBI_WATCHERS_SOURCE_HELPER`. Keep the existing Domain revision columns and Grid `revision` fields.

The mounted walkthrough denies `core.activity.manage` directly on its otherwise authorized human approver. It requires approval to fail without activating the draft, then removes the deny and approves normally. It switches the watcher off, reinstates the deny, requires re-enabling to fail without changing the switch, removes the deny, and re-enables normally. It changes no role permissions. Test must execute this proof after adapting the companions; review compilation alone is not runtime evidence.

## Upgrade notes for agents

Use the Watchers dependency, principal and registrations listed above. Add `Trigger::logical_event` and `Action::approval_capabilities` to existing initializers using the linked contracts. The owning modules document the `FollowUp` handler and `Watchers::action_identity` API. Draft and approve watchers on the new schema.

`bilibi_company::require_writable`, `require_writable_in` and `UserService::company_account_on` now take `&dyn bilibi_tenancy::ProofClient` instead of generic PostgreSQL clients. Concrete clients and transactions can still be passed directly; generic wrappers must satisfy `ProofClient` (PostgreSQL `GenericClient + Sync` supplies its blanket implementation).
