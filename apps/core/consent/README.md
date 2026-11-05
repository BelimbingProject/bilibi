# core/consent

Core Consent keeps one company-scoped answer to whether a person may receive an elective message on a channel for a purpose.

## The boundary

**A sender checks Consent immediately before dispatch.** A queue-time answer is never permission to send later. The check reads the current decisions and suppressions, records a new answer, and leaves delivery idempotency with the sender.

An invoice, payslip, security notice or other required business message does not use Consent. The business module proves that the message is required through its own operation. A caller cannot bypass Consent by naming an elective message `transactional`.

Consent owns decisions, withdrawals, evidence, purpose and channel scope, company-scoped suppressions, eligibility checks, and the panel on a person's record. A Domain owns campaigns, journeys, contact-pressure rules, preference-centre presentation and provider integrations.

## Company scope

Every decision, suppression and check names the company sending the message. A grant for one company never authorises another company in the same tenant.

Staff pages use the selected company. Facade callers pass `company_id`, and the facade proves the caller can work in that company before reading or writing.

## Example

Company 12 records that person 41 agreed to marketing email, with the wording shown as evidence. A campaign queues a message. Before the provider call, the campaign calls `Consent::check` for company 12, person 41, email and marketing.

If the person withdrew after the message was queued, the fresh dispatch-time check returns `Refused(Withdrawn)`. Reusing the campaign correlation key does not replay the earlier answer.

## Records

All tables carry `tenant_id` and `company_id`. A subject is either one Core person or one normalised address. Decisions are append-only; a change adds the next version.

| Table | Holds |
| --- | --- |
| `core_consent_decisions` | Every decision version, including source, capture channel and evidence |
| `core_consent_current` | The latest decision per company, subject, purpose and channel |
| `core_consent_suppressions` | Active and lifted suppressions per company, subject and channel |
| `core_consent_log` | Every fresh dispatch-time answer, its caller and correlation key |

## Eligibility order

`check` stops at the first refusal:

1. An active suppression for the company, person or address on the channel returns `suppressed`.
2. The latest decision for the purpose on the channel applies. If none exists, the purpose decision for `any` applies.
3. Between person and address decisions, the most recently recorded decision wins. A tie refuses.
4. A granted decision allows. A withdrawal returns `withdrawn`.
5. With no decision, `core.consent.default_new_address` chooses `allowed` or `no_consent`.

`check` is a write inside the sender's `MutationTransaction`. The `key` correlates the answer with the sender's delivery record; it is not an idempotency key.

## Facade

The crate root exports `Consent`, its inputs and its result types. Every method takes a sealed `Scope` and rechecks access.

| Method | Purpose |
| --- | --- |
| `check` | Read and log a fresh eligibility answer at dispatch time |
| `preview` | Show the answer now without logging it |
| `record` | Add a grant or withdrawal with evidence |
| `suppress` | Add an active suppression; repeating the same suppression is idempotent |
| `lift` | Lift a suppression with a reason |
| `for_person`, `history` | Read current standing and evidence for one company and person |
| `suppressions` | Read the company suppression list |

The public capability keys are `core.consent.view`, `core.consent.record`, `core.consent.check` and `core.consent.manage`.

## Codes

| Code | Values |
| --- | --- |
| Channel | `email`, `sms`, `messaging`, `voice`, `chat`; a decision may use `any` |
| Source | `portal`, `preference_link`, `agent`, `import`, `inbound_keyword`, `api` |
| Capture channel | A sending channel, `web`, or `in_person` |
| Suppression reason | `hard_bounce`, `complaint`, `unsubscribe` |
| Refusal | `no_consent`, `withdrawn`, `suppressed` |

Purposes come from the `core.consent.purposes` pick-list. The starting values are `service` and `marketing`.

## Address matching

Email addresses are lower-case. Phone numbers use international `+` form. SMS and voice refuse a national number without a country code. Other handles are trimmed.

An address-only question does not infer a person. Pass `person_id` when the sender knows the person.

## Pages and agents

The person detail page receives one Consent panel with current decisions, withdrawals, evidence and suppressions for the selected company. The Suppressions list uses `FlexTable` and `ListState`.

Agent operations reach the same facade methods. The operation contract lives in `src/agent_operations.rs`; no module-specific endpoint exists.

## Composition

Consent is an on-demand Core module. An application links it only when a host, Domain or Extension declares `bilibi-consent` and descriptor dependency `core/consent`. A People-only application does not compile the module or install its tables, pages or operations.

The descriptor, migrations and contributions are in this crate. Composition follows [ADR 25](../../../docs/architecture/decisions/25-core-modules-on-demand.md).

## Where to go next

- [ADR 26](../../../docs/architecture/decisions/26-communication-consent-in-core.md) explains why this record is Core.
- [`base/personal_data`](../../base/personal_data/README.md) owns data-subject access and erasure orchestration.
- [`base/mail`](../../base/mail/README.md) owns email transport, not consent decisions.
