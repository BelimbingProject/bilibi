# Contributing

Comment `/take` on an issue to assign yourself, even when you contribute from a fork. Other assignees stay assigned.
Comment `/untake` to remove yourself; each command must be the whole comment.

## AI agents

AI agents must include an `Agents:` line in the description of every pull request and issue they open.
Use the self-reporting format in the [Header rule](docs/architecture/decisions/README.md#header-rule).

## Guard rails

Guard rails turn repeated review corrections into deterministic checks; each error names the alternative and its reason.

- `clippy.toml` configures disallowed calls and the argument threshold for the Platform and mounted Domains and Extensions. `Cargo.toml` sets the workspace lint levels. The removed unchecked form extractors now fail to compile; use the [UI form contract](apps/base/ui/README.md#form-size).
- **`Html::raw` ratchet:** `Html::raw` outside `base/ui` is held by the ratchet below until its count reaches zero, then it moves to Clippy as a hard rule (`disallowed-methods` in `clippy.toml`). The ratchet blocks increases in existing escape hatches.
- `clippy::allow_attributes` and `clippy::allow_attributes_without_reason` require reasoned exceptions: prefer `#[expect(lint, reason = "…")]`. An unfulfilled expectation warns locally and fails CI under `-D warnings`.
- `scripts/forbidden-code.py` rejects unit-test attributes and `window.confirm`; use running-system walkthroughs and `ConfirmDialog` respectively.
- `scripts/ratchets.py` compares occurrence counts at HEAD against the merge base. In `apps/` and `fixtures/`, `Html::raw(` and `|safe` outside `base/ui`, and the combined `Pending::Every`/`Pending::Events` count outside `base/module_registry`, may only fall; comment lines are excluded.
- `scripts/exceptions.py` lists new escape hatches in the job summary and requires explanations in the PR body. The [PR template](.github/pull_request_template.md) owns the section format and hatch names. Without `PR_BODY`, the local command only lists hatches.
- `scripts/cli-door.py` keeps the agent command line (`apps/cli`) a door: its program may link no workspace crate and no database driver, read from Cargo's resolved graph. It runs in `ci.yml`. The other half is behaviour: `apps/cli/examples/two-doors-walkthrough.rs` sends the same requests through the command line and MCP and compares the answers ([`apps/cli/README.md`](apps/cli/README.md#only-a-door)).
- `.github/workflows/guard-rails.yml` runs these three scripts for Platform pull requests, including when the body is edited. For local comparisons, pass `--base <ref>` to the ratchet and exception scripts.
