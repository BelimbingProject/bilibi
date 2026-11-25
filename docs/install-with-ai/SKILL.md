---
name: install-bilibi
description: Install Bilibi on a Linux server for its owner, from nothing (rent a server, point a domain) or on a server they already have. Use when someone asks you to install, set up or self-host Bilibi. Needs a shell with ssh and the GitHub CLI.
---

# Install Bilibi

You install the official signed Bilibi release on one server for one person: **the owner**. You drive the installer; the owner claims the finished server with their own email and phone. Read the whole skill before you start. The human guide is [README.md](README.md).

## Rules you must not break

- **Never become the owner.** The owner's account is claimed only through the emailed link and 2FA on the owner's phone. Never create, unlock or reset the owner's account any other way: no SQL, no development seed, no `two-factor-reset`, no reading the claim link out of a mailbox or log. Never ask the owner for their Bilibi password or 2FA code.
- **Never skip a signature check.** Install only assets from the official release that pass verification. If a check fails, download again; never patch, rebuild or bypass it.
- **Never put a secret on a command line.** Relay passwords and tokens go on standard input (`--password-stdin`, `--cloudflare-token-stdin`). Never print them.
- **Never compile on the server.** The release is finished programs. Do not install Rust, Cargo or build tools there.
- **On any failure or interruption, rerun the same command.** Every step is safe to repeat and keeps what is already done. When a step failed, first do what its `code` and `next` say (some need a change on the server, such as a DNS record), then rerun it.

Root on the server can still do anything; these rules keep you from holding the owner's sign-in by mistake or shortcut. They are not a defence against a malicious administrator.

## 1. Ask the owner

Ask once, at the start, and write the answers down:

| Ask | Example | Used in |
| --- | --- | --- |
| Do you have a server? Its address and SSH access | `root@203.0.113.10` | step 2 |
| The web address for Bilibi, on a domain you control | `erp.example.com` | steps 3, 5 |
| Your organisation's name | `Example Trading` | step 7 |
| Your name, and the email address that will own the server | `Owner Name`, `owner@example.com` | step 7 |
| Outgoing mail (SMTP) relay: host, port, username, password, sender address | `smtp.example.net`, `587`, … | step 6 |

The owner needs the mail relay from their email provider or a transactional mail service. Production sends no mail without it, and the claim link is an email.

## 2. Get a server

**Have one:** it must run Ubuntu 24.04 or newer, Debian 12 or newer, Rocky Linux 10 or RHEL 10, with systemd, at least 2 CPUs, 4 GB memory and 30 GB disk (the installer does not check these), ports 80 and 443 reachable from the internet and free, and root through `sudo`. Other services may run; the installer adds its own users, database cluster and Caddy sites without removing anything.

**Have nothing:** help the owner rent one in their own account at a cloud provider, with Ubuntu 24.04 LTS and their SSH public key. Examples:

- Hetzner Cloud: Console › Add server › Ubuntu 24.04, type CX32 or larger, add the SSH key, create. Note the public IPv4.
- DigitalOcean: Create › Droplets › Ubuntu 24.04, 4 GB memory or more, SSH key, create. Note the public IPv4.

Any provider works the same way. Then check you can run `ssh root@<address> true`.

## 3. Point the address here

Ask the owner to add a DNS record at their domain's DNS host: type `A`, name `erp` (for `erp.example.com`), value the server's public IPv4. Add `AAAA` with the IPv6 address too if the server has one. Wait until `getent ahosts erp.example.com` on the server prints that address. `create` refuses with `DNS_NOT_FOUND` while it does not resolve.

## 4. Download and verify the release (on your own computer)

The official release is the newest non-prerelease `setup-v*` release of `BelimbingProject/bilibi-try`; its GitHub repository number is `1462459642`.

This needs the GitHub CLI on your computer; if it asks you to sign in, run `gh auth login` first. The server itself needs no GitHub account.

