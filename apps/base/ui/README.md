# Shared UI

**Use it for:** every page element: components, forms, `FlexTable` lists, dialogs, the shell and live pages.

**Not for:** hand-written markup, copied classes, or CSS and JavaScript in a module.

`base/ui` supplies the shared components, form controls and browser behaviour used by every Bilibi page.

Use the typed models exported by the crate or their Askama macros; [`DESIGN.md`](../../../DESIGN.md) sets the design rules and [`AGENTS.md`](AGENTS.md) records the recurring mistakes. The DOM uses Bilibi's floating labels.

Single selects and multi-selects share the 16 px outline chevron and right alignment. Single selects retain native keyboard and popup behaviour; multi-row selects have no dropdown chevron.

Multi-selects show the inset label and selected count above removable chips inside one field. The default keeps chips on one line with “…” for hidden selections; `.wrap_chips()` lets the field grow onto multiple lines. The empty prompt defaults to “Select options…”. Removal uses the checkbox's normal change event; `.chips(event)` can retain an owner's explicit remove event. Every selection remains available in the dropdown, including chips hidden by overflow.

List toolbars put search first and filter boxes to its right, and on a phone fold them into one Filters button that opens a sheet, with each set filter shown as a removable chip. `FilterToolbar::new` preserves the order of filters while placing search first. Controls are `ToolbarControl::search`, `select`, `multi_select`, `date` and `text` (a typed value in the sheet, such as a custom field's text; `.numeric()` gives a phone the decimal keypad). A toolbar label is screen-reader only, so a text control shows its label as the placeholder and a date control says what it bounds in its hint.

Page headers include a pin for the current address in signed-in pages, including workspace tiles. The shell keeps its state with the account. Dialogs do not show it.

## Record field vocabulary

Declare a field once so record labels, Grid columns and operation inputs use the same meaning.

For example, Contacts declares personal email with a key, label, type and personal-data class:

```rust
bilibi_ui::record_fields! { "core/person", pub(crate) mod fields {
    EMAIL: bilibi_ui::RecordField::new("email", "Personal email", bilibi_ui::RecordFieldType::String)
        .personal(bilibi_ui::PersonalDataClass::Email),
} }
```

- Use `fields::EMAIL.fact(value)` on the record and `bilibi_grid::Field::record(fields::EMAIL)` in the Grid table. SQL source keys, protection and links stay in Grid.
- Use `fields::EMAIL.input(json!({"maxLength":254}))` for the corresponding operation property. Required fields, limits, null handling and record references stay with that operation.
- Declare closed choices with `RecordFieldType::Enum` and `.choices(...)`, or an operator list with `RecordFieldType::String` and `.picklist("owner.list")`. An operation may narrow closed choices, but cannot introduce another choice.
- Classify personal values with `PersonalDataClass`; `NonPersonal` means the owner reviewed the field. An absent class remains unknown, so adopting this vocabulary cannot enable oversight accidentally.
- Use `.path(...)`, `.slot(...)` and `.excluded(...)` for read presentation. A linked display can show a name while the write takes an integer ID. Collections and structured facts use `Array` or `Object` and remain excluded from scalar Grid columns.

The macro registers each named field under `RECORD_FIELDS`, with the declaring module and source location. Boot refuses duplicate or invalid vocabulary entries, unknown input references, properties with a different key, and conflicting labels, types, pick-lists or personal-data classes. Grid also refuses a direct record fact backed by an independent field declaration. Contract schemas expose the reference as `x-bilibi-field` and classification as `x-bilibi-personal-data`; neither grants access or changes owner validation.

See [Grid projections](../grid/README.md#detail-fields-become-columns) and the [input schema contract](../agent_api/README.md). Run `cargo run -p bilibi-web --example field-vocabulary` to exercise discovery and disagreement refusals without changing business data.

## Markdown

Use `Markdown::new(source)` for a reading view of untrusted plain-text Markdown; render it with `Html::render` like other typed components.

```rust
use bilibi_ui::{Html, Markdown};

let reading = Html::render(&Markdown::new(&stored_source))?;
// Pass stored_source itself to Field::value or InlineLongText for editing.
```

- **Syntax:** CommonMark paragraphs, headings (levels 1–6), emphasis/strong emphasis, ordered/unordered and nested lists, blockquotes, links (including reference links and autolinks), fenced/indented code and inline code. Soft line breaks follow ordinary paragraph layout; CommonMark hard breaks produce a line break.
- **Literal content:** raw HTML, image syntax and thematic breaks display as escaped text. Optional extensions (tables, task lists, strikethrough, footnotes, math) are disabled; their punctuation remains plain text under CommonMark rules. Code stays literal, without highlighting or execution.
- **Links:** only `http`, `https`, `mailto`, relative paths, query links and fragments become anchors. Unsafe links keep their readable labels without an anchor. Protocol-relative destinations, backslashes, whitespace and control characters are refused. Links open in the current tab; rendering never fetches destinations.
- **Trust boundary:** `pulldown-cmark` parses the source on the server. Base UI converts raw HTML to text, keeps images literal, checks destinations, then uses the parser's escaping HTML writer. Only that filtered output, held privately by `Markdown`, reaches the page; there is no template macro for Markdown. Domain code passes source into `Markdown`, never into `Html::raw` or a `safe` filter. No media, scripts, styles or browser hooks come from the source.
- **Source and editing:** store the original Markdown and pass it unchanged to the existing textarea/`InlineLongText`. The component borrows input during rendering and retains only the reading view; it does not normalize or save source. Owners retain their field limits, authorization and write operations. No editor toolbar or WYSIWYG is added.
- **Typography:** the shared `ui-markdown` rules reuse the compact type scale, link, ink, line and sunken code roles in both themes. No Domain CSS or JavaScript is needed.

The running specimen at `/assets/ui-components.html#specimen-markdown` pairs the reading view with an ordinary textarea and includes literal HTML, unsafe links, images and plain text.

## Signed-in standalone reviews

Return `PageChrome::StandaloneReview` from a page operation that needs the signed-in credential layout instead of the workspace shell. Web keeps the normal route guard, session binding, CSRF and live transport, then `StandaloneReview` draws the Bilibi mark, the installation banner and one review card. OAuth consent is the running use. The page supplies only the card body; it never draws its own logo, instance banner or shell.

## Embedded application frames

Use `AppFrame` for a self-contained same-origin interactive document inside a Bilibi live page. The frame is stable across patches and `assets/app_frame.js` translates its MCP Apps `tools/call`, `ui/open-link`, size and model-context messages into owner events. The page owns authentication and authorization, then handles each call through the existing operation contract; the frame adds no write path.

A module publishes a self-contained document under `/assets` by contributing one `PublicAsset` to `PUBLIC_ASSETS`. Paths are unique and must start with `/assets/`; give sensitive documents a narrow response Content Security Policy. The asset must not contain credentials. Use a contributed asset and `AppFrame` instead of adding a host router entry, copying the frame markup or writing a page-specific message bridge.

## Searchable multi-select

Use `SearchMultiSelect` for a server-searched set of records. Supply selected `SearchMultiOption`s separately from a bounded list of results; the control renders at most 50 hits. An option may carry detail or be disabled with a note and an optional local link shown beside the note. `.create("Create company", "create-company")` adds the trailing action. `.wrap_chips()` shows all selected chips; the default limits the row to one line and offers an overflow button that reveals the remaining chips.

The owner handles the configured search event with `query`, pick and remove events with `value` (pick toggles that value), and the create event with `query`. Search is debounced 300 ms; results for an older query cannot be selected while the person types; the control compares the echoed query with the input ignoring surrounding spaces, so the owner may trim it. Keep selection in the mounted owner across searches, check each event against current authorization and availability, and recheck on save. The control posts selected values under its array name; the empty value denotes no selection. Bound the owner's query and database result count before rendering. The control never downloads or filters the full record set.

The Design Library’s searchable multi-select card exercises a mounted owner and server-side search. Source: [`src/search_multi.rs`](src/search_multi.rs), [`templates/search_multi.html`](templates/search_multi.html), and `SearchMultiSelect` in [`assets/hooks.js`](assets/hooks.js).

## Charts

Use `Chart::new_with_display_values` when values have a stored decimal scale: pass owner-formatted strings indexed by series and category alongside the numeric values used for geometry. The Data table, mark titles and KPI use those strings; absent strings fall back to the numeric value. Missing numeric values remain gaps. `Chart::new` is suitable when the float itself is the intended display value. Pie and donut totals are calculated from numeric values and displayed to 15 significant digits.

## Icons

**Use one bare icon name in menus, templates and Rust callers**, such as `building-office-2`, `edit` or `bookmark-solid`.

```rust
let edit = bilibi_ui::Icon { name: "edit", class: "size-4" };
```

Templates call the shared `ui::icon` macro with the same name. Prefer a semantic action such as `edit` or `manage` for a familiar action. The Design Library’s Graphic page lists every accepted name and copies it to the clipboard.

| Name | SVG variant | Natural size |
| --- | --- | --- |
| `check-circle` | Outline | 24 px |
| `check-circle-solid` | Solid | 24 px |
| `check-circle-mini` | Solid mini | 20 px |
| `check-circle-micro` | Solid micro | 16 px |

The macro defaults to `size-4` (16 px); pass a size class to change it. If you supply classes without a size, the SVG keeps its natural size. Icons inherit `currentColor`; outline strokes stay at 1.5 units. Busy buttons use the same `arrow-path` SVG with their existing motion class.

`Icons` is the public catalogue facade; `Icons::is_known` asks whether a name resolves without the warning. Cargo embeds the vendored Heroicons and product SVGs at build time, preserving path fill and clip rules. Semantic action and shell mappings live in `assets/vendor/product-icons/catalog.json`. There are no runtime file reads, CSS masks, icon plugins or icon safelists.

**An unknown name renders `missing-icon`**, a neutral dashed box in the caller’s size and colour, and logs one warning per name. At boot the host checks every menu contribution and includes its module in the warning; later rendering does not repeat that warning. Multiple contributing modules each get their own diagnostic. Missing icons never stop boot or panic during rendering. Names are exact: a prefixed or misspelled name gets the placeholder.

## Shell feedback

Theme and time-display saves use the shared flash UI through `feedback::local_flash_group`. Success dismisses after eight seconds; saving information and errors remain until replaced or dismissed. Browser-owned flashes dismiss locally without clearing the page’s server-owned flash. Stat-strip value labels use `ink-muted` so they remain readable in both themes.

## Form size

**Every live form shares a 1 MiB limit on its complete JSON event.** The browser measures UTF-8 bytes after JSON encoding, including the request ID. A larger event stays in the browser, keeps its draft and shows a persistent error flash. The host enforces the same limit before calling a page or panel.

- Match each field's `maxlength` to its owner's validation. Character limits are not byte limits: CJK commonly needs three UTF-8 bytes per character, and JSON escaping can need six bytes per UTF-16 code unit.
- Budget the whole form: all field names, values, selected arrays, hidden values and transport metadata. Leave headroom below `bilibi_ui::LIVE_EVENT_LIMIT`; two 10,000-character fields fit even when every code unit needs escaping.
- Existing Platform declarations, including Address's 10,000-character raw input, fit this budget. New repeating forms must bound their total content as well as each field. Use the shared file uploader for larger documents; never put file bytes into JSON.
- **A field's text limit belongs to its owner; read posted text with the checked extractors.** `params::form_values_checked(payload, form, limit)` and `params::string_values_checked(payload, limit)` return `Result<_, params::TooLong>`: a string over `limit` characters is an error, not an empty field. Pass `params::FIELD_LIMIT` (1,024) unless the owner allows more (Address 10,000, a saved query 100,000). Refuse the event with `.map_err(Error::refused)?` (`bilibi_database::Error::Refused`, shown as its message, nothing cleared) and the browser keeps the draft; the owner's validation still reports each field's own, shorter limit on the field. `params::form_values` and `params::string_values` keep their old signatures and silently drop a string over 1 KiB; they are deprecated, so move to the checked ones. `PeriodFilter::event` is deprecated the same way; use `event_checked`. Code that reads `payload` keys directly keeps its own limits.
- Keep using `Form`, `Field` and the shared adapter. Forms, change events, hooks and mounted panels receive the same protection without a module-owned size check or script.

The constants and refusal copy live in [`src/live_transport.rs`](src/live_transport.rs). [Live pages](../../../docs/live-pages.md#transport) defines the server's additional socket cap and reconnect behaviour.

## Colour input

Use **`ColourInput` for a choice of named colours**. Each choice is a chip in that colour's banner surface, border and text colour, with the name inside.

Build it from the form's `Field` and the allowed choices:

```rust
use bilibi_ui::{ColourChoice, ColourInput, ColourRole, Field};

let colour = ColourInput::new(
    Field::named("banner_colour").label("Banner colour").value("info"),
    vec![
        ColourChoice::new("Use role colour", "", ColourRole::Info),
        ColourChoice::new("Blue", "info", ColourRole::Info),
        ColourChoice::new("Amber", "warning", ColourRole::Warning),
    ],
);
```

The label is what people read; the value is what the form submits. The owner supplies the default choice's actual colour. Render with `Html::render(&colour)` or `forms::colour_input(control=colour)`.

- `ColourRole` permits the status roles from `DESIGN.md`. The chip uses the same light and dark surface, border and text tokens as the banner, without inline styles or fixed colour values. `ColourInput` adds the default colour's name to the choice with an empty value, so "Use role colour" shows as "Use role colour (Blue)".
- The field label stays small inside the field. `Field` supplies the hint, errors, required and disabled state.
- Arrow keys, Home, End and typing a colour's name move through the list. Enter or Space selects; Escape cancels and Tab leaves the field. Each option keeps its plain name for screen readers.
- A native select owns the form value and reset behaviour. Without JavaScript, the named select remains available.

The source is [`src/colour.rs`](src/colour.rs), the macro is in [`templates/forms.html`](templates/forms.html), and the browser behaviour is `ColourInput` in [`assets/hooks.js`](assets/hooks.js). The running specimen at `/assets/ui-components.html#specimen-colours` includes inherited, invalid, disabled and required fields.

## Inline form cells

Use `InlineForm` when a read-first table cell needs a complete form, including secret inputs or 2FA, rather than a text fact's automatic save.

- Supply the displayed value, an owner-rendered `Form`, and the open and cancel events. Opening reports the cell's `key`.
- The owner holds `editing` and non-secret drafts, rechecks access on each save, and closes only after a successful write. Pass a refusal through `CommitStatus` so it remains beside the cell.
- Save and Enter submit the form. Cancel and Escape discard the draft. The shared hook focuses the editor on opening and returns focus to the value on closing.
- Keep secret plaintext out of the read value, hint and status. Use `SecretInput::stored` in the form.

Source: [`src/inline.rs`](src/inline.rs), [`templates/inline.html`](templates/inline.html), and the hooks in [`assets/inline.js`](assets/inline.js). `InlineField` supports native text and date inputs; use `.input_type("date")` with `.display_value(...)` to keep the canonical editor value separate from read text. Date facts commit when the date changes. The Design Library includes reading and refused-editor states; `/system/settings` exercises the live lifecycle.

## 2FA proof input

Use `ProofInput` inside a protected form's `StepUp` component. Build `ProofMethod` values from `StepUp::enrolled_methods`, including each method's code length and numeric flag. It keeps the plain `CodeInput` id for one method (the wrapper is `{id}-proof`), suffixes the ids with the method's position when there are several, takes `.optional()` for a form that asks for a code only on some changes, and adds a shared selector when there are several. The `FactorProof` hook posts the chosen method and code as `method_id:code` in the existing code field, so the owner's event schema stays unchanged. Neither selecting nor rendering a method sends a challenge. The owner still calls `require` or `require_code`.

The specimen at `/assets/ui-components.html#specimen-factor-proof` shows six-digit and eight-character methods. The [Step-up provider contract](../step_up/README.md#provider) owns registration, challenge starts and verification.

## Browser location

Use **`LocationAction` to ask the browser where the person is** before an event that needs it (clocking in, a site visit): one button, no page-owned script.

```rust
use bilibi_ui::{ButtonVariant, LocationAction, LocationReport};

let clock_in = LocationAction::new("clock-in", "clock", "Clock in")
    .variant(ButtonVariant::Primary)
    .value("clock_type", "in")
    .sending("Recording clock event…");
```

Render with `Html::render(&clock_in)` or `forms::location_action(control=clock_in)`. On click the button shows "Finding your location…" while the browser locates, then raises `clock` with `clock_type`, `latitude`, `longitude` and `accuracy`, or with `outcome` `permission_denied`, `unavailable` or `insecure_context`. Decode the event with `LocationReport::from_event` and show `LocationReport::refusal` for a report without a position, adding what the person can do instead on that page. The contract is in [`docs/live-pages.md`](../../../docs/live-pages.md#browser-location); the Design Library's Browser location card is the running specimen.

## Day tiles

A work calendar's resolved day is a `DayTile` (Belimbing's `x-ui.day-tile`): the date as shown, its `DayKind` (`Working`, `Rest`, `Off`, `Holiday`) as a tint and as a word, and plain `lines` such as opening hours or a holiday's name. `description` is the whole day in words for the tooltip and for assistive technology, so the tint is never the only signal. `DayMonth` lays whole weeks of tiles under `WEEKDAYS`, Monday first, as a table that scrolls sideways inside its card when narrower than seven tiles; on a phone (below `sm`) it becomes a list of the month's own dates, each after its weekday. A neighbouring month's date is `outside`: dimmed in the grid, left out of the list. A Domain's roster or leave calendar maps its own day type onto `DayKind` and reuses both. The tints are the `day-*` colour roles (`DESIGN.md`); the running specimen is `/assets/ui-components.html#specimen-day-tiles`.

## Calendar

`Calendar` (`src/calendar.rs`, `templates/calendar.html`, hook `Calendar` in `assets/calendar.js`) draws a Domain's records on a day, week, month, year or shared view (one column per person or resource) without the Domain owning any markup, CSS or JavaScript. The component never writes; the owner does. The running owner is `apps/web/examples/schedule-reference.rs`, and the specimens are at `/assets/ui-components.html#specimen-calendars`.

The owner's side, in order:

1. **State.** `CalendarState::parse(query, today, &lanes)` reads `view`, `date` and `lanes` from the address; return `state.to_params()` as `LiveBody.query` so a reload and a shared link reopen the same view. Take `today` and the zone from `bilibi_datetime::Clock::resolve`, and set `Calendar::follows_browser_zone` when the display mode is `local`, so the page receives `browser_timezone`.
2. **Read the visible range only.** `state.range(zone)` is the half-open UTC range, `state.shown_lanes(&offered)` the lanes of a shared view (at most `MAX_LANES`, 20). Read from your own facade with the sealed scope. Pass up to `MAX_ITEMS` (500) items; one more and the calendar says "Too many to show; narrow the filter." instead of drawing.
3. **Describe each record.** `CalendarItem::timed(id, title, start, end)` takes instants, `CalendarItem::all_day(id, title, first, last)` takes dates. `.kind(word, tone)` names the kind in words and picks a `cat-1..8` data colour (never a status colour); `.lane(key)` places it in a shared view; `.href(page)` makes the title a link. Call `.movable(version)` (and `.resizable()` for a timed one) **only when the viewer holds the update capability**: an item that is not movable has no drag handle and no Move action, and nothing else hides them.
4. **Handle the four events.** `calendar::MOVE` (a drop or a resize) and `calendar::MOVE_SUBMIT` (the Move dialog) decode with `CalendarMove::from_event(event, zone)` into one `CalendarMove { id, version, when, lane }`: a wall-clock date and minute in the display zone, resolved with the zone's own rules, so a move across a daylight-saving change keeps its wall-clock time (a time in the gap becomes the first valid instant, one in the fold the earlier). The `Err` is a sentence to show. `calendar::MOVE_OPEN` carries the item to open in the dialog (`calendar::item_id`); set `Calendar::moving`, and clear it on `calendar::MOVE_CANCEL`. Keep an `Err` of the dialog in `Calendar::move_errors`.
5. **Write through your own operation.** Pass the `CalendarMove` and its `version` to the facade that rechecks the capability and the version inside its `MutationBoundary` and refuses a stale one. Show the result in `Calendar::notice` (`StatusKind::Success`, or `Error` with the owner's own message, never a lower module's error text); a refusal must not be an `Error::Forbidden` or `Error::Internal`, which the transport treats as fatal. Re-read after every event: the browser never moves an item itself, so a refused move leaves the item where the server drew it.

6. **Optional: create from a free slot.** Set `Calendar::creatable` only for a viewer who may create the record. A click or a drag down a free time column (a tap on touch), and a click, a tap or Enter on an empty day cell, raise `calendar::SLOT`; decode it with `CalendarSlot::from_event(event, zone)` (`when` is `Timed` for a time column, a one-date `AllDay` for a day) and open your own create dialog with the time filled in. The create itself goes through your own create operation, as every other key to that door does.
7. **Optional: mark late and soon.** `CalendarItem::urgency(Some(Urgency::Overdue | Urgency::Imminent))` (and `BoardCard::urgency`) tints the item's box and writes "Overdue" or "Due soon" beside its time; the owner decides which items qualify and the window. Never use it as the kind colour.

The year view draws twelve small months: each day shows how many items fall on it (with a `!` and the danger colour when one is overdue) and links to that day's view; the month's name links to the month. **Give it `Calendar::day_totals`** (one `DayTotal { date, count, late }` per day that holds something, from one grouped count in your own facade under the same rule as the items) so a year is right however many items it holds; the year is then drawn from the totals and needs no items. Without `day_totals` it counts `items`, which is right only while the year stays under the 500-item bound.

`Calendar::day_marks` puts a `DayKind` tint and word on a week or month cell (the same colours as `DayTile`). Snap is 15 minutes (`snap_minutes` 5, 10, 15, 30 or 60). Positions are classes on a grid of five-minute rows, never a `style` attribute.

Below `md` the grids become an agenda list with the same items and the same Move action. Drag is off on coarse pointers; "Move…" opens the dialog, and is shown when a keyboard reaches it. The month view is a `grid` with arrow keys between days. A timed item that runs past midnight is drawn in each day it touches, and only the part holding its start can be dragged.

## Board

`Board` (`src/board.rs`, `templates/board.html`, hook `Board` in `assets/board.js`) draws a Domain's records as cards in columns by a status, without the Domain owning any markup, CSS or JavaScript. Like the calendar it never writes; the owner does. The running owner is the `/schedule-reference/board` route of `apps/web/examples/schedule-reference.rs`, and the specimens are at `/assets/ui-components.html#specimen-boards`.

The owner's side, in order:

1. **Columns and the address.** `BoardColumn::new(key, label, total)` carries the column's true `total` (what the heading shows), and `.closed()` marks a status that cannot receive a card. `BoardState::parse(query, &columns)` reads `column` (the column a phone shows); keep the state in the mounted page and call `read_column` on `LiveInput::Params`, so "Show more" survives a Back. The board's filters live in the owner's own `ListState` (`Board::query` carries them into the column switcher's links).
2. **Read a bounded page of each column.** `BoardState::limit(column)` is how many cards to read for it (50, plus 50 for each "Show more"); read from your own facade with the sealed scope. A column with more cards than it shows has a "Show more" button that raises `board::MORE`; answer it with `board::column_key(event)` and `BoardState::show_more`.
3. **Describe each card.** `BoardCard::new(id, column, title)` with up to three `.fact` lines and `.href(page)`. Call `.movable(version, targets)` **only when the viewer holds the update capability**, with `targets` the columns this person may move it to (for a workflow subject, the statuses its edges allow them): a card without targets has no drag handle and no Move action, and other columns are not drop targets. The targets are a hint; the operation decides again.
4. **Handle the events.** `board::MOVE` (a drop) and `board::MOVE_SUBMIT` (the Move dialog) decode with `BoardMove::from_event(event)` into `{ id, version, from, to }`. `board::MOVE_OPEN` names the card to open in the dialog (`board::card_id`); set `Board::moving`, and clear it on `board::MOVE_CANCEL`. Keep an `Err` of the dialog in `Board::move_errors`.
5. **Write through your own operation.** Pass the `BoardMove` to the facade that rechecks the capability, the version, that the record is still in `from`, and the edge to `to`, inside its `MutationBoundary`. Show the result in `Board::notice` (`StatusKind::Success`, or `Error` with the owner's own message); a refusal must not be an `Error::Forbidden` or `Error::Internal`, which the transport treats as fatal. Re-read after every event: the browser never moves a card itself, so a refused or stale move leaves the card where the server drew it.

Below `md` the board shows one column at a time with a column switcher whose links patch `column` into the address. Drag is off on coarse pointers; "Move…" opens the dialog, and is shown when a keyboard reaches it. Sorting within a column, swimlanes, work-in-progress limits and grouping several statuses into one column are not part of the board.

## Record references

`RelatedRecord` (a named pill through `RecordLink`, or unavailable) and `RecordPicker` (a server-driven type-ahead inside the owner's live `Form`) show and choose a reference to another module's record. Base Records resolves names and hits with the reader's permissions; this crate only draws them: [`../records/README.md`](../records/README.md).

## File preview

**Show a file in place with `FilePreview { id, src, title }`** (an image, PDF or plain text in a same-origin frame), never a hand-written `<iframe>`, `<img>` or `<embed>` of your own.

- `src` is a route the owner contributes under `DOWNLOADS` and answers with `FileBody::preview(bytes, content_type, filename)`, behind the same authorization and audit as its download. `preview` returns `None` for anything but PNG, JPEG, GIF, WebP, PDF and plain text (`FileBody::PREVIEW_TYPES`, `FileBody::previewable` for deciding whether to offer one), and the route answers not found: the shared contract refuses an SVG or HTML upload, not each owner's own list. The inline policy forbids scripts and framing by another origin.
- The frame adds no write path and no second way to the bytes. Put it in a `Modal` with a Download link beside it; Close and Escape are the modal's.
- Give it a stable `id` (the live patcher keys an iframe by it). The component specimen's File preview card is the running example; Documents' list preview is its first owner.

## Received mail

**Show received mail HTML with `MailFrame::new(id, title, html)`**, never `Html::raw`. `html` is `bilibi_mail::sanitise_html` output, with `cid:` images rewritten by `bilibi_mail::inline_images` (see [Mail](../mail/README.md#received-html)).

- The document is escaped into an `<iframe sandbox srcdoc>`; the sandbox only lets a clicked link open in a new tab, so no script, form or same-origin access.
- The frame keeps the page's CSP and adds its own: images only from `data:`, nothing fetched from elsewhere. Inline `style` attributes are not applied; `/assets/mail-frame.css` gives a paper surface in both themes.
- Give it a stable `id` (the live patcher keys an iframe by it). The component specimen's Received mail card is the running example.

## Where to go next

See [`base/system`](../system/README.md) for Banner colour on Instance settings.

## Softphone

`Softphone` renders a Domain-owned call projection: its last confirmed `CallPhase`, separate `PhoneHealth`, caller, action availability and an escaped `Html` screen-pop. Omit actions the actor cannot use; supply an unavailable reason for a temporarily disabled supported action. The shared control sends the enclosing page or panel its event with `action` and an opaque `version`. The owner binds the call in mounted state, checks the actor, current version and action again, and renders only an authoritative response. It owns transfer and conference inputs, provider I/O and the refusal text. Phone unavailability disables controls while preserving the last call phase and the rest of the page. On reconnect the owner reloads a scoped projection; event wakes are hints and must not be treated as ordered state.

The disposable `softphone-walkthrough` web example is a fake source for manual browser verification; it is not registered by the production application. Its state and action controls exercise the live transport without a phone system.
