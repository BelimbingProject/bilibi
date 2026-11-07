# Portal

A tenant's customers sign in to its portal (`/portal/...`), never to the staff application ([ADR 16](../../../docs/architecture/decisions/16-customer-accounts-and-portal.md)). This module owns the portal site, the invitations that admit a customer, the requests people make for an account, the staff controls over the accounts invitations made, and the portal's own home and account pages.

## Contract

| Item | Value |
| --- | --- |
| Facade | `PortalService`, service `core/portal.access` |
| Staff page | `/customer-access` (`admin.customer-access.view`; writes need `admin.customer-access.manage`) |
| Portal pages | `/portal` and `/portal/account` (`portal.account.view`) |
| Portal role | `portal_customer`, given to every customer an invitation admits |
| Settings | `customer_portal.invitation_days` (tenant, default 7), the installation-wide `customer_portal.registration_*` lifetime and abuse limits, and the company retention class `core/portal.registration_requests` |
| Tables | `customer_portal_sites`, `customer_invitations`, `customer_registrations`, `customer_registration_limits`; customer accounts stay in Core User's `users` |

## Portal site

- One per tenant: an **address** (`acme`, lowercase letters, digits and inner hyphens) and a **title**.
- The address names the tenant before anyone signs in: customers sign in at `/portal/sign-in/{address}`. It is unique across the installation.
- The title brands the portal's anonymous cards, its layout and its emails.
- The **logo** is a PNG, JPEG, GIF or WebP image of at most 1 MiB, chosen in the site form (or `logo_asset_id` / `remove_logo` on `core.portal.site.set`, naming an image already stored in Media). It is stored through `Media::put_public_image` on the attachment disk and shows on the anonymous cards and in the portal layout through a short-lived signed Media link (`Media::tenant_stream_url`, the tenant taken from the site record). A replaced logo's file stays in Media until an operator removes it.
- Changing the address breaks links already emailed.
- **Off until staff turn it on** (`enabled`, issue 137 behaviour 1). While off, its anonymous pages answer 404 (`portal_site` returns `None`), its signed-in customers' pages, downloads and live socket answer 404 (the host checks `site_of`), and no invitation is sent or redeemed.

## Invitations

