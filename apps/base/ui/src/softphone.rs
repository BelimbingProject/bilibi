//! Vendor-neutral call presentation; the mounted owner binds and authorizes the call.
use crate::Html;
use askama::Template;

/// Last owner-confirmed call phase, independent of phone availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPhase {
    Connecting,
    Ringing,
    Talking,
    Held,
    Ended,
    Failed,
}
impl CallPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Ringing => "Ringing",
            Self::Talking => "Talking",
            Self::Held => "On hold",
            Self::Ended => "Ended",
            Self::Failed => "Failed",
        }
    }
}

/// Current phone service availability; a failure never erases the call phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhoneHealth {
    Available,
    Unavailable(String),
}
impl PhoneHealth {
    pub fn message(&self) -> &str {
        match self {
            Self::Available => "Phone available",
            Self::Unavailable(reason) => reason,
        }
    }
    pub fn unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Intents sent to the owner, which binds the call and rechecks authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallAction {
    Answer,
    HangUp,
    Hold,
    Resume,
    Dial,
    Transfer,
    Conference,
    Mute,
    Unmute,
}
impl CallAction {
    pub fn code(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::HangUp => "hang_up",
            Self::Hold => "hold",
            Self::Resume => "resume",
            Self::Dial => "dial",
            Self::Transfer => "transfer",
            Self::Conference => "conference",
            Self::Mute => "mute",
            Self::Unmute => "unmute",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Answer => "Answer",
            Self::HangUp => "Hang up",
            Self::Hold => "Hold",
            Self::Resume => "Resume",
            Self::Dial => "Dial",
            Self::Transfer => "Transfer",
            Self::Conference => "Conference",
            Self::Mute => "Mute",
            Self::Unmute => "Unmute",
        }
    }
}

/// Omit forbidden or unsupported actions; a reason disables a supported one.
#[derive(Clone, Debug)]
pub struct SoftphoneAction {
    pub action: CallAction,
    pub unavailable_reason: Option<String>,
}
impl SoftphoneAction {
    pub fn enabled(action: CallAction) -> Self {
        Self {
            action,
            unavailable_reason: None,
        }
    }
    pub fn disabled(action: CallAction, reason: impl Into<String>) -> Self {
        Self {
            action,
            unavailable_reason: Some(reason.into()),
        }
    }
    pub fn disabled_now(&self) -> bool {
        self.unavailable_reason.is_some()
    }
}

/// Shared fragment. The enclosing page or panel handles `event`, which carries
/// an action and opaque version, rechecks its bound call and renders only
/// acknowledged state. Screen-pop content is reauthorized by the owner.
#[derive(Template, Clone, Debug)]
#[template(path = "softphone.html")]
pub struct Softphone {
    pub id: String,
    pub phase: CallPhase,
    pub health: PhoneHealth,
    pub caller: String,
    pub actions: Vec<SoftphoneAction>,
    pub event: String,
    pub version: String,
    pub notice: Option<String>,
    pub screen_pop: Html,
}
