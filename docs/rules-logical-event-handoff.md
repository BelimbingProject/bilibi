# Logical event identity: companion handoff

The Platform contract is implemented; companion adaptation and the amendment/approval runtime proofs remain required before delivery.

## Required companion edits

Every `bilibi_rules::Trigger` initializer now supplies `logical_event: Option<&'static str>`. The pointer selects an immutable value from the recorded event payload, never from the current source row. The contract is in [Base Rules](../apps/base/rules/README.md#offering-a-trigger-and-an-action).

| Companion | File | Required initializer change | Existing immutable identity |
| --- | --- | --- | --- |
| [Commerce PR 12](https://github.com/BelimbingProject/bilibi-dom-commerce/pull/12) | `sales/src/follow_up.rs` | `logical_event: Some("/revision")` | `sales/src/operations.rs` records the confirmation revision in every line payload; the task key also includes the source ID |
| [Supply Chain PR 28](https://github.com/BelimbingProject/bilibi-dom-supply-chain/pull/28) | `purchasing/src/follow_up.rs` | `logical_event: Some("/snapshot_key")` | `purchasing/src/order_events.rs` records one snapshot key across the approval's line parts |
| [People PR 77](https://github.com/BelimbingProject/bilibi-dom-people/pull/77) | `claims/src/follow_up.rs` | `logical_event: None` | One recorded event per approval: its immutable Events ID identifies the occurrence; `claims/src/workflow.rs` publishes no multipart approval |

These mappings were inspected on the companion branches. No companion checkout was modified: the named executor checkouts are outside this review worktree's write boundary. No new companion commit heads are supplied. The executor must coordinate these edits, update each module README to remove cross-request coalescing claims, pin the final Platform head, and keep the companion PRs together. Other trigger declarations must explicitly select their identity too; the reference `rule_cases` declarations in Platform use `None`.

## Outstanding behavior proofs

Use fresh disposable databases and the companion's real operations. Do not manufacture owner events or replace a source amendment with direct row updates.

1. For Purchasing, create and approve a two-line order. Hold the second part's delivery while the first part completes for one rule. Commit a supported header-only amendment, leaving the order approved and advancing its live revision. Release the second part, wait for both deliveries and successful terminal runs, and require one task for that rule. Require different evaluated revisions and the same captured `base_rule_runs.logical_event` across those runs.
2. On the same order, use its supported material amendment and independent approval flow to publish a second approval snapshot. Delay processing until after the second approval. After both snapshots' deliveries and runs finish, require two tasks per rule and distinct captured identities. This rejects coalescing both snapshots at the latest source revision.
3. Run all three `fixtures/walkthrough/rules-followup.sh` scripts with the adapted triggers. Retain the Platform walkthrough's two independent rules, independent manual requests, stale-source and foreign-recipient refusals, privacy, and deletion checks.
4. The Platform walkthrough now installs a disposable one-shot database fault at the targeted run's terminal update, after its action transaction has committed. Queue must retry that job, finish with `attempt >= 2`, and leave one task per rule and one effect receipt per part. The probe sequence must show that the terminal update was attempted again. The proof removes its trigger, function and sequence after stopping its worker. This scenario is implemented but has not been executed in this unmounted review worktree.
5. The separate Events replay check explicitly requires `created == 0` and `pending == 0`; it proves a no-op, not duplicate execution.

## Upgrade notes for agents

Add `Trigger::logical_event` as above. `Rules::action_identity` retains its signature but now includes the captured event identity as well as rule version and action position. Do not parse the opaque identity or combine it with a live revision. `FollowUp` and its operation input schema retain their signatures. Approval fingerprints include the new trigger binding, so existing approved definitions require review. New migrations remove obsolete activity deduplication bindings without converting them; runs without a captured event identity refuse. Development databases remain disposable.
