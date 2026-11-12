Consent answers whether a company may send an elective message to a person or address, on a channel, for a purpose, now.

- **Check at dispatch.** `core.consent.preview` shows the answer without logging it. The sender's real dispatch calls the facade `check` inside its write transaction and records a fresh answer.
- **Required business messages stay outside Consent.** The module that owns an invoice, payslip or security notice proves why it must send that message.
- **Company scope matters.** A decision for one sending company does not apply to another company.
- **`core.consent.for_person`** shows current decisions and active suppressions for the selected company. **`core.consent.history`** includes source, capture channel and evidence.
- **`core.consent.record`** adds a grant or withdrawal. Record only what the person actually decided.
- **`core.consent.suppress`** stops elective contact after a bounce, complaint or unsubscribe. **`core.consent.lift`** needs a reason.
- Email addresses are lower-case. A phone number needs its country code and starts with `+` or `00`, on every channel.
