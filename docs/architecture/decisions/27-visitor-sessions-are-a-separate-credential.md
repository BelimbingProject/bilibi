# ADR 27: A public visitor session is its own credential, bound to an origin and a Domain's records

**Document Type:** Architecture Decision Record
**Status:** Proposed
**Agents:** claude/sonnet-5.5-default (author)
**Scope:** How a website on another origin lets someone with no account talk to a Domain (`base/visitor`, `/visitor/v1`)
**Last Updated:** 2026-11-11

> Source: [issue 513](https://github.com/BelimbingProject/bilibi/issues/513).

## Context

**A Domain wants a chat window on its customer's own website.** The person using it has no account and the page is on another origin. Until now every credential here was a staff or customer account's: a cookie the browser sends to this origin only, proved by Session, with every staff route behind it. None of that fits a visitor. A cookie does not travel to a third-party page, and a visitor must never be a user in any form.

Three choices had to be made: what the credential is, what it may reach, and where the conversation lives.

## Decision

**A visitor session is a bearer token in its own table, bound to a tenant, a site and one browser origin. It is read by one edge and seals a `VisitorScope`, which is not a `Scope`.**

- **A separate store and token.** `visitor_sessions` is not `sessions`. The token starts `vst_` and only a digest is stored. No staff check, customer check, agent grant or step-up reads this table, and this edge reads nothing else. The token travels in a body or a socket's first frame, never in a cookie or URL, so no staff route receives it and no log line carries it.
- **Bound to a tenant, a site and an origin.** A Domain's `Site` names its tenant and the origins its pages are served from. A session records the origin that opened it; every use must present exactly that `Origin`, and the Domain is asked for the site again each time, so a closed site or a removed origin stops its sessions at once.
- **No authority beyond a subject.** The Domain's channel returns a `subject` when it admits the visitor (the conversation). `VisitorScope` carries tenant, session, channel, site and subject. It has no constructor outside the crate, no `Deserialize` and no conversion to `Scope`, so Authz, Media and every owner API that takes a `Scope` cannot be reached with it. A Domain acts on the records its subject names and nothing else.
- **The Platform carries, the Domain decides.** Messages and typing are opaque JSON, a file is bytes, and the Domain's `VisitorChannel` answers. Files are stored by the Domain through Base Media with a declared system principal, so the type check, the scan and the subject owner apply; the Platform never writes bytes.
- **Simple cross-origin requests.** The HTTP calls are `POST text/plain`, so no preflight is ever sent to an address nobody has checked. `Access-Control-Allow-Origin` is set only for an accepted origin; a refusal is one fixed answer.
- **Short, renewable, revocable, limited.** Fifteen minutes from the last use, twelve hours in all, by operator setting; staff end a session or a conversation's; each of sender, site, session and server has a limit.

We rejected reusing `sessions` with a new `kind`, as customer accounts do (ADR 16), because a visitor has no `users` row for the kind check, Authz audience or standing proof to hold, and a shared table puts a public credential one missed `WHERE` from a staff check. We rejected a cookie, because a third-party cookie is dropped by browsers and would be sent to staff routes. We rejected a signed stateless token, because the revocation and per-session limits the Domain needs require a row. We rejected a Platform-owned site table and chat model: the origin list and the conversation are the Domain's records with the Domain's own screens, and the Platform proves only the origin list it is handed.

## Consequences

- A visitor can reach `/visitor/v1` and nothing else that authenticates: a staff route reads a cookie only, `/api/v1` proves an agent grant, and a `vst_` token matches neither.
- A Domain that adds a channel keeps its site records and their screen, implements five methods, scopes every query to `scope.subject()`, and stores files through Media.
- A visitor's identity across sessions is the Domain's: `admit` may use a Domain-issued value in `details` to resume a conversation. The Platform guarantees nothing of it.
- Refusals are rate-limited rather than audited one by one.
