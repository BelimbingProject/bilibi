//! An activity's free tags (`core.activity.tags`): Base Picklists values the
//! tenant curates, held by their stable code in `core_activity_tags`. A tag is
//! not a kind: a call tagged "call" is still a call, and a meeting can carry
//! it too. The list's starting values are only a suggestion.
use bilibi_picklists::{PickList, Start};

/// The tenant-curated tags an activity carries: free text to the reader, a
/// stable code to the record.
pub const TAGS: &str = "core.activity.tags";

bilibi_picklists::declare!(
    "core/activity",
    PickList::new(
        TAGS,
        "Activity tag",
        &[
            Start::new("meeting", "Meeting"),
            Start::new("call", "Call"),
            Start::new("visit", "Visit"),
            Start::new("follow-up", "Follow-up"),
        ]
    )
);
