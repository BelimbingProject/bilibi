# Install Bilibi with your own AI

Give your AI assistant the [Install Bilibi skill](SKILL.md) and it installs the official signed release on your server; you claim the finished server with your own email and phone.

## What you need

- An AI assistant that can run commands on your computer, such as Claude Code, Codex or a similar agent with a shell and `ssh`.
- A domain you control, where you can add a DNS record.
- A server, or an account at a cloud provider to rent one (the skill shows Hetzner and DigitalOcean as examples).
- An outgoing mail (SMTP) relay from your email provider or a mail service. The server emails you the link that makes you its owner.
- Your phone with an authenticator app, for 2FA.

## How to start

Tell your AI: "Install Bilibi on my server using this skill", and give it the skill's address: `https://github.com/BelimbingProject/bilibi/blob/main/docs/install-with-ai/SKILL.md`. It asks you for the details it needs once, then works through the steps.

## What the AI does, and what only you do

| The AI | You |
| --- | --- |
| Checks or helps you rent the server | Pay for the server in your own account |
| Tells you which DNS record to add | Add the record |
| Downloads the release and checks its signature | — |
| Runs the installer and repeats any step that failed | Give the mail relay details |
| Sends your claim email | Open it, choose your password, set up 2FA |

The AI never learns your Bilibi password or your 2FA codes, and the installer has no option that creates or claims the owner without them.

## What this protects, and what it does not

The claim flow and the signature check keep an AI, a script or a hurried person from holding your sign-in by mistake or by a shortcut, and from installing anything but the official release. Whoever has root on the server can still change anything on it, as on any server. Give root only to people and tools you trust.

## If something goes wrong

Every step is safe to repeat, so the AI reruns the step that failed or was interrupted. Each step reports a stable code and what to do next ([error codes](SKILL.md#error-codes)). The installer's full reference is [Running Bilibi on a server](../server-install.md).

## Not covered yet

What the AI does after the install (updates beyond the Software page, connecting AI tools to the running server), backups and restore, and a hosted service are separate work.