- Staff invite a **Person**, optionally through one of its **affiliations** (a business customer, signing in for that company); without one the customer is a private individual.
- The invite dialog finds the Person by name on the server (`base/records` search, with the actor's permission to see people), so any number of people can be invited. An agent Person is refused: agents act through Agent Access, never the portal.
- The address must not already belong to a customer of the tenant. Inviting it again revokes the open invitation first.
- The email carries a one-time link, `/portal/accept/{address}/{token}`. The token is stored hashed and never read back, not even by agents.
- Accepting creates the account through Core User (`create_customer_in`, address verified), gives it `portal_customer` through Authz (`assign_portal_role_in`) and closes the invitation, in one transaction audited as `system · portal`. The host then signs the customer in with the password just chosen, through the ordinary portal sign-in; when that cannot finish (a tenant that requires a second step), the customer signs in on the next page.
- **Resend** sends a new link to an invitation's Person and address through the same contract as a new invitation, which revokes the old link.
- A Person that retired, an affiliation that ended or a company that was archived since the invitation makes the link invalid.

## Requests for an account

- **Off until staff turn it on** (`registration` on the site, issue 137 behaviour 7). While off, or while the portal is off, `/portal/register/{address}` and `/portal/verify/...` answer 404 and the sign-in card has no link to them.
- Someone gives a name, an email and an optional company name. The answer is the same for known and unknown addresses, including when a limit suppresses the request. A live unverified request or one waiting for staff stays unchanged and sends no new mail, even for concurrent requests from different senders; only an expired unverified request is replaced. The existing token remains valid until `customer_portal.registration_days` expires. The tenant/normalized-recipient unique index arbitrates creation in the owner transaction.
- Registration has its own installation-wide settings for sender, normalized-recipient and tenant-portal limits over rolling one-minute and one-day windows, plus a cap on pending requests. The counters and admission lock are in PostgreSQL, so every application node shares them; registration consumes none of password recovery's budget. The table keeps only tenant-bound one-way subject fingerprints; each admitted request purges up to 100 inactive counter rows older than two days. Reaching the portal-wide rate or pending cap notifies staff who can manage customer access at most once per day. The public response remains the ordinary success response and no mail or registration row is created.
- The emailed link (`/portal/verify/{address}/{token}`, hashed, single use) shows a button; pressing it confirms the address, so a mail scanner that opens links confirms nothing. An unconfirmed request expires after `customer_portal.registration_days`; a confirmed one waits.
- Nothing is written to Person or Company. Staff **approve** a confirmed request by choosing the Person (matching or creating it first) and, for a business customer, its affiliation: this sends the ordinary invitation with the request's name and confirmed address, through the same checks. **Reject** closes it; the person is not told and may ask again.
- Each step is audited: the request and its confirmation as `system · portal`, the decision as the staff member.

### Personal data and retention

- A new unconfirmed request writes only its record ID and status to audit; its unverified name, email and company stay in the registration row rather than append-only evidence.
- `portal.registration` declares `name`, `email` and `company_name` as personal. A confirmed request that matches an email supplied for a data-subject request is a candidate the operator must confirm. Approval links the request to the chosen Person through its invitation, so it is found without an identifier match. Access and portability export confirmed requests; erasure deletes confirmed or linked requests unless a legal hold names the request or Person, and clears their declared fields from audit evidence.
- Expired unconfirmed requests are deleted by default, with no operator setting: each new registration on the portal's tenant first deletes up to 100 of them (the same address first), except any a legal hold names. Confirmed requests still waiting for staff are not expired.
- The shared Privacy retention class **Customer account requests** also purges expired unconfirmed requests and approved or rejected requests after the primary company's configured period. As with every privacy class, an operator must set its period, turn it on and enable the daily privacy sweep before that cleanup runs.

## Portal access panel

- A contact's record page (`core/person`) shows this panel (embed `person.portal-access`, `src/panel.rs`, `VIEW`): the Person's customer accounts and the invitations sent to them, with Invite, Resend, Revoke invitation, Revoke access and Restore.
- Invite asks which of the Person's current affiliations the customer signs in under, or "as an individual", and an address (default: the chosen affiliation's work email, else the Person's email).
- It writes through the same contracts as the staff page and agents (`core.portal.invitation.create`, `.invitation.revoke`, `.account.revoke`, `.account.restore`); there is no second code path.

## Second sign-in step

- A tenant turns on the step-up operation `core.user.portal_sign_in` (default off, on its 2FA requirements page) to ask customers for an authenticator code after the password. The portal sign-in then sends the customer to `/portal/sign-in/{site}/challenge` (the host's page; the pending sign-in is kept in the `bilibi_portal_sign_in` cookie, `Path=/portal/sign-in`), where they start the challenge and enter the code, or cancel. Core User proves the code and admits the session as for staff (`portal_pending_sign_in`, `portal_start_sign_in_challenge`, `portal_complete_sign_in`).
- A customer sets up the authenticator app on `/portal/account/two-factor` (linked from My account): the staff page's stages (password, QR code and key, first code, move to a new phone) drawn without the settings shell (`bilibi_user::PortalTwoFactor`). A customer who never set one up cannot finish a sign-in while the tenant asks for it; staff reset the factor as for staff.

## Delivery of the invitation email

- A relay that refuses the message when it is sent (an unreachable server, a refused recipient) marks the open invitation **Not delivered**; a bounce an Extension reports through `base/mail`'s delivery reports (`DeliveryReports`, the message's `X-Bilibi-Reference` is `portal.invitation:<id>`) marks it **Bounced**. Either way the invitation stays open, the inviter gets one notification (`portal.invitation_failed`, linking to the contact) and the row is audited as `system · portal`.
- A report for another address than the invitation's own changes nothing, and a second report is a no-op.
- The contact's portal-access panel shows the status and offers **Invite again**: the invite dialog opens with the invitation's company and name and its address to correct. Revoke and Resend work on an open invitation in either state.

## Customer accounts

- The staff page lists the tenant's customer accounts (`bilibi_user::customer_accounts`, the one read of `users` that lists customers; staff reads never do).
- **Revoke access** suspends the account through Core User (`set_customer_active_in`): it cannot sign in, fails its standing, and every session it holds ends at once. **Restore** lets it sign in again; no session comes back.

## Agent operations

| Key | Kind | Capability |
| --- | --- | --- |
| `core.portal.site.get` | read | view |
| `core.portal.site.set` | safe write | manage |
| `core.portal.invitation.list` | read | view |
| `core.portal.invitation.create` | safe write | manage |
| `core.portal.invitation.revoke` | safe write | manage |
| `core.portal.account.list` | read | view |
| `core.portal.account.revoke` | safe write | manage |
| `core.portal.account.restore` | safe write | manage |
| `core.portal.registration.list` | read | view |
| `core.portal.registration.approve` | safe write | manage |
| `core.portal.registration.reject` | safe write | manage |

The staff page writes through the same contracts.

## What the host does

The host (`apps/web`) serves the credential pages under `/portal` (`sign-in`, `forgot`, `reset`, `accept`, `register`, `verify`) and asks this facade which site an address names (`portal_site`, `registration_site`), whether an invitation or confirmation link is open (`open_invitation`, `verification_open`), and to redeem an invitation (`accept`), take a request (`register`) or confirm one (`verify`).

## Not yet

- Changing the password in place on `/portal/account`; it links to the emailed reset.
