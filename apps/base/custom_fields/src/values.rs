//! Checking the values a record holds against the tenant's definitions. The
//! owner calls this inside its own write (and its reviewed preview), on its
//! own connection, so the definitions it checks are the ones the write sees.
//! A value is a JSON string (text, exact decimal, `YYYY-MM-DD`, a pick-list
//! code or a record's id), or for several choices a JSON array of codes;
//! `null`, an empty string or an empty array clears it.
use crate::{
    definitions::{CustomFields, Definition, FieldError, Kind},
    MAX_TEXT_LENGTH,
};
use bilibi_database::{Error, Result};
use bilibi_settings::Target;
use bilibi_tenancy::{ReadClient, Scope};
use serde_json::{Map, Value};
use tokio_postgres::Transaction;

fn valid_phone(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let (number, extension) = match lower
        .find("ext")
        .map(|at| (at, 3))
        .or_else(|| lower.find('x').map(|at| (at, 1)))
    {
        Some((at, marker)) => (&lower[..at], Some(&lower[at + marker..])),
        None => (lower.as_str(), None),
    };
    number.chars().any(|c| c.is_ascii_digit())
        && number
            .chars()
            .all(|c| c.is_ascii_digit() || " +()-./".contains(c))
        && extension.is_none_or(|extension| {
            let digits = extension.trim_start_matches(['.', ' ']);
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        })
}

/// A decimal number as text: optional minus, up to 18 integer digits and up to
/// 6 fraction digits. No exponent, no plus sign, no separators.
pub fn valid_number(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    !whole.is_empty()
        && whole.len() <= 18
        && fraction.len() <= 6
        && (digits.find('.').is_none() || !fraction.is_empty())
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
}

/// A calendar date written `YYYY-MM-DD`, year 0001–9999.
pub fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        && value
            .parse::<jiff::civil::Date>()
            .is_ok_and(|date| date.year() > 0)
}

/// The submitted value as the text it would be stored as: strings are
/// trimmed, a JSON number is written out as it is, and an empty or null value
/// clears the field (`None`). Anything else is not a value.
fn submitted(value: &Value) -> std::result::Result<Option<String>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.trim().to_owned()).filter(|text| !text.is_empty())),
        Value::Number(number) => Ok(Some(number.to_string())),
        _ => Err(()),
    }
}

/// Several choices as submitted: an array of codes (each trimmed, blanks
/// dropped, a repeat kept once in its first place), or null. An empty
/// selection clears the field (`None`). A pick-list holds at most
/// `VALUES_MAX` values, so a longer array cannot be a selection.
fn submitted_codes(value: &Value) -> std::result::Result<Option<Vec<String>>, ()> {
    let items = match value {
        Value::Null => return Ok(None),
        Value::Array(items) if items.len() <= bilibi_picklists::VALUES_MAX => items,
        _ => return Err(()),
    };
    let mut codes: Vec<String> = Vec::new();
    for item in items {
        let code = item.as_str().ok_or(())?.trim();
        if !code.is_empty() && !codes.iter().any(|held| held == code) {
            codes.push(code.to_owned());
        }
    }
    Ok(Some(codes).filter(|codes| !codes.is_empty()))
}

