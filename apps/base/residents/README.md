# Resident readers

**Use it for:** a reader that holds a live connection (a phone system's event stream, a chat socket) and handles events as they arrive: declared by your module, started outside the queue's job slots, and owned by exactly one server process per key.

**Not for:** work that ends (use `base/queue`), work at set times (use `base/schedule`), or a `tokio::spawn`, connection loop or lease table of your own.

A resident reader is a long-lived background worker. A module declares the reader once, keeps one key per connection it wants held (a source ID), and the Platform starts a reading for each key in one server process, restarts it when it fails, and moves it to another process when its owner stops. Decision: [ADR 32](../../../docs/architecture/decisions/32-resident-readers.md).

The first consumer will be cx/voice, in a separate Domain pull request after this one merges. This pull request ships the Platform mechanism and the walkthrough specimen; cx/voice adoption will retain its checkpoint, replay and one-transaction-per-answer contract.

## Why not a queue job

**A job that never finishes takes a queue slot for good.**

| | Queue job | Resident reader |
| --- | --- | --- |
| Lifetime | One attempt, at most 15 minutes in the Web host | Until its key is released |
| Capacity | 10 slots a process | Outside the slots: `Config::capacity` (512) a process |
| Where it runs | Whichever process claims the attempt | One owner per key; the connection stays in that process |
| On failure | Retried up to `max_attempts`, then discarded | Restarted with backoff, for ever |

## Declare a reader

**One `Reader` contribution, and a service that implements `Resident`.**

```rust
inventory::submit! { Contribution::new(bilibi_residents::READERS, "voice/source", provenance!(), &Reader {
    service: "voice/calls.source-reader",
    principal: "voice-reader",
    capability: "voice.sources.read",
}) }
```

| Part | Rule |
| --- | --- |
| Entry ID | The reader ID: starts with a lowercase letter or digit; contains lowercase letters, digits, `_`, `/`, `-`, at most 128 bytes. A meta-term, never a vendor or product name |
| `service` | A service of the same module, resolving to `ReaderService(Arc<dyn Resident>)` |
| `principal` | A system principal the same module declares; boot refuses another module's |
| `capability` | Inside that principal's declaration. The tenant's grant decides (`Grants::set_system`), or a standing grant (`bilibi_authz::STANDING_GRANTS`) when no tenant should have to switch it on; a tenant's deny still wins |
| Boot | `Supervisor::from_services` refuses a missing or foreign service, principal or capability before any reader starts |

## Keep and release a key

**Call `Residents::keep` in the transaction that makes the connection wanted, and `Residents::release` in the one that ends it.**

| Call | Effect |
| --- | --- |
| `keep(tx, scope, reader, key)` | Makes sure a reading exists for this tenant, reader and key. Idempotent: a kept reading is unchanged; a released row is reused without clearing its old lease |
| `release(tx, scope, reader, key)` | Marks it released. A running reader is cancelled at its owner's next heartbeat; its row retains the ownership barrier until it stops or its lease expires. Idempotent |

- Both join your audited `MutationTransaction` and record a `Change` (`base/residents.reader`), so the reading commits or rolls back with your write. Take the service from `bilibi_residents::SERVICE`.
- `scope` is the sealed scope of whoever runs your command (a person, or a queued job's `Execution::scope()`); its tenant must be the transaction's. Neither call grants anything: authorize your own command first.
- A key is a reference such as a record ID: starts with a lowercase letter or digit; contains lowercase letters, digits, `.`, `_`, `:`, `/`, `-`, at most 128 bytes. Never a name, address or number of a person: operators see keys.
- Keep is level-triggered. A scheduled task that keeps every key that should exist, and releases the others, repairs any drift.

## Write the reader

**`Resident::read` runs until the connection closes, the reader fails, or `Reading::cancelled()` resolves.**

```rust
impl Resident for SourceReader {
    fn read<'a>(&'a self, reading: &'a Reading) -> ReadFuture<'a> {
        Box::pin(async move {
            let Ok(mut stream) = self.connect(reading.scope(), reading.key()).await else {
                return Ended::Failed("connect_failed");
            };
            loop {
                tokio::select! {
                    _ = reading.cancelled() => return Ended::Closed,
                    event = stream.next() => match event {
                        Some(event) => if self.settle(reading, event).await.is_err() { return Ended::Failed("settle_failed") },
                        None => return Ended::Closed,
                    },
                }
            }
        })
    }
}
```

| Part | Rule |
| --- | --- |
| `Reading::scope()` | The principal's sealed scope in the key's tenant. Hand it to owner facades; each decides by the principal's name and live grants. Never a borrowed user's scope |
| `Reading::key()`, `tenant()` | The key this reading holds and its tenant |
| `Reading::cancelled()` | Resolves when the process shuts down, the lease is lost, the key is released or the principal is refused. Stop within `Config::grace` (5 seconds); after that the reader is dropped |
| `Reading::begin(client, action)` | Opens the audited transaction for a write. It locks this reading's live lease and proves the principal and capability again, so a reader whose lease moved on gets `Error::Conflict` instead of writing. Keep it short and never hold it over external IO: the lease cannot be renewed while the transaction holds it, and a reader that holds one for two heartbeats is cancelled |
| `Ended::Closed` | The connection ended in the ordinary way: start again after `Config::restart_delay` (1 second) |
| `Ended::Failed(code)` | Start again after backoff. `code` is lowercase `[a-z][a-z0-9_]{0,63}` and is what operators see; error text, payloads and addresses are never stored |
| Delivery | At least once across a takeover: a new owner may see an event the old one was handling. Make effects idempotent (a checkpoint, an idempotency key) |

## One owner per key

**A lease row in PostgreSQL names the one process that runs each key; a heartbeat keeps it.**

| Event | What happens |
| --- | --- |
| Claim | Each process polls once a second and claims due readings with `FOR UPDATE SKIP LOCKED`, up to its capacity, writing its owner label and a fresh lease token |
| Heartbeat | Every 10 seconds (a third of the default 30-second lease), one update renews eligible leases; a follow-up read distinguishes locked leases from lost ones when any did not renew |
| Owner dies | Its leases run out; readings become due without backoff (`owner_lost` is recorded). Takeover waits for a successful poll by a process that declares the reader and has spare capacity |
| Lease lost | A confirmed loss cancels the reader immediately; an unproven renewal cancels it before the conservative lease deadline minus grace. `Reading::begin` refuses writes without a live lease |
| Clean stop | On shutdown every reader is cancelled and waited for, then handback is attempted within the grace period. Successful handback makes the reading due at the next poll; otherwise its lease expires |
| Failure | `Ended::Failed`, a panic or a refused principal backs off `min(300, 2^failures)` seconds; a run of 60 seconds or more clears the count |
| Authority | Proven before each start, in every `Reading::begin`, and again every five minutes while reading: a suspended tenant, a removed declaration or a withdrawn grant stops the reader with `actor_refused` until it is restored |
| Database error | Claim and preparation waits are bounded by one heartbeat. Heartbeat waits also end by the earliest held lease deadline minus grace. Reproof yields to shutdown and lease loss. After a child stops, it is removed from heartbeats before handback, which is bounded by grace; failed handback leaves expiry as the recovery path |

Lease time is conservatively measured from before the claim or renewal request. Each reading independently watches that deadline, reserves the cancellation grace, and caps its final wait at lease expiry, including during reproof.

Release immediately fences writes and stops renewals, even if keep follows before the next heartbeat. Keep reuses the row but cannot start a replacement until the previous reader has stopped and handed back its token, or its lease deadline has passed. Released keys are omitted from the operator list and System Info. Handback deletes released rows; bounded poll cleanup removes unowned or expired released rows, including declarations no longer mounted.

A process claims only readers its own composition declares, so during a rolling update an older process leaves a new module's readings to the newer one.

## What operators see

**Administration › System › Resident readers lists every reading.**

| Column | Meaning |
| --- | --- |
| Reader, Key, Tenant | What is read, for whom |
| Status | Running, Starting (due, waiting for a process with a free place), Waiting to retry (waiting out a restart delay or the previous owner’s release barrier, with the next eligible start time), Taking over (its owner stopped renewing the lease; the next poll takes it over), Not mounted (not running, and this server has no reader of that ID) |
| Owner | The host name and process ID of the server process that holds, or last held, the lease |
| Last heartbeat, Running since | When the owner last renewed the lease, and when the current running lease began |
| Last error | The reader's last failure code and when it happened |

- The page needs the platform operator and `admin.system.readers.view` (in the System Viewer role).
- System Info's Health card shows one line: how many readings run, wait and retry.
- The server log names a reader ID, a reading's number and a failure code. It never holds a key, an event or a payload.

## Hosting

The Web host starts one `Supervisor` beside the queue executor (`Config::default()`). A private host builds it with `Supervisor::from_services(&services, config)` after `Services::install` and runs it with a Tokio watch shutdown receiver.

These limits are internal tuning with safe defaults that operators do not yet need to change; the owner granted an exception to a Settings surface for this release. Add Settings and an operator editor if operators need to tune them.

| `Config` field | Default | Purpose and default rationale |
| --- | --- | --- |
| `capacity` | 512 | Caps concurrent connections while allowing hundreds of sources |
| `poll_interval` | 1 s | Finds due readings promptly without continuous polling |
| `lease_seconds` | 30 | Allows transient database delays with a 10-second heartbeat and bounded takeover |
| `grace` | 5 s | Allows cooperative cleanup within the lease safety window |
| `restart_delay` | 1 s | Avoids a tight reconnect loop after ordinary closure |
| `max_backoff` | 300 s | Limits repeated failures while retrying at least every five minutes |
| `healthy_after` | 60 s | Two default lease periods distinguish a healthy run from repeated early failures |
| `reproof` | 300 s | Bounds idle authority revocation latency without frequent authorization queries |
| `owner` | host name and process ID | Distinguishes server processes without personal data |

## Running proof (no unit tests)

On a fresh, disposable PostgreSQL database:

```sh
BILIBI_DISPOSABLE_RESIDENTS_DEMO=yes cargo run -p bilibi-residents --example residents-walkthrough
```

It migrates the database, installs the walkthrough fixture, starts private server processes of itself and checks, exiting non-zero on the first failed check. Its reader belongs to a reference module (`extensions/residents_walkthrough`) linked only into the example, declared the way a Domain declares one:

| Step | Proves |
| --- | --- |
| A refused principal | A reading whose principal holds no grant does not start (`actor_refused`); it starts once the grant is given |
| One owner per key | Two processes, four keys: every key has one owner, and the reader's own fenced, audited writes come from that process alone |
| Takeover | The owning process is killed; the other takes its keys after the lease, with `owner_lost` recorded and no overlap in the writes |
| Fenced write | A reader whose lease was taken is refused by `Reading::begin` and cancelled; the unowned lease runs out and the reading starts again |
| Failure and backoff | A reader that fails, then panics, is restarted after its backoff and then runs; one whose connection closes is restarted without a failure |
| Release and keep | Two processes keep running while a released key is kept again; connection lifetime events prove the old reader stops before its replacement starts. The row is reused, then removed after final release; operator status excludes it |
| Withdrawn grant | Revoking the principal's grant stops the readers; granting it again starts them |
| Clean stop | A stopped process cancels its readers, hands its leases back, and a new process starts them sooner than a lease could expire, its first write after the old owner's last |
| Tenancy | The same key in another tenant is its own reading, refused there until that tenant grants the principal |
| Operator view | `Residents::list` and the System Info line as the platform operator; another tenant's administrator is refused |

`residents-walkthrough serve`, with `APP_ORIGIN`, `BIND_ADDR` and a database that `./bilibi start` prepared, serves the whole app with the specimen reader, for a person to look at the Resident readers page: two readings run, one waits to retry and one waits for a free place.
