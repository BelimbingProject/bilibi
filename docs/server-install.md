# Running Bilibi on a server

`scripts/install-server.sh` sets up a Bilibi server: one or more instances (Dev, Staging, Production), each with its own address, database, system service and release folders under `/opt/bilibi`. This page explains what it creates and why, how to put a release into service, and how an instance knows which one it is.

## The short version

Publish your application's protected setup tag, then download its setup archive, installer and server-tools assets. Verify `bilibi-server-tools.tar.gz` before you extract or run its root-owned script. Pin one release proof policy per instance: GitHub artifact attestations, or the operator's Ed25519 public key for builds from any CI. The runtime needs neither Cargo nor a Rust compiler; Production receives the tested programs and never builds them. On a fresh Ubuntu, Debian, Rocky Linux 10 or RHEL 10 server, a GitHub-attested application uses:

```sh
sudo scripts/install-server.sh create production --role production --site app.example.com --public-site shop.example.com
sudo scripts/install-server.sh trust production --github --repo your-company/my-bilibi --repo-id 123456789 --minimum-tag setup-v1.0.0
sudo scripts/install-server.sh release production /path/to/bilibi-setup-v1.0.0-x86_64-unknown-linux-gnu.tar.gz --tag setup-v1.0.0
```

```sh
printf '%s\n' "$RELAY_PASSWORD" | sudo scripts/install-server.sh mail production --host smtp.example.net --port 587 \
    --tls starttls --username relay-user --from-address bilibi@example.com --password-stdin
sudo scripts/install-server.sh create-owner production --email owner@example.com --company-name "Example Operations"
sudo scripts/install-server.sh installer production /path/to/bilibi-installer-x86_64-unknown-linux-gnu --tag setup-v1.0.0
```

An operator-signed application pins the public key instead, then gives the proof published beside each asset:

```sh
sudo scripts/install-server.sh trust production --operator-key /path/to/operator-release.pub \
  --repo your-company/my-bilibi --repo-id 123456789 --workflow your-ci/release --minimum-tag setup-v1.0.0
sudo scripts/install-server.sh release production /path/to/bilibi-setup-v1.0.0-x86_64-unknown-linux-gnu.tar.gz \
  --tag setup-v1.0.0 --proof /path/to/bilibi-setup-v1.0.0-x86_64-unknown-linux-gnu.tar.gz.proof.json
sudo scripts/install-server.sh installer production /path/to/bilibi-installer-x86_64-unknown-linux-gnu \
  --tag setup-v1.0.0 --proof /path/to/bilibi-installer-x86_64-unknown-linux-gnu.proof.json
```

The first command installs PostgreSQL 18 and Caddy, creates the users and folders, writes the service and two proxy site blocks, and prints what to do next. The second pins the repository, immutable repository number, workflow and oldest acceptable tag. The third verifies and copies the program into `releases/1`, updates the database, points `current` at it and starts the service. The remaining commands set mail, create the first account and start the installer service that carries out later owner-reviewed Software jobs. Open `https://app.example.com`, sign in with the owner's claimed account and Caddy serves the staff site. `https://shop.example.com` is the public site.

Letting an AI assistant do the install? Give it [the Install Bilibi skill](install-with-ai/SKILL.md). Every command takes `--json` for one machine-readable line per step, with a stable error code and what to do next.

### Verify the server tools first

The server-tools archive contains a script that runs as root, so verify the archive on a trusted computer before extracting it. Use the same proof method you will pin on the server; never try one and fall back to the other.