/// The codes a several-choices value holds: its array's strings (anything
/// else, such as a value written before the field existed, holds none).
pub fn codes_of(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

impl CustomFields {
    async fn max_text_length(&self, client: &dyn ReadClient, scope: &Scope) -> Result<usize> {
        self.settings
            .get(client, scope, Target::Tenant, MAX_TEXT_LENGTH)
            .await?
            .value
            .as_u64()
            .map(|length| length as usize)
            .ok_or(Error::Internal)
    }

    /// Checks `patch` (key to new value) for `record_type` against the active
    /// definitions on the caller's read connection: the reviewed preview. A
    /// key that is not an active field, a value of the wrong shape or a
    /// pick-list code the list does not offer is refused on that key. A code
    /// the record already holds (`current`) stays valid even when retired.
    /// Returns the values as they would be stored; `null` clears a key.
    pub async fn validate(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        record_type: &str,
        patch: &Map<String, Value>,
        current: &Map<String, Value>,
    ) -> Result<std::result::Result<Map<String, Value>, Vec<FieldError>>> {
        self.check_values(client, scope, record_type, patch, current, None)
            .await
    }

    /// The same check on the owner's write transaction. A changed pick-list
    /// code is held `FOR SHARE`, so a concurrent retirement waits for the
    /// owner's commit.
    pub async fn validate_locked(
        &self,
        sql: &Transaction<'_>,
        scope: &Scope,
        record_type: &str,
        patch: &Map<String, Value>,
        current: &Map<String, Value>,
    ) -> Result<std::result::Result<Map<String, Value>, Vec<FieldError>>> {
        self.check_values(sql, scope, record_type, patch, current, Some(sql))
            .await
    }

    async fn check_values(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        record_type: &str,
        patch: &Map<String, Value>,
        current: &Map<String, Value>,
        lock: Option<&Transaction<'_>>,
    ) -> Result<std::result::Result<Map<String, Value>, Vec<FieldError>>> {
        if patch.is_empty() {
            return Ok(Ok(Map::new()));
        }
        let active = self.active(client, scope, record_type).await?;
        let max_text = self.max_text_length(client, scope).await?;
        let mut errors = Vec::new();
        let mut stored = Map::new();
        for (key, value) in patch {
            let Some(definition) = active.iter().find(|definition| &definition.key == key) else {
                errors.push(FieldError::new(key, "is not an active custom field"));
                continue;
            };
            if definition.kind == Kind::Choices {
                let Ok(codes) = submitted_codes(value) else {
                    errors.push(FieldError::new(key, "must be a list of pick-list codes"));
                    continue;
                };
                let Some(codes) = codes else {
                    stored.insert(key.clone(), Value::Null);
                    continue;
                };
                match self
                    .check_codes(client, scope, definition, &codes, current, lock)
                    .await?
                {
                    Ok(()) => {
                        stored.insert(
                            key.clone(),
                            Value::Array(codes.into_iter().map(Value::String).collect()),
                        );
                    }
                    Err(message) => errors.push(FieldError::new(key, message)),
                }
                continue;
            }
            let Ok(text) = submitted(value) else {
                errors.push(FieldError::new(key, "must be text"));
                continue;
            };
            let Some(text) = text else {
                stored.insert(key.clone(), Value::Null);
                continue;
            };
            match self
                .check_one(client, scope, definition, &text, max_text, current, lock)
                .await?
            {
                Ok(text) => {
                    stored.insert(key.clone(), Value::String(text));
                }
                Err(message) => errors.push(FieldError::new(key, message)),
            }
        }
        Ok(if errors.is_empty() {
            Ok(stored)
        } else {
            Err(errors)
        })
    }

    async fn check_one(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        definition: &Definition,
        text: &str,
        max_text: usize,
        current: &Map<String, Value>,
        lock: Option<&Transaction<'_>>,
    ) -> Result<std::result::Result<String, String>> {
        let owned = || Ok(text.to_owned());
        Ok(match definition.kind {
            Kind::Text | Kind::LongText => {
                if text.chars().count() > max_text {
                    Err(format!("must be at most {max_text} characters"))
                } else if text.chars().any(|c| c.is_control() && c != '\n') {
                    Err("must not contain control characters".into())
                } else {
                    owned()
                }
            }
            Kind::Boolean if matches!(text, "true" | "false") => owned(),
            Kind::Boolean => Err("must be true (yes) or false (no)".into()),
            Kind::Email => {
                let shaped = text.split_once('@').is_some_and(|(local, domain)| {
                    !local.is_empty()
                        && !domain.is_empty()
                        && !domain.contains('@')
                        && domain.split('.').all(|part| !part.is_empty())
                });
                if text.len() <= 254
                    && shaped
                    && !text.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    owned()
                } else {
                    Err("must be an email address of at most 254 bytes".into())
                }
            }
            Kind::Phone => {
                if text.chars().count() <= 64 && valid_phone(text) {
                    owned()
                } else {
                    Err("must be a phone number of at most 64 characters: digits, spaces, + ( ) - . / and an optional x or ext extension".into())
                }
            }
            Kind::Link => {
                if text.chars().count() <= max_text
                    && !text.chars().any(|c| c.is_whitespace() || c.is_control())
                    && url::Url::parse(text).is_ok_and(|uri| {
                        matches!(uri.scheme(), "http" | "https") && uri.host_str().is_some()
                    })
                {
                    owned()
                } else {
                    Err("must be a full http or https link within the text length limit".into())
                }
            }
            Kind::Number if valid_number(text) => owned(),
            Kind::Number => {
                Err("must be a number such as 12 or -3.5 (up to 18 digits, 6 decimals)".into())
            }
            Kind::Date if valid_date(text) => owned(),
            Kind::Date => Err("must be a date written YYYY-MM-DD".into()),
            Kind::Choice => {
                let list = definition.picklist.as_deref().ok_or(Error::Internal)?;
                if current.get(&definition.key).and_then(Value::as_str) == Some(text) {
                    return Ok(Ok(text.to_owned()));
                }
                self.check_code(client, scope, list, text, lock)
                    .await?
                    .map(|()| text.to_owned())
            }
            Kind::Reference => {
                let kind = definition.target.as_deref().ok_or(Error::Internal)?;
                // The record already held stays, even when this reader may no
                // longer select it (or see it): saving another field never
                // asks them to choose it again.
                if current.get(&definition.key).and_then(Value::as_str) == Some(text) {
                    return Ok(Ok(text.to_owned()));
                }
                // Checked on the caller's connection (the write's transaction
                // in a commit), with the reader's own access: a record they
                // cannot see is refused exactly as a missing one is named.
                match self.records.check(client, scope, kind, text).await? {
                    Ok(reference) => Ok(reference.id),
                    Err(refusal) => Err(refusal.message().to_owned()),
                }
            }
            Kind::Choices => return Err(Error::Internal),
        })
    }

    /// One pick-list code a write may newly hold: active in `list`. On the
    /// owner's transaction it is held `FOR SHARE`.
    async fn check_code(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        list: &str,
        code: &str,
        lock: Option<&Transaction<'_>>,
    ) -> Result<std::result::Result<(), String>> {
        Ok(match lock {
            Some(sql) => self
                .picklists
                .check_in(sql, scope, list, code, None)
                .await?
                .map_err(|refusal| refusal.message().to_string()),
            None => {
                if self
                    .picklists
                    .contains_choice(client, scope, list, code, None)
                    .await?
                {
                    Ok(())
                } else {
                    Err("is not an active choice".into())
                }
            }
        })
    }

    /// Several choices: each code the record does not already hold must be an
    /// active value of the field's pick-list; a held one stays even retired.
    async fn check_codes(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        definition: &Definition,
        codes: &[String],
        current: &Map<String, Value>,
        lock: Option<&Transaction<'_>>,
    ) -> Result<std::result::Result<(), String>> {
        let list = definition.picklist.as_deref().ok_or(Error::Internal)?;
        let held = codes_of(current.get(&definition.key));
        for code in codes.iter().filter(|code| !held.contains(&code.as_str())) {
            if let Err(message) = self.check_code(client, scope, list, code, lock).await? {
                return Ok(Err(format!("{code} {message}")));
            }
        }
        Ok(Ok(()))
    }
}
