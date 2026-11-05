//! Manual inbound mail walkthrough: parses received messages from files and
//! prints what it found. Uses only example.test addresses.
//!
//! cargo run -p bilibi-mail --locked --example inbound-walkthrough -- <output-dir>
//!
//! The committed fixtures are in `examples/inbound/`. The round-trip message
//! and the oversize, too-deep and too-many-parts inputs are written to
//! `<output-dir>` and read back from there.
use bilibi_mail::{
    header_block, inline_images, parse, render, sanitise_html, Address, Attachment, InlineImage,
    Limits, Message, ParseLimits, Parsed, Report,
};
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: inbound-walkthrough <output-dir>")?,
    );
    std::fs::create_dir_all(&out)?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/inbound");
    let limits = ParseLimits::default();

    // 1. Round trip: what `render` wrote parses back to the same fields.
    let message = sample();
    let rendered = render(
        &message,
        &Address::new(Some("Support"), "support@example.test")?,
        Limits::default(),
    )?;
    let path = out.join("round-trip.eml");
    std::fs::write(&path, &rendered.bytes)?;
    let parsed = parse(&std::fs::read(&path)?, &limits)?;
    show("round-trip.eml", &parsed);
    let check = |label: &str, same: bool| println!("  round trip {label}: {same}");
    check(
        "from",
        parsed
            .from
            .first()
            .map(|a| (a.name.as_deref(), a.email.as_str()))
            == Some((Some("Support"), "support@example.test")),
    );
    check("to", emails(&parsed.to) == ["customer@example.test"]);
    check("cc", emails(&parsed.cc) == ["copy@example.test"]);
    check(
        "reply-to",
        emails(&parsed.reply_to) == ["replies@example.test"],
    );
    check(
        "subject",
        parsed.subject.as_deref() == Some(message.subject.as_str()),
    );
    check(
        "message id",
        parsed.message_id.as_deref() == Some(rendered.message_id.as_str()),
    );
    check("in-reply-to", parsed.in_reply_to == message.in_reply_to);
    check("references", parsed.references == message.references);
    check(
        "text",
        parsed.text.as_deref().map(str::trim_end) == Some(message.text.trim_end()),
    );
    check(
        "html",
        parsed.html.as_deref().map(str::trim_end) == message.html.as_deref(),
    );
    check(
        "attachments",
        parsed.attachments.len() == message.attachments.len()
            && parsed
                .attachments
                .iter()
                .zip(&message.attachments)
                .all(|(got, sent)| {
                    got.bytes == sent.bytes && got.filename.as_deref() == Some(&sent.filename)
                }),
    );
    check("auto-submitted", parsed.auto_submitted);

    // 2. Committed fixtures.
    for name in [
        "multipart-attachments.eml",
        "latin1-encoded.eml",
        "auto-reply.eml",
        "bounce-hard.eml",
        "bounce-soft.eml",
        "complaint.eml",
        "malformed.eml",
    ] {
        match parse(&std::fs::read(fixtures.join(name))?, &limits) {
            Ok(parsed) => {
                assert_report_auto_submitted(name, &parsed);
                show(name, &parsed);
            }
            Err(error) => println!("\n== {name}\n  refused: {error}"),
        }
    }

    // 3. Inputs that are refused.
    println!();
    let refuse = |name: &str, bytes: &[u8], limits: &ParseLimits| -> std::io::Result<()> {
        let path = out.join(name);
        std::fs::write(&path, bytes)?;
        let result = parse(&std::fs::read(&path)?, limits);
        println!(
            "{name}: {}",
            match result {
                Ok(_) => "parsed (expected a refusal)".to_owned(),
                Err(error) => format!("refused ({error})"),
            }
        );
        Ok(())
    };
    let small = ParseLimits {
        max_bytes: 4096,
        ..limits
    };
    let oversize = oversize();
    refuse("oversize.eml", &oversize, &small)?;
    let headers = parse(header_block(&oversize), &small)?;
    println!(
        "  oversize headers only: subject {:?}, from {:?}",
        headers.subject,
        emails(&headers.from)
    );
    refuse("too-deep.eml", &nested(25), &limits)?;
    refuse("too-many-parts.eml", &many(250), &limits)?;
    // Header floods are refused before mail-parser allocates per header.
    refuse("many-headers.eml", &many_headers(false), &limits)?;
    refuse("many-part-headers.eml", &many_headers(true), &limits)?;
    refuse("many-addresses.eml", &many_addresses(), &limits)?;
    refuse("encoded-message.eml", &encoded_message(), &limits)?;
    refuse("not-a-message.bin", &[0xff, 0x00, 0xfe, 0x01], &limits)?;
    refuse("empty.eml", b"", &limits)?;
    println!(
        "within limits: nested(18) parsed = {}, many(150) parsed = {}",
        parse(&nested(18), &limits).is_ok(),
        parse(&many(150), &limits).is_ok()
    );
    let flooded = parse(&flooded_report(), &limits)?;
    println!(
        "flooded delivery status (1 MiB of fields) read as: {:?}",
        flooded.report.map(|report| match report {
            bilibi_mail::Report::Delivery { recipients, .. } => recipients.len(),
            _ => 0,
        })
    );

    // 4. Received HTML reduced to the allow-list.
    let hostile = std::fs::read_to_string(fixtures.join("hostile.html"))?;
    let clean = sanitise_html(&hostile);
    println!("\n== sanitise_html(hostile.html)\n{clean}\n");
    for (label, gone) in [
        (
            "script removed",
            !clean.contains("<script") && !clean.contains("alert"),
        ),
        (
            "on* handlers removed",
            !clean.contains("onload") && !clean.contains("onerror"),
        ),
        ("javascript: link removed", !clean.contains("javascript:")),
        (
            "remote images removed",
            !clean.contains("tracker.example.test"),
        ),
        ("<style> removed", !clean.contains("<style")),
        (
            "iframe/form/object/embed removed",
            !["<iframe", "<form", "<object", "<embed", "<input"]
                .iter()
                .any(|tag| clean.contains(tag)),
        ),
        (
            "tables kept",
            clean.contains("<table") && clean.contains("<td"),
        ),
        (
            "cid: image kept",
            clean.contains("src=\"cid:logo@example.test\""),
        ),
        (
            "allowed inline style kept",
            clean.contains("color:#b91c1c") || clean.contains("color: #b91c1c"),
        ),
        ("disallowed style dropped", !clean.contains("position")),
        (
            "https link kept with rel",
            clean.contains("href=\"https://pay.example.test/invoice/7\"")
                && clean.contains("noopener noreferrer"),
        ),
        (
            "mailto link kept",
            clean.contains("mailto:billing@example.test"),
        ),
    ] {
        println!("  {label}: {gone}");
    }

    // 5. Inline images for the reading view: cid: becomes a data: URI.
    let received = parse(
        &std::fs::read(fixtures.join("multipart-attachments.eml"))?,
        &limits,
    )?;
    let shown = inline_images(
        &sanitise_html(received.html.as_deref().unwrap_or("")),
        received.attachments.iter().filter_map(|part| {
            Some(InlineImage {
                content_id: part.content_id.as_deref()?,
                content_type: &part.declared_type,
                bytes: &part.bytes,
            })
        }),
        limits.max_bytes,
    )?;
    println!("\n== inline_images(multipart-attachments.eml)\n{shown}");
    println!(
        "  cid: replaced by data:image/png: {}",
        shown.contains("src=\"data:image/png;base64,") && !shown.contains("cid:")
    );
    let repeated = "<img src=\"cid:logo@example.test\">".repeat(128);
    let image = InlineImage {
        content_id: "logo@example.test",
        content_type: "image/png",
        bytes: &[0; 1024],
    };
    assert!(inline_images(&repeated, [image], repeated.len() * 2).is_err());
    println!("  repeated cid expansion refused before allocation: true");
    Ok(())
}

