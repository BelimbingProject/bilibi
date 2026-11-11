# ADR 27: An event type may be bound to the system principal that produces it

**Document Type:** Architecture Decision Record
**Status:** Proposed
**Agents:** claude-code/claude-fable-5-2-medium (author), codex/gpt-6.1-sol (coordination)
**Scope:** How `base/events` records and hands over who produced an event, for a subscriber that must not act on an event from anyone else
**Last Updated:** 2026-11-12

> Source: [issue 521](https://github.com/BelimbingProject/bilibi/issues/521). The contract is [`apps/base/events/README.md`](../../../apps/base/events/README.md#binding-an-event-type-to-its-producer); Authz's is [`apps/base/authz/README.md`](../../../apps/base/authz/README.md).

## Context

An Extension that connects an outside system (a phone system, a messaging provider) has to hand what it observed to a Domain, and the Domain has to write it in its own audited transaction. Events already do the second half: a subscriber's writes commit with its receipt. They did not do the first: `Events::record` took the publisher from the event type's declaration and nothing from the caller, so any mounted code holding any transaction could record any declared event type, and the subscriber was told nothing about who did. A source ID inside the payload is data the caller wrote, not proof.

The transaction's `Context.actor` cannot fill the gap on its own. `Context` has public fields and public constructors and is documented as attribution, never an identity or a capability. What Authz does decide is a named system principal's durable grant in one tenant (`authorize_system`): a live tenant, the installed declaration, the tenant administrator's stored grant, a denying grant winning.

## Decision

1. **Opt-in, per event type.** A publisher contributes `Producer { principal, capability }` under `base/events.producers`, keyed by the event name, beside its `EventType` (the same shape as `PERSONAL_EVENTS`). `EventType` is unchanged, so every existing publisher compiles and behaves as before.
2. **Boot holds the binding to the publisher's own vocabulary.** The event type, the principal and the capability must all be declared by the contributing module, the capability must be inside the principal's declaration, and it must not be a standing grant: only a grant a tenant administrator stored admits a producer.
3. **`Events::record_as(tx, &SystemScope, event)` is the only way to record a bound type.** It requires the scope to be the bound principal, in the transaction's tenant, with no impersonator, the transaction's actor to be that same system principal (so the audit row and the stamped producer cannot disagree), and then `authorize_system` for the bound capability inside the caller's transaction. Plain `record` on a bound type, and `record_as` on an unbound one, are refused. An authorization refusal poisons the caller's transaction, so Authz's decision row for a denial rolls back with it: a refusal leaves no durable record in Base, and the caller reports it.
4. **The producer is stored with the event** (`base_events.producer`, null for unbound types) and is part of the idempotent-replay comparison: the same key with a different stored producer is `Conflict`. Authorization runs before the lookup, so a producer without the grant learns nothing about recorded keys.
5. **Delivery hands the stored producer to the subscriber** as `Event::producer()`. The field is private and only `base/events` builds an `Event`. The delivery scope stays the shared `domain-events` principal; the subscriber decides what the producer may do, against its own configuration, inside the delivery transaction.

## What this does and does not prove

- **Proved by Base:** the event was recorded through `record_as` by code presenting the bound principal, in a tenant whose administrator had granted that principal the bound capability at that moment, and delivery reads the producer as it was stored. Nothing makes the stored value tamper-proof: it holds under the trusted-mounted-code boundary below, like every other row.
- **Not proved: which mounted crate called.** `SystemScope::rehydrate` is public, so any code linked into the application can present any declared principal. Mounted code is trusted; the boundary this decision adds is the tenant's grant and the publisher's declaration, not isolation between crates in one process. Nothing here is sealed or cryptographic.
- **Not proved: anything about the outside system.** An Extension still authenticates its provider transport (signature, credential, connection) before it calls `record_as`. The producer names the Bilibi principal, never the provider.
- **Not changed: delivery guarantees.** Delivery stays at-least-once and ordered per subject; a provider's own duplicates and ordering are the Extension's to reconcile. This is not exactly-once ingestion.
- **Left to the subscriber:** matching the producer and the delivery tenant to its own active source. A grant withdrawn after recording does not unrecord an event, so a subscriber that must stop accepting a source checks its own source row at delivery.

## Rejected alternatives

- **A mutating port that receives the caller's transaction.** It would let one module write another's tables inside a transaction it does not own; Events already give the Domain its own transaction, receipt and audit.
- **A `producer` field on `EventType`.** One declaration instead of two, but it breaks every publisher's struct literal for a property most never use.
- **Copying `Context.actor` into the event without an Authz decision.** Forgeable by construction.
- **A source ID in the payload, a service account through the Agent API, or a caller-supplied actor.** Each lets the caller name its own authority.
- **A signed ingress route per connector.** Authenticating the provider is the Extension's job and stays there; a route would not give the Domain a transactional write.

## Open for the owner

- **Refusal handling.** A subscriber that returns an error blocks the subject and is retried up to 100 times. A subscriber may instead record the event as refused in its own table and return `Ok`; Base does not choose.
- **A vendor-neutral name for the subscriber to read.** A subscriber names each event type it reads, and an event name carries its publisher's module ID. How a vendor-neutral Domain reads several vendor Extensions without naming them is not settled here.
- **Volume and retention** of one `base_events` row per provider event.
- **Durable denial records.** Whether a refused `record_as` should leave a record that survives the rollback is a policy choice not made here.
- **Operator visibility (follow-up, not built).** The stored producer is not shown on the Events page, in `deliveries` or in the agent operations.
- **Rows recorded before a type was bound** keep a null producer. Base never fills one in: recording their key again is `Conflict`, and a subscriber that requires a producer refuses them until an operator decides what to do with them.
