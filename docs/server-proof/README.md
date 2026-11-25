# Prove a fresh server

Build on your own machine, then prove installation, 2FA, Undo and automatic switch-back in a disposable systemd server.

## Prepare the lab

Use a disposable Bilibi checkout on a Linux host with `sudo -n`, `systemd-nspawn`, `machinectl`, `debootstrap`, `iptables`, Rust, Node.js >=22, npm, Python 3 and `chrome-devtools-axi`.

- The lab owns machine `fsproof`, network `10.77.61.0/24` and its tagged firewall rules. It refuses an existing machine or interface with that name.
- Host IP forwarding must already be enabled. The script does not change it or any host service, Caddy configuration or database.
- `prod.10.77.61.2.sslip.io` and `dev.10.77.61.2.sslip.io` must resolve to the lab address. Each is also its instance's exact `APP_ORIGIN`.
- Keep generated files outside the checkout. The driver keeps the generated password, lab authenticator key and bearer in `private/`; answers are redacted.
- The browser uses its own named AXI session and profile. Certificate checking is relaxed only in that lab browser for Caddy's internal certificates.

Create the task folder:

```sh
export BILIBI_PROOF_DATA="/tmp/bilibi-server-proof-$(date +%s)"
export CHROME_DEVTOOLS_AXI_SESSION=bilibi-fresh-server-proof
mkdir -p "$BILIBI_PROOF_DATA/private"
chmod 700 "$BILIBI_PROOF_DATA/private"
openssl rand -base64 30 > "$BILIBI_PROOF_DATA/private/password"
chmod 600 "$BILIBI_PROOF_DATA/private/password"
```

Publish a disposable application release through its protected `setup-vX.Y.Z` tag. Keep the setup archive and matching installer asset outside this checkout:

```sh
export PROOF_REPOSITORY=your-company/bilibi-proof
export PROOF_REPOSITORY_ID=123456789
export PROOF_TAG=setup-v1.0.0
mkdir -p "$BILIBI_PROOF_DATA/assets"
gh-axi release download "$PROOF_TAG" --repo "$PROOF_REPOSITORY" --dir "$BILIBI_PROOF_DATA/assets"
docs/server-proof/lab.sh prepare
docs/server-proof/lab.sh boot
```

Copy `PROOF_REPOSITORY_ID` beforehand from the proof repository's GitHub **Settings › General** page or another trusted record. `prepare` bootstraps Ubuntu 26.04 with `main` and `universe`. Pass a clean debootstrap rootfs as its second argument to copy a base instead. The checkout is mounted read-only at `/src`; task material is mounted at `/data`.

## Install the fresh server

Run the product installer inside the lab:

```sh
docs/server-proof/lab.sh run '/src/scripts/install-server.sh create production --role production --site prod.10.77.61.2.sslip.io --tls internal'
docs/server-proof/lab.sh run "/src/scripts/install-server.sh trust production --github --repo $PROOF_REPOSITORY --repo-id $PROOF_REPOSITORY_ID --minimum-tag $PROOF_TAG"
docs/server-proof/lab.sh run "env GH_TOKEN=unusable GITHUB_TOKEN=unusable /src/scripts/install-server.sh release production /data/assets/bilibi-$PROOF_TAG-x86_64-unknown-linux-gnu.tar.gz --tag $PROOF_TAG"
docs/server-proof/lab.sh run '/src/scripts/install-server.sh create-owner production --company-name "Proof Operations" --company-code proof_operations --email owner@proof.invalid --name "Proof Owner"'
docs/server-proof/lab.sh run "/src/scripts/install-server.sh installer production /data/assets/bilibi-installer-x86_64-unknown-linux-gnu --tag $PROOF_TAG"
docs/server-proof/lab.sh run 'if command -v cargo || command -v rustc; then exit 1; fi'
docs/server-proof/drive.py login
docs/server-proof/drive.py enrol
docs/server-proof/drive.py authorize
docs/server-proof/drive.py read base.software.status
```

`create-owner` emails the owner's claim link. Production sends mail only through a relay, so first run `install-server.sh mail production …` with a relay whose inbox you can read; a Dev instance writes the email to `/opt/bilibi/dev/data/mailbox`. Open the link and choose the password stored in `/data/private/password` before `drive.py login`. The driver uses the browser to enroll the lab owner's authenticator and approve an API connection. Inspect the Dashboard and Software page in that browser.

