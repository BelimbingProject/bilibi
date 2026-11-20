# Templates

**Use it for:** declaring, previewing and publishing wording for a workspace or company.

**Not for:** choosing recipients, building attachments or sending messages.

Open **Administration → Templates** to read, preview and publish the wording that installed modules let you change. Choose the workspace or a company and the language. The page shows what that choice inherits and previews wording with sample details. **Restore inherited wording** removes that choice's override.

## Declare a template

Contribute a static `Template` under `bilibi_templates::CATALOGUE` through Module Registry. Use `<domain>.<module>.<purpose>` for the key. Base and Core use their layer name in place of the domain. Give each packaged language an explicit version and declare a base language. The registry checks duplicate keys, slot and variable IDs, and whether the defaults render.

- Declare which slots a variable can appear in and give it a sample of the same type as the runtime fact. The first slice supports text and calendar dates.
- Pass the sealed scope and the authorised business record's company to `Templates::resolve`. Pass the resolved slots and typed facts to `Template::render`. Rendering performs no lookups.
- Use `{{variable_id}}` for substitution. The renderer treats every supplied value as text, and makes control characters in subject values spaces. A missing required fact stops rendering of a slot that uses it. The error identifies the field.
- The sending module owns recipients, attachments, access, and delivery. Its approved wording can be snapshotted before the job queues.

## Which wording is used?

For each language in order, the company override wins, then the workspace override, then the packaged text. Resolution tries the exact requested tag, its base language, the caller's business fallback if it has one, and the template's packaged base language. The first match supplies every slot and reports its language and version or revision.

## Publish through the operation

Use `base.templates.catalogue.list` and `base.templates.template.get` to find slots, allowed variables, current wording and revision. Both the browser and an agent use `base.templates.template.publish` and `base.templates.template.restore`. They append audited revisions and require the exact revision the editor read. A restore appends a marker so an author holding a draft from before a restore cannot overwrite a later change.

Company changes require access to that company. The shared Customisations page lists published overrides and can restore them through the same operation.

## Check the statement cover

With Finance mounted, the `receivables-companies` walkthrough checks the Finance draft operation, two companies in the same workspace, the audit and stale revision checks, and restoration. Set `BILIBI_PDF_CHROMIUM` to a Chromium binary and `BILIBI_MAIL_DIR` to a private empty directory to run its delivery assertion against Base Mail's development mailbox as well.