```sh
tag=$(gh release list --repo BelimbingProject/bilibi-try --exclude-pre-releases --json tagName --jq '[.[] | select(.tagName | startswith("setup-v"))][0].tagName')
mkdir bilibi-release && cd bilibi-release
gh release download "$tag" --repo BelimbingProject/bilibi-try \
  --pattern 'bilibi-setup-*-x86_64-unknown-linux-gnu.tar.gz' \
  --pattern 'bilibi-installer-x86_64-unknown-linux-gnu' \
  --pattern 'bilibi-server-tools.tar.gz'
for asset in bilibi-setup-*.tar.gz bilibi-installer-x86_64-unknown-linux-gnu bilibi-server-tools.tar.gz; do
  gh attestation verify "$asset" --repo BelimbingProject/bilibi-try \
    --signer-workflow BelimbingProject/bilibi-try/.github/workflows/release.yml \
    --source-ref "refs/tags/$tag" --format json \
    --jq '.[].verificationResult.signature.certificate.sourceRepositoryIdentifier' |
    awk 'NF { found=1; if ($0 != "1462459642") exit 1 } END { if (!found) exit 1 }' || { echo "REFUSED: $asset"; exit 1; }
done
```

Stop if any asset is refused. Copy the folder to the server so it ends up as `/root/bilibi-release`. As root:

```sh
scp -r ../bilibi-release root@<address>:/root/
```

As a sudo user, copy it to that user's home, then move it into `/root`:

```sh
scp -r ../bilibi-release <user>@<address>:
ssh <user>@<address> 'sudo rm -rf /root/bilibi-release && sudo mv bilibi-release /root/'
```

## 5. Run the installer (on the server)

Run these as root. Over ssh as a sudo user, send each batch of commands as a script on standard input, so no quoting is mangled: `ssh <server> 'sudo -i bash -s' <<'EOF'` … `EOF`. Set `tag` again on the server to the tag from step 4.

```sh
tag=setup-vX.Y.Z   # the tag you verified in step 4
cd /root/bilibi-release
apt-get update && apt-get install -y python3-snappy   # Rocky/RHEL: dnf install -y python3-snappy
rm -rf tools && mkdir tools && tar --no-same-owner -xzf bilibi-server-tools.tar.gz -C tools
i=tools/scripts/install-server.sh
```

The tools archive is checked only on your computer in step 4: it holds the root-owned script and checking program. After `create`, `trust` pins GitHub attestations from the official repository, workflow and oldest acceptable tag. `release` and `installer` then apply that record on the server. The repository is public. The server retrieves attestation bundles without GitHub credentials and verifies them locally. `create` installs PostgreSQL, Caddy and the GitHub CLI from their official package repositories. Run each step with `--json`. It prints a `started` line, then one result line:

```json
{"step": "create", "state": "completed", "code": "OK", "next": "...", "detail": "..."}
```

Go on only after `"state": "completed"`. On `failed`, do what `code` and `next` say, then rerun the same command.

| Step | Command | Done when |
| --- | --- | --- |
| Create | `$i --json create production --role production --site erp.example.com` | `completed` |
| Release trust | `$i --json trust production --github --repo BelimbingProject/bilibi-try --repo-id 1462459642 --minimum-tag "$tag"` | `completed` |
| First release | `$i --json release production bilibi-setup-*.tar.gz --tag "$tag"` | `completed` |
| Installer service | `$i --json installer production bilibi-installer-x86_64-unknown-linux-gnu --tag "$tag"` | `completed` |

`release` reports success when the same verified release is already in service. If a failed install already recorded the tag in Release trust, publish a new tag before retrying. Its `detail` may warn about integration settings that are unset; those are for later and do not block the install.

## 6. Outgoing mail

Pipe the relay password; never type it into the command:

```sh
printf '%s\n' "$RELAY_PASSWORD" | $i --json mail production --host smtp.example.net --port 587 \
  --tls starttls --username relay-user --from-address bilibi@example.com --from-name "Example Trading" --password-stdin
```

Use `--tls implicit_tls` for port 465. Ask the owner to type the password into your session's environment if you can, so it never appears in your transcript.

## 7. The owner claims the server

```sh
$i --json create-owner production --email owner@example.com --name "Owner Name" --company-name "Example Trading"
```

Rerun it with the same flags; it never creates a second owner. The result's `claim` field tells you where the owner is. Tell the owner what to do, wait, and rerun the same command to check:

