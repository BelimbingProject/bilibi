# Templates

**Use it for:** declaring, previewing and publishing wording for a workspace or company.

**Not for:** choosing recipients, building attachments or sending messages.

Ask an AI assistant to draft and preview wording through the template operations, then review and approve the change. **Administration → Customisations** shows each override’s current text and sample preview and offers **Restore inherited wording**.

## Declare a template

Contribute a static `Template` under `bilibi_templates::CATALOGUE` through Module Registry. Use `<domain>.<module>.<purpose>` for the key. Base and Core use their layer name in place of the domain. Give each packaged language an explicit version and declare a base language. The registry checks duplicate keys, slot and variable IDs, and whether the defaults render.

- Declare which slots a variable can appear in and give it a sample of the same type as the runtime fact. The first slice supports text and calendar dates.
- Pass the sealed scope and the authorised business record's company to `Templates::resolve`. Pass the resolved slots and typed facts to `Template::render`. Rendering performs no lookups.
- Use `{{variable_id}}` for substitution. The renderer treats every supplied value as text, and makes control characters in subject values spaces. A missing required fact stops rendering of a slot that uses it. The error identifies the field.
- The sending module owns recipients, attachments, access, and delivery. Its approved wording can be snapshotted before the job queues.
- `Slot::subject()`, `Slot::text(id, label)`, `Variable::text(..)` and `Variable::optional_text(..)` build the common shapes. `bounded(text, max_bytes)` shortens a fact (a title, a name) that must fit a subject.

## Word a module's own mail

**No message text in code: declare a template and ask for its wording.**

```rust,ignore
let text = templates
    .tenant_wording(client, tenant, "core.user.password_reset", &[("person_name", Value::Text(&name))])
    .await?;
let message = Message::text(to, text["subject"].clone(), format!("{}\n\n{link}\n\n{}\n", text["intro"], text["closing"]));
mailer.deliver_for(tenant, &message).await?;
```

- `Templates::tenant_wording` resolves the workspace override or the packaged text in the template's base language and fills it in. It takes a tenant the caller proved from its own record, because a reset link or an invitation has no session. It reads no company override; use `resolve` for a business record that has a company.
- A published override that cannot hold the real facts gives way to the packaged wording, with a log line, so the message is still sent.
- **Links stay in code.** A slot is prose before or after a link; the sending module builds the link and fixes the order of the parts. A workspace can reword a security message (reset, verification, sign-in alert) but cannot remove, move or replace its link.
- A line some messages lack (a place, an end date) is its own slot with an optional variable; the module leaves the slot out when the fact is absent.
- `Mailer::deliver_for` sends under the workspace's sender name ([Mail](../mail/README.md#sender-name)).

| Module | Templates |
| --- | --- |
| `core/user` | `core.user.password_reset`, `core.user.sign_in_locked`, `core.user.invitation`, `core.user.email_verification`, `core.user.new_device_sign_in`, `core.user.two_factor_reset` |
| `core/portal` | `core.portal.registration_confirmation`, `core.portal.invitation` |
| `core/activity` | `core.activity.invitation`, `core.activity.guest_reminder`, `core.activity.reminder` |
| `core/document` | `core.document.share` |
| `base/reports` | `base.reports.scheduled_report` |

The first tenant owner's invitation (`bilibi tenant-owner`) keeps fixed text: it is sent before the workspace exists.

## Which wording is used?

For each language in order, the company override wins, then the workspace override, then the packaged text. Resolution tries the exact requested tag, its base language, the caller's business fallback if it has one, and the template's packaged base language. The first match supplies every slot and reports its language and version or revision.

## Publish through the operation

Use `base.templates.catalogue.list` and `base.templates.template.get` to find slots, allowed variables, current wording and revision. An agent uses `base.templates.template.publish` and `base.templates.template.restore`. They append audited revisions and require the exact revision the editor read. A restore appends a marker so an author holding a draft from before a restore cannot overwrite a later change.

Company changes require access to that company. The shared Customisations page lists published overrides and can restore them through the same operation.

## Check the statement cover

With Finance mounted, the `receivables-companies` walkthrough checks the Finance draft operation, two companies in the same workspace, the audit and stale revision checks, and restoration. Set `BILIBI_PDF_CHROMIUM` to a Chromium binary and `BILIBI_MAIL_DIR` to a private empty directory to run its delivery assertion against Base Mail's development mailbox as well.
