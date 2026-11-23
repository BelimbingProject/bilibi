# ADR 34: Staff and public pages are separate sites, and the listener is the boundary

**Document Type:** Architecture Decision Record
**Status:** Accepted
**Agents:** codex/gpt-5 (author)
**Scope:** The staff and public HTTP sites, their listeners, hostnames, credentials, machine paths and public customisation
**Last Updated:** 2026-11-23

> Source: [issue 829](https://github.com/BelimbingProject/bilibi/issues/829).

## Context

**A public catalogue, form or customer area must be reachable from the internet without exposing the staff application beside it.** A path prefix cannot provide that boundary: a shop has public pages at several paths, a root-scoped customer cookie on one hostname would reach staff paths, and both sides would share browser storage and same-origin trust. The existing host also accepted any `Host` header that reached its one socket.

## Decision

**Bilibi is one program serving a staff site and, when configured, one public site on different hostnames and different listening sockets. The socket selects the site; the path and `Host` header never switch it.**

- The staff router is mounted only on the staff listener. It serves staff credentials and pages, downloads, live traffic and the machine edge: `/api/v1`, `/mcp`, OAuth, `/.well-known` and SCIM.
- The public router is mounted only on the public listener. It serves anonymous pages and token links, the customer portal, `/visitor/v1` and `/webhooks`.
- Each router also accepts only the authority configured for its listener and answers an unexpected or missing `Host` with a bare 404. The `Host` check prevents DNS rebinding; it does not select a site.
- There is no combined one-hostname mode. An installation with public pages uses two hostnames, including in development. Without a public origin and listener, no public route is served.
- One process and two listeners come first so background work starts once and process-local live wake-ups remain immediate. Separate staff and public processes wait until wake-ups cross processes.
- A tenant has one public site for now, with several hostnames allowed. Tenant sites are never path partitions on one shared hostname.
- `base/public` is the eventual owner of the public boundary, public-site records, guest identity, checks, limits and caching. An anonymous visitor with a state cookie is a guest. Existing `/portal` paths keep their names.
- The shared public layout may vary by data tokens only: logo, colours and a font from the supported list. A tenant cannot add scripts. A third-party script is available only through an operator-installed Extension that declares every origin it needs; wildcard and non-HTTPS origins are refused.

The machine edge remains on the staff site. An operator that needs a cloud identity provider or AI application to reach it publishes only the documented machine paths through its identity-aware proxy. A third API hostname would add another issuer and metadata address without adding a distinct authority.

## Consequences

- Staff sign-in and invitation links may require the VPN or identity-aware proxy. That is the intended cost of keeping the staff site private.
- Staff, customer and guest sessions use `__Host-` cookies on their exact hostname. The listener chooses which credential parser runs, and no credential converts into another realm.
- A staff path on the public listener, or a public path on the staff listener, does not exist there. Route contributions are classified once at boot and cannot ask a request to change sites.
- The public site has its own content-security policy. Third-party origins are narrow Extension contributions, never tenant-supplied free text.
- Multiple branded public sites per tenant, arbitrary themes, a separately hosted storefront API and separate processes are later decisions driven by real cases.

We rejected a shared hostname with path fences because it exposes the staff edge wherever the public site is exposed and shares browser trust. We rejected separate processes now because live wake-ups are process-local. We rejected a third machine hostname because the staff authority already owns those operations and a proxy can publish its exact path list.
