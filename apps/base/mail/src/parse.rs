//! Received mail: one RFC 5322 message read into its parts. No IO, best effort
//! on malformed input, never trusted. Only input that is not a message at all,
//! or that exceeds the limits, is refused.
use crate::{message::valid_id, report, Address, Error, Limits, Report, Result};
use mail_parser::{HeaderValue, Message, MessageParser, MimeHeaders, PartType};

/// Parts and nesting are bounded whatever the caller asks for.
const MAX_PARTS: usize = 200;
const MAX_DEPTH: usize = 20;
const MAX_REFERENCES: usize = 50;

/// What `parse` accepts. Build it from the installation limits
/// (`Mailer::parse_limits`, or `ParseLimits::from(limits)`); a caller may lower
/// any value, and parts and depth never go above 200 and 20.
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    pub max_bytes: usize,
    pub max_parts: usize,
    pub max_depth: usize,
}
impl From<Limits> for ParseLimits {
    fn from(limits: Limits) -> Self {
        Self {
            max_bytes: limits.max_message_bytes,
            max_parts: MAX_PARTS,
            max_depth: MAX_DEPTH,
        }
    }
}
impl Default for ParseLimits {
    fn default() -> Self {
        Limits::default().into()
    }
}

/// One received message. Addresses are as written by the sender, not checked:
/// `render` validates any address a reply sends to. Ids are `<local@domain>`,
/// the form `render` writes, so a reply threads by exact match.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    /// Up to 50 ancestor ids, oldest first.
    pub references: Vec<String>,
    pub from: Vec<Address>,
    pub sender: Option<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: Option<String>,
    /// The `Date` header as an instant (UTC).
    pub date: Option<jiff::Timestamp>,
    /// Sent automatically (an auto-reply, a list, a bulk mailing): never answer it.
    pub auto_submitted: bool,
    /// The message's own plain body; never derived from its HTML.
    pub text: Option<String>,
    /// The HTML body as received. Store or show it only after `sanitise_html`.
    pub html: Option<String>,
    pub attachments: Vec<Part>,
    /// A delivery status notification or a feedback (complaint) report.
    pub report: Option<Report>,
}

/// An attachment or inline part. `declared_type` is the sender's claim: detect
/// the real type before storing it through its owner.
#[derive(Clone, Debug)]
pub struct Part {
    pub filename: Option<String>,
    pub declared_type: String,
    /// Without angle brackets; an HTML body refers to it as `cid:<content_id>`.
    pub content_id: Option<String>,
    pub disposition: Disposition,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    Attachment,
    Inline,
}

/// The header block of `bytes` (up to and including its blank line), for a
/// caller that records only the headers of a message `parse` refused as too
/// large: `parse(header_block(&raw), &limits)`.
pub fn header_block(bytes: &[u8]) -> &[u8] {
    let end = [&b"\r\n\r\n"[..], b"\n\n"]
        .iter()
        .filter_map(|blank| {
            bytes
                .windows(blank.len())
                .position(|window| window == *blank)
                .map(|at| at + blank.len())
        })
        .min()
        .unwrap_or(bytes.len());
    &bytes[..end]
}

/// Reads one received RFC 5322 message. A missing or broken header is `None`;
/// `Error::Invalid` means the input is not a message or exceeds `limits`.
pub fn parse(bytes: &[u8], limits: &ParseLimits) -> Result<Parsed> {
    if bytes.is_empty() || bytes.len() > limits.max_bytes {
        return Err(Error::Invalid);
    }
    // The parser promises not to panic; a panic is still refused, never fatal.
    std::panic::catch_unwind(|| {
        let message = MessageParser::default()
            .parse(bytes)
            .ok_or(Error::Invalid)?;
        // At least one standard header (From, Date, Subject, Content-Type, ...);
        // bytes that only look like unknown fields are not a message.
        if message
            .parts
            .first()
            .is_none_or(|root| root.headers.iter().all(|header| header.name.is_other()))
        {
            return Err(Error::Invalid);
        }
        within(
            &message,
            limits.max_parts.min(MAX_PARTS),
            limits.max_depth.min(MAX_DEPTH),
        )?;
        Ok(read(&message))
    })
    .unwrap_or(Err(Error::Invalid))
}

