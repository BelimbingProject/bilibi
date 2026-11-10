//! A document's values and the rules on them that need no database: field
//! shapes, the file name, the link address and what a file's bytes are.
//! Pick-list codes, people, records and files are checked by `store`.
use crate::Reference;
use jiff::civil::Date;
use serde_json::{Map, Value};

/// The tenant's custom field values of one document: key to value. In a
/// `Draft` or `Patch` it is a patch (an empty or null value clears a key);
/// on a `Document` it is what is stored.
pub type Custom = Map<String, Value>;

/// Where a document's content lives. Fixed when the document is created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// Every revision is a file stored with `store_file`.
    #[default]
    Internal,
    /// Every revision is an `https` link.
    External,
}
impl Source {
    pub fn code(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::External => "external",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "internal" => Some(Self::Internal),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

/// Whether a revision's file was scanned. Every revision is `NotScanned`
/// until upload scanning reaches artifacts (slice 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScanStatus {
    #[default]
    NotScanned,
    Clean,
    Quarantined,
}
impl ScanStatus {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotScanned => "not_scanned",
            Self::Clean => "clean",
            Self::Quarantined => "quarantined",
        }
    }
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "not_scanned" => Some(Self::NotScanned),
            "clean" => Some(Self::Clean),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    /// A form field, or `cf_<key>` for a tenant's custom field.
    pub field: String,
    pub message: String,
}
impl FieldError {
    pub(crate) fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_owned(),
            message: message.into(),
        }
    }
    pub(crate) fn owned(field: String, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// A new document. Omitted values take their defaults: the name from the
/// first file, today's publish date in the creator's zone, the status
/// list's default and the creator as the assignee.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub name: Option<String>,
    pub publish_date: Option<Date>,
    pub source: Source,
    /// An internal document's first file, as `store_file` returned it.
    pub artifact_id: Option<String>,
    /// An external document's first link.
    pub external_url: Option<String>,
    pub category: Option<String>,
    pub sub_category: Option<String>,
    pub document_type: Option<String>,
    pub status: Option<String>,
    /// When the document expires; a reminder task is kept for it.
    pub expires_on: Option<Date>,
    /// Values of the tenant's custom fields (`base/custom_fields`).
    pub custom: Custom,
    pub sync_id: Option<String>,
    pub related_document_id: Option<i64>,
    pub relation: Option<String>,
    pub revision_label: Option<String>,
    /// The first revision's change log; "Document created" when omitted.
    pub change_log: Option<String>,
    /// Records the document relates to from the start.
    pub related: Vec<Reference>,
}

/// The fields an edit changes. `None` leaves a field unchanged; for a
/// field that may be empty, `Some(None)` clears it. The revision number,
/// source and files change only through `add_revision`.
#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub publish_date: Option<Date>,
    pub category: Option<Option<String>>,
    pub sub_category: Option<Option<String>>,
    pub document_type: Option<Option<String>>,
    pub status: Option<String>,
    /// `Some(None)` clears the expiry date.
    pub expires_on: Option<Option<Date>>,
    /// Custom values to set; an empty or null value clears a key.
    pub custom: Custom,
    pub sync_id: Option<Option<String>>,
    pub related_document_id: Option<Option<i64>>,
    pub relation: Option<Option<String>>,
}
impl Patch {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.publish_date.is_none()
            && self.category.is_none()
            && self.sub_category.is_none()
            && self.document_type.is_none()
            && self.status.is_none()
            && self.expires_on.is_none()
            && self.custom.is_empty()
            && self.sync_id.is_none()
            && self.related_document_id.is_none()
            && self.relation.is_none()
    }
}

/// A new revision: a stored file for an internal document, a link for an
/// external one. `revision` defaults to the latest number plus one.
#[derive(Clone, Debug, Default)]
pub struct RevisionInput {
    pub artifact_id: Option<String>,
    pub external_url: Option<String>,
    pub revision: Option<i32>,
    pub change_log: String,
    pub label: Option<String>,
    pub replaces_revision_id: Option<i64>,
}

/// Bytes to keep as a document file, before any document exists.
#[derive(Clone, Copy, Debug)]
pub struct Upload<'a> {
    pub bytes: &'a [u8],
    pub file_name: &'a str,
}