fn assert_report_auto_submitted(name: &str, parsed: &Parsed) {
    let recognized = match name {
        "bounce-hard.eml" | "bounce-soft.eml" => {
            matches!(parsed.report, Some(Report::Delivery { .. }))
        }
        "complaint.eml" => matches!(parsed.report, Some(Report::Complaint { .. })),
        _ => return,
    };
    assert!(recognized, "{name} must be recognized as a report");
    assert!(
        parsed.auto_submitted,
        "a recognized report must never be answered: {name}"
    );
}

fn sample() -> Message {
    let mut message = Message::text(
        Address::new(Some("Customer"), "customer@example.test").expect("valid"),
        "Your case CASE-7 · update",
        "We have shipped a replacement.\nhttps://example.test/cases/7\n",
    );
    message.from = Some(Address::new(Some("Support"), "support@example.test").expect("valid"));
    message.reply_to = Some(Address::new(None, "replies@example.test").expect("valid"));
    message
        .cc
        .push(Address::new(None, "copy@example.test").expect("valid"));
    message.html = Some("<p>We have shipped a <strong>replacement</strong>.</p>".into());
    message.attachments = vec![
        Attachment {
            filename: "label.pdf".into(),
            content_type: "application/pdf".into(),
            bytes: b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n%%EOF\n".to_vec(),
        },
        Attachment {
            filename: "notes.txt".into(),
            content_type: "text/plain".into(),
            bytes: b"lone LF\nand no final newline".to_vec(),
        },
    ];
    message.message_id = Some("<case-7-update@example.test>".into());
    message.in_reply_to = Some("<case-7-question@example.test>".into());
    message.references = vec![
        "<case-7@example.test>".into(),
        "<case-7-question@example.test>".into(),
    ];
    message.auto_submitted = Some("auto-replied".into());
    message
}

fn emails(addresses: &[Address]) -> Vec<&str> {
    addresses.iter().map(|a| a.email.as_str()).collect()
}

