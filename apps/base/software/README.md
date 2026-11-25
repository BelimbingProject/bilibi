# Base Software

**Use it for:** what the server runs, finding more modules, and installing tested updates with a backup and undo.

**Not for:** reading a pin file, tag or the catalogue to learn what is installed (use `Composition`).

`base/software` shows what this server runs, helps you find more modules, and installs tested updates with a database backup and undo. Build releases in your application workspace, with or without a coding agent; servers never build them.
## Add a Domain

Ask the agent to find Finance, or open **Administration › System › Software**:

```
read base.software.open {"propose":{"domains":["finance"]}}
```

The answer includes Finance’s description, official repository, a release that accepts this server’s Bilibi version, whether the exact commits were recorded as tested together, and `local_steps`. A proposal highlights entries; it creates no job.

Copy **Installation commands** from the Software grid, or use the `local_steps` returned by the agent operation. The catalogue generates the commands for a recommended release from a BelimbingProject repository with a safe module ID and a full commit ID. Run the whole block in your Bilibi checkout, keeping any other mounted Domains and Extensions. The block stops at the first failed command, including a refused `add`.

The block starts with `./bilibi add` using `--kind`, `--source` and `--commit` to mount the exact release, even when the local checkout’s catalogue does not list it. If the folder already exists at another commit, `add` leaves it in place and prints the steps for moving it after you have saved your work. Extensions mount under `apps/extensions/<id>`.

