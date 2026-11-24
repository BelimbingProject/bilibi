# Public site

**Use it for:** anything a person with no staff account opens on the public site: an anonymous page (a form, a survey, a preference page) or a token link a mail client, image proxy or calendar service calls (a tracking pixel, a click redirect, a one-click unsubscribe, a calendar feed), each with its sender budget.

**Not for:** staff pages (a `Route` on the staff site), the customer's signed-in portal pages (`Access::Portal`, [Core Portal](../../core/portal/README.md)), a chat window on another website ([`base/visitor`](../visitor/README.md)) or a provider's signed callback (`/webhooks`, [`base/integration`](../integration/README.md)).

`base/public` owns every first-party request from someone who is not staff ([ADR 34](../../../docs/architecture/decisions/34-two-sites-listener-boundary.md)). A module declares what it serves and how much one sender may ask of it; the host admits the request and hands the owner the site's sealed `PublicScope`. The owner never sees a sender address, a cookie or a header.

## Contract

| Item | Value |
| --- | --- |
| Facade | `Public`, service `base/public.site`: the listener budget (`admit`) and the scope of the site the listener serves (`site`), both for the host |
| Contributions | `PUBLIC_PAGES` (`PublicPage`), `PUBLIC_LINKS` (`PublicLink`) |
| Owner types | `PageService` (`bilibi_ui::page::PageService<PublicScope, Error>`), `LinkService`, `Link`, `LinkRequest`, `Reply`, `Target`, `Cache`, `PIXEL` |
| Budgets | `SenderBudget { per_minute, per_hour }`, required on every page and link; `Counter` holds one budget's counters |
| Settings | `public.sender_per_minute` (default 1,200) and `public.sender_per_hour` (default 20,000), installation-wide: the listener budget |
| Tables | None |

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

## Budgets

Every public request passes two counts, cheapest first, before the owner is called. Over either, the answer is `429 Too many requests`.

| Count | Counts | Set by |
| --- | --- | --- |
| Listener | Every request of one sender to the whole public site: pages, links, assets, portal pages, the visitor edge. Signed `/webhooks` calls are machines with their own limits and are not counted | The operator: `public.sender_per_minute`, `public.sender_per_hour` |
| Route | Every request of one sender to one page or link | The declaring module: `budget` |

- A sender is an IPv4 address or an IPv6 /64, through the trusted-proxy boundary (`session.trusted_proxies`), as on the visitor edge and the sign-in limiter.
- Counts are fixed one-minute and one-hour windows in the server's memory; allowed traffic writes nothing to the database. Several servers each count their own.
- A route counts up to 65,536 senders, the listener 262,144. When full, senders of an earlier hour go first, then senders not seen this minute (who lose their hour's count); only a minute with more distinct senders than that refuses a new one.
- Size a link's budget for the callers that share addresses: mail providers, image proxies and calendar services fetch for many people from few addresses.

## Boot refusals

Discovery refuses a page or link with a malformed path, an empty operation, a path under `/portal`, a zero limit or a minute limit above its hour's, a repeated pattern, and a link that is not GET or POST or has no `{token}`. When the host binds the public site, it refuses a `{token}` page or link whose own module did not declare it under `CREDENTIAL_PATHS`, and one that overlaps a host path or another public route. `apps/web/examples/public-site-reference.rs --refusals` prints each.

## PublicScope

- **Sealed.** No public constructor, no `Deserialize`, no conversion to a staff `Scope`. It names the tenant whose public site the request reached (`tenant()`) and grants nothing.
- An owner reads and writes as its own declared system principal in that tenant, and finds a token's or address's record there only.

## Proof

`apps/web/examples/public-site-reference.rs --sender-admission` (walkthrough `public-sender-admission`) drives the real public router: a page and its POST link refuse a sender past its minute budget and admit another sender, an hour budget still refuses after the minute resets, a link of tenant 2 is refused on tenant 1's site like an unknown token, and with a listener budget of 3 a fourth request from one sender is refused even on a link with room of its own while another sender is admitted.

## Not yet

- Site records and hostnames: the listener serves the one tenant named by `PUBLIC_TENANT_ID`.
- Actions: a POST that creates a record with no session behind it (tickets, trap field, fill time, places per hour, receipts).
- Guest cookie, caching rules, the public content-security policy and the challenge port.
