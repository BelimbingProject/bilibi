# Mail

`base/mail` builds complete outgoing email, renders its wire bytes once, and sends those bytes through SMTP or a private development mailbox. It also reads received email and makes its HTML safe to store and show.

## Sending a message

Build a plain message, then add the parts your business email needs:

```rust,ignore
use bilibi_mail::{Address, Message};

let mut message = Message::text(
    Address::new(Some("Customer"), "customer@example.test")?,
    "Your statement",
    "Your statement is attached.",
);
message.html = Some("<p>Your statement is attached.</p>".into());
let sent = mailer.deliver(&message).await?;
// Archive sent.bytes; sent.message_id identifies this exact message.
```

**Use the registered `Mailer` (`base/mail.mailer`) for installation mail.** `Sent.bytes` are exactly the RFC 5322 bytes passed to the transport. The service reads current installation limits before rendering and before sending.

| Message field | Meaning |
| --- | --- |
| `to`, `subject`, `text` | Required primary recipient, subject and plain body |
| `from` | Optional sender for a Domain mailbox; otherwise the installation sender |
| `reply_to`, `cc`, `bcc` | Reply address and additional recipients; Bcc appears only in the envelope |
| `html` | Optional HTML part in `multipart/alternative`; the caller uses its own escaped template (`sanitise_html` is for received HTML, not for this) |
| `attachments` | `Attachment { filename, content_type, bytes }`; attachments wrap the body in `multipart/mixed` |
| `message_id`, `in_reply_to`, `references` | A chosen `<local@domain>` ID, one parent ID, and up to 50 ancestor IDs |
| `auto_submitted` | For example, `auto-replied` |
| `list_unsubscribe` | `ListUnsubscribe { urls, one_click }`; one-click adds `List-Unsubscribe-Post: List-Unsubscribe=One-Click` |
| `extra_headers` | Up to ten distinct `X-` headers; names contain only ASCII letters, digits and hyphens |

Read each attachment through the owning service's authorised `download_attachment` or `Artifacts::read`, and pass its **detected** MIME type. Mail receives bytes and never depends on Media or Artifacts.

## Delivery reports

A relay that takes a message says nothing later, and Base reads no mailbox, so a bounce reaches Bilibi when an Extension adapting a mail provider (a webhook, a feed) reports it, or when a module that fetches mail finds a `Parsed.report` (see Receiving a message). `base/mail.reports` (`DeliveryReports`) is that one door:

- An owner tags a message with `X-Bilibi-Reference: <kind>:<its record>` (`reference(kind, record)`, `REFERENCE_HEADER`) and contributes an `Observer { kind, observe }` under `OBSERVERS`.
- The adapter reads the header back from the provider's event and calls `report(&DeliveryReport { reference, address, outcome: Outcome::Bounced, detail })`. It names no owner and the owner names no provider.
- `report` hands it to the observer of that kind, which re-reads its own record (and its tenant) and answers whether it still held it. `Ok(false)` means an unknown kind or record, an old or foreign message; the adapter ignores it. A malformed report is `Error::Invalid`.
- `apps/web/examples/delivery-walkthrough.rs` stands in for an adapter against a disposable database. Core Portal is the first observer (`portal.invitation`): it marks the open invitation `bounced` and tells the inviter.

## Receiving a message

**`parse(&bytes, &limits)` reads one received RFC 5322 message into its parts.** It is pure (no IO) and best effort: a missing or broken header is `None` or left out, not an error. Fetching mail (IMAP, provider APIs, webhooks) is an Extension behind a Domain port; storing the message and its attachments is the receiving module's.

```rust,ignore
use bilibi_mail::{parse, sanitise_html};

let parsed = parse(&raw, &mailer.parse_limits().await?)?;
if parsed.auto_submitted { /* never answer it */ }
let safe_html = parsed.html.as_deref().map(sanitise_html); // store this, never parsed.html
```

| `Parsed` field | Meaning |
| --- | --- |
| `message_id`, `in_reply_to`, `references` | `<local@domain>` ids, the form `render` writes, so a reply threads by exact match; up to 50 references, oldest first; malformed ids are dropped |
| `from`, `sender`, `reply_to`, `to`, `cc` | `Address` values as the sender wrote them (RFC 2047 names decoded), not validated; `render` validates any address a reply sends to |
| `subject`, `date` | Decoded subject; `Date` as a UTC `jiff::Timestamp` |
| `auto_submitted` | `Auto-Submitted` other than `no`, `Precedence: bulk / junk / auto_reply`, `X-Autoreply` or `X-Autorespond`: never answer an auto-reply with another |
| `text`, `html` | The message's own plain and HTML bodies, converted to UTF-8 from their declared charset, with `\n` line ends. Base never derives `text` from HTML |
| `attachments` | `Part { filename, declared_type, content_id, disposition (Attachment / Inline), bytes }`: every non-body part, inline images and `text/calendar` included. `declared_type` is the sender's claim; detect the real type before storing it through its owner |
| `report` | A bounce or complaint, below |

