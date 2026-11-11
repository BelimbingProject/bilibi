# Visitor sessions

**Use it for:** a public visitor session for someone with no account (a website's chat window), the origin allow-list that admits it, and the transport that carries its messages, typing signals and files to a Domain's channel.

**Not for:** a staff or customer sign-in (use `base/session`), a chat model of your own on top of `/live`, or a token table, CORS handler or upload endpoint of your own.

`base/visitor.sessions` (`Visitors`) owns the visitor session store. The Web host owns the edge under `/visitor/v1/` (`apps/web/src/visitor.rs`). A Domain contributes a [`Channel`](#what-a-domain-contributes): the conversation behind a site. The Platform carries opaque JSON and bytes; it knows nothing about what a message means.

Decision: [ADR 27](../../../docs/architecture/decisions/27-visitor-sessions-are-a-separate-credential.md).

## What a visitor session is

| Property | How |
| --- | --- |
| No account | The session names no user, person or role. Its authority is the channel's `subject`, which the Domain chose. |
| Bound to a tenant and a site | The Domain's `Site` names the tenant and the origins its pages are served from. The session records both. |
| Bound to one browser origin | The origin that opened it. Every use must present exactly that `Origin`, and the site must still list it. |
| Short-lived and renewable | `visitor.session_minutes` (default 15) from the last use, never past `visitor.session_max_hours` (12) from the start. Using it renews it. |
| Never a staff session | A separate table, a `vst_` token no other check parses, no cookie, and a `VisitorScope` that converts to no `Scope`. |
| Revocable | Staff end one session or a whole conversation's; the next use is refused and open sockets close. |
| Rate-limited | Per sender, per site, per session and per server (settings below). |

Only a SHA-256 digest of the token is stored. The token is returned once.

## What a Domain contributes

```rust
inventory::submit! { Contribution::new(CHANNELS, "cx/chat", provenance!(), &Channel { key: "cx/chat", service: "cx/chat.visitor" }) }
// service "cx/chat.visitor" resolves to bilibi_visitor::ChannelService(Arc<dyn VisitorChannel>)
```

`VisitorChannel` has five methods. The host calls them only after it has proved the origin, the token and the limits.

| Method | The Domain decides |
| --- | --- |
| `site(key)` | Which tenant and origins a site key means, from its own records. Asked before a session exists and again on every use, so a closed site or a removed origin stops its sessions at once. |
| `admit(site, hello)` | Whether this visitor starts a conversation, and its `subject` (at most 128 printable characters). `hello.details` is untrusted JSON the embed sent (at most 4 KiB). |
| `receive(scope, frame)` | A message (JSON, at most 16 KiB), a typing signal (1 KiB) or a complete file (`Upload`, bytes). The reply, if any, is the acknowledgement. |
| `since(scope, cursor)` | What the visitor has not seen. The browser returns the last cursor, so a dropped connection loses nothing. |
| `watch(scope)` | A wake-up when `since` has something new. No I/O. |

Rules a channel keeps:

- **Scope everything to `scope.subject()` and `scope.tenant()`.** A visitor reads and writes the records its subject names and nothing else. Never take a record id, tenant or party from a frame.
- **Take the site from your own record.** `Site::new(tenant, key, origins)` rejects any origin that is not `scheme://host[:port]` exactly as a browser sends it, and an empty list. `Site::admit(origin)` is the only way to an `Admission`.
- **Files go to Base Media.** `Upload` carries the bytes; the Platform has bounded their size. Store them through Media's attachment path with a declared system principal (`apps/base/media/README.md`, "Stored by a system principal"), so the type check and the malware scan apply and your `SubjectOwner` decides who may read them. Never write bytes yourself.
- **Never trust the name.** `Upload.name` is the browser's text.
- **Return `Forbidden`, `Unauthorized` or `NotFound` to refuse**, `Invalid` for a bad value. They reach the visitor as one `refused` or `invalid`; anything else is `unavailable` and is logged.

## The edge

All HTTP calls are `POST` with `Content-Type: text/plain` and a JSON body, so a browser sends no preflight. A reply carries `Access-Control-Allow-Origin` only when the origin is accepted; every refusal is a bare `{"error": "refused"}` (403), `invalid` (400), `rate_limited` (429) or `unavailable` (503).

| Path | Body | Reply |
| --- | --- | --- |
| `/visitor/v1/sessions` | `{channel, site, details?}` | `{token, session, expires_in}` once the site lists the `Origin` and the channel admits the visitor |
| `/visitor/v1/session/renew` | `{token}` | `{expires_in}` |
| `/visitor/v1/session/end` | `{token}` | `{ended: true}` |
| `/visitor/v1/live` | WebSocket, `Origin` required | below |

**The socket.** The first frame must be `{"type":"hello","token":"…","cursor":"…"}` within 10 seconds, or the socket closes. The token never travels in a URL or cookie. Then:

| From the browser | Meaning |
| --- | --- |
| `{"type":"message","id":1,"body":{…}}` | A message. Answered `{"type":"ack","id":1,"reply":…}` or `{"type":"error","id":1,"code":…}` |
| `{"type":"typing","body":{…}}` | A typing signal. No answer |
| `{"type":"upload","id":3,"name":"…","size":N}` | Announces a file; answered `{"type":"upload_ready","id":3}`. Then binary frames `[id u32][offset u32][bytes ≤ 64 KiB]` in order, the same header as the staff live socket's. The channel is handed the file when every byte is there |
| `{"type":"upload_cancel"}` | Drops the file being received |

| From the server | Meaning |
| --- | --- |
| `ready` | The session proved; `since` follows |
| `items` | `{items:[…], cursor}` from the channel |
| `renewed` | The session was renewed by use (`expires_in`) |
| `error` | A refused frame (`refused`, `invalid`, `rate_limited`, `unavailable`); the socket stays open |
| `closed` | `{reason: "refused" \| "ended" \| "rate_limited" \| "unavailable"}`, then the socket closes |

Before any frame acts the host proves the token and origin again and asks the channel for the site again; a session that no longer holds closes the socket with `ended`. A socket also rechecks every 10 seconds, and a staff revocation closes it at once.

## Operator settings

Platform capability `admin.system.visitor.manage`; group Visitor in the settings editor.

| Key | Default | Bounds |
| --- | --- | --- |
| `visitor.session_minutes` | 15 | A session's lifetime from its last use |
| `visitor.session_max_hours` | 12 | Its longest life, however often renewed |
| `visitor.sessions_per_sender_per_minute` | 10 | New sessions per network address, per server; renewals, endings and socket upgrades from one address get six times as many |
| `visitor.sessions_per_site_per_minute` | 120 | New sessions per site, per server |
| `visitor.messages_per_minute` | 60 | Frames per session; the 101st refusal in a row closes the socket |
| `visitor.sockets_per_session` | 3 | Open sockets per session, per server |
| `visitor.sockets_total` | 2,000 | Open visitor sockets per server; staff and portal sockets have their own places |
| `visitor.upload_max_bytes` | 5 MiB | A file's size; Media's own limit can only be smaller |
| `visitor.uploads_per_minute` | 6 | Files per session |

Counts are per server, as the HTTP throttles' are. A changed setting applies within two seconds.

## Staff side

`Visitors::revoke_session(client, scope, channel, session)` and `revoke_subject(client, scope, channel, subject)` end sessions of the staff scope's own tenant, record who did it as `visitor.session.revoked`, and close open sockets. **Check your own capability first**; the Platform proves only that the scope is live. Opening and ending a session are audited as `visitor.session.opened` and `visitor.session.ended` by a guest of the tenant. Refusals are not audited one by one, because a public caller could flood the ledger; they are rate-limited instead. Rows ended or expired more than a day ago are removed by the next session opened, in batches of 100.

## Checks

`apps/web/examples/visitor-walkthrough.rs` is the running proof. With a disposable, migrated `DATABASE_URL`, `BILIBI_DISPOSABLE_VISITOR_DEMO=yes` and an empty directory in `BILIBI_MEDIA_DEMO_ROOT`:

    cargo run -p bilibi-web --example visitor-walkthrough           # prepare: every case, exits non-zero on a FAIL
    cargo run -p bilibi-web --example visitor-walkthrough -- serve  # the host for a browser, with a staff control address

`prepare` plays a browser by hand against the real host: each refused origin, a session over the socket with messages, typing, a clean and an infected file (stored through Media by a declared principal and settled by the scan), an oversize file and a broken chunk, rate limits, renewal, expiry, a revoked session and its socket, an origin removed from a site, another tenant's visitor, and a visitor token sent as a cookie, a bearer and over every live socket to staff, portal and agent routes. The file's header lists `serve`'s addresses.
