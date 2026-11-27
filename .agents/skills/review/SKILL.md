---
name: review
description: Use when reviewing a PR or a change in this repository.
---

# Review a change

## Read the rules

Before reviewing, read `AGENTS.md`, the touched area's `AGENTS.md` files, and `DESIGN.md`. They are the rules; point to them instead of copying them here.

## Review lenses

- **Entropy:** Flag needless machinery, duplication of existing Platform pieces, speculative code, and settings nobody asked for.
- **Non-compliance:** Flag breaches of the repository's rules. Cite the relevant rule or authoritative contract.
- **Non-functionalities:** Check whether the change's claims hold up in the running app; source inspection alone is not proof.
- **Doors:** The API, MCP, pages, jobs and the command line (`apps/cli`) are doors to one operation contract and hold no decision of their own. Flag a door that validates business input, decides a permission, computes a preview, defaults a business value or reshapes an answer's meaning; the rule belongs in the contract's owner, where every door gets it. A change to `apps/cli` may call only the versioned API and print what it answered; `apps/cli/README.md` ("Only a door") is the contract, and its two-doors walkthrough must still pass and cover any new command.

## Prove it in the running app

Use your own port and a disposable database. Do not run `./bilibi start` or `local-https.sh` against the owner's app, and never touch Caddy. Use the changed behavior in a browser at the exact `APP_ORIGIN`; rendered HTML alone is not proof. Run `python3 scripts/discover-modules.py --check`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `./scripts/build-assets.sh --check`.

## Report

Start with one verdict: **merge as is**, **small changes**, or **needs rework**. Group findings under the three review lenses; give each finding a severity, `file:line` evidence, and a concrete fix. Say what held up, then include a ready-to-post review comment in plain language.
