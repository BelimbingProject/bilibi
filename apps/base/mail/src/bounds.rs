//! The allocation-light budget check that runs before mail-parser builds the
//! message tree. It mirrors mail-parser's header and MIME traversal closely
//! enough to refuse oversized structures before the dependency allocates one
//! `Header`, address, list entry or `MessagePart` for each attacker input item.
use crate::{Error, Result};
use mail_parser::{MessageParser, MimeHeaders};

/// Distinct MIME boundaries accepted; each can retain one bounded copy.
const MAX_BOUNDARIES: usize = 32;

/// Refuses input that exceeds any structural budget before the full parser
/// builds its tree. Header parsing below happens only after the allocation-free
/// scan has bounded the block's bytes, field count and longest unfolded field.
pub(crate) fn check(
    bytes: &[u8],
    max_header_bytes: usize,
    max_headers: usize,
    max_header_field_bytes: usize,
    max_parts: usize,
    max_depth: usize,
) -> Result<()> {
    let mut walk = Walk {
        bytes,
        header_budget: max_header_bytes,
        header_count_budget: max_headers,
        max_header_field_bytes,
        part_budget: max_parts,
        max_depth,
        boundaries: Vec::new(),
        known_boundaries: Vec::new(),
    };
    walk.block(0, 1, false)?;

    // mail-parser consumes only the innermost active boundary. A child is
    // pushed when its bounded header declares it and popped by its closing
    // delimiter, which gives every part its exact tree depth before parsing.
    let mut from = 0;
    while let Some(delimiter) = walk.next_delimiter(from) {
        from = delimiter.at.saturating_add(delimiter.len);
        if delimiter.closing {
            walk.boundaries.pop();
            from = from.saturating_add(2);
            continue;
        }
        walk.block(
            skip_crlf(bytes, delimiter.at + delimiter.len),
            delimiter.depth + 1,
            delimiter.digest,
        )?;
    }
    Ok(())
}

struct Boundary {
    /// mail-parser searches for two hyphens followed by this value.
    needle: Vec<u8>,
    /// Depth of the multipart container that declared the boundary.
    depth: usize,
    /// A child without Content-Type is a nested message in multipart/digest.
    digest: bool,
}

struct Delimiter {
    at: usize,
    len: usize,
    depth: usize,
    digest: bool,
    closing: bool,
}

struct Walk<'a> {
    bytes: &'a [u8],
    header_budget: usize,
    header_count_budget: usize,
    max_header_field_bytes: usize,
    part_budget: usize,
    max_depth: usize,
    /// Active multipart boundaries, outermost first.
    boundaries: Vec<Boundary>,
    /// Every distinct boundary retains a bounded copy for this preflight.
    known_boundaries: Vec<Vec<u8>>,
}

impl Walk<'_> {
    /// Reads one part's header block at `start`, then any unencoded nested
    /// message headers which follow it directly.
    fn block(&mut self, mut start: usize, mut depth: usize, mut digest: bool) -> Result<()> {
        loop {
            if depth > self.max_depth {
                return Err(Error::Invalid);
            }
            self.part_budget = self.part_budget.checked_sub(1).ok_or(Error::Invalid)?;
            let end = header_end(
                self.bytes,
                start,
                self.header_budget,
                &mut self.header_count_budget,
                self.max_header_field_bytes,
            )?;
            self.header_budget -= end - start;

            // The allocation-free scan has bounded this parse by all three
            // header budgets, so its per-header and per-value allocations are
            // bounded too.
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
                let boundary = content_type
                    .and_then(|value| value.attribute("boundary"))
                    .filter(|boundary| !boundary.is_empty());
                if let Some(boundary) = boundary {
                    self.push_boundary(
                        [b"--", boundary.as_bytes()].concat(),
                        depth,
                        subtype.as_deref() == Some("digest"),
                    )?;
                }
            }

            let message = (is("message")
                && matches!(subtype.as_deref(), Some("rfc822" | "global")))
                || (content_type.is_none() && digest);
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
            depth = depth.checked_add(1).ok_or(Error::Invalid)?;
            digest = false;
        }
    }

    fn push_boundary(&mut self, needle: Vec<u8>, depth: usize, digest: bool) -> Result<()> {
        // Reusing an active ancestor boundary makes the MIME tree ambiguous and
        // cannot be traversed with an exact allocation budget.
        if self.boundaries.iter().any(|known| known.needle == needle) {
            return Err(Error::Invalid);
        }
        if !self.known_boundaries.contains(&needle) {
            if self.known_boundaries.len() == MAX_BOUNDARIES {
                return Err(Error::Invalid);
            }
            self.known_boundaries.push(needle.clone());
        }
        self.boundaries.push(Boundary {
            needle,
            depth,
            digest,
        });
        Ok(())
    }

    /// Finds the next delimiter for the innermost active multipart, matching
    /// mail-parser's traversal. The dependency accepts a delimiter anywhere,
    /// not only at the start of a line, so this deliberately does too.
    fn next_delimiter(&self, from: usize) -> Option<Delimiter> {
        let boundary = self.boundaries.last()?;
        let at = find(self.bytes, &boundary.needle, from)?;
        let after = at + boundary.needle.len();
        Some(Delimiter {
            at,
            len: boundary.needle.len(),
            depth: boundary.depth,
            digest: boundary.digest,
            closing: self.bytes.get(after..after.saturating_add(2)) == Some(b"--"),
        })
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

/// Finds the end of the header block starting at `start`, while counting
/// fields and refusing a logical field whose unfolded size is too large.
/// This mirrors mail-parser's treatment of whitespace and continuation lines.
fn header_end(
    bytes: &[u8],
    start: usize,
    byte_budget: usize,
    field_budget: &mut usize,
    max_field_bytes: usize,
) -> Result<usize> {
    let cap = start.saturating_add(byte_budget).min(bytes.len());
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

        let field_start = at;
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
            *field_budget = field_budget.checked_sub(1).ok_or(Error::Invalid)?;
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
            let unfolded = bytes[field_start..at]
                .iter()
                .filter(|byte| !matches!(byte, b'\r' | b'\n'))
                .count();
            if unfolded > max_field_bytes {
                return Err(Error::Invalid);
            }
        }
        if at >= cap {
            return over();
        }
    }
}