pub const NAME_MAX: usize = 255;
pub const CODE_MAX: usize = 64;
pub const CHANGE_LOG_MAX: usize = 2_000;
pub const SYNC_ID_MAX: usize = 255;
pub const REVISION_LABEL_MAX: usize = 100;
pub const URL_MAX: usize = 2_000;
/// The change log of a document's first revision when none is given.
pub const FIRST_CHANGE_LOG: &str = "Document created";

/// A bounded single-line text: `None` when blank, an error when too long or
/// holding control characters.
pub(crate) fn text(
    errors: &mut Vec<FieldError>,
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> Option<String> {
    let value = bilibi_ui::params::blank_to_none(value)?;
    if value.chars().count() > max {
        errors.push(FieldError::new(
            field,
            format!("must be at most {max} characters"),
        ));
    } else if value.chars().any(char::is_control) {
        errors.push(FieldError::new(field, "must be on one line"));
    }
    Some(value)
}

/// Like `text`, for several lines.
pub(crate) fn long_text(
    errors: &mut Vec<FieldError>,
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> Option<String> {
    let value = bilibi_ui::params::blank_to_none(value)?;
    if value.chars().count() > max {
        errors.push(FieldError::new(
            field,
            format!("must be at most {max} characters"),
        ));
    } else if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
    {
        errors.push(FieldError::new(
            field,
            "contains characters that are not text",
        ));
    }
    Some(value)
}

/// An `https` address with a host, at most `URL_MAX` bytes.
pub(crate) fn https_url(errors: &mut Vec<FieldError>, value: Option<&str>) -> Option<String> {
    let value = bilibi_ui::params::blank_to_none(value)?;
    let valid = value.len() <= URL_MAX
        && url::Url::parse(&value).is_ok_and(|url| {
            url.scheme() == "https" && url.host_str().is_some_and(|h| !h.is_empty())
        });
    if !valid {
        errors.push(FieldError::new(
            "external_url",
            "must be an https:// address",
        ));
    }
    Some(value)
}

/// The last part of an uploaded file's name, without any folder a browser
/// sent with it.
pub(crate) fn file_name(errors: &mut Vec<FieldError>, value: &str) -> String {
    let name = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if name.is_empty() || name == "." || name == ".." {
        errors.push(FieldError::new("file", "needs a file name"));
    } else if name.chars().count() > NAME_MAX {
        errors.push(FieldError::new(
            "file",
            format!("name must be at most {NAME_MAX} characters"),
        ));
    } else if name.chars().any(char::is_control) {
        errors.push(FieldError::new("file", "name must be on one line"));
    }
    name
}

/// Every type a document file may have, by extension. The tenant's
/// `core.document.allowed_types` narrows this list; it never widens it.
pub const FILE_TYPES: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("rtf", "application/rtf"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
];

/// Whether `value` names one of `FILE_TYPES`.
pub(crate) fn known_type(value: &str) -> bool {
    FILE_TYPES.iter().any(|(_, t)| *t == value)
}

/// The type a file's extension names, if it is one of `FILE_TYPES`.
pub(crate) fn type_of(file_name: &str) -> Option<&'static str> {
    let (_, extension) = file_name.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    FILE_TYPES
        .iter()
        .find(|(e, _)| *e == extension)
        .map(|(_, t)| *t)
}

/// Whether the bytes are what their type says: the file's own signature,
/// or plain UTF-8 text for text types. The browser's content type is never
/// trusted; the extension chooses the type and this confirms it.
pub(crate) fn content_matches(content_type: &str, bytes: &[u8]) -> bool {
    const OLE: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    const ZIP: &[u8] = b"PK\x03\x04";
    match content_type {
        "application/pdf" => bytes.starts_with(b"%PDF-"),
        "application/msword" | "application/vnd.ms-excel" | "application/vnd.ms-powerpoint" => {
            bytes.starts_with(OLE)
        }
        "application/rtf" => bytes.starts_with(b"{\\rtf"),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "text/plain" | "text/csv" => std::str::from_utf8(bytes).is_ok_and(|t| !t.contains('\0')),
        // Office Open XML and OpenDocument files are ZIP archives.
        _ if content_type.starts_with("application/vnd.") => bytes.starts_with(ZIP),
        _ => false,
    }
}
