# Templates

**Use it for:** declaring, previewing and publishing wording for a workspace or company.

**Not for:** choosing recipients, building attachments or sending messages.

Ask an AI assistant to draft and preview wording through the template operations, then review and approve the change. **Administration → Customisations** shows each override’s current text and sample preview and offers **Restore inherited wording**.

## Declare a template

Contribute a static `Template` through Module Registry. Use `bilibi_templates::CATALOGUE` for editable wording and `bilibi_templates::PLATFORM_CATALOGUE` for packaged-only wording. Use `<domain>.<module>.<purpose>` for the key. Base and Core use their layer name in place of the domain. Give each packaged language an explicit version and declare a base language. The registry checks duplicate keys, slot and variable IDs, and whether the defaults render.

- Declare which slots a variable can appear in and give it a sample of the same type as the runtime fact. The first slice supports text and calendar dates.
- Pass the sealed scope and the authorised business record's company to `Templates::wording` to select and render business wording using language preferences. For explicit language selection, use `Templates::resolve` and pass the resolved slots and typed facts to `Template::render`.
- Use `{{variable_id}}` for substitution. The renderer treats every supplied value as text, and makes control characters in subject values spaces. A missing required fact stops rendering of a slot that uses it. The error identifies the field.
- The sending module owns recipients, attachments, access, and delivery. Its approved wording can be snapshotted before the job queues.
- `Slot::subject()`, `Slot::text(id, label)`, `Variable::text(..)` and `Variable::optional_text(..)` build the common shapes. `bounded(text, max_bytes)` shortens a fact (a title, a name) that must fit a subject.

## Word a module's own mail

**No message text in code: declare a template and ask for its wording.**

```rust,ignore
let text = templates.platform_wording("core.user.password_reset", &[])?;
let link = build_trusted_link();
let message = Message::text(to, text["subject"].clone(), format!("{}\n\n{link}\n\n{}\n", text["intro"], text["closing"]));
mailer.deliver_linked_for(tenant, &message, Some(&link)).await?;
```

- `Templates::wording` renders scoped mail with the authorised record's company. Pass the message's existing locale as the context fallback. `Templates::tenant_wording` reads the preferred language of a tenant the caller proved from its own record. `Templates::platform_wording` renders packaged security mail in its base language without a database connection.
- Supply full facts for mail. If the rendered subject exceeds its byte limit, delivery shortens the text facts in that slot and preserves body text.
- A published override that cannot hold the real facts gives way to the packaged wording, with a log line, so the message is still sent.
- **Links stay in code.** A slot is prose before or after a link; the sending module builds the link and fixes the order of the parts. Security messages use packaged wording. Publish and restore refuse those keys.
- A line some messages lack (a place, an end date) is its own slot with an optional variable; Pass omitted slots to `Templates::wording` when the message leaves out those lines.
- Use the registered [workspace mailer](../mail/README.md#branded-workspace-mail) for the sender name and branded HTML layout.

| Module | Templates |
| --- | --- |
| `core/user` | `core.user.password_reset`, `core.user.sign_in_locked`, `core.user.invitation`, `core.user.email_verification`, `core.user.new_device_sign_in`, `core.user.two_factor_reset` |
| `core/portal` | `core.portal.registration_confirmation`, `core.portal.invitation` |
| `core/activity` | `core.activity.invitation`, `core.activity.guest_reminder`, `core.activity.reminder` |
| `core/document` | `core.document.share` |
| `base/reports` | `base.reports.scheduled_report` |

## Word a portal notice

Contribute `Output::PortalNotice`, then render with `Templates::wording` after authorizing the customer page. Use the company from the portal proof and put the prose through `Html::text`. The portal home uses `core.portal.welcome_notice`; its surrounding actions and links belong to the page.

## Declare a print contract

Contribute one `PrintContract` per document type through `bilibi_templates::PRINT_CATALOGUE`, using the same owner-prefixed key convention as wording. Give the contract a version, printable lifecycle states, typed fields with synthetic samples, and a versioned packaged layout per supported language. Mark mandatory output with `required_output`, independently of whether the fact may be absent. Layouts must show required fields and tables outside presence conditions and include every required column in each use of its table. The registry compiles each default and renders its samples at boot.

- Resolve the packaged layout by exact locale, base language, optional business fallback, then the contract's packaged base. The result includes the language and packaged version. Company and workspace print overrides will use the same per-language order as wording when print revisions are enabled.
- The owning module authorizes the source, chooses the versioned business snapshot and passes the state and typed facts to the compiled layout. Text, calendar dates, integers, exact decimals, currency amounts, booleans and multiline addresses are supported. A repeat field has a named row schema. Required facts and row fields are checked even if a layout omits them.
- Layouts are JSON with a `blocks` array, compiled into HTML by Templates. Blocks are `heading` and `paragraph` with `text`, `field` with `field` and `label`, `section` with `heading` and `blocks`, `if_present` with an optional `field` and `blocks`, and `table` with `rows` and `columns` (each with a row `field` and `heading`). Required scalar fields and required table columns take their wording from the owner’s field labels for the selected layout language. Omit their layout `label` or `heading`. Pass the resolved language when compiling a packaged layout. Layout wording and record text are escaped. Templates supplies the markup.
- A layout is bounded to 48 KiB, 160 blocks, eight levels and 16 columns per table. A table holds up to 2,000 rows. A fact string holds up to 16 KiB, and there is a 4 MiB budget each for facts and rendered HTML.

Run `cargo run --locked -p bilibi-templates --example print-contract-walkthrough` to exercise registry validation, packaged English and Malay selection, optional fields, escaping and repeated rows.

The first tenant owner's invitation (`bilibi tenant-owner`) keeps fixed text: it is sent before the workspace exists.

## Which wording is used?

Set **Wording language** (`templates.language`) through the delegated [tenant settings operations](../settings_editor/README.md#tenant-agent-operations) or choose the company under **System Settings → Settings scope**. Restore the setting to inherit. It accepts a language tag such as `ms-MY` and applies to business email and portal notices. It leaves the interface locale and the language of platform security mail as they are.

1. Try the record company's preference, the workspace preference, the consumer's requested locale, and finally the template's packaged base language. Try the exact tag and then its base language at each preference.
2. At each language, try the company override, the workspace override and the packaged text. The first match supplies every slot and reports the language and version or revision.

Template inspection and publication still address the scope and language given explicitly in the operation.

## Publish through the operation

Use `base.templates.catalogue.list` and `base.templates.template.get` to find slots, allowed variables, current wording and revision. An agent uses `base.templates.template.publish` and `base.templates.template.restore`. They append audited revisions and require the exact revision the editor read. A restore appends a marker so an author holding a draft from before a restore cannot overwrite a later change.

Company changes require access to that company. The shared Customisations page lists published overrides and can restore them through the same operation.

## Check the statement cover

With Finance mounted, the `receivables-companies` walkthrough checks the Finance draft operation, two companies in the same workspace, the audit and stale revision checks, and restoration. Set `BILIBI_PDF_CHROMIUM` to a Chromium binary and `BILIBI_MAIL_DIR` to a private empty directory to run its delivery assertion against Base Mail's development mailbox as well.

Run `scripts/run-walkthroughs.sh templated-wording` for the language chain and the resulting mail. To inspect the light and dark layout, run the `wording-walkthrough` example on a migrated disposable database with `BILIBI_WORDING_DEMO=disposable`, `BILIBI_WORDING_PREVIEW` set to an existing private directory, and a separate development mailbox. Open `mail-preview.html` from that directory. The preview comes from the delivered MIME and substitutes its inline image for the browser.