/// Counts every part, nested messages included, and refuses a message with
/// too many parts or nested too deeply. The parser itself is iterative.
fn within(message: &Message<'_>, max_parts: usize, max_depth: usize) -> Result<()> {
    let mut stack = vec![(message, 0u32, 1usize)];
    let mut count = 0usize;
    while let Some((message, id, depth)) = stack.pop() {
        let Some(part) = message.parts.get(id as usize) else {
            continue;
        };
        count += 1;
        if count > max_parts || depth > max_depth {
            return Err(Error::Invalid);
        }
        match &part.body {
            PartType::Multipart(children) => {
                stack.extend(children.iter().map(|child| (message, *child, depth + 1)))
            }
            PartType::Message(nested) => stack.push((nested, 0, depth + 1)),
            _ => {}
        }
    }
    Ok(())
}

fn read(message: &Message<'_>) -> Parsed {
    let text =
        message
            .text_body
            .iter()
            .find_map(|id| match &message.parts.get(*id as usize)?.body {
                PartType::Text(text) => Some(lines(text)),
                _ => None,
            });
    let html =
        message
            .html_body
            .iter()
            .find_map(|id| match &message.parts.get(*id as usize)?.body {
                PartType::Html(html) => Some(lines(html)),
                _ => None,
            });
    let mut references = ids(message.references());
    let skip = references.len().saturating_sub(MAX_REFERENCES);
    references.drain(..skip);
    Parsed {
        message_id: message.message_id().and_then(id),
        in_reply_to: ids(message.in_reply_to()).pop(),
        references,
        from: addresses(message.from()),
        sender: addresses(message.sender()).into_iter().next(),
        reply_to: addresses(message.reply_to()),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        subject: message
            .subject()
            .map(str::trim)
            .filter(|subject| !subject.is_empty())
            .map(str::to_owned),
        date: message
            .date()
            .filter(|date| date.is_valid())
            .and_then(|date| jiff::Timestamp::from_second(date.to_timestamp()).ok()),
        auto_submitted: auto_submitted(message),
        text,
        html,
        attachments: message
            .attachments
            .iter()
            .filter_map(|id| message.parts.get(*id as usize))
            .map(|part| {
                let content_id = part
                    .content_id()
                    .map(|value| value.trim().trim_start_matches('<').trim_end_matches('>'))
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                let disposition = match part.content_disposition() {
                    Some(value) if value.is_attachment() => Disposition::Attachment,
                    Some(value) if value.is_inline() => Disposition::Inline,
                    _ if content_id.is_some() => Disposition::Inline,
                    _ => Disposition::Attachment,
                };
                Part {
                    filename: part
                        .attachment_name()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                    declared_type: declared_type(part),
                    content_id,
                    disposition,
                    bytes: part.contents().to_vec(),
                }
            })
            .collect(),
        report: report::read(message),
    }
}

/// Bodies travel with CRLF line ends; return them as `\n`, as they were written.
fn lines(body: &str) -> String {
    body.replace("\r\n", "\n")
}

/// `type/subtype` in lower case; a part without one is plain text (RFC 2045).
pub(crate) fn declared_type(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(value) => match value.subtype() {
            Some(subtype) => format!("{}/{}", value.ctype(), subtype),
            None => value.ctype().to_owned(),
        }
        .to_ascii_lowercase(),
        None => "text/plain".into(),
    }
}

/// A parsed id (the parser drops the brackets) back in the form `render` writes.
pub(crate) fn id(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('<').trim_end_matches('>');
    let id = format!("<{value}>");
    valid_id(&id).is_ok().then_some(id)
}
fn ids(value: &HeaderValue<'_>) -> Vec<String> {
    match value {
        HeaderValue::Text(value) => id(value).into_iter().collect(),
        HeaderValue::TextList(values) => values.iter().filter_map(|value| id(value)).collect(),
        _ => Vec::new(),
    }
}

fn addresses(value: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    value
        .map(|value| {
            value
                .iter()
                .filter_map(|addr| {
                    let email = addr.address()?.trim();
                    email.contains('@').then(|| Address {
                        name: addr
                            .name()
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned),
                        email: email.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// RFC 3834 `Auto-Submitted` other than `no`, and the common auto-reply markers.
fn auto_submitted(message: &Message<'_>) -> bool {
    let raw = |name: &str| {
        message
            .header_raw(name)
            .map(|value| value.trim().to_ascii_lowercase())
    };
    raw("Auto-Submitted").is_some_and(|value| {
        let token = value.split([';', '(', ' ']).next().unwrap_or("");
        !token.is_empty() && token != "no"
    }) || raw("Precedence").is_some_and(|value| {
        matches!(
            value.as_str(),
            "bulk" | "junk" | "auto_reply" | "auto-reply"
        )
    }) || raw("X-Autoreply").is_some()
        || raw("X-Autorespond").is_some()
}
