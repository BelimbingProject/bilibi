//! Personal settings navigation. All seven pages use the
//! same layout and typed active tab, including the pages owned by
//! `core/agent` and `base/search`.
use crate::Tab;

/// The active section of the signed-in account's settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsTab {
    Profile,
    Password,
    TwoFactor,
    Appearance,
    Search,
    AiConnections,
    CalendarFeed,
    Sessions,
}

/// One navigation source for every personal settings page: its tab, id, label
/// and address.
const PAGES: [(SettingsTab, &str, &str, &str); 8] = [
    (
        SettingsTab::Profile,
        "settings-nav-profile",
        "Profile",
        "/settings/profile",
    ),
    (
        SettingsTab::Password,
        "settings-nav-password",
        "Password",
        "/settings/password",
    ),
    (
        SettingsTab::TwoFactor,
        "settings-nav-two-factor",
        "2FA",
        "/settings/two-factor",
    ),
    (
        SettingsTab::Appearance,
        "settings-nav-appearance",
        "Theme",
        "/settings/appearance",
    ),
    (
        SettingsTab::Search,
        "settings-nav-search",
        "Search",
        "/settings/search",
    ),
    (
        SettingsTab::AiConnections,
        "settings-nav-ai-connections",
        "AI connections",
        "/settings/ai-connections",
    ),
    (
        SettingsTab::CalendarFeed,
        "settings-nav-calendar-feed",
        "Calendar feed",
        "/settings/calendar-feed",
    ),
    (
        SettingsTab::Sessions,
        "settings-nav-sessions",
        "Sessions",
        "/settings/sessions",
    ),
];

/// Where the personal settings pages live, as the page search names it.
pub const SETTINGS_LOCATION: [&str; 2] = ["Account", "My settings"];

impl SettingsTab {
    pub fn tabs(&self) -> [Tab<'static>; 8] {
        PAGES.map(|(section, id, label, href)| Tab {
            id,
            ..Tab::link(href, label, *self == section)
        })
    }
    /// Every personal settings page as (label, address), in tab order.
    pub fn pages() -> [(&'static str, &'static str); 8] {
        PAGES.map(|(_, _, label, href)| (label, href))
    }
}