| `claim` | Meaning | Tell the owner |
| --- | --- | --- |
| `sent` | The claim link was emailed | Open the email "Claim your Bilibi server", choose a password, sign in |
| `recently-sent` | A link went out under a minute ago | Use that email; rerun after a minute to send a new one |
| `password-chosen` | Password chosen, no 2FA yet | Sign in, open the 2FA link from the email, scan the QR code with an authenticator app |
| `claimed` | Password and 2FA done | Finished |
| `mail-not-ready` / `not-delivered` (failed) | Mail is not set up or the relay refused; `detail` says why | Fix step 6 (or the cause in `detail`), rerun |

The install is finished only at `claimed`. Then confirm with `$i --json status` and give the owner the address `https://erp.example.com`.

## If you were interrupted

A dropped connection or a killed command does not stop the installer on the server: the step keeps running there and finishes on its own. Only one step runs at a time, so a rerun waits for the earlier one and never races it.

1. Run `$i --json status`. While its `detail` starts with "A run is in progress", wait a minute and run it again.
2. Read where each instance is, then rerun from the first step that is not done:

| `status` shows | Rerun |
| --- | --- |
| No instance, or "No instances" | Create |
| `current release` empty | First release |
| `installer inactive` | [Installer service](../server-install.md#the-installer-service). See the [status guide](../server-install.md#putting-a-release-into-service) for normal idle operation. |
| `mail not set up` | Outgoing mail (step 6) |
| Everything above done | `create-owner` (step 7) |

Rerunning a finished step is harmless. A `next` line names the plain command; run it with `--json` as in the table in step 5.

## Error codes

| `code` | Meaning | What to do |
| --- | --- | --- |
| `OK` | The step finished | Next step |
| `USAGE`, `INVALID_INPUT`, `INVALID_COMMAND` | A flag or value is wrong, or your shell quoting changed the command | Compare your command with this skill's; send it as a script on standard input |
| `NOT_ROOT` | Not running as root | Rerun with `sudo` |
| `OS_UNSUPPORTED` | Not a supported system | Use Ubuntu, Debian, Rocky Linux 10 or RHEL 10 |
| `PYTHON_MISSING` | No Python 3 | Install `python3`, rerun |
| `DNS_NOT_FOUND` | The address does not resolve | Step 3, wait, rerun |
| `PACKAGE_INSTALL_FAILED`, `NETWORK_UNREACHABLE` | A package or key download failed | Check internet access and `/var/log/bilibi-install-server.log`, rerun |
| `SUDOERS_INVALID` | This system's sudo refused the installer's rule; nothing was changed | Report it with `/var/log/bilibi-install-server.log` |
| `PORT_IN_USE` | The chosen backend port is taken | Rerun with another `--port` or without it |
| `NO_INSTANCE` | `create` has not run | Run `create` |
| `NO_RELEASE` | No release in service | Run `release` |
| `TRUST_NOT_SET`, `TRUST_INVALID`, `PROOF_METHOD_WRONG` | The server has no valid pinned release policy, or the command gave the other proof kind | Pin each instance once through the terminal. For an existing instance, choose a minimum tag newer than its previously installed releases. Later changes use Software with fresh 2FA |
| `SIGNATURE_INVALID`, `SIGNATURE_WRONG_REPOSITORY`, `SIGNATURE_REPLAY` | The asset is not the official signed release, or its setup tag is not newer than the highest admitted one | Download the correct newer release from step 4; use Software Undo for a deliberate rollback; never bypass |
| `ARCHIVE_INVALID` | The archive or its checksum is damaged | Download again, rerun |
| `MAIL_NOT_READY`, `MAIL_NOT_DELIVERED` | `create-owner` cannot email the claim link: mail is not set up, or the relay refused | Step 6 (or the cause in `detail`), rerun |
| `MIGRATION_FAILED` | The database update failed; the previous release, if any, is back | Read `detail`; report it to the owner |
| `UNHEALTHY` | The service did not answer | Read `journalctl -u bilibi@production`, fix, rerun |
| `<STEP>_FAILED` | Any other failure | Read `detail` and `next`, rerun |

Any other failure from `create-owner` names its cause in `detail` and `next`.

## Out of scope

Updates after the install go through the owner's Software page. Backups, restores and connecting AI tools to the running server are not part of this skill.
