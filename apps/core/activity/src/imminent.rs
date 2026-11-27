//! The imminent window: how long before it starts or falls due an open
//! activity is marked "Due soon" on the list, the calendar, the status tiles
//! and record pages. A tenant setting (README, Calendar).
use crate::{Activities, MODULE};
use bilibi_database::Result;
use bilibi_module_registry::{Contribution, Provenance};
use bilibi_settings::{Definition, Editor, ScopeKind, Setting, Target, ValueType, DEFINITIONS};
use bilibi_tenancy::Scope;
use serde_json::json;

/// Hours before an open activity starts or falls due that it counts as due
/// soon.
pub const IMMINENT_HOURS: &str = "core.activity.imminent_hours";
/// What the window is until a tenant sets it: a day.
const DEFAULT_HOURS: i64 = 24;
/// The longest window: 30 days.
const MAX_HOURS: i64 = 720;

inventory::submit! { Contribution::new(DEFINITIONS, IMMINENT_HOURS, Provenance { module: MODULE, source: file!(), line: line!() }, &Setting::Definition(Definition {
    value_type: ValueType::Integer, scopes: &[ScopeKind::Global, ScopeKind::Tenant], default: || json!(DEFAULT_HOURS),
    nullable: false, encrypted: false, minimum: Some(1.0), maximum: Some(MAX_HOURS as f64), validator: None,
    editor: Some(Editor {
        group: "Activities",
        label: "Due soon window (hours)",
        help: "An open call, meeting or task that starts or falls due within this many hours is marked Due soon on the Activities list, calendar and status tiles. Once the time has passed it is marked Overdue.",
        capability: None,
        reveal_duration_ms: 5_000,
    }),
})) }

impl Activities {
    /// The tenant's imminent window (setting `core.activity.imminent_hours`,
    /// 24 hours until it is set).
    pub async fn imminent_window(&self, scope: &Scope) -> Result<jiff::SignedDuration> {
        let client = self.db.get().await?;
        let hours = self
            .settings
            .get(&**client, scope, Target::Tenant, IMMINENT_HOURS)
            .await?
            .value
            .as_i64()
            .filter(|hours| (1..=MAX_HOURS).contains(hours))
            .unwrap_or(DEFAULT_HOURS);
        Ok(jiff::SignedDuration::from_hours(hours))
    }
}
