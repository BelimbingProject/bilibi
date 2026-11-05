//! The budget check that runs before mail-parser builds anything. mail-parser
//! allocates a header (and, for address and list fields, one entry per item)
//! for every few bytes of header text, and a part for every boundary, so a
//! small message could cost hundreds of MiB. This walk finds every place a
//! header block can start, as mail-parser does, charges each block's bytes to
//! the header budget and counts the parts, and refuses before any allocation
//! that grows with the input. `parse::within` still checks the built tree.
use crate::{Error, Result};
use mail_parser::{MessageParser, MimeHeaders};

/// Distinct MIME boundaries searched; each costs one pass over the input.
const MAX_BOUNDARIES: usize = 32;

/// Refuses `bytes` when its header blocks together exceed `max_header_bytes`
/// or it has more than twice `max_parts` part starts (a part start may also be
/// a closing delimiter; `within` then applies the exact part and depth limits
/// to the parsed tree). A `message/*` part in base64 or quoted-printable is
/// refused too: RFC 2046 forbids it, and mail-parser would decode and parse it
/// out of sight of this walk.
pub(crate) fn check(bytes: &[u8], max_header_bytes: usize, max_parts: usize) -> Result<()> {
    let mut walk = Walk {
        bytes,
        header_budget: max_header_bytes,
        part_budget: max_parts.saturating_mul(2).saturating_add(1),
        boundaries: Vec::new(),
        digest: false,
    };
    walk.block(0)?;
    // Every `--<boundary>` of a boundary declared so far may start a part:
    // mail-parser matches it anywhere, not only at a line start.
    let mut from = 0;
    while let Some((at, len)) = walk.next_delimiter(from) {
        walk.block(skip_crlf(bytes, at + len))?;
        from = at + 1;
    }
    Ok(())
}

struct Walk<'a> {
    bytes: &'a [u8],
    header_budget: usize,
    part_budget: usize,
    /// Each boundary as `--<boundary>`, with its first occurrence at or
    /// after the last search (`usize::MAX`: none), or `None` before one.
    boundaries: Vec<(Vec<u8>, Option<usize>)>,
    /// A `multipart/digest` was seen: a part without a type may be a message.
    digest: bool,
}

impl Walk<'_> {
    /// One header block at `start`, then the nested message after it while
    /// the block is a `message/*` part.
    fn block(&mut self, mut start: usize) -> Result<()> {
        loop {
            self.part_budget = self.part_budget.checked_sub(1).ok_or(Error::Invalid)?;
            let end = header_end(self.bytes, start, self.header_budget)?;
            self.header_budget -= end - start;
            // The block is within budget, so reading it is bounded.
            let Some(headers) = MessageParser::default().parse_headers(&self.bytes[start..end])
            else {
                return Ok(());
            };
            let Some(part) = headers.parts.first() else {
                return Ok(());
            };
            let content_type = part.content_type();
            let is = |kind: &str| {
                content_type.is_some_and(|value| value.ctype().eq_ignore_ascii_case(kind))
            };
            let subtype = content_type
                .and_then(|value| value.subtype())
                .map(str::to_ascii_lowercase);
            if is("multipart") {
                self.digest |= subtype.as_deref() == Some("digest");
                let boundary = content_type
                    .and_then(|value| value.attribute("boundary"))
                    .filter(|boundary| !boundary.is_empty());
                if let Some(boundary) = boundary {
                    let needle = [b"--", boundary.as_bytes()].concat();
                    if !self.boundaries.iter().any(|(known, _)| *known == needle) {
                        if self.boundaries.len() == MAX_BOUNDARIES {
                            return Err(Error::Invalid);
                        }
                        self.boundaries.push((needle, None));
                    }
                }
            }
            let message = (is("message")
                && matches!(subtype.as_deref(), Some("rfc822" | "global")))
                || (content_type.is_none() && self.digest);
            if !message {
                return Ok(());
            }
            let encoded = part.content_transfer_encoding().is_some_and(|value| {
                value.eq_ignore_ascii_case("base64")
                    || value.eq_ignore_ascii_case("quoted-printable")
            });
            if encoded {
                return Err(Error::Invalid);
            }
            start = end;
        }
    }

    /// The first `--<boundary>` at or after `from`, of any boundary declared,
    /// as its offset and length. A boundary's search resumes only after its
    /// last match is passed, so the input is read at most once per boundary.
    fn next_delimiter(&mut self, from: usize) -> Option<(usize, usize)> {
        let bytes = self.bytes;
        let mut first: Option<(usize, usize)> = None;
        for (needle, found) in &mut self.boundaries {
            let at = match *found {
                Some(at) if at >= from => at,
                _ => {
                    let at = find(bytes, needle, from).unwrap_or(usize::MAX);
                    *found = Some(at);
                    at
                }
            };
            if at != usize::MAX && first.is_none_or(|(first, _)| at < first) {
                first = Some((at, needle.len()));
            }
        }
        first
    }
}

fn find(bytes: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    memchr::memmem::find(bytes.get(from..)?, needle).map(|at| from + at)
}

/// After a delimiter mail-parser skips spaces, tabs and CR, then one LF.
fn skip_crlf(bytes: &[u8], mut at: usize) -> usize {
    while matches!(bytes.get(at), Some(b'\r' | b' ' | b'\t')) {
        at += 1;
    }
    if bytes.get(at) == Some(&b'\n') {
        at += 1;
    }
    at
}

/// Where mail-parser's header loop stops for a block at `start`: after the
/// line holding only whitespace that ends it, or at the end of the input. A
/// field runs to a newline not followed by a space or tab; a line without a
/// field name and colon is skipped. Refuses a block longer than `budget`.
fn header_end(bytes: &[u8], start: usize, budget: usize) -> Result<usize> {
    let cap = start
        .saturating_add(budget)
        .saturating_add(1)
        .min(bytes.len());
    let over = || {
        if cap < bytes.len() {
            Err(Error::Invalid)
        } else {
            Ok(bytes.len())
        }
    };
    let mut at = start;
    loop {
        while at < cap && bytes[at] != b'\n' && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= cap {
            return over();
        }
        if bytes[at] == b'\n' {
            return Ok(at + 1);
        }
        let mut token = false;
        let mut field = false;
        while at < cap {
            let byte = bytes[at];
            at += 1;
            match byte {
                b':' if token => {
                    field = true;
                    break;
                }
                b'\n' => break,
                byte if !byte.is_ascii_whitespace() => token = true,
                _ => {}
            }
        }
        if field {
            while at < cap {
                let byte = bytes[at];
                at += 1;
                if byte == b'\n' {
                    if matches!(bytes.get(at), Some(b' ' | b'\t')) {
                        at += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        if at >= cap {
            return over();
        }
    }
}
