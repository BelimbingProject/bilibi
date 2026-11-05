//! The codes and inputs of `core/consent`: channels, sources, suppression
//! reasons, the check's answer, and the validation every door shares.
use bilibi_party_ids::PersonId;

/// Evidence is a reference or the wording shown, at most this long.
pub const EVIDENCE_MAX: usize = 2000;
/// A normalised address is at most this long.
pub const ADDRESS_MAX: usize = 254;
/// A check's caller correlation key is at most this long.
pub const KEY_MAX: usize = 200;
/// A lift reason is at most this long.
pub const LIFT_REASON_MAX: usize = 500;

macro_rules! codes {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $code:literal, $label:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn code(self) -> &'static str {
                match self { $(Self::$variant => $code),+ }
            }
            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
            pub fn parse(code: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.code() == code)
            }
        }
    };
}

codes!(
    /// The channel a decision or a check is about. `Any` is only for a
    /// decision: it applies to every channel without a decision of its own.
    Channel {
        Email => "email", "Email",
        Sms => "sms", "SMS",
        Messaging => "messaging", "Messaging",
        Voice => "voice", "Voice",
        Chat => "chat", "Chat",
        Any => "any", "Any channel",
    }
);
impl Channel {
    /// The channels a message is sent on, in display order.
    pub const SENDING: &'static [Self] = &[
        Self::Email,
        Self::Sms,
        Self::Messaging,
        Self::Voice,
        Self::Chat,
    ];
}

codes!(
    /// Who or what captured a decision.
    Source {
        Portal => "portal", "Portal",
        PreferenceLink => "preference_link", "Preference link",
        Agent => "agent", "Staff",
        Import => "import", "Import",
        InboundKeyword => "inbound_keyword", "Inbound keyword",
        Api => "api", "API",
    }
);

codes!(
    /// Where a decision was captured: a sending channel, the web or in person.
    Capture {
        Email => "email", "Email",
        Sms => "sms", "SMS",
        Messaging => "messaging", "Messaging",
        Voice => "voice", "Voice",
        Chat => "chat", "Chat",
        Web => "web", "Web",
        InPerson => "in_person", "In person",
    }
);

codes!(
    /// Why an address or a person is not contacted on a channel.
    Reason {
        HardBounce => "hard_bounce", "Hard bounce",
        Complaint => "complaint", "Complaint",
        Unsubscribe => "unsubscribe", "Unsubscribe",
    }
);
codes!(
    /// Why `check` refused.
    Refusal {
        NoConsent => "no_consent", "No consent",
        Withdrawn => "withdrawn", "Consent withdrawn",
        Suppressed => "suppressed", "Suppressed",
    }
);

/// What `check` and `preview` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    Refused(Refusal),
}
impl Answer {
    /// The log's `outcome`: `allowed` or the refusal's code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Refused(refusal) => refusal.code(),
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "allowed" => Some(Self::Allowed),
            other => Refusal::parse(other).map(Self::Refused),
        }
    }
}

/// What `core.consent.default_new_address` says about a contact with no
/// decision for the purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewAddress {
    OptedOut,
    OptedIn,
}
impl NewAddress {
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "opted_out" => Some(Self::OptedOut),
            "opted_in" => Some(Self::OptedIn),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::OptedOut => "opted out",
            Self::OptedIn => "opted in",
        }
    }
}

/// Who a decision, a suppression or a question is about: a Core person, an
/// address, or for a question both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    pub person: Option<PersonId>,
    pub address: Option<String>,
}

/// May we contact this person or address, on this channel, for this
/// purpose, at dispatch time?
#[derive(Clone, Debug)]
pub struct Question {
    /// The company on whose behalf the message would be sent.
    pub company_id: i64,
    pub contact: Contact,
    pub channel: Channel,
    pub purpose: String,
}

/// One consent decision to record: a new version for its person or address,
/// purpose and channel.
#[derive(Clone, Debug)]
pub struct DecisionInput {
    /// The company for which this decision was made.
    pub company_id: i64,
    pub contact: Contact,
    pub purpose: String,
    pub channel: Channel,
    pub granted: bool,
    pub source: Source,
    pub captured_on: Capture,
    pub evidence: Option<String>,
}

/// An address or a person no longer to be contacted on a channel.
#[derive(Clone, Debug)]
pub struct SuppressionInput {
    /// The company on whose behalf contact is suppressed.
    pub company_id: i64,
    pub contact: Contact,
    pub channel: Channel,
    pub reason: Reason,
    pub evidence: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}
pub(crate) fn field(field: &'static str, message: impl Into<String>) -> FieldError {
    FieldError {
        field,
        message: message.into(),
    }
}

/// The address as stored and matched: an email in lower case, a phone
/// number in international form (`+`, country code and number, 6 to 15
/// digits), any other handle trimmed. Email needs an email address and SMS
/// and voice a phone number.
pub fn normalise(channel: Channel, raw: &str) -> Result<String, &'static str> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("can't be blank");
    }
    let email = email(value);
    let phone = phone(value);
    let normalised = match channel {
        Channel::Email => email.ok_or("must be an email address")?,
        Channel::Sms | Channel::Voice => {
            phone.ok_or("must be a phone number with its country code")?
        }
        _ => email.or(phone).unwrap_or_else(|| value.to_owned()),
    };
    if normalised.chars().any(char::is_control) {
        return Err("must not contain control characters");
    }
    if normalised.len() > ADDRESS_MAX {
        return Err("must be at most 254 characters");
    }
    Ok(normalised)
}
fn email(value: &str) -> Option<String> {
    let (local, domain) = value.split_once('@')?;
    let shaped = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !value.chars().any(char::is_whitespace);
    shaped.then(|| value.to_lowercase())
}
/// `+60 12-345 6789`, `0060123456789` and `60123456789` are all
/// `+60123456789`. A national number (a leading single `0`) has no country
/// code, so it is not a phone number here.
fn phone(value: &str) -> Option<String> {
    let compact: String = value
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    let digits = match compact.strip_prefix('+') {
        Some(digits) => digits,
        None => match compact.strip_prefix("00") {
            Some(digits) => digits,
            None if compact.starts_with('0') => return None,
            None => &compact,
        },
    };
    let shaped = (6..=15).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0');
    shaped.then(|| format!("+{digits}"))
}

/// Trims evidence and turns blank into `None`.
pub(crate) fn evidence(value: Option<&str>) -> Result<Option<String>, FieldError> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > EVIDENCE_MAX {
        return Err(field("evidence", "must be at most 2,000 characters"));
    }
    Ok(Some(value.to_owned()))
}

/// The contact with its address normalised for `channel`. `both` allows a
/// person and an address together (a question); otherwise exactly one.
pub(crate) fn contact(
    contact: &Contact,
    channel: Channel,
    both: bool,
    errors: &mut Vec<FieldError>,
) -> Contact {
    let address = contact
        .address
        .as_deref()
        .and_then(|raw| match normalise(channel, raw) {
            Ok(address) => Some(address),
            Err(message) => {
                errors.push(field("address", message));
                None
            }
        });
    match (contact.person, contact.address.is_some()) {
        (None, false) => errors.push(field("person_id", "or an address is required")),
        (Some(_), true) if !both => {
            errors.push(field("address", "can't be given with a person_id"))
        }
        _ => {}
    }
    Contact {
        person: contact.person,
        address,
    }
}
