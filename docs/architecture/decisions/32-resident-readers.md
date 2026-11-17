# ADR 32: Live connections are held by resident readers, outside the queue, one owner per key

**Document Type:** Architecture Decision Record
**Status:** Accepted; implemented
**Agents:** claude/claude-fable-5-2-high
**Scope:** How a module keeps a long-lived connection open and handles its events in a multi-process installation
**Last Updated:** 2026-11-17

> Source: [issue 765](https://github.com/BelimbingProject/bilibi/issues/765). The contract is [`apps/base/residents/README.md`](../../../apps/base/residents/README.md); Queue's is [`apps/base/queue/README.md`](../../../apps/base/queue/README.md).

## Context

A Domain that reads a live stream (a phone system's events; later chat sockets and other channels) had only Queue and Schedule to run in the background. A schedule fired each minute and a queue job per source polled a few times and stopped. An event reached its record up to a minute late, and with several server processes the next job usually ran elsewhere and reconnected. A job that keeps reading instead would hold one of a process's 10 job slots for good and still be cut off at the 15-minute attempt timeout.

## Decision

1. **A third kind of background work.** `base/residents` runs *resident readers*: a module declares a reader (`READERS`), and each key it keeps (a source ID) is one reading that runs until the key is released. Queue stays for work that ends and Schedule for work at set times.
2. **Outside the job slots.** Readings never use a queue slot or an attempt timeout. Each server process runs one supervisor beside the executor, bounded by its own capacity.
3. **One owner per key, by a lease row.** A reading is a row in PostgreSQL holding the owner's label, a lease token and an expiry. Processes claim due rows with `FOR UPDATE SKIP LOCKED`; one statement per beat renews every lease a process holds. A lease that runs out is taken over at the next poll of any process; a clean shutdown hands leases back first.
4. **The token is the fence.** Every statement that renews or ends a lease names its token, and `Reading::begin` locks the live lease in the owner's transaction, so a process that lost a reading cannot write for it. Cancellation (shutdown, lease loss, release, refused authority) is a signal the reader selects on; a reader that ignores it is dropped after a grace period shorter than a heartbeat.
5. **Keys are data, kept in the owner's transaction.** `Residents::keep` and `release` join the owner's audited `MutationTransaction`, as `Queue::enqueue_for` does, so a reading exists exactly when the owner's write committed. Keep is idempotent and level-triggered. Release marks the row unwanted, fences writes and stops renewals while retaining its owner, token and deadline. Keep reuses that row; a replacement cannot be claimed until the previous reader confirms it stopped or its lease expires. Released rows are excluded from operator status and deleted on handback or by bounded poll cleanup once their barrier expires.
6. **Authority as for system queue jobs.** A reading runs as a system principal its own module declares, in the key's tenant, decided on a capability inside that declaration: before each start, inside every `Reading::begin`, and periodically while it reads. A refusal stops the reader and is retried with backoff, so restoring the grant starts it again.
7. **Failures restart for ever, with backoff.** A reader has no attempt limit: it backs off up to five minutes and a healthy run clears the count. Only a bounded code is stored and shown to operators.

## Consequences

- An event is handled when it arrives, and a connection stays in one process until that process stops.
- At-least-once across a takeover: the new owner may see an event the old one was handling, so owners keep effects idempotent, as with Queue.
- After an owner dies its keys are unread for up to one lease (30 seconds by default); a clean stop hands over within a poll.
- Operators get one list of every reading (owner, state, last heartbeat, last error) and a System Info line.
- A module that holds a live connection declares a reader; a job that keeps reading, a `tokio::spawn` loop or a lease table of its own is the pattern this replaces.

## Alternatives rejected

- **A Domain-only workaround** (poll more often, or reconnect in each job): keeps the delay or the reconnects, and every channel would rebuild the same ownership problem.
- **A queue job that keeps reading:** takes a job slot per live source for good, fights the attempt timeout, and Queue's retry budget would discard a reader that must never stop.
- **A session advisory lock per key:** needs one dedicated database connection per reading and vanishes silently with that connection; a lease row costs one statement per process per beat and gives operators the owner, heartbeat and last error for free.
- **Sticky routing (one process designated to read):** needs a coordinator the installation does not have, and a stopped process would stop every reading.
