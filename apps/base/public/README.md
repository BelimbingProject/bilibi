# Public site

**Use it for:** anything a person with no staff account opens on the public site: an anonymous page (a form, a survey, a preference page), an action that makes a record from that page (a lead, a contact request) or a token link a mail client, image proxy or calendar service calls (a tracking pixel, a click redirect, a one-click unsubscribe, a calendar feed), each with its sender budget.

**Not for:** staff pages (a `Route` on the staff site), the customer's signed-in portal pages (`Access::Portal`, [Core Portal](../../core/portal/README.md)), a chat window on another website ([`base/visitor`](../visitor/README.md)) or a provider's signed callback (`/webhooks`, [`base/integration`](../integration/README.md)).

`base/public` owns every first-party request from someone who is not staff ([ADR 34](../../../docs/architecture/decisions/34-two-sites-listener-boundary.md)). A module declares what it serves and how much one sender may ask of it; the host admits the request and hands the owner the site's sealed `PublicScope`. The owner never sees a sender address, a cookie or a header.

## Contract

| Item | Value |
| --- | --- |
| Facade | `Public`, service `base/public.site`: site records and hostnames (`site_of`, `set_site_in`, `add_hostname_in`, `remove_hostname_in`, with previews), hostname resolution (`resolve`; `resolve_from` for a request, spending its sender's lookup budget), the address for emailed links (`origin`), the site a request reached (`site`, `logo_matches`), the listener budget (`admit`), action tickets and receipts (`ticket`, `receipt`, `tally`, `submit`) and the deployment binding (`bind_hostname`) |
| Staff page | **Administration → Public site** at `/public-site` (`admin.public-site.view`; writes need `admin.public-site.manage`) |
| Agent operations | `base.public.site.get` (read), `base.public.site.set`, `base.public.hostname.add`, `base.public.hostname.remove` (safe writes); the page writes through the same contracts |
| Command | `bilibi-web public-hostname --tenant ID --hostname NAME [--on]` (`install-server.sh public-hostname`) |
| Contributions | `PUBLIC_PAGES` (`PublicPage`), `PUBLIC_LINKS` (`PublicLink`), `PUBLIC_ACTIONS` (`PublicAction` with `Checks`) |
| Owner types | `PageService` (`bilibi_ui::page::PageService<PublicScope, Error>`), `LinkService`, `Link`, `LinkRequest`, `Reply`, `Target`, `Cache`, `PIXEL`; `ActionService`, `Action`, `ActionRequest`, `Acted`, `Ticket`, `Receipt`, `Tally` |
| Budgets | `SenderBudget { per_minute, per_hour }`, required on every page and link; `Counter` holds one budget's counters |
| Settings | Installation-wide: `public.lookups_per_minute` (default 120) and `public.lookups_per_hour` (default 2,000), the lookup budget; `public.sender_per_minute` (default 1,200) and `public.sender_per_hour` (default 20,000), the listener budget |
| Tables | `public_sites` (one per tenant: address, title, logo, on or off, its ticket key), `public_site_hostnames` (unique across the installation, one primary per site), `public_receipts` (one per used ticket, kept 7 days), `public_abuse_counts` (per action, subject and hour) |

## Sites and hostnames

**The hostname names the tenant.** The public listener finds a request's site by its `Host` alone (`Public::resolve`): a hostname of a site that is on, of a live tenant, at the listener's own port. An unknown hostname, a site that is off and a tenant that is not live are one bare 404, for every path. The request then runs with that site's `PublicScope`.

- **One site per tenant**, off until staff turn it on: an address (`acme`, lowercase letters, digits and inner hyphens, unique; `www`, `app`, `admin`, `api`, `mail`, `staff`, `portal` and `public` are kept), a title (what visitors and customers see on its pages and emails) and a logo (a PNG, JPEG, GIF or WebP image in Media, streamed to the site's own pages only).
- **Hostnames**: lowercase names such as `shop.example.com`, without scheme or port, never an IP address or the staff site's own name. A hostname belongs to one site across the installation. The first is primary, a newly primary one takes over, and the primary one goes only when it is the last.
- **Emailed links** (customer invitations, account-request confirmations, calendar feeds) use `Public::origin(tenant)`: the public listener's scheme (and port, when not the default) with the site's primary hostname, and nothing while the site is off or has none. A customer password reset uses the hostname the customer asked from.
- Answers are reused for five seconds per server (at most 10,000 hostnames); a write on the same server forgets them. Every other lookup, on any path, first spends one of its sender's lookups (`Public::resolve_from`); past the lookup budget the answer is 429 without a lookup, so a flood of made-up hostnames from one sender is bounded before it reaches the database. A `Host` whose port is not a number names nothing.
- **Staff** manage the site on **Administration → Public site**: the site card reads first and opens its form in place (address, title, the on/off toggle, the logo); the hostnames list has **+ Hostname**, a primary action and a confirmed remove. Agents use `base.public.*`; every page write is one of those contracts.
- **Deployment**: `bilibi-web public-hostname --tenant 1 --hostname shop.example.com [--on]` makes the site (address `site-<tenant>`, the tenant's name as title, off) if it has none and gives it that primary hostname, audited as `system · public-site`. The launcher binds `shop.<name>.localhost` to tenant 1 and turns it on at first setup.

## A page

```rust
inventory::submit! { Contribution::new(PUBLIC_PAGES, "cx/surveys.response", provenance!(), &PublicPage {
    path: "/survey/{token}", service: "cx/surveys.public", operation: "survey",
    budget: SenderBudget { per_minute: 20, per_hour: 120 },
}) }
```

- **GET only.** The service resolves to `bilibi_ui::page::PageService<PublicScope, Error>`; `render` gets `PageRequest { scope: Some(&scope), .. }` with the path and query, no body and `framed: false`. The host wraps the owner's HTML in the shared document with no shell, menu or live socket, and refuses a framed request.
- **A POST from the page** goes to a `PublicLink` at the same path when the path's token or address is the credential (a survey answer, a preference save), answered with a redirect back to the page.
- `Error::NotFound` from the owner is the bare 404 page.

## A link

```rust
inventory::submit! { Contribution::new(CREDENTIAL_PATHS, "cx/email.click", provenance!(), &CredentialPath { path: "/email/c/{token}" }) }
inventory::submit! { Contribution::new(PUBLIC_LINKS, "cx/email.click", provenance!(), &PublicLink {
    method: Method::Get, path: "/email/c/{token}", service: "cx/email.links", operation: "click",
    budget: SenderBudget { per_minute: 600, per_hour: 10_000 },
}) }
```

- **The token is the only credential.** The path carries `{token}`, and the module declares the same pattern under `CREDENTIAL_PATHS`, so the token is `[redacted]` in logs and debug reports and never sent on in a referrer.
- **No session, no `Origin`, no CSRF.** The host reads no cookie or `Origin` here, which is why a mail provider's one-click POST (no `Origin`) is admitted. Nothing here acts as a signed-in person.
- **Owner.** The service resolves to `LinkService<Error>` wrapping the owner's `Link`. `answer` gets the operation, the site's `PublicScope`, the path parameters, the request's content type and the bounded body (empty for a GET). Find the token's record by its SHA-256 digest **in `scope.tenant()`**, never by comparing the raw token, so one tenant's link answers nothing on another tenant's site; then write as unattended work through your facade.
- **Reply.** `Reply::Bytes { bytes, content_type, cache }` (`Reply::pixel()` is a transparent 1×1 GIF), `Reply::Redirect(Target)` (302), `Reply::SeeOther(Local)` (303 to a path on this same site, where a page's own form returns after its POST link acted) or `Reply::NoContent` (204). `Reply::headers` decides every header: `no-store` unless the owner asks for `Cache::Private`, `nosniff`, `X-Frame-Options: DENY` and `default-src 'none'; frame-ancestors 'none'; sandbox`; a redirect also sends `Referrer-Policy: no-referrer`.
- **No open redirect.** A `Target` exists only through `Target::parse`: an absolute `http`/`https` address with a host, no user name or password, no backslash, whitespace or control character, at most 2,048 characters. Parse the address your record stored; the owner never sees the query string. A `Local` exists only through `Local::parse`: a path starting with one `/`, no backslash, `#`, whitespace or control character, so it never leaves the hostname the browser asked; build it from the link's own path, never from the request body.
- **One refusal.** Any refusal from the owner (`NotFound`, `Invalid`, `Forbidden`, ...) and a framed request get the same bare `404 Not found`, so an unknown token, a token of another tenant and a known one with an unusable target look alike. Where a different answer would reveal which tokens exist, answer as for a known one: the pixel for any token, 204 for any one-click POST whose body says `List-Unsubscribe=One-Click`.

## An action

```rust
inventory::submit! { Contribution::new(PUBLIC_ACTIONS, "cx/forms.submit", provenance!(), &PublicAction {
    path: "/f/{form}", service: "cx/forms.submissions", operation: "submit",
    budget: SenderBudget { per_minute: 10, per_hour: 60 },
    checks: Checks { trap_field: true, min_fill_seconds: 3, places_per_hour: Some("cx.forms.submissions_per_hour") },
    carry: &["utm_source", "utm_medium", "utm_campaign"],
}) }
```

**A POST from the site's own page that makes a record with no session behind it.** The page is a `PUBLIC_PAGES` page at the same path; `base/public` proves the request before the owner sees it, and the owner never sees a ticket, an address or a header.

- **The page** asks `Public::ticket(scope, action_id, subject)` when it renders and puts `Ticket::fields()` into its form: a hidden `public[ticket]` and, with `trap_field`, a visually hidden `public[check]` out of the tab order. The subject is the owner's own key (a form and its version, `cx-form:12:3`): ASCII letters, digits and `:._-`, at most 100. Nothing is written.
- **The POST** passes the route budget, then must carry the site's own `Origin` (else 403), then, in order:

| Check | When it fails | The owner |
| --- | --- | --- |
| Ticket: signed for this action with this site's key, at most a day old | 303 to `?expired=1` with the `carry` fields; counted as expired | not called |
| Used before | 303 to the first receipt again | not called |
| Trap field filled, or sent back sooner than `min_fill_seconds` | spam: a receipt whose page reads as accepted; counted | not called |
| Places: this hour's receipts for the subject (spam not counted) under the tenant setting, counted under one PostgreSQL lock so every server shares it | 303 to `?busy=1` with the `carry` fields; counted as busy | not called |
| Otherwise | a pending receipt, then the owner | `act(ActionRequest)` |

- **The owner** gets the operation, the site's `PublicScope`, the subject the ticket named, the path parameters and each posted name's first value (at most 100 names; `public[...]` removed). It writes as its own system principal in `scope.tenant()` and answers `Acted::Accepted` or `Acted::Refused(words)`: words about what was typed (at most 500 characters), never about the tenant's records. An `Invalid`, `NotFound`, `Forbidden` or `Refused` error settles the receipt as refused with no words; any other error is a server fault.
- **The answer** is always a 303 to the request's own path with `?receipt=<token>`. The page reads `Public::receipt(scope, action_id, token)` for a day: `Accepted` (also while pending, and for spam) or `Refused(words)`. Another tenant's or an older receipt reads `None`.
- **Receipts** keep the ticket's and the receipt's SHA-256 digests, the outcome and the refusal's words, never a typed value; later actions remove those older than 7 days, 100 at a time. Receipts and counts are audited as `system · public-site`.
- **Abuse counts** (trap, too fast, busy, expired) are kept per tenant, action, subject and hour. Each server writes a subject's counts at most once a minute, so a flood writes little; the owner's staff page reads them with `Public::tally`.
- **Each site signs with its own key**, so a ticket from one tenant's page is an expired ticket on another tenant's hostname.

## Budgets

Every public request passes these counts, in order, before the owner is called. Over any, the answer is `429 Too many requests`.

| Count | Counts | Set by |
| --- | --- | --- |
| Lookup | Every hostname of one sender the server must look up in the database, on any path, before the request is known to reach a site. An answer read in the last five seconds is free | The operator: `public.lookups_per_minute`, `public.lookups_per_hour` |
| Listener | Every request of one sender to the whole public site, after its hostname named a site: pages, links, assets, portal pages, the visitor edge, any path under `/webhooks/` that is no registered webhook. A POST to a registered signed webhook is a machine with its own limits (`base/integration`) and is not counted | The operator: `public.sender_per_minute`, `public.sender_per_hour` |
| Route | Every request of one sender to one page or link | The declaring module: `budget` |

- A sender is an IPv4 address or an IPv6 /64, through the trusted-proxy boundary (`session.trusted_proxies`), as on the visitor edge and the sign-in limiter.
- Counts are fixed one-minute and one-hour windows in the server's memory; allowed traffic writes nothing to the database. Several servers each count their own.
- A route counts up to 65,536 senders, the listener 262,144. When full, senders of an earlier hour go first, then senders not seen this minute (who lose their hour's count); only a minute with more distinct senders than that refuses a new one.
- Size a link's budget for the callers that share addresses: mail providers, image proxies and calendar services fetch for many people from few addresses.

## Boot refusals

Discovery refuses a page, link or action with a malformed path, an empty operation, a path under `/portal`, a zero limit or a minute limit above its hour's, a repeated pattern, a link that is not GET or POST or has no `{token}`, and an action whose fill time is over an hour, whose places setting is blank or whose carried field is empty or under `public[`. When the host binds the public site, it refuses a `{token}` page, link or action whose own module did not declare it under `CREDENTIAL_PATHS`, and one that overlaps a host path or another public route. `apps/web/examples/public-site-reference.rs --refusals` prints each.

## PublicScope

- **Sealed.** No public constructor, no `Deserialize`, no conversion to a staff `Scope`. It names the tenant whose public site the request reached (`tenant()`) and grants nothing.
- An owner reads and writes as its own declared system principal in that tenant, and finds a token's or address's record there only.

## Proof

`apps/web/examples/public-site-reference.rs --sender-admission` (walkthrough `public-sender-admission`) drives the real public router with two tenants' sites on two hostnames: a page and its POST link refuse a sender past its minute budget and admit another sender, an hour budget still refuses after the minute resets, each hostname names its own tenant and a link of one tenant is refused on the other's hostname like an unknown token, an unknown hostname and the listener's bare address get a bare 404, a site switched off answers 404 while the other still answers, and with a listener budget of 3 a fourth request from one sender is refused even on a link with room of its own while another sender is admitted. `public-actions-walkthrough` (walkthrough `public-actions`) proves an action through the real router: a POST without the page's `Origin` is refused, a missing or forged ticket goes back expired with its UTM field, a ticket sent back within the fill time and a filled trap field are spam that reads as accepted and never reaches the owner, a good submission reaches the owner once and its replay answers with the same receipt, the owner's refusal shows its words, past the tenant's places the form comes back busy, tenant 1's ticket on tenant 2's hostname is refused, and the abuse counts are written. `site-boundary-walkthrough` proves the hostnames at the portal and a customer of one tenant being nobody on the other's hostname; then, with the listener budget spent, a path under `/webhooks/` that is no webhook is refused while five deliveries to the registered signed webhook still reach its boundary, and with a lookup budget of 5 a sender rotating made-up hostnames on `/webhooks/x` gets five 404s and then 429 for twenty more new names without a lookup, while another sender's lookup and a hostname read in the last five seconds still answer, and a `Host` whose port is not a number names nothing.

## Not yet

- Verifying a custom hostname (a TXT record) and base-domain subdomains for a shared server; the on-demand TLS "ask" address.
- Rotating a site's ticket key; abuse counts on the Public site page.
- Guest cookie, caching rules, the public content-security policy and the challenge port.
