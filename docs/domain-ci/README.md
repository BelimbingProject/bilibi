# Domain CI

Every Domain or Extension repository runs the Platform's gates with itself mounted; the Platform runs the same gates with every official Domain and Extension in `catalogue.json` mounted separately.

| Where | Workflow | Platform | Domain or Extension |
| --- | --- | --- | --- |
| Domain or Extension repository | `ci.yml` here, copied to `.github/workflows/ci.yml` | pinned in `.github/bilibi-revision` | the change |
| Domain or Extension repository | `platform-reuse.yml` here, copied unchanged to `.github/workflows/platform-reuse.yml` | `main` (the reuse rules only) | the change |
| Bilibi | `.github/workflows/domains.yml` (stable check `Official compatibility`) | the change | selected `main`, companion branch, or release commit |

- Both workflows call `scripts/domain-ci.sh`. Each phase checks module discovery and CSS compilation and coverage, then resolves and pins `Cargo.lock`. `DOMAIN_CI_PHASE=lint` runs fmt and Clippy without PostgreSQL. `DOMAIN_CI_PHASE=build` builds the binary, migrates a fresh database, applies production seeds, boots the app and loads `GET /`. The default `all` phase combines lint and build. Set `DATABASE_URL` to an empty PostgreSQL 18 database for `all` or `build`.
- `DOMAIN_CI_BUILD=release` is the script default and builds every workspace member with `--release --locked`. `DOMAIN_CI_BUILD=check` builds only the composed debug `bilibi-web` for the boot check.
- For Platform compatibility, each affected pull request uses a debug build. A push to `main` uses a locked release build and also runs a debug build to warm the cache for pull requests. Manual runs and calls from the Release workflow use the locked release build.
- A Bilibi pull request merges its base branch into the GitHub merge ref (`scripts/merge-live-base.sh`) to pick up coordinated Platform and mounted changes that landed since the pull request was last pushed. A conflict asks the author to update the branch. Catalogue selection and every required mounted job must pass for **Official compatibility** to pass.
- A Bilibi pull request whose files cannot affect a mounted Domain or Extension passes **Official compatibility** without mounting. A `Bilibi-Companion:` line still mounts. The paths and the companion exception are in [`docs/versions.md`](../versions.md#tested-sets).
- A Bilibi change that turns "Domains on this change" red must fix the official Domain or Extension in the same task. Link the companion PR and select its branch using [the companion-PR convention](../versions.md#platform-changes-and-companion-prs); never mark an entry allowed-to-fail.
- See [tested-set recording](../versions.md#tested-sets) for manual and Release workflow runs.
- A Domain or Extension tag `vX.Y.Z` runs these gates. Bilibi's daily Catalogue workflow then lists the release when the `Mounted in Bilibi` job passed on the tagged commit; the repository needs no secret and opens no pull request. Write `.github/bilibi-platform` (one line, for example `>=0.1.0, <0.2.0`) only when the Platform requirement changes. A repository that is not in the catalogue is never listed. Keep the job named `Mounted in Bilibi`: the Catalogue workflow looks for that check.
- **Platform reuse** flags added code that looks like what a Base module already does (an approvals table, an HTTP client crate, local time, a timer loop; rules in `scripts/platform-reuse.toml`). It fails until the pull request body says, under `## Platform reuse`, one line per flagged rule: `<id>: uses <base module> (how)` or `<id>: no fit: <reason>`. It reads the script and rules from Bilibi `main`, so a new rule reaches every repository without a copy; the Platform runs the same check in `.github/workflows/platform-reuse.yml`. Map of Base: [`apps/base/CAPABILITIES.md`](../../apps/base/CAPABILITIES.md).

## Finance compatibility

Platform compatibility runs one Finance lint job alongside its build and boot jobs so each has its own time limit. Other mounts use the combined gates. **Official compatibility** requires all Finance jobs to pass with the same Platform commit, Finance commit and `Cargo.lock` hash.

Catalogue selection resolves the Finance commit and, on pull requests, the Platform base commit before the jobs start. If a merge is necessary, every Finance job uses the selected base commit's timestamp to reproduce the same Platform SHA. Other entries resolve their refs in their own jobs.

Choose **Re-run all jobs** after creating or updating a Finance ref or after a Finance composition mismatch. This refreshes catalogue selection and resolves `Cargo.lock` again in every Finance job. **Re-run failed jobs** can reuse an earlier selection, resolution error or lockfile hash. Starting a fresh Platform run also refreshes these inputs. [GitHub rerun behavior](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs)
