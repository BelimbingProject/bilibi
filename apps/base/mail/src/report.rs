//! Bounces (RFC 3464 delivery status notifications) and complaints (RFC 5965
//! feedback reports) found inside a received `multipart/report`.
use crate::parse::{declared_type, id};
use mail_parser::{Message, MessageParser, MimeHeaders, PartType};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Report {
    /// A bounce or delay: mark what was sent, and stop mailing a `permanent`
    /// recipient.
    Delivery {
        /// The `<local@domain>` id of the message this report is about.
        original_message_id: Option<String>,
        recipients: Vec<DeliveryRecipient>,
    },
    /// A recipient marked the message as spam or abuse: stop mailing them.
    Complaint {
        original_message_id: Option<String>,
        recipient: Option<String>,
        /// `abuse`, `fraud`, `virus`, `not-spam` or `other` (RFC 5965).
        feedback_type: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryRecipient {
    pub address: String,
    pub action: Action,
    /// The enhanced status code, for example `5.1.1`.
    pub status: Option<String>,
    /// The address will not accept mail: a failure whose status is not a
    /// temporary `4.x.x`.
    pub permanent: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Failed,
    Delayed,
    /// Delivered, relayed or expanded.
    Delivered,
}

pub(crate) fn read(message: &Message<'_>) -> Option<Report> {
    let root = message.parts.first()?;
    let content_type = root.content_type()?;
    if !content_type.ctype().eq_ignore_ascii_case("multipart")
        || !content_type
            .subtype()
            .is_some_and(|subtype| subtype.eq_ignore_ascii_case("report"))
    {
        return None;
    }
    let kind = content_type.attribute("report-type")?.to_ascii_lowercase();
    let PartType::Multipart(children) = &root.body else {
        return None;
    };
    let parts: Vec<_> = children
        .iter()
        .filter_map(|id| message.parts.get(*id as usize))
        .collect();
    let section = |types: &[&str]| {
        parts
            .iter()
            .find(|part| types.contains(&declared_type(part).as_str()))
            .map(|part| part.contents())
    };
    let original = parts.iter().find_map(|part| match &part.body {
        PartType::Message(nested) => Some(Original::Message(nested)),
        _ => matches!(
            declared_type(part).as_str(),
            "text/rfc822-headers" | "message/rfc822-headers" | "message/global-headers"
        )
        .then(|| Original::Headers(part.contents())),
    });
    let original_message_id = original.as_ref().and_then(Original::message_id);
    match kind.as_str() {
        "delivery-status" => {
            let groups = groups(section(&[
                "message/delivery-status",
                "message/global-delivery-status",
            ])?);
            let envelope_id = groups
                .first()
                .and_then(|fields| field(fields, "original-envelope-id"))
                .and_then(id);
            let recipients = groups
                .iter()
                .skip(1)
                .filter_map(|fields| recipient(fields))
                .collect();
            Some(Report::Delivery {
                original_message_id: original_message_id.or(envelope_id),
                recipients,
            })
        }
        "feedback-report" => {
            let fields = groups(section(&["message/feedback-report"])?)
                .into_iter()
                .next()
                .unwrap_or_default();
            let recipient = field(&fields, "original-rcpt-to")
                .map(address)
                .or_else(|| original.as_ref().and_then(Original::first_to));
            Some(Report::Complaint {
                original_message_id,
                recipient,
                feedback_type: field(&fields, "feedback-type")
                    .map(|value| value.to_ascii_lowercase())
                    .unwrap_or_else(|| "other".into()),
            })
        }
        _ => None,
    }
}

/// The returned copy of the reported message: whole, or its headers only.
enum Original<'a, 'x> {
    Message(&'a Message<'x>),
    Headers(&'a [u8]),
}
impl Original<'_, '_> {
    fn message_id(&self) -> Option<String> {
        match self {
            Original::Message(message) => message.message_id().and_then(id),
            Original::Headers(bytes) => MessageParser::default()
                .parse_headers(*bytes)?
                .message_id()
                .and_then(id),
        }
    }
    fn first_to(&self) -> Option<String> {
        let first = |message: &Message<'_>| {
            message
                .to()?
                .first()?
                .address()
                .map(|value| value.trim().to_owned())
        };
        match self {
            Original::Message(message) => first(message),
            Original::Headers(bytes) => first(&MessageParser::default().parse_headers(*bytes)?),
        }
    }
}

/// Field groups separated by blank lines, folded lines joined; names lower case.
fn groups(bytes: &[u8]) -> Vec<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(bytes);
    let mut groups: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for line in text.lines() {
        let Some(fields) = groups.last_mut() else {
            continue;
        };
        if line.trim().is_empty() {
            if !fields.is_empty() {
                groups.push(Vec::new());
            }
        } else if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            fields.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    groups.retain(|fields| !fields.is_empty());
    groups
}
fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}
/// `rfc822; user@example.test` or `<user@example.test>` -> `user@example.test`.
fn address(value: &str) -> String {
    let value = value.split_once(';').map_or(value, |(_, address)| address);
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_owned()
}
fn recipient(fields: &[(String, String)]) -> Option<DeliveryRecipient> {
    let address = field(fields, "final-recipient")
        .or_else(|| field(fields, "original-recipient"))
        .map(address)
        .filter(|address| !address.is_empty())?;
    let action = match field(fields, "action")?.to_ascii_lowercase().as_str() {
        "failed" => Action::Failed,
        "delayed" => Action::Delayed,
        "delivered" | "relayed" | "expanded" => Action::Delivered,
        _ => return None,
    };
    let status = field(fields, "status")
        .and_then(|value| value.split_whitespace().next())
        .map(str::to_owned);
    // A failure is permanent unless its status says it is temporary (4.x.x).
    let permanent = action == Action::Failed
        && !status
            .as_deref()
            .is_some_and(|status| status.starts_with('4'));
    Some(DeliveryRecipient {
        address,
        action,
        status,
        permanent,
    })
}
