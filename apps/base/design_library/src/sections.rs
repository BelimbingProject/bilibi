//! The cards on Components use these same titles and anchors in their templates.
use bilibi_module_registry::{
    contributions::{PageSection, PAGE_SECTIONS},
    Contribution, Provenance,
};

macro_rules! card {
    ($name:ident, $anchor:literal, $title:literal $(, $keyword:literal)*) => {
        pub(crate) const $name: PageSection = PageSection {
            route: "design_library.components",
            anchor: $anchor,
            title: $title,
            keywords: &[$($keyword),*],
            query: None,
        };
        inventory::submit! {
            Contribution::new(PAGE_SECTIONS, concat!("design_library.", $anchor), Provenance {
                module: "base/design_library", source: file!(), line: line!()
            }, &$name)
        }
    };
}

card!(COMPONENT_BUTTONS, "component-buttons", "Buttons");
card!(
    COMPONENT_ICON_BUTTON,
    "component-icon-button",
    "Compact icon actions"
);
card!(
    COMPONENT_INDEX_PATTERN,
    "component-index-pattern",
    "Index page pattern"
);
card!(
    COMPONENT_FILTER_TOOLBAR,
    "component-filter-toolbar",
    "Filter toolbar"
);
card!(
    COMPONENT_TABLE,
    "component-table",
    "Company table",
    "flex table",
    "list"
);
card!(
    SAMPLE_FLEX_PLAIN_CARD,
    "sample-flex-plain-card",
    "Flexible table in a card"
);
card!(COMPONENT_CARD, "component-card", "Card");
card!(
    COMPONENT_LIST,
    "component-list",
    "Record facts",
    "record facts",
    "description list"
);
card!(COMPONENT_MARKDOWN, "component-markdown", "Markdown");
card!(COMPONENT_STAT_STRIP, "component-stat-strip", "Statistics");
card!(
    COMPONENT_DATETIME,
    "component-datetime",
    "Date and time",
    "time zone",
    "timestamp"
);
card!(
    COMPONENT_BADGES_AND_CREATE_BUTTON,
    "component-badges-and-create-button",
    "Badges and create button"
);
card!(
    COMPONENT_RESTRICTED_FIELDS,
    "component-restricted-fields",
    "Restricted fields"
);
card!(COMPONENT_STORED, "component-stored", "Stored");
card!(COMPONENT_REFUSED, "component-refused", "Refused");
card!(
    COMPONENT_NOTHING_YET,
    "component-nothing-yet",
    "Nothing yet"
);
card!(
    COMPONENT_NOTHING_MATCHED,
    "component-nothing-matched",
    "Nothing matched"
);
card!(
    COMPONENT_NOT_PERMITTED,
    "component-not-permitted",
    "Not permitted"
);
card!(COMPONENT_CODE_INPUT, "component-code_input", "Code input");
card!(
    COMPONENT_PERIOD_FILTER,
    "component-period_filter",
    "Period filter"
);
card!(
    DESIGN_LIBRARY_INPUT_GUIDANCE,
    "design-library-input-guidance",
    "Choice guidance"
);
card!(
    COMPONENT_TEXT_AND_LONG_FORM_INPUTS,
    "component-text-and-long-form-inputs",
    "Text and long-form inputs"
);
card!(
    COMPONENT_CHOICE_CONTROLS,
    "component-choice-controls",
    "Choice controls"
);
card!(
    COMPONENT_MULTI_SELECT,
    "component-multi-select",
    "Multi-select"
);
card!(
    COMPONENT_RADIO_GROUP_STATES,
    "component-radio-group-states",
    "Data region"
);
card!(
    COMPONENT_DATE_TIME_AND_SECRET_INPUTS,
    "component-date-time-and-secret-inputs",
    "Date, time, and secret inputs"
);
card!(
    COMPONENT_SECRET_INPUT,
    "component-secret_input",
    "Stored secret"
);
card!(
    DESIGN_LIBRARY_INPUT_LIVE_STATE,
    "design-library-input-live-state",
    "Live state"
);
card!(
    COMPONENT_INPUT_STATES,
    "component-input-states",
    "Help, error, and locked states"
);
card!(
    COMPONENT_INPUT_VARIANTS,
    "component-input-variants",
    "Select, multi-value, and search inputs"
);
card!(
    COMPONENT_COMBOBOX,
    "component-combobox",
    "Combobox",
    "combo box",
    "autocomplete",
    "searchable dropdown"
);
card!(
    COMPONENT_SEARCH_MULTI,
    "component-search-multi",
    "Searchable multi-select",
    "multiple choices"
);
card!(COMPONENT_FILE_INPUT, "component-file-input", "File input");
card!(
    COMPONENT_LOCATION_ACTION,
    "component-location-action",
    "Browser location"
);
card!(
    COMPONENT_INLINE_EDIT,
    "component-inline-edit",
    "Inline edit"
);
card!(
    COMPONENT_INLINE_LONG_TEXT,
    "component-inline-long-text",
    "Inline long text"
);
card!(
    COMPONENT_INLINE_CHOICE,
    "component-inline-choice",
    "Inline choice fact"
);
card!(
    DESIGN_LIBRARY_NAVIGATION,
    "design-library-navigation",
    "Navigation"
);
card!(COMPONENT_TABS, "component-tabs", "Tabs");
card!(COMPONENT_BACK_LINK, "component-back-link", "Back link");
card!(
    COMPONENT_ACTION_LINK,
    "component-action-link",
    "Action link"
);
card!(
    COMPONENT_MODAL,
    "component-modal",
    "Modal dialog",
    "popup",
    "overlay"
);
card!(
    COMPONENT_CONFIRM_DIALOG,
    "component-confirm-dialog",
    "Confirmation dialog",
    "confirmation popup"
);
card!(
    COMPONENT_DISPLAY_CONTROLS,
    "component-display-controls",
    "Display controls"
);
card!(
    COMPONENT_ACCOUNT_MENU,
    "component-account-menu",
    "Account menu"
);
card!(
    COMPONENT_TILE_CONTROLS,
    "component-tile-controls",
    "Workspace tile controls"
);
card!(
    COMPONENT_RECORD_LINK,
    "component-record-link",
    "Record link"
);
card!(
    COMPONENT_SPLIT_HANDLE,
    "component-split-handle",
    "Workspace split handle"
);
card!(COMPONENT_PAGE, "component-page", "Page widths");
card!(COMPONENT_HEADER, "component-header", "Page header");
card!(
    COMPONENT_SECTION_HEADING,
    "component-section-heading",
    "Section heading"
);
card!(
    COMPONENT_COMPANY_PROFILE,
    "component-company-profile",
    "Company profile"
);
card!(
    EXAMPLE_COMPANY_PROFILE,
    "example-company-profile",
    "Company profile"
);
