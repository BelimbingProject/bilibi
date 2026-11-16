# Logical event identity: companion handoff

The mounted walkthrough contains executable amendment and approval regression checks; runtime execution belongs to the Test phase.

## Companion bindings

Every `bilibi_rules::Trigger` initializer now supplies `logical_event: Option<&'static str>`. The pointer selects an immutable value from the recorded event payload, never from the current source row. The contract is in [Base Rules](../apps/base/rules/README.md#offering-a-trigger-and-an-action).

| Companion | File | Trigger binding | Existing immutable identity |
| --- | --- | --- | --- |
| [Commerce PR 12](https://github.com/BelimbingProject/bilibi-dom-commerce/pull/12) | `sales/src/follow_up.rs` | `logical_event: Some("/revision")` | `sales/src/operations.rs` records the confirmation revision in every line payload; the task key also includes the source ID |
| [Supply Chain PR 28](https://github.com/BelimbingProject/bilibi-dom-supply-chain/pull/28) | `purchasing/src/follow_up.rs` | `logical_event: Some("/snapshot_key")` | `purchasing/src/order_events.rs` records one snapshot key across the approval's line parts |
| [People PR 77](https://github.com/BelimbingProject/bilibi-dom-people/pull/77) | `claims/src/follow_up.rs` | `logical_event: None` | One recorded event per approval: its immutable Events ID identifies the occurrence; `claims/src/workflow.rs` publishes no multipart approval |

The bindings above were inspected at Commerce `06a6596`, Supply Chain `ecc65c0`, and People `8487775`. Purchasing also supplies `fixtures/walkthrough/rules-source-hook.sh`, which invokes real amendment and independent approval facades. No companion source is copied into Platform.

## Running the proof

Mount the companion branches, discover modules, and run each companion's `fixtures/walkthrough/rules-followup.sh` against its own fresh disposable database. Supply Chain's script sets `BILIBI_RULES_SOURCE_HELPER` to the absolute hook path; the Platform walkthrough requires it and fails if either helper invocation fails. Pin companions to the final Platform head before delivery.

The Supply Chain path holds the other existing event-delivery jobs under row locks, allowing only the first approval part to deliver and complete for both rules. It stops its worker and confirms part two has neither delivered nor evaluated, invokes `amend-header`, releases the jobs, and processes part two. Assertions require one task and one immutable identity per rule across two evaluated revisions.

With the worker stopped again, the proof invokes `reapprove` twice and requires both snapshots' complete declared parts to remain undelivered and unevaluated (including retained line generations). It then processes both snapshots at the latest source state and requires two distinct tasks and identities per rule at one evaluated revision, with all parts of each snapshot returning the same task. Aggregate counts include the first approval's tasks. Later manual writes re-read the live source revision.

Every processing stage waits for all expected deliveries, successful terminal runs and completed Queue jobs before stopping its worker. The one-shot completion fault still requires a completed retry with at least two attempts and unchanged task/effect counts. The replay check remains explicitly a no-op. Independent rules, manual requests, privacy, stale-version and foreign-recipient refusals, and creation after deletion remain covered.

These checks are implemented; this review does not claim they have run. The Test phase executes the mounted walkthroughs and records their results.

## Required action adaptation

All three companion follow-up `Action` initializers must add `approval_capabilities: &[bilibi_activity::MANAGE]`: Commerce `sales/src/follow_up.rs`, Supply Chain `purchasing/src/follow_up.rs`, and People `claims/src/follow_up.rs`. The executor owns these companion edits. This declaration is independent of `OfferedCapabilities` for the rules principal; keep its system-create permission and tenant consent checks. Other Actions use `&[]` only when their operation capability covers every human permission. The Platform reference actions have been updated.

The mounted walkthrough denies `core.activity.manage` directly on its otherwise authorized human approver. It requires approval to fail without activating the draft, then removes the deny and approves normally. It switches the rule off, reinstates the deny, requires re-enabling to fail without changing the switch, removes the deny, and re-enables normally. It changes no role permissions. Test must execute this proof after adapting the companions; review compilation alone is not runtime evidence.

## Upgrade notes for agents

Add `Trigger::logical_event` and `Action::approval_capabilities` as above. `Rules::action_identity` retains its signature but now includes the captured event identity as well as rule version and action position. Do not parse the opaque identity or combine it with a live revision. `FollowUp` and its operation input schema retain their signatures. Approval fingerprints include the new trigger binding, so existing approved definitions require review. New migrations remove obsolete activity deduplication bindings without converting them; runs without a captured event identity refuse. Development databases remain disposable.
