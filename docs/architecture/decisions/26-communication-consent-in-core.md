# ADR 26: Communication consent is an on-demand Core record

**Document Type:** Architecture Decision Record
**Status:** Accepted; implemented
**Agents:** codex/gpt-daybreak-blue-latest-medium (author)
**Scope:** Ownership and composition of communication consent
**Last Updated:** 2026-11-05

## Context

Commerce, CX and other Domains must not disagree about the same person's withdrawal. Consent has the same meaning outside campaigns, but campaign execution and provider policy do not.

A queued message may wait after a person withdraws. Replaying a queue-time answer would send against the current decision. A caller-provided `transactional` purpose would also let an elective sender bypass the rule.

## Decision

**`core/consent` owns company-scoped communication decisions, withdrawals, evidence, purpose and channel scope, suppressions, one eligibility check, and the person contact panel.** It is an on-demand Core module under [ADR 25](25-core-modules-on-demand.md).

Every elective sender calls `Consent::check` immediately before dispatch. The check reads current state and logs a fresh answer. Its correlation key does not replay an earlier answer; delivery idempotency remains with the sender.

Required business messages, such as invoices and HR notices, do not use Consent. Their owning module proves why the message is required. Consent has no `transactional` purpose.

CX keeps campaigns, journeys, contact-pressure policy, preference-centre presentation and provider integrations.

## Consequences

- A decision for one sending company never authorises another company.
- A withdrawal after queueing stops the later dispatch check.
- Base Mail stays independent of Core. Business senders call Consent before transport.
- A People-only application has no Consent module unless another mounted component declares it.
- Moving the former CX record is a clean break: callers declare `bilibi-consent` and use `core/consent` keys.

## Rejected alternatives

- **Keep Consent in CX.** Non-CX senders would need CX or keep conflicting decisions.
- **Put Consent in Base Mail.** Base cannot depend on Core people or companies, and transport does not own business permission.
- **Trust a queue-time check.** Permission can change before dispatch.
- **Let callers label a message transactional.** The sender would be proving its own exemption.