**Limits.** `ParseLimits { max_bytes, max_parts, max_depth }`: bytes from `mail.max_message_bytes` (`Mailer::parse_limits`, or `ParseLimits::from(limits)`), 200 parts, nesting depth 20. A caller may lower any value; parts and depth never go above the defaults. An oversize message is refused before it is parsed; record its headers only with `parse(header_block(&raw), &limits)`. Input that is not a message (no standard header) or exceeds a limit is `Error::Invalid`. Parsing never panics.

**Round trip.** Parsing what `render` wrote returns the same addresses, subject, ids, bodies and attachment bytes.

### Bounces and complaints

`Parsed.report` is set for a `multipart/report`:

| Report | From | Use |
| --- | --- | --- |
| `Report::Delivery { original_message_id, recipients }` | `report-type=delivery-status` (RFC 3464) | Each `DeliveryRecipient { address, action (Failed / Delayed / Delivered), status, permanent }`; stop mailing a `permanent` address (a failure whose status is not a temporary `4.x.x`) |
| `Report::Complaint { original_message_id, recipient, feedback_type }` | `report-type=feedback-report` (RFC 5965) | The recipient marked the message as spam or abuse; stop mailing them |

`original_message_id` comes from the returned message or its headers (falling back to a valid `Original-Envelope-Id`), so the owner finds what it sent by the id `render` returned. A report found here and one an Extension sends to `DeliveryReports` lead to the same owner action.

## Received HTML

**`sanitise_html(&html)` is the one allow-list for HTML written by a stranger.** Apply it before storing; never store or show `Parsed.html` as received.

- Kept: text structure, lists, tables, links and images.
- Removed: `script`, `style`, `iframe`, `object`, `embed`, `form`, comments and every `on*` attribute.
- Links: only `http`, `https` and `mailto`, with `rel="noopener noreferrer"`.
- Images: only `cid:` (an inline part) and `data:image/*`. Remote images are removed, so opening a message never fetches anything or fires a tracking pixel.
- Inline `style`: only a fixed list of layout and text properties, never `url(...)` or an escape.

**Show it with `bilibi_ui::MailFrame`**, never `Html::raw`. The frame is an `<iframe sandbox srcdoc>` that runs no script and fetches nothing from elsewhere; a clicked link opens a new tab. To show inline images, rewrite the stored HTML with `inline_images(&html, images)`, passing `InlineImage { content_id, content_type, bytes }` with the type detected when the part was stored. A `srcdoc` frame takes the page's CSP, which allows no inline styles, so kept `style` attributes are stored but not applied in the frame.

## Rendering for another transport

Render once when a Domain must pass the message through an Extension's vendor API:

```rust,ignore
let rendered = mailer.render(&message).await?;
// Pass rendered.bytes and rendered.envelope through the Domain's port.
// Or send this same rendering through the installation transport:
let sent = mailer.deliver_rendered(rendered).await?;
```

`Rendered` contains `bytes`, `message_id` and `Envelope { from, recipients }`. The envelope includes To, Cc and Bcc addresses. Wire headers omit Bcc. Attachments use base64 so decoding restores the original bytes. `deliver_rendered` takes ownership and returns those bytes without rebuilding MIME or changing the ID or Date. Pass trusted renderer output, not user-supplied raw email.

The free `render(&message, &default_from, limits)` function performs no IO. Its explicit `Limits` snapshot keeps settings access outside rendering; obtain current installation values with `mailer.limits().await?`. It generates an ID on the sender's domain when none is supplied. IDs use the RFC 5322 dot-atom form; comments and quoted IDs are refused.

A Domain can build `Smtp::new(SmtpConfig)` from its own encrypted settings and call `Transport::send_raw(&envelope, &bytes)`. An Extension can implement that same trait without linking `lettre`. Standalone `Mailer::new(sender, transport, limits)` uses the supplied limits; `from_env`/`from_lookup` use defaults until the registry binds installation settings.

## Validation and limits

**Rendering is the single header validation boundary.** It rechecks every address, including public struct fields. Controls in names, subjects, filenames or header values are refused. Extra headers cannot replace From, To, Date, MIME-Version, Content-* or other reserved headers.