Run the composition locally with its own database and check its pages in the browser. Then publish it through your application’s trusted release workflow, which creates the installable archive, checksums and either GitHub build provenance or an operator-key proof. See [Application releases](../../../docs/server-install.md#application-releases).

## Compatibility

The catalogue recommends the newest compatible release tested with this exact Platform commit, or the newest compatible release when there is no such record. Every entry has a `compatibility` line. **Tested with Bilibi X** means the recommended release and the running Platform were checked together at their exact commits. **Works with Bilibi X** means the release accepts the running version and the catalogue has no matching test record. When no release fits, the line shows the newest release’s requirement as **Needs Bilibi X** when available, or **No published version supports Bilibi X**.

- Official modules also carry `status` (`installed`, `not_installed`, `update_available`) and `latest_version` (the newest release compatible with the running version, when one exists). `update_available` means that release is newer than the installed version. `warning` is present as text only when no release fits.
- Each entry includes `recommended_version`, `official`, `local_steps` and `next_step`. Each release includes `fits_running` and `tested_with_running`. A development build or an unreleased commit does not inherit a released commit’s evidence.
- Domains were tested individually; separate pairs do not prove a combined composition. Check your composition locally.
- The published catalogue is read over HTTPS through `base/integration` as the requesting person and cached for five minutes, including an unavailable answer. **Software › Catalogue address** can point to an operator’s catalogue. An unavailable catalogue returns `available: false` and a reason; installed status and installation remain available.
- `domains` and optional `extensions` hold repository and release information. The exact-commit test records apply to Domains. `plugins` holds release add-on information.

## Data sources

| Fact | Source |
| --- | --- |
| Instance role, label and colour | `bilibi_system::Instance::resolve` |
| Installed Platform, Domains and Extensions | Host-supplied `bilibi_module_registry::Composition`, compiled into the program |
| Available software and CI evidence | Published `catalogue.json` ([versions contract](../../../docs/versions.md)) |
| Jobs, steps and enabled add-ons | `base_software_jobs`, `base_software_steps`, `base_software_plugins` |
| What a backup restore loses | Server-wide `base_audit_mutations` since the backup |
| Trusted release proof | `/opt/bilibi/<instance>/trust/release.json`, pinned at setup and replaceable only by the installer from an owner-reviewed fresh-2FA job |
| Highest admitted release | `/opt/bilibi/<instance>/trust/release-state.json`, outside the database and release backups |

## Access

Only a server owner with the appropriate platform capability can manage software, because installation affects every company.

| Capability | Allows |
| --- | --- |
| `admin.system.software.view` | Status, catalogue, release listings, Software page and card |
| `admin.system.software.manage` | Install, undo, go-ahead, cancel, release upload, GitHub read access and release-trust replacement |

Install, undo, go-ahead, GitHub settings and release-trust replacement require the owner’s own fresh 2FA code on every commit, including Dev. The existing step-up key `base.software.change` remains `.always()` to preserve its policy and audit identity; it is **not a build operation**. Cancel requires no code. The facade rechecks authorization inside the mutation transaction. Other organisations fail closed with `denied_platform_scope`; a missing grant is `denied_missing_capability`.

## Agent contracts

Reads and reviewed writes use the shared Agent API; the page uses the same contracts through `PageContracts`.

| Contract | Input | Answer or review |
| --- | --- | --- |
| `base.software.status` | none | Instance, read-only release trust, installed composition, add-ons, uploads, current or last job with steps and cancel state, Undo and recent history |
| `base.software.catalogue` | none | Discovery, compatibility and local build instructions on every instance |
| `base.software.open` | optional `propose.domains`, `propose.extensions` | Status plus catalogue, matching and unknown proposals, local next step and `card.resource` |
| `base.software.releases` | optional `refresh`, `prerelease` | Read-only release trust, exact GitHub assets with a required proof sidecar under operator trust, installed identity and uploads; `github.app` is the client ID of this server's GitHub App, or null |
| `base.software.install` | Uploaded `bundle`, or GitHub `asset` with `repository_id`, `release_id`, `tag`, `target` and `digest`; optional `switch`, `staging` report | Source, composition changes, add-on changes, checks, pause and installer steps |
| `base.software.github` | optional `refresh_minutes` | Configure refresh for the repository pinned in Release trust (see [Install from GitHub](#install-from-github)) |
| `base.software.trust` | `kind`, `repository`, `repository_id`, `workflow`, `minimum_tag`, and `public_key` for `operator` | Review and queue replacement of the installation's one release-proof policy; the review shows the exact key fingerprint and every pinned identity |
| `base.software.undo` | optional `job` | Previous release, backup restore and backup time; only the newest completed job can be undone |
| `base.software.go` | optional `job` | Go-ahead for a running install copied and waiting with `switch: ask`, before any later step starts |
| `base.software.cancel` | optional `job` | Remove a queued job, or request stop before the pause; refused after the pause starts |

The `base.software.change` agent operation is removed; only its step-up key stays (see Access). There is no server build operation, builder worker, compiler setting or builder service.

Install takes an exact GitHub asset or an uploaded bundle from the repository pinned in Release trust. The one selected proof method must bind the archive SHA-256 to that repository and immutable number, exact workflow, setup tag, artifact kind and target; a checksum alone is refused. Installing the program already in service, a tag at or below the instance's replay record, or a second active job is refused. `switch: auto` proceeds after copying; `switch: ask` waits for `go` before the pause.

## Page and ChatGPT card

The **Software** page (`/system/software`) has two tabs named **Domains and extensions** and **Releases**. The first tab is a paginated grid with installed modules first. Search covers name, ID and description. Search and the **Status** and **Kind** filters are kept in the address. **Not installed** includes catalogue entries without a compatible release. **Latest version** shows `latest_version` when supplied, otherwise `recommended_version` or **—**. The **Update available** badge follows the catalogue status. When commands are available, the **Installation commands** icon opens a dialog that names the recommended version and includes a copy icon. See [Compatibility](#compatibility) for how these versions are selected.

Installed Domains and Extensions missing from the selected catalogue also have a row, with **Not in the catalogue** as their compatibility and the installed release or build label. Extensions also show the recorded code review kind. These rows remain visible when the catalogue cannot be read. Each module appears once, matched by kind and ID.

The page, ChatGPT card and agents read the same installed facts from the compiled composition. The `uncatalogued` field in `base.software.catalogue` (and so in `base.software.open`) lists installed modules missing from the catalogue; `base.software.status` stays free of catalogue reads.

**Releases** (`?tab=releases`) shows the installed and latest stable versions and links to the release notes. Choose **Update to** for a compatible newer release, or copy the AI prompt. Both paths use the same install review, fresh 2FA and installer job. Active progress appears inline, and a failed or interrupted job shows its reason, a diagnosis prompt and **Undo** when available. Read-only release trust, GitHub access settings, manual file selection, uploads, backups and history are under **Details**. The old `/system/software/releases` page redirects to the Releases tab.

The MCP App (`ui://bilibi/software-v1.html`, `card/software-v1.html`) is titled **Manage domains and extensions** and uses the same software contracts. Its catalogue cards show the repository under **Source** and the generated **Installation commands** when available.

- `?propose=finance,factory` highlights requested page entries; `open_software` accepts catalogue IDs in `propose` for the card.
- A view-only account sees discovery and status. The facade refuses writes without manage permission.
- Install, undo and go-ahead show the shared instance banner and a dry-run review, then recheck and commit the unchanged operation with the person’s chosen 2FA method and code through `ProofInput`. The install headline uses the operation's summary to name an uploaded file or GitHub asset. A wrong code stays in the review.
- The page follows `NOTIFY bilibi_software_jobs` through `follow.rs`; the card polls status every four seconds while a job is active. Closing either leaves the job running.
- The card calls only the host’s read/plan/commit bridge, holds no credential and fetches no independent URL. It reports job outcomes to the model and can send the person’s question back to the chat.
- Plugin packages obtain the card guidance from `CARD_SKILL` and identify the connected instance.
- The History column picker also offers **Worker**, matching the job panel. Its optional columns use shared record fields; the default columns stay the same. Steps and long result messages stay on the panel.

## Jobs and upgrades

**Install, undo and release-trust changes are queued.** One job per tenant can be queued or running. Jobs finish as `done`, `failed`, `cancelled` or `interrupted`; their result explains the outcome. A trust job has one atomic, non-pausing step. A running job whose installer is gone for good would hold that rule for ever, so the installer marks it alive every half minute (`heartbeat_at`) and the server's settle loop (every 15 seconds) ends a running job that has not begun its pause (`paused_at` empty) as `interrupted` once its installer has been silent for `software.installer_silence_minutes` (20; 5–1440, server owner only). Nothing changed, so the owner starts the operation again; the job is then audited and announced like any outcome. A job whose installer never reported is left to it, and one past an install or undo pause is left to the installer's recovery, which alone knows how far the switch got.

Install steps are *fetch, backup, migrate, switch, verify*, with *download* first for a GitHub file. Undo uses *backup, restore* (when needed), *switch, verify*. The first `backup` pauses the instance; cancellation is refused after it starts. The installer backs up before updating, checks the new program, and automatically switches back on a failed check. Undo restores the earlier backup when migrations changed the database; its review names the backup time and the page separately refreshes the count of later audited changes. The committed review records `lost_at_commit`.

Migration `base-software-003-local-builds` cancels legacy queued or running `change` jobs, skips unfinished steps, records the reason, gives never-claimed jobs the worker name `software-migration` and clears their settlement mark so the server audits and notifies the outcome. It preserves completed history and prior Undo information. A constraint prevents new active change jobs. The only builder setting, `software.build_jobs`, is removed. All existing tables and columns remain needed for install, undo, release add-ons or historical evidence; no applied migration is edited.

## Database backups

**Software shows each backup's time and size for installs and undos among the ten most recent jobs; only accounts that can manage software see the file.** The page and assistant card read the same `backups` evidence from completed backup steps, including jobs that later failed; missing older metadata is labelled as not recorded. A view-only account gets no file path from the page, `base.software.status` or `base.software.open`: the `backup` fields are dropped and job messages that name a file say "a backup file on this server" instead. A recorded file path does not prove that the file is still kept.

- The installer writes a PostgreSQL dump under the instance folder at `backups/job-<n>/<database>.dump` before changing anything.
- After a successful install or undo, it keeps the newest `KEEP_BACKUPS` backup folders (`apps/installer/src/flow.rs`, `prune`) and removes older ones. There is no fixed age in days. The backup step records the number as `keep_backups`; `status` carries the sentence once as `backup_rule`, and only the Backups section states it.
- Undo restores the pre-install backup only when the install changed the database structure. It then loses data entered since that backup. Otherwise, Undo switches the program back and leaves data alone. Undo takes a new safety backup first.
- If restoration fails, the service stays stopped and the job message names the file to restore by hand.

## How a worker plugs in

The separate installer (`apps/installer`) reads and writes the durable job tables with its own connection. It never creates audit or notification rows itself.

1. The app preflights `incoming/installer-jobs/` before it commits a job. After commit it writes one `job-<id>.ready` marker; a GitHub install writes it only after the download and proof check finish. `bilibi-installer@<instance>.path` lets systemd start the worker without giving the app permission to control a unit. The web process reconciles missing ready markers at startup and on its regular settlement interval. The service also starts at boot for recovery. On startup, scan for interrupted and ready work. Listen for database notifications while active. Claim the oldest queued install, undo or trust change with a conditional update; ignore installs whose download step has not finished. A trust job validates the reviewed request again and atomically replaces the instance’s record without pausing the instance.
2. Mark each step running, then done, failed or skipped, with timestamps and a plain `detail.message`. Send `NOTIFY` after changes. A switch-back step may be appended after failure.
2a. While it works on a job, write `heartbeat_at = now()` every 30 seconds (its own connection), so the server can tell a silent installer from a slow step.
3. Lock the job before starting the pausing step. Honour `cancel_requested_at` there, or start the pause under the same lock, so cancel cannot cross it. In the same transaction set `paused_at`: a restart reads it to know the service was stopped, never a step's state.
4. Finish with state, time and result, and skip the steps not reached in that same transaction, so a job is never found running with its steps already skipped. Result keys include `release`, `previous_release`, `migrated`, `backup`, `backup_at`, composition and `changes`, `pause_seconds`, and a plain `message`. An undo with a restore records `lost: {since, changes, by_kind}`; status adds its loss sentence.
5. Apply `plugins/enabled.json` from the release in the transaction that finishes the job and record `plugins: {before, after}`. Undo restores `before`. An absent file preserves existing add-ons.
6. After the queue is empty, retain the markers seen before the idle scan, scan once more after ten seconds, remove only that snapshot when it is still empty, and exit. A new marker written during the final scan stays behind and activates another run.
7. The server settles finished outcomes every 15 seconds and at startup through `MutationBoundary` as a `base/software.job` change under `software.finished`, attributed to the recorded worker. After commit it re-proves the requester’s live account through `Authentication` and sends a `software.job` notice. Missing accounts get no notice; notification failure never prevents settlement. Notice bodies are bounded to 900 characters.

Before a database update committed, a restarted install ends interrupted and restores the previous running release. After the update committed, or during restore/switch-back, the installer resumes. It carries existing software and step-up evidence unchanged across a restore; every other write since the backup is lost. See [`apps/installer/README.md`](../../installer/README.md).

## Release archives

Install a published GitHub asset or upload a saved archive. Both paths apply the one proof policy pinned at server setup, then use the same installer checks, replay protection, backup, switch-back and Undo.

The upload is resumable: while Media has a staging disk (`media.upload.staging_disk`), a dropped connection or a restarted server carries on from the bytes already sent, and choosing the same file again within `media.upload.staging_hours` does too. It is still at most 64 MiB.

A gzip-compressed tar carries `bilibi-web`, `SHA256SUMS` and optionally `manifest.txt`, `Cargo.lock`, `release.json`, `bundle.json` and `plugins/`. The application repository created by `./bilibi new <name>` builds and packages the complete composition in the operator's chosen CI; see [application workspaces](../../../docs/application-workspaces.md). Only an archive accepted by Release trust is installable. After verification, Bilibi writes `admission.json` beside the unpacked files with the proof kind, archive digest and pinned identities; it never trusts that file from inside an archive. Archive parsing, safe paths and compatibility checks live in private `src/transport.rs`.

Release trust is one closed policy, never a fallback list:

| Kind | Proof | Network needed to verify |
| --- | --- | --- |
| `github` | GitHub artifact attestation for the archive from the pinned repository number, workflow and tag | Yes; anonymous for public repositories, server GitHub App for private repositories |
| `operator` | One canonical DSSE statement signed by the pinned Ed25519 key, binding archive SHA-256, repository name and number, workflow, tag, kind and target | No; local archive, proof, trust record and OpenSSL only |

The initial `scripts/install-server.sh trust` writes the first `/opt/bilibi/<instance>/trust/release.json` and records the terminal operator’s UID with the UTC time. Existing instances can enroll using a minimum tag newer than their installed releases. Root owns both the policy and replay files. The instance’s application and installer can read them. Only the instance’s privileged installer helper may write them after enrollment. A later `base.software.trust` safe write requires the installation owner's fresh 2FA and queues a one-step installer job; the reviewed step-up target includes the complete new facts and derived key fingerprint. The installer’s privileged helper records the requesting account, validates the key again and atomically replaces the record. A queued trust job can be cancelled until the installer claims it. Software shows that actor and time plus the instance's highest admitted tag. Operator verification uses no transparency log, so private repository and asset names are not published to Rekor.

### Update notices

The automatic check sends the active installation owner one bell notice per newly discovered stable setup tag in the accepted repository. The notice opens Software › Releases. Publisher-written change notes appear as plain text. When the release body contains only App Maker’s standard packaging instructions, the notice reads “No change notes were supplied.”

Bilibi checks the official public repository every 15 minutes without credentials. Opening Releases checks the configured source using the listing cache and the signed-in operator’s access. The receipt and bell notice commit together, so retries cannot announce the same release twice.

The `update` field of `base.software.releases` reports the running setup version and one of the following states.

| State | Meaning |
| --- | --- |
| `ready` | A complete, fresh listing has a newer stable setup with one matching file. The result includes the publisher’s notes and the exact asset for review. Admission and proof verification occur during installation. |
| `current` | The running setup version is at least as new as the newest stable setup found. |
| `unavailable` | Source acceptance, the running setup, the listing or a unique compatible file could not be established. The reason identifies the missing check. |
| `source_checkout` | This program has no installed release folder. |

### Install from GitHub

The repository shown on **Software › Releases** comes from Release trust; changing GitHub settings cannot redirect installation to another repository.

Set up the server’s [GitHub App for private releases](../../../docs/server-install.md#private-application-repositories). Software shows the reason when GitHub access is refused. The App client for Software lives in `src/github/app.rs`.

The repository name and immutable number are accepted when Release trust is pinned. If GitHub later reports a different number for the same name, installs stop. Only an installation-owner release-trust change with fresh 2FA can accept a different identity; Bilibi never replaces it by itself.

The list shows setup files for this server: names that start with `bilibi-setup-` and end with this computer's target, such as `-x86_64-unknown-linux-gnu.tar.gz`. Under operator trust, each archive must have a release asset named exactly `<archive>.proof.json`; a file without its sidecar is not offered. Drafts stay hidden. Pre-releases stay hidden until you choose **Show pre-releases**. The list is kept on this server and refreshed at most as often as **Check GitHub every this many minutes** (15 by default), and again when you choose **Refresh**. The list is saved in the instance's `data/` folder. If GitHub is limiting this server, or cannot be reached, the saved list stays on the page, and a failed look is not repeated before the interval has passed; **Refresh** asks again. The running program, Undo and a file already downloaded do not call GitHub.

Choose **Review** on a file. The install records the repository number, release number, file number, tag, target and archive check code; operator trust also records the proof asset number and check code. Before download, Bilibi reads the release again and refuses when any identity differs. It follows only GitHub's own file hosts and drops the token when the host changes. Under GitHub trust, GitHub CLI verifies the pinned attestation. Under operator trust, Bilibi downloads the proof with a 64 KiB bound, checks its reviewed digest and runs the offline verifier. The files inside are checked after copying. The job result keeps the GitHub and proof identity. Undo does not call GitHub.

An upload is verified before it is kept: its published file name supplies the exact tag and target, and `release.json` must name the pinned repository and tag. GitHub trust retrieves the attestation using the server’s [GitHub access rules](../../../docs/server-install.md#private-application-repositories). Operator trust requires the detached `.proof.json` in the second upload control and verifies it offline, so neither GitHub nor any transparency log must be reachable. Verification failure removes the archive, proof and unpacked copy; the running release is untouched.

### What happens on the target

1. Review an exact GitHub asset or a queued upload. GitHub review binds repository, release and asset IDs, tag, target and archive SHA-256. It checks the selected identity again at commit.
2. The server's supervised `run_downloads` loop downloads queued GitHub installs through `base/integration` as the reauthenticated requester. It rechecks GitHub, streams into `incoming/job-<id>/`, checks the approved digest before unpacking, and wakes the installer. Cancel stops a download within about two seconds; failures finish before any pause.
3. The installer takes `incoming/job-<id>/` or `incoming/upload-<n>/` only after the complete archive's pinned proof verified. It rechecks the instance replay record, checks checksums, processor, system libraries and disk space, refuses the program already in service, then copies and checks again. For GitHub, it compares the program with the downloader's recorded SHA-256 before executing `status`. A downloaded folder is removed after the attempt; an upload stays for retry unless installed, corrupt or discarded.
4. After the new release answers, the installer atomically records its tag and archive SHA-256 in the instance's trust directory. A later tag must be strictly newer. Database restore, release-folder restore and manual `switch` cannot lower this high-water mark; Software Undo remains the deliberate rollback path.

Each failed check is a plain sentence that says what to do next: "This release was built for Linux ARM 64-bit computers, and this server is Linux x86-64. Build it locally for the same kind of server."

`incoming/` is the one folder the instance’s application and installer both write. `scripts/install-server.sh` creates it with the instance’s own group and mode `2770`, including `incoming/installer-jobs/` for ready markers. The service unit allows the program to write there (the path has the `-` prefix, so an instance that has no `incoming/` yet still starts). A program run without `BILIBI_INSTANCE_DIR` cannot receive a release, each time with a sentence that says so.

### Release controls

The **Releases** tab uses `release_files.rs` within `web.rs`. Each GitHub row names its asset and full archive SHA-256, and marks the installed asset. Installed facts show the local release number separately from repository/release/asset IDs, target, tag, setup commit, archive SHA-256 and program SHA-256. `status.installed_asset` and `releases.installed_asset` return the same evidence, recovered from successful install history and the current release folder. Missing older evidence is null, never inferred from a tag. Undo returns the identity of the restored release.

Upload and queued files live inside **Details**. Uploads require manage permission. The operator proof input appears beside the archive and is required only by operator trust. GitHub release-list settings and release-trust replacement require fresh 2FA. Opening the tab uses the cached list; **Refresh**, expiry or an install review asks GitHub through the App. Job polling does not. The [terminal update command](../../../docs/server-install.md#putting-a-release-into-service) uses the same reviewed install and Undo operations.

### Staging and Production

Use separate authenticated connections and two separate approvals to install the same asset on Staging, then Production.

1. Read each server's `status`; identify it by canonical origin and role. A label alone is insufficient.
2. Review and install the exact GitHub asset on Staging with the owner's fresh 2FA. Wait for the durable job to finish and verify `installed_asset.github`.
3. Check relevant business workflows. Retain the Staging instance, job, exact installed asset, UTC check time, named checks and outcome. Keep failed results and stop promotion if anything fails; HTTP health alone is insufficient.
4. Re-read Production status and releases immediately before planning. Submit the same asset with a `staging` report, review the recorded checks, backup and pause, and obtain a separate approval and fresh 2FA. A Production plan must not be precommitted inside a long Staging plan.
5. Wait for Production completion and compare the full installed identity. Use cancel before the pause, `switch: ask` and separately reviewed `go` for a quiet window, or a separately reviewed Undo when needed.

The optional report has this shape:

```json
{
  "staging": {
    "instance": {"origin": "https://staging.example.com", "label": "Staging", "role": "staging"},
    "asset": "<the complete installed_asset.github object from Staging status>",
    "job": 12,
    "checked_at": "2026-10-20T12:00:00Z",
    "outcome": "passed",
    "checks": [{"name": "Create and post a purchase order", "outcome": "passed"}]
  }
}
```

Replace `asset` with the object, including repository/release/asset IDs, tag, target and archive SHA-256. Include program SHA-256 and `setup_commit` when recorded; omit missing values. The review and job retain the report. Failed or incomplete checks refuse promotion. Reports are supplied by the agent, not signed or independently verified by Production. Manual installation does not require a report.

The review describes how the selected file relates to the report:

| Comparison | Review wording |
| --- | --- |
| Same repository/release/asset IDs, tag, target and archive SHA-256 | Same file as tested on Staging |
| Same repository, tag and setup commit, with a different target | Same setup as tested on Staging, built for a different computer type |
| Same repository and tag, but the setup commit of the selected release or of the report is unavailable | Could not check whether this is the setup tested on Staging |
| Anything else from GitHub | Not the setup tested on Staging |
| An uploaded file | An uploaded file cannot be compared with the Staging report |

For this comparison, Production reads the selected tag's commit from GitHub when a report is supplied. An unavailable commit gets the "could not check" wording, never "different setup". **The comparison informs the owner and never refuses installation.** The installer retains the report without enforcing its program checksum or setup commit. Different targets contain different bytes; tags and local release numbers never establish an equal asset. Never give the agent an authenticator seed or GitHub token.

### Upgrade existing servers

Finish or cancel all active installs using instance sources before applying `base-software-004-github-and-upload`. The migration refuses while any remains, including a job paused for approval or recovery, and including the install that delivers this upgrade: deliver the upgrade itself as a GitHub asset or an uploaded archive, never from an instance source. Keep the old installer until such a job finishes. The migration removes the retired settings and prevents new active installs from any source except GitHub and upload. Completed jobs, audit evidence, release folders and Undo remain intact. Update the installer alongside the app.

## Caveats

- An unpinned program cannot prove its exact Domain versions or CI pair. Pin before building locally.
- A local source build is still supported for `./bilibi start`, checks and development. It is not installable on a server: publish the saved application composition so the archive has `release.json` and the proof selected by that server's pinned release-trust policy.
- Live uploads are limited to 64 MB. GitHub downloads stream with bounded archive checks.

## Where to go next

- [Local builds and server installation](../../../docs/server-install.md).
- [Repeatable fresh-server proof](../../../docs/server-proof/README.md).
- [Versions and CI evidence](../../../docs/versions.md).
- [Installer checks and recovery](../../installer/README.md).
- [Step-up](../step_up/README.md) and [Agent API plans](../agent_api/README.md).

Software reviews and GitHub settings use the shared `bilibi_ui::StepUp` form: an account without 2FA sees a setup link instead of the code and confirmation. Setup returns to Software; the person reviews again before confirming. Agent previews include `two_factor_enabled`, `two_factor_methods` (id, label, code length and numeric flag of each enrolled method; the card sends `method_id:code`) and an absolute `two_factor_setup_url`; the ChatGPT card uses these to show the same setup-first flow through the host’s `ui/open-link` request. Cancellation needs no 2FA.
