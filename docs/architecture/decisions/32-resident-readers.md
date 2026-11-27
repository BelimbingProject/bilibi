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
3. **One owner per key, by a lease row.** PostgreSQL coordinates ownership across processes without a dedicated connection per reader. Claim, renewal and handback follow the [module’s ownership contract](../../../apps/base/residents/README.md#one-owner-per-key).
4. **The token is the fence.** A process that lost a reading must not write for it. Fenced transactions and bounded cancellation follow the [reader contract](../../../apps/base/residents/README.md#write-the-reader).
5. **Keys are data, kept in the owner’s transaction.** The desired connection commits or rolls back with the business write. Release must preserve an ownership barrier so a quick keep cannot overlap connections; the [keep and release contract](../../../apps/base/residents/README.md#keep-and-release-a-key) owns the lifecycle.
6. **Authority as for system queue jobs.** A reading runs as a system principal its own module declares, in the key's tenant, decided on a capability inside that declaration: before each start, inside every `Reading::begin`, and periodically while it reads. A refusal stops the reader and is retried with backoff, so restoring the grant starts it again.
7. **Failures restart for ever, with backoff.** A reader has no attempt limit: it backs off up to five minutes and a healthy run clears the count. Only a bounded code is stored and shown to operators.

## Consequences

- An event is handled when it arrives, and a connection stays in one process until that process stops.
- At-least-once across a takeover: the new owner may see an event the old one was handling, so owners keep effects idempotent, as with Queue.
- Failover has a gap while ownership is recovered; see the [lease and handback contract](../../../apps/base/residents/README.md#one-owner-per-key).
- Operators get one list of every reading (owner, state, last heartbeat, last error) and a System Info line.
- A module that holds a live connection declares a reader; a job that keeps reading, a `tokio::spawn` loop or a lease table of its own is the pattern this replaces.

## Alternatives rejected

- **A Domain-only workaround** (poll more often, or reconnect in each job): keeps the delay or the reconnects, and every channel would rebuild the same ownership problem.
- **A queue job that keeps reading:** takes a job slot per live source for good, fights the attempt timeout, and Queue's retry budget would discard a reader that must never stop.
- **A session advisory lock per key:** needs one dedicated database connection per reading and vanishes silently with that connection; lease rows allow batched heartbeats and expose ownership and health to operators.
- **Sticky routing (one process designated to read):** needs a coordinator the installation does not have, and a stopped process would stop every reading.
