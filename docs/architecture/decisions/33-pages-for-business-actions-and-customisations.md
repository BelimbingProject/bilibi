# ADR 33: Pages for business actions and customisations

**Document Type:** Architecture Decision Record
**Status:** Accepted
**Agents:** codex/gpt-6.1-terra-low
**Scope:** Which agent actions need a human page, and how people inspect and reverse customisations
**Last Updated:** 2026-11-19

## Context

The owner decided on 2026-11-19 to replace the 2026-10-20 rule that every MCP action needs a GUI fallback. Setup is rare and conversation can be a better editor for it, while daily work must remain possible without AI. A person also needs to see and reverse an AI mistake or a customisation made before they adopted Bilibi.

## Decision

- **Business actions have a page.** Records, approvals, money and anything a customer or employee does day to day must be possible on a Bilibi page as well as through an agent. Examples include creating a customer, approving a request and issuing an invoice.
- **Setup and customisation may be AI-only to create or edit.** Examples include labels, custom fields, list views, rules, settings, menu order and setup packs. They do not each need a human editor.
- **Weigh UI complexity.** A rarely used feature that adds screens, controls or settings counts against building a page. Eligible customisation instead uses AI and the read-only Customisations page. This does not override the business-action requirement.
- **Classify by effect, not name.** A setting that changes money or security behaviour is a business action and keeps a page. Calling an action setup or a rule does not exempt it when its effect belongs to daily business, money or security.
- **One Customisations page shows every change.** The design calls for a shared page where people can inspect customisations and use a restore-default or undo action for each change without AI. It supports adopters without AI, recovery from mistakes and audit. Secrets never show their value on the page.
- **Every action remains an operation contract.** Pages, agents and the undo or restore action call the same contracts and retain their authorization, validation and audit as in [ADR 01](./01-agent-first-operations.md). This decision changes the required human interface, not the operation boundary. Tenant customisation remains data on shared servers under [ADR 28](./28-tenant-customisation-is-data-on-shared-servers.md).

## First examples

The owner identified these five proposals as candidates for AI-led setup, with inspection and reversal on Customisations where they change customisation data:

- **Company settings:** AI changes eligible settings; Customisations offers restore default or the inherited value. Money and security settings keep their own page.
- **Customise this list:** AI changes labels, fields and views, so a separate entry point on each list is unnecessary.
- **Setup packs:** The proposal includes labels, fields, views and rules, with imported changes visible on Customisations. See the [Setup packs contract](../../../apps/base/setup_packs/README.md) for current transfer support.
- **HTML and PDF templates:** AI drafts and previews template changes through operations for a person to approve. Customisations shows current text and a rendered preview and offers restore; no template editor page is planned.
- **Rule trial runs:** AI runs a draft against past records and presents the results; approval and other business actions keep their pages.
- **[Tenant menu order](../../../apps/base/menu_order/README.md):** AI changes the order. Customisations will offer a restore-default action.

## Consequences

- A business action without a page is missing its human route. An eligible setup editor may be AI-only, but its changes must be visible and reversible on Customisations.
- See the [Customisations page contract](../../../apps/base/labels/README.md#customisations-page) for current coverage.
- One shared inspection and recovery page avoids a full editor for each infrequent customisation, while preserving a route to understand and undo changes without AI.

## Alternatives rejected

- **A full page editor for every agent action:** adds an editor for each rare setup task when conversation can serve it.
- **AI-only business actions or invisible customisations:** leaves people without AI unable to do daily work or recover from a mistake.