fn show(name: &str, parsed: &Parsed) {
    let first_line = |body: &Option<String>| {
        body.as_deref().map(|body| {
            format!(
                "{} chars, \"{}\"",
                body.chars().count(),
                body.lines().next().unwrap_or("")
            )
        })
    };
    println!("\n== {name}");
    println!("  message id: {:?}", parsed.message_id);
    println!(
        "  in-reply-to: {:?}; references: {:?}",
        parsed.in_reply_to, parsed.references
    );
    println!(
        "  from: {:?}; sender: {:?}; reply-to: {:?}",
        parsed
            .from
            .iter()
            .map(|a| format!("{:?} <{}>", a.name, a.email))
            .collect::<Vec<_>>(),
        parsed.sender.as_ref().map(|a| a.email.as_str()),
        emails(&parsed.reply_to)
    );
    println!(
        "  to: {:?}; cc: {:?}",
        emails(&parsed.to),
        emails(&parsed.cc)
    );
    println!(
        "  subject: {:?}; date: {:?}",
        parsed.subject,
        parsed.date.map(|d| d.to_string())
    );
    println!("  auto-submitted: {}", parsed.auto_submitted);
    println!("  text: {:?}", first_line(&parsed.text));
    println!("  html: {:?}", first_line(&parsed.html));
    for part in &parsed.attachments {
        println!(
            "  part: {:?} {} {:?} cid={:?} {} bytes",
            part.filename,
            part.declared_type,
            part.disposition,
            part.content_id,
            part.bytes.len()
        );
    }
    if let Some(report) = &parsed.report {
        println!("  report: {report:?}");
    }
}

fn oversize() -> Vec<u8> {
    let mut bytes = b"From: big@example.test\r\nTo: support@example.test\r\nSubject: Large scan\r\nMessage-ID: <big@example.test>\r\nContent-Type: text/plain\r\n\r\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 8192));
    bytes
}

fn nested(depth: usize) -> Vec<u8> {
    let mut bytes =
        b"From: deep@example.test\r\nTo: support@example.test\r\nSubject: Deep\r\n".to_vec();
    for level in 0..depth {
        bytes.extend(
            format!("Content-Type: multipart/mixed; boundary=\"b{level}\"\r\n\r\n--b{level}\r\n")
                .as_bytes(),
        );
    }
    bytes.extend(b"Content-Type: text/plain\r\n\r\ninnermost\r\n");
    for level in (0..depth).rev() {
        bytes.extend(format!("--b{level}--\r\n").as_bytes());
    }
    bytes
}

fn many(parts: usize) -> Vec<u8> {
    let mut bytes = b"From: many@example.test\r\nTo: support@example.test\r\nSubject: Many\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n".to_vec();
    for index in 0..parts {
        bytes.extend(format!("--m\r\nContent-Type: text/plain; name=\"p{index}.txt\"\r\nContent-Disposition: attachment\r\n\r\npart {index}\r\n").as_bytes());
    }
    bytes.extend(b"--m--\r\n");
    bytes
}

/// 250,000 one-letter headers (about 1.25 MiB), at the top or in a part.
fn many_headers(in_part: bool) -> Vec<u8> {
    let mut bytes =
        b"From: flood@example.test\r\nTo: support@example.test\r\nSubject: Flood\r\n".to_vec();
    if in_part {
        bytes.extend(b"Content-Type: multipart/mixed; boundary=\"f\"\r\n\r\n--f\r\n");
    }
    for _ in 0..250_000 {
        bytes.extend(b"a:\r\n");
    }
    bytes.extend(b"\r\nbody\r\n");
    if in_part {
        bytes.extend(b"--f--\r\n");
    }
    bytes
}

/// One To header listing 300,000 addresses.
fn many_addresses() -> Vec<u8> {
    let mut bytes = b"From: flood@example.test\r\nSubject: Flood\r\nTo: ".to_vec();
    for _ in 0..300_000 {
        bytes.extend(b"a@b,");
    }
    bytes.extend(b"\r\n\r\nbody\r\n");
    bytes
}

/// A forwarded message in base64, which RFC 2046 forbids for message/rfc822.
fn encoded_message() -> Vec<u8> {
    b"From: fwd@example.test\r\nTo: support@example.test\r\nSubject: Fwd\r\nContent-Type: multipart/mixed; boundary=\"e\"\r\n\r\n--e\r\nContent-Type: message/rfc822\r\nContent-Transfer-Encoding: base64\r\n\r\nRnJvbTogYUBleGFtcGxlLnRlc3QNCg0KaGk=\r\n--e--\r\n".to_vec()
}

/// A bounce whose delivery-status part holds 1 MiB of recipient fields: only
/// its first 64 KiB is read.
fn flooded_report() -> Vec<u8> {
    let mut bytes = b"From: mailer-daemon@example.test\r\nTo: support@example.test\r\nSubject: Bounce\r\nContent-Type: multipart/report; report-type=delivery-status; boundary=\"r\"\r\n\r\n--r\r\nContent-Type: message/delivery-status\r\n\r\nReporting-MTA: dns; example.test\r\n".to_vec();
    while bytes.len() < 1024 * 1024 {
        bytes.extend(b"\r\nFinal-Recipient: rfc822; x@example.test\r\nAction: failed\r\n");
    }
    bytes.extend(b"\r\n--r--\r\n");
    bytes
}