## Retire the old worker

Prove an active legacy unit is stopped, disabled and removed, while the app keeps serving:

```sh
docs/server-proof/lab.sh run '/src/docs/server-proof/legacy.sh'
```

The script installs a lab-only legacy worker unit and old sudoers contents, then runs the new `create` twice. It checks the worker process is gone, the unit and enablement link are removed, sudoers matches the product rule, and the app PID survives. The second run preserves configuration contents and the sudoers timestamp.

## Check a release with Finance

Add Finance to the disposable application's saved composition before you publish the proof release. Check the release on its Dev instance:

```sh
docs/server-proof/lab.sh run '/src/scripts/install-server.sh create dev --role dev --site dev.10.77.61.2.sslip.io --tls internal'
docs/server-proof/lab.sh run "/src/scripts/install-server.sh trust dev --github --repo $PROOF_REPOSITORY --repo-id $PROOF_REPOSITORY_ID --minimum-tag $PROOF_TAG"
docs/server-proof/lab.sh run "/src/scripts/install-server.sh release dev /data/assets/bilibi-$PROOF_TAG-x86_64-unknown-linux-gnu.tar.gz --tag $PROOF_TAG"
docs/server-proof/lab.sh run '/src/scripts/install-server.sh create-owner dev --company-name "Proof Operations" --company-code proof_operations --email owner@proof.invalid --name "Proof Owner"'
LAB_ORIGIN=https://dev.10.77.61.2.sslip.io docs/server-proof/drive.py login
chrome-devtools-axi open https://dev.10.77.61.2.sslip.io/finance/payroll
```

For the seeded Dev owner, put `bilibi-dev` in a private password file and add `LAB_EMAIL=ai@agent.my LAB_PASSWORD_FILE=/absolute/private/path` to the driver commands. The driver defaults to the Production proof owner and `$BILIBI_PROOF_DATA/private/password`.

Open Finance and use its Payroll page before packaging the release. An empty payroll list states that People is not mounted; it is not fixture data.

## Instance trust and replay

After both instances are running, install the verified Dev installer explicitly with `install-server.sh installer dev <binary> --tag <tag>`. Run `docs/server-proof/lab.sh run 'bash /src/docs/server-proof/trust-check.sh'` to check file permissions and the privilege boundary. A first Production release with the unusable token environment above proves the public bundle path completes without personal credentials.

Use a disposable instance with a numbered release and no trust record to exercise enrollment. Give `trust` a minimum tag no newer than the retained release and confirm it refuses. Use a newer floor and confirm the root-owned policy records the terminal UID and UTC timestamp. Repeat the pin and confirm it refuses replacement. Review a new policy through Software with fresh 2FA and wait for the worker to rotate it. Compare both instances’ policy and replay records before and after. Only the target policy should change.