| Limit | Default or rule |
| --- | --- |
| `mail.max_message_bytes` | 25 MiB, including headers and encoded attachments |
| `mail.max_recipients` | 50, counting To, Cc and Bcc together |
| Subject / display name | 200 bytes each |
| Extra header value / name | 998 / 78 bytes |
| Attachment filename / MIME type | 255 / 200 bytes |
| Message-ID | 254 bytes, including angle brackets |

The two installation settings appear under **Settings → Mail** for the server owner. Oversize input is refused before MIME allocation where possible, and the final encoded size is checked before sending. Settings read failures refuse delivery.

## Choosing the transport

`Mailer::from_env` chooses once at boot:

| Environment | Transport |
| --- | --- |
| `MAIL_HOST` unset | Development mailbox in `BILIBI_MAIL_DIR` (default `.local/mailbox`) |
| `MAIL_HOST` set | SMTP relay with verified TLS |

SMTP also requires `MAIL_PORT`, `MAIL_USERNAME`, `MAIL_PASSWORD`, `MAIL_TLS_MODE` (`starttls` or `implicit_tls`), `MAIL_FROM_NAME` and `MAIL_FROM_ADDRESS`. A half-configured relay refuses boot and names the missing variable. It never falls back to files. The development sender defaults to `Bilibi <noreply@bilibi.localhost>`. The boot log names the transport without credentials.

`deliver` returns `Error::Delivery(detail)` for transport failures. The detail is for logs. Callers show a generic error. A caller that hides whether an address exists sends after answering and undoes work that depended on delivery when it fails; `core/user`'s password-reset request is the reference.

## Development mailbox

`mailer.dev_mailbox()` returns `Some(&Mailbox)` only for the active development transport, and `None` for SMTP. **A development-only tool must also require `Instance::resolve(...).role` to be development** (`base/system`): a production installation without `MAIL_HOST` still uses files.

Open the newest `.eml` file in a mail client to see its plain and HTML parts and attachments. Simple plain-text messages retain their decoded (`8bit`) readable copy so reset links can be copied directly. Multipart messages keep their MIME encoding intact. Returned `Sent.bytes` always remain the wire form.

Mail creates the directory as `0700` and does not change an existing one; files are `0600`, and only the newest 100 messages are kept. `Mailbox::entries` lists messages newest first; `Mailbox::read` reads one accepted filename. The `inbound/` subfolder is excluded from listing and pruning. A module that offers inbound development tools owns reading that folder.

## Walkthrough

Use a private disposable output directory, without `MAIL_HOST`, to send text, HTML, two attachments and the supported threading headers:

```sh
BILIBI_MAIL_DIR=/tmp/bilibi-mail-walkthrough \
  cargo run -p bilibi-mail --locked --example mail-walkthrough -- /tmp/bilibi-mail-wire.eml
```

The walkthrough prints returned wire bytes, the complete envelope, and validation refusals. Open the stored `.eml` file in a mail client. With a configured local TLS SMTP sink, append `--also-smtp` to send the very same `Rendered` to both transports and confirm `dev_mailbox()` is `None` for SMTP. Compare the saved wire file with the sink’s raw DATA after SMTP dot unstuffing, without added receipt headers. A local certificate must be trusted by the TLS verifier.

Parse received mail from files (the committed fixtures in `examples/inbound/`, and a round-trip message plus refused inputs written to the output directory):

```sh
cargo run -p bilibi-mail --locked --example inbound-walkthrough -- /tmp/bilibi-inbound-walkthrough
```

It prints what each message parsed to (a `render` round trip, attachments with an inline `cid:` image, a Latin-1 message with encoded headers, an auto-reply, a hard and a soft bounce, a complaint, a malformed message), the refusals of oversize, too-deep and too-many-parts input, and `sanitise_html` on hostile HTML with a check per rule.

## Upgrade notes for agents

- Replace `Message { to, subject, text }` with `Message::text(to, subject, text)` and set optional fields afterward.
- Replace `Message::render(&from)` with `render(&message, &from, limits)`, or use the configured `mailer.render(&message).await`.
- Replace `Transport::send(&lettre::Message)` with `send_raw(&Envelope, &[u8])`. SMTP envelope addresses are now public Mail types.
- `Mailer::deliver` returns `Sent`; map success to `()` when your facade still returns no value. Archive `Sent.bytes` when required.
- Pass a resolved `Limits` snapshot as the third argument to standalone `Mailer::new`.

## Where to go next

See [Settings](../settings/README.md) for installation limits and [Instance identity](../system/README.md) for development-only tools.
