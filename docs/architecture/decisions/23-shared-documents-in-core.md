# ADR 23: Shared documents in Core, business workflows in Domains

**Document Type:** Architecture Decision Record
**Status:** Accepted
**Agents:** codex/gpt-6-astra-high (author)
**Scope:** Shared document library in `apps/core/document`; CX integration and Finance evidence remain with their owners
**Last Updated:** 2026-11-04

## Context

**A company should keep one document that authorized people can find from their work.** A service agreement may support an opportunity, a case and an invoice. Finance and People must not need CX installed to keep documents.

Base already supplies file storage and protection. A library adds business identity, revisions, classification and a place to find documents. Those shared enterprise records belong in Core, while customer and accounting workflows stay in Domains.

## Decision

**Use `apps/core/document`, owner `core/document`, Rust crate `bilibi-document`, with the menu label “Documents”.** The singular noun matches `company`, `person`, `currency` and `product`.

### Essential library

Core owns the facts needed to identify, find, revise and control a business document:

| Fact | Purpose |
| --- | --- |
| Tenant-scoped identity, name and filing company | Keep one named record under the responsible company. |
| Publication date and numbered revisions with dates and change logs | Establish which version people used and preserve earlier versions. |
| Internal file or external link for each revision | Keep the content or reference for that version; a remote link does not guarantee preservation of remote content. |
| Category, optional subcategory, type and status | Organize and filter the library without customer-specific classifications in code. |
| Links to registered records | Find the same document from different work without duplicating it. |
| Explicit document access and sharing | Control who can discover, view, download or manage it. |

Move the generic foundation from [the CX document proposal](https://github.com/cx2027/bilibi-dom-cx/pull/31): document, revision, link and mark records; file ledger and Artifact owner; public operations, record kind, capabilities, settings and pick-lists. Personal favourites and follows remain library preferences, separate from document access.

Core supplies one Documents list, one record page and one set of operations. Search, metadata CSV, revision actions and permission-checked sharing use the same access rules. Planned copy/merge and email-link actions also belong here, preserving provenance and restrictions; they must not disappear from the requirements when ownership moves. Keep an office suite, cloud-drive synchronization and template merging out of this initial scope.

### Boundaries

| Owner | Responsibility |
| --- | --- |
| Base | [Artifacts](../../../apps/base/artifacts/README.md) private bytes, audited reads and deletion, retention and legal holds; [Media](../../../apps/base/media/README.md) upload/scanning mechanisms; [Records](../../../apps/base/records/README.md) references; [Privacy](../../../apps/base/privacy/README.md) governed disclosure and erasure. Reuse Base Mail for email delivery. |
| Core Document | Library facts, revisions, access and screens; the trusted Artifact owner that authorizes each file operation. |
| CX | Case “Save to documents”, customer/case/opportunity panels and filters, timeline activity and customer-portal presentation. Case attachments remain case-owned until deliberately saved to the library. |
| Finance | Protected source evidence, source authorization, posting finality and accounting retention obligations in `finance/attachments`. A mutable library record must not replace or unlock that evidence. |
| Other Domains | Their own approval, signature, employment, purchasing or fulfilment workflows and retention obligations. |

Domains call Core and register panels through [Module Registry](../../../apps/base/module_registry/README.md); Core never calls a Domain. Panels and quick-create open the same library records and operations, following [DESIGN.md](../../../DESIGN.md). CX supplies portal context, while Core enforces explicit document sharing; replace the `shared_with_customer` shortcut. Generic email links confer only their expressly authorized access.

### Required safeguards

The move must fix these contract gaps before the library is usable for governed documents:

1. **Links grant no document access.** Access to a contact, or membership in the filing company, must never expose an HR file linked to that contact. Require explicit document authority using current Platform company/desk proofs. Apply it to metadata, revisions, search, pickers, panels, reports, exports and bytes. Viewing and downloading remain separate permissions; portal sharing cannot bypass either.
2. **Artifact storage is not a malware scan.** Validate upload content and integrate the shared scanning policy for every uploaded revision. Pending, infected or failed files cannot be downloaded, previewed or shared. A required scan that cannot run keeps the file unavailable; do not copy the proposal's unscanned-Artifact gap into Core.
3. **Retention protects history as well as bytes.** A retained or held revision keeps its document identity, revision metadata, provenance and file. Never delete library metadata first and discover afterward that Artifacts refuses byte deletion. Apply the relevant guards to deletion, merges, cleanup and privacy erasure; use Base's audited, recoverable file lifecycle rather than ad hoc cleanup. Finance evidence retains its own stronger rules.

## Rejected alternatives

- **Library in CX:** other Domains would need CX or a duplicate library.
- **Library in Base:** business filing and revision records would be mixed with reusable storage mechanisms.
- **A separate Documents Domain:** every consumer would need optional cross-Domain integration for the same shared records.
- **Move the CX proposal unchanged:** its linked-record visibility, missing scanning and metadata-first deletion are unsuitable for restricted records.

## Consequences

The owner approved Core placement; this ADR ships no implementation. Adapt CX callers and retain the existing library requirements, including access from accounts, contacts, cases and opportunities. Implement the safeguards with runtime proof before claiming the library complete.

Replace `cx/documents` ownership cleanly: no compatibility aliases, synchronization or backfills. Drop obsolete schema through new migrations and recreate disposable databases under the no-production-users rule, valid before 2027-01-01. Obtain renewed owner direction before relying on that rule after the date.

## Prior art

The 2026-11-04 document-library research distinguishes ownership from placement on a screen. [Salesforce Files](https://developer.salesforce.com/docs/platform/data-models/guide/salesforce-files.html) provides shared documents, versions and record links. [Dynamics 365](https://learn.microsoft.com/en-gb/power-platform/admin/manage-documents-using-sharepoint) embeds SharePoint documents in business apps. [Odoo Documents](https://www.odoo.com/documentation/20.0/applications/productivity/documents.html) centralizes documents across apps. These support shared ownership while business workflows remain separate.