For an operator-signed release, create the key and sign the setup archive as described in [Build releases](../server-install.md#build-releases). Pin an operator key on a disposable instance, omit GitHub credentials, then install via terminal and Software with the detached proof. Use a valid previously signed archive to try an older tag through upload, GitHub download and the terminal. Check that the archive is refused before an incoming folder or numbered release appears and that the root-owned replay record stays at the newer tag and digest. During an interrupted install, inspect the replay record before the worker’s copy and verify that a paused job recovers the service even if its admitted tag is no longer the highest. Attempt to cancel a queued trust job and then a claimed trust job. The queued job should cancel and the claimed job should report that cancellation is too late.

## Upload and install

Upload the same published archive on Production:

```sh
docs/server-proof/drive.py upload "$BILIBI_PROOF_DATA/assets/bilibi-$PROOF_TAG-x86_64-unknown-linux-gnu.tar.gz"
docs/server-proof/drive.py receive
docs/server-proof/drive.py refuse-install upload-1
docs/server-proof/drive.py review upload-1
docs/server-proof/drive.py shot install-review.png
docs/server-proof/drive.py confirm
docs/server-proof/drive.py wait 1
chrome-devtools-axi open https://prod.10.77.61.2.sslip.io/finance/payroll
docs/server-proof/drive.py shot finance-installed.png
docs/server-proof/lab.sh run '/src/docs/server-proof/rows.sh'
```

`refuse-install` checks that a write without the required 2FA is refused. `confirm` submits the browser's reviewed operations with a fresh code from the enrolled lab authenticator. Observe the backup before migrations, the switch and verification, then use Finance's menu in Production.

The upload is kept only when GitHub verifies its repository number, setup tag and release workflow. The install result must identify Finance and its pinned commit.

The driver works around an AXI limit: it fills the real file input through browser scripting because the native upload command's argument shape differs from the installed DevTools tool. Bilibi still streams, receives and validates the file through its ordinary page.

## Install from GitHub

Use the same disposable application release. A hand-created release asset has no valid build provenance and must fail before unpacking. GitHub allows only 60 unauthenticated calls an hour per address, and the lab shares the host's address, so refresh sparingly.

Name the repository and accept it on Software › Releases, choose **Show pre-releases**, then drive the same reviewed install the page offers:

```sh
docs/server-proof/drive.py github-install <asset-id> wrong-digest      # refused at the plan
docs/server-proof/drive.py github-install <asset-id> wrong-repository  # refused at the plan
docs/server-proof/drive.py github-install <asset-id> refuse            # refused without required 2FA
docs/server-proof/drive.py github-install <asset-id>                   # fresh 2FA code, then download
docs/server-proof/drive.py cancel                                      # cancel while it downloads
docs/server-proof/drive.py wait <job>
```

To make the download last long enough to cancel or interrupt, slow the lab's veth on the host for the run: `sudo -n tc qdisc add dev ve-fsproof root tbf rate 3mbit burst 64kb latency 500ms`, and `tc qdisc del` it afterwards. Restart `bilibi@production` inside the lab during the download to see an interrupted job. `fail-check.sh <in-service-release> <failing-release>` arms the failed check for the release the GitHub install will create.

## Prove on-demand and terminal updates

Download a second, newer protected release into the same task assets, then prove the installer is normally absent and the path unit is waiting:

```sh
export PROOF_UPDATE_TAG=setup-v1.0.1
gh-axi release download "$PROOF_UPDATE_TAG" --repo "$PROOF_REPOSITORY" --dir "$BILIBI_PROOF_DATA/assets"
docs/server-proof/lab.sh run 'systemctl is-active --quiet bilibi-installer@production.path; [[ $(systemctl is-active bilibi-installer@production.service) == inactive ]]'
```

The driver writes one fresh code to the private proof folder; the PTY helper waits until the command has disabled echo before supplying it, redacts an unexpected echo and deletes the file. Run the installed command, then check its journal proves activation and its clean idle exit:

```sh
docs/server-proof/drive.py terminal-code production-terminal-code
docs/server-proof/lab.sh run "since=\$(date --iso-8601=seconds); python3 /src/docs/server-proof/terminal.py /data/private/production-terminal-code bilibi update production --to ${PROOF_UPDATE_TAG#setup-v} | tee /data/proof/terminal-update.log; timeout 30 bash -c 'until [[ \$(systemctl is-active bilibi-installer@production.service) == inactive ]]; do sleep 1; done'; journalctl -u bilibi-installer@production.service --since \"\$since\" --grep='checking production' --quiet; journalctl -u bilibi-installer@production.service --since \"\$since\" --grep='no work ready; exiting' --quiet; systemctl is-active --quiet bilibi-installer@production.path; test -z \"\$(find /opt/bilibi/production/incoming/installer-jobs -name 'job-*.ready' -print -quit)\""
```

The transcript must show the pinned available releases, From, To, composition, migrations, backup, every installer step and `Done`, without the authenticator code. The service journal must contain both its start scan and idle exit after the job. Queue a reviewed `switch: ask` install through `drive.py plan`, confirm the job is running before its pause, run `docs/server-proof/lab.sh reboot`, then confirm the enabled boot service marks that job interrupted and returns to inactive while its path remains waiting; this is the boot-recovery case in [Installer recovery](../../apps/installer/README.md#if-the-server-restarts-mid-way).

Finally prove the same command door performs Undo through the same job contract:

```sh
docs/server-proof/drive.py terminal-code production-terminal-code
docs/server-proof/lab.sh run "python3 /src/docs/server-proof/terminal.py /data/private/production-terminal-code bilibi update production --undo | tee /data/proof/terminal-undo.log; timeout 30 bash -c 'until [[ \$(systemctl is-active bilibi-installer@production.service) == inactive ]]; do sleep 1; done'; systemctl is-active --quiet bilibi-installer@production.path"
docs/server-proof/lab.sh run '/src/docs/server-proof/rows.sh'
```

The Undo transcript must show the restore-loss review before the fresh code, follow backup, restore, switch and verify to `Done`, and `rows.sh` must show the prior release in service. Run `bilibi update` as a non-root account and with redirected input as separate negative checks; both must fail before queueing a job.

## Promote the same asset

Create a Staging instance with its own database and exact origin. Put one additional bootstrap release there so its next local release number differs from Production's. Configure the same GitHub repository independently on each instance.

1. Install one GitHub asset on Staging with its own fresh 2FA. Save its completed `status.installed_asset`, including the local release number and complete `github` object.
2. Use the installed app in the browser and check a business workflow. Record the named checks, outcome, Staging origin, job and UTC time. A failed check stops promotion.
3. Make a separate Production operation file outside the checkout. Use the same GitHub IDs, target, tag and archive digest, with `staging` shaped as documented in [Software](../../apps/base/software/README.md#staging-and-production).
4. Use `drive.py plan-only <file>` to inspect that review, then `drive.py plan <file> refuse-first` for a separately approved install with fresh 2FA. Check that the Staging result survives in the completed job.
5. Compare both installed GitHub objects after restart. Archive and program SHA-256 and GitHub IDs must match; local release numbers must differ.
6. Use `drive.py plan-only <file>` with the matching report, a report for another target of the same setup tag and commit, a report for a different setup, and a report for the same tag and another target that omits `setup_commit`. Check the four GitHub relation messages documented in Software; all plans remain reviewable. Check a failed Staging outcome with `drive.py plan-only <file> refused`. Confirm that planning left Production unchanged. Then upload the saved archive through the folded file section, check that its review says an uploaded file cannot be compared with the Staging report, and complete an upload install.
7. Request the retired listing, archive and export addresses: `/transport/releases`, `/transport/releases/1` and `/system/software/releases/1/download`. Each must answer 404. The old page bookmark may redirect to the Releases tab.

Do not put authenticator seeds, passwords or API credentials in operation files or evidence. The driver stores its lab credentials privately and redacts secrets from answers.

## Undo and fail a check

Undo the completed install and confirm the baseline returns:

```sh
docs/server-proof/drive.py undo
docs/server-proof/drive.py wait 2
chrome-devtools-axi open https://prod.10.77.61.2.sslip.io/system/software
docs/server-proof/lab.sh run '/src/docs/server-proof/rows.sh'
```

The result must say release 1, restored backup, no installed Domains and no Finance migrations. The review and result record what the restore loses.

Make the next release answer on the wrong port, then install the same tested file again:

```sh
docs/server-proof/lab.sh run '/src/docs/server-proof/fail-check.sh'
docs/server-proof/drive.py upload
docs/server-proof/drive.py receive
docs/server-proof/drive.py install upload-1
docs/server-proof/drive.py wait 3
docs/server-proof/lab.sh run '/src/docs/server-proof/rows.sh'
chrome-devtools-axi open https://prod.10.77.61.2.sslip.io/system/software
docs/server-proof/drive.py shot automatic-switch-back.png
```

The hook affects only release 3. Its process starts, but the real verify step times out. The installer must restore the backup, return to healthy release 1 and record the failed check and successful switch-back. Inspect the page and audit evidence, rather than treating the expected failed job as a failed proof.

## Remove the lab

Stop the owned launcher by its recorded PID, remove only the lab's firewall rules and delete its rootfs:

```sh
chrome-devtools-axi stop
docs/server-proof/lab.sh stop
docs/server-proof/lab.sh destroy
rm -r -- "$BILIBI_PROOF_DATA/private" "$BILIBI_PROOF_DATA/chrome"
```

Before committing Platform work, move the mounted Finance checkout and `.local/composition` into task material, run discovery again and restore the Platform `Cargo.lock`. Keep the proof's pin and evidence outside the checkout; never commit a mounted composition.

## Where to go next

- [Server installation](../server-install.md).
- [Software contracts](../../apps/base/software/README.md).
- [Installer recovery](../../apps/installer/README.md).