For a GitHub-attested build, a public repository needs no GitHub login. Install a current GitHub CLI from its official package repository if `gh attestation verify --help` is not available; [GitHub maintains the package commands](https://github.com/cli/cli/blob/trunk/docs/install_linux.md). Then run:

```sh
set -o pipefail
EXPECTED_REPOSITORY_ID=123456789
gh attestation verify bilibi-server-tools.tar.gz \
  --repo your-company/my-bilibi \
  --signer-workflow your-company/my-bilibi/.github/workflows/release.yml \
  --source-ref refs/tags/setup-v1.0.0 \
  --format json \
  --jq '.[].verificationResult.signature.certificate.sourceRepositoryIdentifier' |
  awk -v expected="$EXPECTED_REPOSITORY_ID" 'NF { found=1; if ($0 != expected) exit 1 } END { if (!found) exit 1 }'
```

Copy `EXPECTED_REPOSITORY_ID` from the application repository's GitHub **Settings › General** page or another trusted record before you start the install. Never derive it from the repository name during the install.

For an operator-signed build, take the public key from the operator through a separate trusted channel. Use `release-proof.py` from a Bilibi Platform checkout whose commit you already trust, not the copy inside the unverified archive:

```sh
python3 /trusted/bilibi/scripts/lib/release-proof.py make-trust \
  --kind operator --repository your-company/my-bilibi --repository-id 123456789 \
  --workflow your-ci/release --minimum-tag setup-v1.0.0 \
  --changed-by trusted-workstation --public-key operator-release.pub --output release-trust.json
python3 /trusted/bilibi/scripts/lib/release-proof.py verify-operator \
  --artifact bilibi-server-tools.tar.gz --proof bilibi-server-tools.tar.gz.proof.json \
  --trust release-trust.json --tag setup-v1.0.0 --kind server-tools --target server-tools
```

This check is offline: it reads only the archive, proof, pinned trust record and local OpenSSL. It sends no private repository or asset name to a transparency log. Extract and run the tools only after the selected check succeeds. If it fails, delete both files and get them again from the operator's release; never run or repair them by hand. `install-server.sh create` installs the current GitHub CLI for GitHub-attested releases and the offline verifier for operator-signed releases.

Want a Dev instance on the same machine to try changes first? Add one:

```sh
sudo scripts/install-server.sh create dev --role dev --site dev.erp.example.com
sudo scripts/install-server.sh trust dev --github --repo your-company/my-bilibi --repo-id 123456789 --minimum-tag setup-v1.0.0
sudo scripts/install-server.sh release dev /path/to/bilibi-setup-v1.0.0-x86_64-unknown-linux-gnu.tar.gz --tag setup-v1.0.0
```

Notice what you did **not** do: pick ports, write database passwords, write a unit file or a proxy config, or edit the database to get a first login. The script generated them, and it tells you when something you asked for conflicts with what exists.

You may already have questions: where do the files go, how does a page know it is on Production, what happens to people using the app during a switch, and what about building? We cover each below.

## Supported systems

The installer supports Ubuntu, Debian, Rocky Linux 10 and RHEL 10 on a systemd server.

On Ubuntu and Debian, it installs PostgreSQL 18 from the official PostgreSQL APT repository and Caddy from the official Caddy repository. On Rocky Linux and RHEL, it installs PostgreSQL 18 from PGDG, including the contrib package used by migrations (`btree_gist`), and Caddy from the official Caddy COPR repository.

Rocky Linux and RHEL keep SELinux enforcing. The installer labels Bilibi's writable folders and Caddy's waiting page, and it allows Caddy to connect to the local Bilibi service. It stops with instructions if SELinux is disabled or permissive.

On Rocky Linux and RHEL, the installer starts firewalld without removing or changing existing services. It adds only the standard `http` and `https` services. The Bilibi backend remains bound to `127.0.0.1` and is not opened in the firewall.

## What the script creates

Each instance lives in one folder:

```
/opt/bilibi/
  production/
    env               the instance's settings for the service: role, address, database, keys (root and the instance’s group, 0640)
    caddy-env         the Cloudflare token when DNS certificates are used (root:caddy, 0640)
    releases/1/       bilibi-web, SHA256SUMS, and the build's manifest.txt and Cargo.lock when it has them
    releases/2/
    releases/current -> 2        the release in service; the installer moves this link
    current -> releases/current  fixed, so the service always starts current/bilibi-web
    backups/job-<n>/  the database backup the installer took before job <n> changed anything
    incoming/         a release on its way in: downloaded from the source or uploaded as a file, until the installer copies it
    installer         the verified installer for this instance (root:root)
    trust/release.json        the instance’s pinned release proof policy (root:bilibi, 0640)
    trust/release-state.json  highest admitted setup tag and archive digest (root:bilibi, 0640)
    data/             the folder the running program writes: media/ uploads, mailbox/ outgoing mail
    site/waiting.html the page Caddy shows while the service is stopped
  dev/                the same, for Dev
  release-proof.py    installed offline verifier
  github-app.json     the GitHub App that reads a private application repository, when you add one (root:bilibi, 0640)
```

Each instance has a dedicated application account and a dedicated installer account. The application owns `data/` and shares `incoming/` with its installer. The installer owns `releases/` and `backups/`. The instance’s `env` is readable only by root and the instance’s own group. Candidate programs run as the application account, including status, migration and seeding.

Only root writes either file under `trust/`. The installer’s sudo rule names one instance and permits only that instance’s trust helper and service controls. The application account has no such rule. Each instance runs its own verified, root-owned installer binary.

The systemd templates run these services under the accounts assigned when the instance is configured. The services never compile source and need no toolchain. Publish the saved composition through the operator’s chosen CI. See [Build releases](#build-releases) and [application workspaces](application-workspaces.md).

Each instance has its own PostgreSQL role and database (`bilibi_production`, `bilibi_dev`) on the server's PostgreSQL 18 cluster, with a generated password kept only in `env`. Dev cannot read Production's data.

The offline waiting page identifies the instance folder and address; it does not cache a server role.

Caddy has one generated file per instance in `/etc/caddy/sites/bilibi-<instance>.caddy`, imported by `/etc/caddy/Caddyfile`. An instance with `--public-site` has two site blocks and two loopback upstreams in that file. Both serve the waiting page while their listener is down.

## Keep the staff site private

**The public hostname goes to the internet; the staff hostname goes to a private address or an identity-aware proxy.** Bilibi still checks the exact `Host` on both listeners, but a host check is not a network boundary.

On a server with a VPN or private interface, give Caddy separate IP addresses for staff and public traffic. Replace the example addresses with IPs assigned to the server.

```sh
printf '%s\n' "$CLOUDFLARE_API_TOKEN" | sudo scripts/install-server.sh create production \
  --role production --site app.example.com --public-site shop.example.com \
  --staff-bind 10.20.0.5 --public-bind 203.0.113.10 --tls cloudflare --cloudflare-token-stdin
```

`--staff-bind` takes the server's private or VPN IP and binds Caddy’s staff block to that address. `--public-bind` binds the public block to a different interface. Point each hostname at its corresponding address. `--tls cloudflare` obtains certificates by DNS without making the staff address public; `--tls internal` is the alternative when every staff device trusts Caddy's local authority. If the machine has only one reachable address, an address allow-list on the staff block is a weaker last resort.

For a tunnel running on the same server, use `--staff-bind 127.0.0.1 --public-bind 203.0.113.10` with the installation command above. Point the identity-aware tunnel at `https://127.0.0.1:443` and configure the staff hostname as both the HTTP Host and the TLS server name. Require the proxy’s identity check before staff sign-in. For a proxy running elsewhere, use a private staff IP and restrict ingress on that interface to the proxy. Send the public hostname directly to the public Caddy address. Staff invitation and password-reset links intentionally require that same private path. If a cloud identity provider or AI application must reach the staff machine edge, publish only the [machine paths](#a-firewall-identity-proxy-or-cdn-in-front-of-bilibi), with the provider or application authentication that fits that path; do not publish all staff pages.

## Which certificate should I use?

Use automatic HTTPS for a public site, Cloudflare DNS for a VPN-only site whose public DNS zone is on Cloudflare, or an internal certificate when every client can trust Caddy's local authority.

| Site | Create option | Certificate |
| --- | --- | --- |
| Publicly reachable | no `--tls` option | Caddy's automatic HTTP or TLS challenge |
| VPN-only, public Cloudflare DNS | `--tls cloudflare --cloudflare-token-stdin` | Public certificate through ACME DNS-01 |
| Private name or private trust | `--tls internal` | Caddy's internal certificate authority |

For Cloudflare DNS, create an API token limited to the site's zone with `Zone:DNS:Edit` and `Zone:Read`. Supply it on standard input so it does not enter the shell history or process list:

```sh
printf '%s\n' "$CLOUDFLARE_API_TOKEN" | sudo scripts/install-server.sh create dev \
    --role dev --site bilibi-dev.example.com --tls cloudflare --cloudflare-token-stdin
```

The installer adds Caddy's Cloudflare DNS module and stores the token only in `/opt/bilibi/<instance>/caddy-env`. The file is readable by root and Caddy, and the token is not copied into the Caddyfile, Bilibi's service environment or logs. A systemd override also removes Caddy's packaged `--environ` flag because that flag would write environment secrets to the journal. Rerun the command with a new token on standard input to rotate it.

Caddy keeps serving other instances while one is created or changed: the installer reloads it, and restarts it only when a token file, the systemd override or the Caddy binary changed.

The Cloudflare module makes `/usr/bin/caddy` a custom build, so a normal package update would replace it with the stock build and Caddy would then fail on the `dns cloudflare` directive. The installer therefore holds the `caddy` package whenever you use `--tls cloudflare` (`apt-mark hold caddy` on Ubuntu and Debian, `dnf versionlock` on Rocky Linux and RHEL).

To update Caddy:

1. Release the hold: `sudo apt-mark unhold caddy`, or `sudo dnf versionlock delete caddy`.
2. Update it: `sudo apt-get install --only-upgrade caddy`, or `sudo dnf update caddy`.
3. Rerun the same `create ... --tls cloudflare` command for any instance that uses it. It adds the module again and restores the hold. Pipe the token again, or leave `--cloudflare-token-stdin` off to keep the saved one.

If Caddy fails with an unknown `dns cloudflare` directive after an update, run step 3. Caddy restarts, and certificates renew again.

> [!WARNING]
> Do not put the Cloudflare token directly on the command line. Command arguments can appear in shell history and process listings.

## How an instance knows which one it is

The role comes from **`instance.role` in the settings table**. The explicit install seed takes its first value from `BILIBI_ENV` in the instance's `env` file: `development`, `staging` or `production` (unset means Production). After that, the saved setting wins, including after a restart or repeat install. Editing `BILIBI_ENV` does not change an installed server's role.

| Role | Install value | Purpose | 2FA step-up |
| --- | --- | --- | --- |
| Dev | `development` | Try a locally built composition | Off, except role changes and other actions that always require a fresh code |
| Staging | `staging` | Test a release before Production | On |
| Production | `production` | Run the tested release | On |

Builds run in a development workspace, which may live on a Dev server, or in your application repository’s GitHub Actions. The role identifies the instance; it never enables a build job in Bilibi.

Click the role in the status bar to open **Instance settings** (`/system/instance`). A Core Administrator in the platform-operator organisation can change the role, name and colour. Every role change needs a fresh 2FA code, even on Dev, and is audited. The page warns before moving to or from Production: this changes warnings and 2FA rules without moving any data. Other accounts cannot change the role.

The name and colour also remain available in the generic operator settings. Empty name and colour follow the current role:

| Setting | Default | Meaning |
| --- | --- | --- |
| Instance name shown on every page | Dev, Staging or Production | What the banner and every confirm page say, like "Production" or "Dev (laptop)" |
| Banner colour | info (blue), warning (amber), danger (red) by role | One of the design's status colours |

In code, `bilibi_base::system::Instance::resolve` returns the role, label and colour together. **The instance banner, the Software page and the agent confirm pages all read it**, so a purchase order confirmed on Dev and the Software page on Dev cannot disagree about where they are.

## One machine or two

**Dev and Production on one machine** use separate databases, ports and addresses. Build in a separate development checkout, install the release on Dev or Staging to check it, then promote the tested file to Production without rebuilding.

**Dev and Production on two machines** use the same verified release archive from GitHub, either downloaded directly or saved and uploaded again. Each instance keeps its own local release numbers, database and backup. No instance serves release files to another.

**Install from GitHub**: Software › Releases lists the application repository pinned in Release trust. A private repository is read with the server's [GitHub App](#private-application-repositories). That asks for your 2FA code. Then review one setup file for this server. The server downloads that file, checks it, backs up, switches and can switch back or Undo, the same way as an upload. Uploading a saved copy of that published asset still works when you do not want GitHub to supply the bytes.

Staging is optional. When used, install and check the exact GitHub asset there first, then review a separate Production install of that asset with fresh 2FA. See [Staging and Production](../apps/base/software/README.md#staging-and-production) for the agent report and identity checks.

## Putting a release into service

**After the first release, the owner does this from the Software page**, and the installer service does the work: copy and check, pause, back up, update the database, switch, check, and switch back by itself when the check fails. The page also offers Undo. What each step does, and what Undo loses, is in [`apps/installer/README.md`](../apps/installer/README.md).

The installer records the admitted tag and digest before copying a numbered release. A failure after that write consumes the tag. Fix the cause and publish a new setup tag.

The first release has no program to ask through, so `release` puts a published setup archive in service from the terminal. First run `trust` once, after `create` and before any release. It pins one proof method, repository name and immutable number, workflow identity and minimum setup tag for that instance:

```sh
sudo scripts/install-server.sh trust production --github --repo your-company/my-bilibi --repo-id 123456789 --minimum-tag setup-v1.0.0
sudo scripts/install-server.sh release production /home/me/bilibi-setup-v1.0.0-x86_64-unknown-linux-gnu.tar.gz --tag setup-v1.0.0
```

For an operator key, use `trust <instance> --operator-key <public.pem>` and name the CI workflow with `--workflow`; then add `--proof <asset>.proof.json` to `release`, `installer` and `verify`. The proof is a DSSE envelope over the archive SHA-256, repository name and number, workflow, tag, artifact kind and target. Verification is offline and never falls back to GitHub. GitHub trust refuses an operator proof, and operator trust refuses a missing or wrong proof.

Copy `--repo-id <number>` beforehand from the application repository's GitHub **Settings › General** page or another trusted record. Never look it up from the repository name during the install. For known official `BelimbingProject` repositories, `trust` checks the supplied number against its built-in value. Public GitHub-attested repositories need no GitHub login. A private repository is read only through the server's [GitHub App](#private-application-repositories). If the checksum, key or attestation, repository, workflow, tag, kind or target does not match, the command stops before unpacking, migration or service changes.

On an existing instance with no pin, root can run `trust <instance>` once. Set the minimum tag to the first eligible upgrade, newer than every release previously installed there. The command checks tags recorded in retained release recipes and records the terminal operator’s UID and UTC time. Once a pin exists, the terminal command refuses to change it. Later replacement is `base.software.trust`: the installation owner reviews the complete new policy and key fingerprint, gives a fresh 2FA code, and the separate installer atomically applies the audited job. Software shows the current policy read-only. There is no compatibility fallback.

Step by step, we:

1. Verify the archive with the one pinned proof policy, unpack only the expected files, and check its SHA-256 list. Record the admitted setup tag and archive digest in the instance’s trust directory before copying into the next numbered folder, `releases/<n>`.
2. Stop the service. From here Caddy shows the waiting page.
3. Update the database with the new program's `migrate` and apply its production seeds. Nothing migrates on boot, so this must happen before the new program serves. If it fails, the previous release is started again.
4. Point `releases/current` at `<n>` and start the service. The instance's `current` is a fixed link to `releases/current`, so the link that moves lives in the folder the instance installer owns, and the installer can move it too; an instance created before this shape is converted the next time you run `create`, `installer`, `release` or `switch`. A job that finds an unconverted instance ends before the pause with "Nothing changed" and says to run `installer` or `create`.
5. Check that the backend answers within 60 seconds. If it does not, point `current` back at the previous release and start that.

If a terminal release fails after admission, fix the failure and publish a newer setup tag. See [Replay and rollback](architecture/decisions/35-server-release-proof-follows-installation-trust.md#replay-and-rollback) for the admission rules and audited rollback path.

`switch` goes to any kept release the same way, after checking its checksum:

```sh
sudo scripts/install-server.sh switch production 1
```

> [!WARNING]
> Migrations are forward-only. Switching back to an older release after a newer one changed tables leaves those changes in place; the older program may not expect them. That case is the Software page's Undo, which restores the backup the installer took first and says what would be lost, with a count. Use `switch` only for releases that share the same database shape.

`status` names the server's GitHub App, if any, then lists every instance, its role, address, service, installer and mail states, and releases. It says so first when another run is still working: only one run goes at a time, a rerun waits for the earlier one, and a dropped connection does not stop a run on the server.

## Private application repositories

**Give the server a GitHub App once, and every later check reads your private repository with a new token that expires within the hour.** Nobody makes or renews a token again, so upgrades can run unattended.

On your own computer, in your application workspace, run:

```sh
./bilibi connect-servers --server admin@erp.example.com
```

It asks you for two clicks on GitHub, "Create GitHub App" and "Install", and sends the App's private key over ssh, on standard input, to this command on each server. By hand, you pipe the key file GitHub gave you:

```sh
sudo scripts/install-server.sh github-app --client-id Iv23li0a1b2c3d4e5f6g --repo your-company/my-bilibi < app.pem
```

```text
GitHub App Iv23li0a1b2c3d4e5f6g saved in /opt/bilibi/github-app.json for your-company/my-bilibi. It can read your-company/my-bilibi (repository 123456789) and cannot change anything.
```

The application maker's [guide to connecting servers](https://github.com/BelimbingProject/bilibi-app-maker/blob/main/docs/guides/connecting-servers.md) explains each step, and how to do it by hand. The key arrives on standard input only, so it never enters the shell history or the process list.

From then on:

- A configured App must be installed on the repository pinned in Release trust. Its short-lived token is scoped to that repository and used for release listing, downloads and GitHub attestation verification.
- With no App configured, the terminal and Software can fetch public attestations by digest and verify the bundles locally with GitHub CLI. See the [GitHub attestation API](https://docs.github.com/en/rest/repos/attestations?apiVersion=2022-11-28).
- Operator signature verification runs offline. Software uses GitHub credentials only to obtain private release files.
- Bilibi ignores a person's `gh` login and environment tokens. Servers accept no hand-made GitHub token; only public-repository requests are anonymous.

The App lives in `/opt/bilibi/github-app.json`, one per server, because every instance on the machine serves the same application. Root and the Bilibi services can read it, like each instance's `env`, which already holds the key that unlocks saved secrets.

| Command | What it does |
| --- | --- |
| `github-app --client-id <id> --repo <owner/name>` | Check the key on standard input against GitHub, then keep it. Rerun with a new key to replace it |
| `github-app --check` | Get a token and read the repository, then say whether the App works |
| `github-app --remove` | Forget the App on this server. Also delete the App, or its key, on GitHub |
| `verify <instance> <file> --kind server-tools\|installer\|setup --tag <setup-vX.Y.Z> [--proof <proof.json>]` | Check a file with the pinned release proof policy without installing it, such as newer server tools before extraction |

Every use refuses, and says what to change on GitHub, when the App can change anything, cannot read Contents and Attestations, is installed on all of the owner's repositories, or its key was deleted. A server only ever needs to read.

**Why an App and not a token?** A fine-grained token expires, at the latest after a year, and someone has to remember how to make a new one. The App's key does not expire, but it can only ask for tokens that read the repositories you installed it on. Revoke it in one place: delete the key on the App's settings page on GitHub.

## The installer service

Download the installer asset from the same published application release, then run:

```sh
sudo scripts/install-server.sh installer production /path/to/bilibi-installer-x86_64-unknown-linux-gnu --tag setup-v1.0.0
```

For operator trust, add the detached proof:

```sh
sudo scripts/install-server.sh installer production /path/to/bilibi-installer-x86_64-unknown-linux-gnu \
  --tag setup-v1.0.0 --proof /path/to/bilibi-installer-x86_64-unknown-linux-gnu.proof.json
```

The script verifies the installer against the named instance’s pin, writes it to `/opt/bilibi/<instance>/installer` as root, and restarts that instance’s installer service. Repeat the command for each instance that should use Software installs. A stopped job is settled as described in [If the server restarts mid-way](../apps/installer/README.md#if-the-server-restarts-mid-way).

The installer reads its own instance’s database and port from `env`. The unit skips startup until the instance has a verified installer binary.

## The first account

A fresh database has no accounts, so nobody can sign in. `create-owner` creates the platform operator's first account, the server owner (a Core Administrator of the organisation that runs the server), and emails the owner a link to claim it. No password passes through the command: the owner chooses it from the link, then sets up 2FA. Set up [outgoing mail](#outgoing-mail) first, then run it after the first release is in service:

```sh
sudo scripts/install-server.sh create-owner production --email owner@example.com --name "Ng Kiat Siong" --company-name "Example Operations"
```

```
Platform operator ready: tenant 1 “Example Operations”, company 1 “Example Operations”.
Owner created: owner@example.com (account 1), Core Administrator of the platform operator.
The owner now has a Person, business affiliation and desk in the platform operator's company.
A claim link was emailed to owner@example.com. The owner opens it, chooses a password, signs in at https://erp.example.com/login and sets up 2FA.
Next: when the owner has done both, rerun the same command to confirm.
Owner claim: sent
```

- `--company-name` (and optional `--company-code`, `--tenant-name`) is needed only while no operator company exists; `bilibi-web platform-provision` may have made one.
- `--name` defaults to the part of the email before `@`.
- **Run it again at any time.** It never creates a second owner. It finishes an unfinished Person, affiliation and desk, resends the link while the owner has not chosen a password, and reports the claim. The last line is always `Owner claim: <state>`:

| State | Meaning |
| --- | --- |
| `sent` | The link was emailed now |
| `recently-sent` | A link went out less than a minute ago and still works |
| `password-chosen` | The owner chose a password; 2FA is not set up yet |
| `claimed` | Password and 2FA are set up: the install is finished |
| `mail-not-ready`, `not-delivered` | Mail cannot be sent; fix [outgoing mail](#outgoing-mail) and rerun |

The link is a password-reset link and expires with the **Password reset link lifetime** setting (three days by default). The command has no option to create or claim the owner without that email, on purpose: whoever runs the install never holds the owner's sign-in. Root on the server can still change anything; this guards against mistakes and shortcuts, not a malicious administrator.

Everything the command creates is written the way the app writes it, so it shows in the audit log under Administration → Audit Log → Actions as **System · create-owner**: the retained `user.bootstrap.completed` action with the account, email and role, and the data changes for the account, its Core Administrator role and the bootstrap receipt that marks the account as the server owner. The emailed link is recorded as `user.reset_requested`. On Dev, the development seed's `ai@agent.my` already exists and the command reports that owner instead.

Behind the scenes this is the development seed's bootstrap with your details instead of the seed's, through the same owners: Company provisions the operator company, User creates the account and receipt, Authz grants the role, and User gives the owner a Person, business affiliation and first desk ([`apps/core/user/README.md`](../apps/core/user/README.md), Bootstrap administrator).

## Outgoing mail

Staging and Production send no mail until the instance has an SMTP relay, and the owner's claim link is the first mail. `mail` writes the relay into the instance's `env` and restarts a running service. The password comes on standard input:

```sh
printf '%s\n' "$RELAY_PASSWORD" | sudo scripts/install-server.sh mail production --host smtp.example.net --port 587 \
    --tls starttls --username relay-user --from-address bilibi@example.com --from-name "Example Operations" --password-stdin
```

Use `--tls implicit_tls` for port 465. Rerun it to change any value. The variables are described in [Mail](../apps/base/mail/README.md#choosing-the-transport).

## Machine-readable mode

Put `--json` before any command (`create`, `release`, `installer`, `mail`, `create-owner`, `switch`, `status`) to get one JSON object per line instead of text, for an AI assistant or script: a `started` line, then the result with `state` (`completed` or `failed`), `code` (`OK` or a stable error name), `next` (what to do), `claim` (from `create-owner`) and `detail` (the full text output). The codes are listed in [the Install Bilibi skill](install-with-ai/SKILL.md#error-codes). Every command is safe to rerun after a failure or interruption: `create` keeps what exists and reuses the saved database password, `release` completes without changes when the same verified release is already in service, and `create-owner` never creates a second owner. A public site (no `--tls`) is refused with `DNS_NOT_FOUND` until its name resolves.

### The first owner of a customer tenant

After putting a release in service, run this command once for each customer tenant. It creates the tenant and its primary company and invites its first Tenant Owner. Set up [mail delivery](../apps/base/mail/README.md#choosing-the-transport) first. The invitation uses the installation's HTTPS `APP_ORIGIN`. Dev instances also accept loopback HTTP.

```sh
sudo scripts/install-server.sh tenant-provision production \
    --tenant-name "Acme workspace" \
    --company-name "Acme Sdn. Bhd." --company-code acme \
    --owner-email owner@acme.example
```

Use a separate email address for each first owner. The address must be unused by any staff sign-in on the installation when the invitation is sent and when it is accepted. The link works once and uses the [default invitation duration](../apps/core/user/README.md#membership-invitations). The recipient chooses a password on the invitation page, then signs in with the new credentials and enrols a second factor if required. Acceptance gives the owner a Person, business affiliation and first desk in the primary company.

The audit log records `user.tenant_owner.invited` as **System · tenant-provision**. The part of the email address before `@` supplies the initial Person name.

The command exits unsuccessfully if delivery fails after commit. To recover from failed delivery or an expired link, rerun it with the same tenant name, company name and code. Use the same owner email to resend, or supply a different unused address in `--owner-email` to change the recipient. Reissue invalidates the previous link and records `user.tenant_owner.reissued` before sending. It requires a unique matching bootstrap receipt and an invitation that has yet to be accepted or revoked. For a fresh bootstrap, choose a tenant name that is unused on the installation.

Use the signed-in Users page to invite later administrators.

Ordinary membership invitations can link the owner's portable identity to another tenant. Company SSO proof remains confined to its issuing tenant; entering another tenant requires portable proof or that target's own SSO ([ADR 21](architecture/decisions/21-one-sign-in-several-tenant-memberships.md)).

When running the binary directly, use the same flags with `bilibi-web tenant-provision` and load the instance's database and mail settings and `APP_ORIGIN` into its environment. The installer wrapper loads these settings and runs the command as the Bilibi service account.

## Lost phone

Bilibi has no 2FA recovery codes, and customer administrators cannot reset another person's installation-wide factor. A staff member who loses their phone asks the server owner to reset it from the server.

```sh
sudo scripts/install-server.sh two-factor-reset production --email owner@example.com
```

```
2FA is off for owner@example.com. The account was signed out and notified. Sign in at https://erp.example.com/login with the password, then set up the authenticator app again.
```

- It turns off every 2FA method of one account, signs the account out and notifies the person. Their password is untouched.
- Already off is not an error: it says so and changes nothing.
- It shows in the audit log as **System · two-factor-reset**.
- The command is `bilibi-web two-factor-reset --email <address>` in the instance's environment.

## Portable sign-in recovery and retention

Customer administrators can remove memberships but cannot recover or delete the installation-wide sign-in. The server owner handles the exceptional lifecycle from the server.

```sh
sudo scripts/install-server.sh identity-recover production --email old@example.com --new-email new@example.com
sudo scripts/install-server.sh identity-delete production --email unused@example.com --confirm unused@example.com
sudo scripts/install-server.sh identity-retention production
```

- `identity-recover` is for a person who lost the old mailbox. It treats the new mailbox as operator-verified, revokes pending reset links, signs every membership out, notifies the new address and records **System · identity-recover**. It refuses an address already used by another portable identity.
- Recovery remains available while an identity is unused but still inside its retention window, so losing the old mailbox does not prevent a later invitation from reactivating it.
- Removing the last membership starts the **Unused sign-in retention** clock (90 days by default, configurable in Settings › Session). Accepting another invitation before it ends cancels the clock.
- `identity-retention` anonymises at most 100 due identities per run. Run it repeatedly from the server's ordinary maintenance schedule until it reports zero.
- `identity-delete` performs the same final anonymisation immediately, but only for an identity with no live membership. Repeating the address in `--confirm` prevents an accidental target typo. It records **System · identity-delete**.
- Final anonymisation releases the address and removes the password, reset token, credential-attempt state, 2FA and pending sign-ins. Historical tenant memberships and their tenant audit evidence remain under their own privacy and retention rules.

## The waiting page

While the service is stopped, Caddy answers every request with status 503 and the waiting page, marked with the header `X-Bilibi-Waiting`. The page says which instance is pausing (**PRODUCTION erp.example.com**, "Production is updating", in the role's colour), polls the same address every second or two, and reloads itself once an answer arrives without that header. People land back on what they asked for. A pause that runs long shows how the server owner can check the service.

The page is rendered once per instance from `scripts/waiting-page.html` into `site/waiting.html`, so the development launcher and the server share one page.

## Build releases

[ADR 02](architecture/decisions/02-build-in-development-keep-everything-in-step.md) records why builds stay separate from the running application, Production only installs tested releases, and official software is kept in step.

### Building on a Dev server

**A Dev server can also be the build machine.** Use a separate unprivileged account, such as `bilibi-compile`, with its own home directory. Keep it distinct from the instance’s application and installer accounts. Give it no sudo access and no access to instance secrets.

- Keep its source checkout, Rust toolchain, Node.js >=22, npm and Cargo output under its home directory, outside `/opt/bilibi`.
- Compose the required Domains and Extensions in that checkout using the normal mounting workflow. Save the composition manifest and lockfile with the release.
- Build at low CPU and disk priority, with a bounded number of compiler jobs. Run long builds in a persistent session or service so an SSH disconnect does not kill them.

For example, from the build account's composed checkout:

```sh
nice -n 15 ionice -c 3 cargo build --release --locked -j 2 -p bilibi-web -p bilibi-installer
```

Local builds are for development checks. They are not server releases. An application release must come from the protected setup tag and the workflow identity pinned on the server. The operator may use GitHub Actions or any other CI. **Production never builds or resolves source dependencies.**

### Application releases

**Prepare application releases from the outer application repository.** Create it from your trial with `./bilibi new <name>`. Commit and push component changes in their own repositories. Run `./bilibi check` with a separate empty disposable database, then `./bilibi build`. Each asks whether to use new commits and saves the record when you accept. Commit and review that record on the application's default branch, then create the protected `setup-vX.Y.Z` tag and run the workflow whose identity the server trusts. The builder verifies the source selection and saved lock; the application's README describes publication. See [application workspaces](application-workspaces.md) for the directory layout and migration.

GitHub-attested public builds keep the existing release workflow. For operator signatures, generate the Ed25519 key on the operator’s trusted machine and export its public half:

```sh
(umask 077; openssl genpkey -algorithm Ed25519 -out operator-release.pem)
openssl pkey -in operator-release.pem -pubout -out operator-release.pub
```

Provision the private key in the CI secret store and distribute the public key to operators through a trusted channel. Publish one detached proof per immutable asset, named `<asset>.proof.json`, using the exact pinned values:

```sh
python3 path/to/release-proof.py sign --artifact "$asset" --private-key "$RELEASE_SIGNING_KEY" \
  --repository your-company/my-bilibi --repository-id 123456789 --workflow your-ci/release \
  --tag setup-v1.0.0 --kind setup --target x86_64-unknown-linux-gnu --output "$asset.proof.json"
```

Use `--kind installer --target x86_64-unknown-linux-gnu` for the installer and `--kind server-tools --target server-tools` for server tools. The workflow must not modify an asset after signing it. Bilibi accepts exactly one signature from the pinned key and exact canonical statement; a different digest, repository, workflow, tag, kind or target is refused. The public key is not secret. Rotate it only through the installation owner's fresh-2FA `base.software.trust` operation.

Before preparing a release with customer code, follow the [installation policy](../apps/base/system/README.md#installation-policy-srcinstallation_editorrs) and [build-time admission requirements](versions.md#versions-inside-the-program). The [installer contract](../apps/installer/README.md) covers admission, rollback and preservation of the owner's policy during recovery.

### Focused component builds

Direct Platform and component clones remain supported for focused contributions. Follow the [Software installation commands](../apps/base/software/README.md#add-a-domain) to mount and check a recommended release in your local checkout. Build for the server’s operating system, processor and system library version.

Do not turn this local binary into a server archive. Create an application with `./bilibi new <name>` and use its trusted release workflow; see [application workspaces](application-workspaces.md). That workflow publishes the complete archive, its checksums and the proof method pinned on the server. Install the same published asset on Staging and Production with separate reviews and fresh 2FA. Backup, automatic switch-back, audit and Undo use the same path for a GitHub download and an uploaded copy of that verified asset.

When upgrading an older installation, `create`, `installer` and `release` stop and disable each former `bilibi-worker@<instance>` service, then remove that obsolete unit and its sudoers rule. Rerunning is safe, and a fresh server is left alone. The new `base-software-003-local-builds` migration cancels queued and running build jobs with local build guidance, preserving their history. Before upgrading to the GitHub-and-upload release, finish or cancel active instance-source installs; the migration refuses while one remains, including the install that delivers the upgrade. Deliver the upgrade itself as a GitHub asset or an uploaded archive, and update the installer alongside the app. Completed history and Undo are preserved.

## Reference

Script commands:

| Command | What it does |
| --- | --- |
| `create <instance> --role dev\|staging\|production --site <staff-host> [--public-site <public-host> or --no-public-site] [--public-tenant <id>] [--staff-bind <address> or --no-staff-bind] [--public-bind <address>] [--port <n>] [--tls internal\|cloudflare] [--cloudflare-token-stdin]` | Packages, users, folders, database, protected environment files, service and Caddy blocks. `--public-site` enables the public listener; `--staff-bind` keeps the staff Caddy block on one private interface. Rerun to change the sites. Omitted public-site and bind settings keep their saved values. Use `--no-public-site` to disable the public listener or `--no-staff-bind` to remove the explicit Caddy binds. Changes to listener configuration restart a running instance and check both active listeners. The saved role and staff port stay fixed |
| `trust <instance> --github\|--operator-key <public.pem> --repo <owner/name> --repo-id <n> [--workflow <identity>] --minimum-tag <setup-vX.Y.Z>` | Pin an instance’s first release proof policy |
| `release <instance> <setup archive> --tag <setup-vX.Y.Z> [--proof <proof.json>]` | Verify the pinned proof and checksums, reject replay, copy into `releases/<n>`, migrate, switch, check |
| `mail <instance> --host <relay> --port <n> --username <user> --tls starttls\|implicit_tls --from-address <address> [--from-name <name>] --password-stdin` | [Outgoing mail](#outgoing-mail) relay in `env`; restarts a running service |
| `create-owner <instance> --email <address> [--name <name>] [--company-name <name> [--company-code <code>] [--tenant-name <name>]]` | [Server owner setup](#the-first-account): creates the owner and emails the claim link; rerun to resend or check the claim |
| `--json <command> ...` | [Machine-readable mode](#machine-readable-mode) |
| `tenant-provision <instance> --tenant-name <name> --company-name <name> --company-code <code> --owner-email <address>` | [Customer tenant and first-owner setup](#the-first-owner-of-a-customer-tenant) |
| `two-factor-reset <instance> --email <address>` | Turn off one staff account's authenticator after a lost phone, sign the account out and notify it ([Lost phone](#lost-phone)) |
| `identity-recover <instance> --email <old> --new-email <new>` | Recover an installation-wide staff sign-in address and sign it out everywhere |
| `identity-delete <instance> --email <address> --confirm <address>` | Immediately anonymise a portable identity after its last membership is gone |
| `identity-retention <instance>` | Anonymise up to 100 unused portable identities whose configured retention ended |
| `installer <instance> <bilibi-installer> --tag <setup-vX.Y.Z> [--proof <proof.json>]` | Verify the pinned proof, copy the installer to `/opt/bilibi/<instance>/installer` and start `bilibi-installer@<instance>`; rerun to update it |
| `github-app --client-id <id> --repo <owner/name>` (key on standard input), `github-app --check`, `github-app --remove` | [The server's GitHub App](#private-application-repositories) for a private application repository |
| `verify <instance> <file> --kind server-tools\|installer\|setup --tag <setup-vX.Y.Z> [--proof <proof.json>]` | Check a file with the pinned proof policy without installing it |
| `switch <instance> <n>` | Put a kept release in service |
| `status [<instance>]` | The GitHub App, then each instance's role, address, service and installer states, releases |

Keys in `env` (the service's environment; `README.md` explains each):

| Key | Set to |
| --- | --- |
| `BILIBI_ENV` | Initial role for the install seed: `development`, `staging` or `production`; later changes use Instance settings |
| `APP_ORIGIN` | `https://<staff-site>`, the exact staff address |
| `BIND_ADDR` | `127.0.0.1:<port>`, the staff listener where Caddy forwards |
| `PUBLIC_ORIGIN` | `https://<public-site>`, present only when `--public-site` enables public routes |
| `PUBLIC_BIND_ADDR` | The public listener’s separate loopback address |
| `PUBLIC_TENANT_ID` | The one tenant served by the public listener. Set with `--public-tenant <id>`, defaulting to tenant 1 when enabling the public site |
| `DATABASE_URL` | the instance's own database and role on the local cluster |
| `BILIBI_SETTINGS_KEY`, `BILIBI_USER_SIGNING_KEY`, `BILIBI_MEDIA_SIGNING_KEY` | generated 64-hex-character keys; keep them, losing `BILIBI_SETTINGS_KEY` loses every secret setting and authenticator key |
| `BILIBI_MAIL_DIR` | `data/mailbox`, used only by a Dev instance. A Staging or Production instance sends no mail until you add the `MAIL_*` relay variables ([Mail](../apps/base/mail/README.md#choosing-the-transport)); until then every send fails and System Info's Mail line says why |
| `BILIBI_INSTANCE_DIR` | The instance folder: the program lists and serves releases, and receives bundles under `incoming/` for the installer |

**A proxy on another machine.** The script puts Caddy on the same machine, and Bilibi then sees each person's own address with nothing to set. If you put a load balancer or reverse proxy on another machine in front of Bilibi, add its address or network to **Trusted proxy addresses** in the Session settings (`session.trusted_proxies`, for example `10.0.0.5` or `10.1.0.0/16`). Until you do, everyone counts as the proxy's one address and shares one sign-in limit. List only proxies you run: anyone at a listed address can claim to be any client.

Files outside `/opt/bilibi`: `/etc/systemd/system/bilibi@.service`, `/etc/systemd/system/bilibi-installer@.service`, `/etc/systemd/system/caddy.service.d/bilibi.conf`, `/etc/sudoers.d/bilibi-build`, `/etc/caddy/sites/bilibi-<instance>.caddy`, and one `import` line in `/etc/caddy/Caddyfile`.

## Where to go next

- [Repeat the fresh-server proof](server-proof/README.md): disposable systemd lab, verified application release, file upload, 2FA, Undo and a failed health check.
- The design behind this layout: the Software page plan, in the Firstmate scout report `bilibi-domain-chooser` (sections 2, 3 and 6).
- What each environment variable does: [`README.md`](../README.md), Configuration.
- 2FA step-up and why Dev skips it: [`apps/base/step_up/README.md`](../apps/base/step_up/README.md).
- The instance identity in code: [`apps/base/system/README.md`](../apps/base/system/README.md).

## HTTPS for OAuth

Use an HTTPS `APP_ORIGIN` on Staging and Production. This is a breaking change: Bilibi now refuses to start there with an HTTP origin, and `/oauth/token` refuses to issue tokens on one. Move such an install behind HTTPS and update `APP_ORIGIN` before upgrading.

Startup also reads the instance role from settings. If the database is not migrated and seeded, it reports that instead of the HTTPS message.

The address must be the exact public origin people browse, without a path, query or credentials. TLS may terminate at your reverse proxy while Bilibi listens on a private HTTP socket. A Dev installation may use HTTP only on `localhost` or a loopback IP. Token issuance rechecks the stored instance role, so changing a running Dev copy to Staging or Production also requires HTTPS.

AI apps register themselves without an operator-maintained client list. Their return addresses must use HTTPS, except HTTP callbacks to the person's own loopback address. Each app supplies its own callback paths; loopback ports may vary. See [Agent](../apps/core/agent/README.md#client-registration) for matching and retention rules.

## A firewall, identity proxy or CDN in front of Bilibi

AI apps are programs, not browsers. A browser check in front of Bilibi lets a person through and blocks the app, usually after its sign-in already worked.

| | |
| --- | --- |
| **Symptom** | The app gets its code approved and a token, then its first `/mcp` or `/api/v1` request answers with an HTML page instead of JSON. Cloudflare's reads `403`, "Error 1010: Access denied", "blocked access based on your browser's signature". |
| **How to tell** | Every answer Bilibi gives on these paths is JSON. The blocked request never reached Bilibi, so it is missing from **Administration › System › API exchanges**. |
| **Cause** | A rule that expects a browser: Cloudflare's Browser Integrity Check, Bot Fight Mode, or a managed or JavaScript challenge. |
| **Fix** | Skip that check for the paths programs call, and only for them. |

The staff-site machine paths are:

- `/mcp`
- `/api/v1/` and everything under it
- `/oauth/token`, `/oauth/device_authorization` and `/oauth/register`
- `/.well-known/` and everything under it
- `/scim/v2/` and everything under it
- `/login/sso/backchannel/` and everything under it

The public-site machine paths are `/webhooks/` and `/visitor/v1/`, with everything under each. They do not belong in a staff-site bypass rule.

The pages people open (`/oauth/device`, `/oauth/authorize`, sign-in) keep the browser check.

On Cloudflare, add a custom rule under **Security › WAF › Custom rules** with the action **Skip** and Browser Integrity Check ticked (add the bot and managed-rule products your plan shows, if they also challenge these paths):

```text
(http.request.uri.path eq "/mcp")
or starts_with(http.request.uri.path, "/api/v1/")
or (http.request.uri.path in {"/oauth/token" "/oauth/device_authorization" "/oauth/register"})
or starts_with(http.request.uri.path, "/.well-known/")
```

- The free plan's Bot Fight Mode cannot be skipped by a rule. Turn it off for the zone if it is what blocks these paths.
- Look a blocked request up by its Ray ID under **Security › Events** to see which product stopped it.
- These paths keep Bilibi's own protection: a budget per sender, a bearer token on every request, and each connection's AI switch. See [the agent edge](../apps/web/README.md#agent-edge-srcapirs).

Upgrading from a static client list:

- Migration `004-client-registration` deletes the retired `agent_access.oauth_clients` setting, including any client an operator added.
- Apps configured with `bilibi-claude-code` or `bilibi-codex` keep working on their current grants and refreshes. On their next authorization they must register again; point them at the AI connections prompt.
