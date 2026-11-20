# Agent command line (`bilibi-axi`)

**Use it for:** letting an agent in a terminal use Bilibi as the signed-in person: check the connection, search, read, and make reviewed writes.

**Not for:** operating a server (`bilibi-web migrate`, `seed`, …), or anything the API does not offer. It adds no operation of its own.

`bilibi-axi` is one more door to the operation contracts ([ADR 01](../../docs/architecture/decisions/01-agent-first-operations.md)). It signs in with the OAuth device grant and calls `/api/v1`. The same person, the same capabilities, the same reviews and the same audit apply as through MCP.

## Build and sign in

```sh
cargo build --release --locked -p bilibi-axi      # target/release/bilibi-axi
bilibi-axi login https://bilibi.example.com        # prints a code and a link
bilibi-axi login --wait                            # after the person approved
bilibi-axi hello
```

- **The person approves in their browser.** `login` prints the code and the `/oauth/device` link; the person opens it, checks the code and approves. Nothing is typed into the terminal.
- **`login --wait [seconds]`** polls until the person decided, the code ran out (ten minutes) or the seconds passed (120 by default). Run it again while it still says waiting.
- **Several Bilibis.** Each sign-in is saved under its server name (`Instance::server_name`); commands use the last one, or `--server <name>`.
- **`logout`** forgets the saved sign-in. The connection itself ends when the person revokes it under **AI connections**.

## Commands

| Command | Does | API |
| --- | --- | --- |
| *(none)* | Where this machine is signed in, the instance, and what to run next | `GET /api/v1/hello` |
| `hello [message]` | Checks the connection and the sign-in | `GET /api/v1/hello` |
| `search <words> [--kind read\|write] [--limit N] [--unavailable] [--full]` | Operations, guides and pages for a task | `GET /api/v1/search` |
| `describe <key> [--full]` | An operation's inputs and permissions | `GET /api/v1/operations/{key}` |
| `guide <key>` | A guide's text | `GET /api/v1/guides/{key}` |
| `read <key> [field=value ...] [--input <json\|@file\|->]` | Calls a read operation | `POST /api/v1/operations/{key}/call` |
| `plan <key> [field=value ...]` or `plan --file <path\|->` | Checks writes without writing; prints the review and a `ref` | `POST /api/v1/plans` |
| `commit <ref> <same operations>` | Writes the reviewed operations | `POST /api/v1/plans/commit` |

- **Values.** `field=value` reads the value as JSON when it parses (`3`, `true`, `[1,2]`) and as text otherwise; write `field='"3"'` for the text 3. A `--file` is a JSON list of `{"key":…,"input":{…}}` steps, as `plan` takes them everywhere.
- **Output** is [TOON](https://toonformat.dev): `key: value` lines, `name[N]{columns}:` tables and a `help[N]:` list of the next commands. `--json` prints the server's JSON instead.
- **Exit codes:** 0 done, 1 refused or failed (`error:` and `code:` say why), 2 wrong usage.
- **Self-report.** Set `BILIBI_AGENT_SELF_REPORT` to `harness/model-version-effort`; it is sent with every request as evidence, never authority (`apps/web/README.md`).

### Reviewed writes and the ref

`plan` prints the server's review and a `ref`, a short hash of the ordered operations. `commit` takes that ref and the same operations again; operations that no longer hash to the ref are refused as `STALE_REF` before anything is sent, so what is committed is what the person reviewed. The server still rechecks every operation when it commits: the ref is a convenience of this door, not an approval Bilibi relies on.

When a commit answers `STEP_UP_REQUIRED`, ask the person for their current 2FA code and run the same commit with `BILIBI_STEP_UP_CODE` set to it.

### What a failure means

| Code | Meaning |
| --- | --- |
| `NO_CONNECTION` | No answer came back: the network or the address |
| `BLOCKED_BEFORE_BILIBI` | Something in front of Bilibi answered with a page that is not JSON: a firewall or CDN ([server installation](../../docs/server-install.md#a-firewall-or-cdn-in-front-of-bilibi)) |
| `NOT_SIGNED_IN`, `SIGN_IN_NEEDED`, `AUTHORIZATION_PENDING` | The sign-in: run `login` |
| `AI_SWITCHED_OFF` | The connection's switch is off; the answer carries the settings address |
| `FORBIDDEN`, `INVALID_INPUT`, `PLAN_REJECTED`, `UNKNOWN_OPERATION`, … | The server's own answer about this request, with its fields. `INVALID_INPUT` also lists the operation's input fields |

## The saved sign-in

**A token never appears in an argument, an environment variable, a log line or an error.** It arrives in a token response and leaves in an `Authorization` header to the address it was issued by.

- **Where:** `BILIBI_AXI_HOME`, else `bilibi-axi` under `XDG_CONFIG_HOME` (or `~/.config`). The folder is `0700` and each file `0600`. A file other accounts can read is refused, with the `chmod` to run.
- **Writes** replace the file whole: a new `0600` file, then a rename.
- **One refresh at a time.** Bilibi's refresh tokens are single-use, and presenting a spent one ends the connection. Every read-refresh-write runs under a lock file, so two commands at once never spend the same one.
- **One address.** Tokens go only to the address given to `login`, which must be the address that Bilibi states for itself (`/.well-known/bilibi`). The client follows no redirect and refuses plain HTTP except to this computer.
- No other credential is kept: no password, no database address. There is no in-process mode that would need one.

## Only a door

**The command line holds no decision.** Whether an input is valid, whether the account may do it, what a write would change and whether it still may are the server's answers, printed as they came. Two checks keep it that way:

| Check | What it holds | Where |
| --- | --- | --- |
| Structure | The program links no workspace crate and no database driver, so it has nothing to decide with and no path to data but the API | `scripts/cli-door.py`, run by CI |
| Behaviour | The same request through the command line and through MCP gives the same JSON: `hello`, search, describe, read, a refused input, an unknown key, a write's review, and a write the account may not make. Every request it made reached `/api/v1`, a changed operation is a stale ref, no token is printed, and the saved sign-in is private | `examples/two-doors-walkthrough.rs`, run by `scripts/run-walkthroughs.sh two-doors` |

To add a command: add the operation or the endpoint on the server first, make the command print its answer, and add the same call through MCP to the walkthrough. A command that needs a rule of its own is a sign the rule belongs in the contract.

```sh
DATABASE_URL=postgres://… scripts/run-walkthroughs.sh two-doors
```
