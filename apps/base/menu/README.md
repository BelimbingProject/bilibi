# Menu contract

**Use it for:** adding your pages to the navigation.

**Not for:** hard-coded links in a template or a host-side link list.

`base/menu` owns the navigation tree; it has no tables and does no IO. An owning
module contributes items with `bilibi_menu::contribute!("core/x", MenuItem::page(...))`
(see `apps/core/company/src/contributions.rs`) and adds `base/menu` to its
descriptor dependencies. Do not keep a host-side link list or hard-code
navigation in a template.

- `Menu::from_catalog(services.catalog()?)` is built once at boot (`App::menu`).
  Duplicate IDs, self-parents and parent cycles fail boot as contributor defects.
  Icon names use the bare names documented in `../ui/README.md`. Web checks every
  contribution at boot; an unknown name warns with its module and renders the
  shared missing-icon glyph, without blocking boot.
  A missing parent drops that item and its descendants, listed by `dropped()` and
  logged once, so one bad contributor cannot remove everyone's navigation.
- Use `visible_staff_tree(&capabilities, owner)` for the staff tree. Pass the actor's effective capabilities and verified installation-owner standing. For a custom `visible_tree(&decide)` call, resolve the actor's `capability_keys()` in one authorization pass before filtering. `tree()` is for diagnostics. A container with no visible child is hidden, so unported sections never render.
- `Requirement::AnyOf` opens a combined page only. Hiding a link is presentation:
  the route and each operation must enforce their own capability.
- The staff tree's product order is by contributed label, ignoring case, with ties broken by `(order, id)`. See [`base/menu_order`](../menu_order/README.md) for sidebar overrides.
- The catalog retains the contributed labels. For display overrides, see [`base/labels`](../labels/README.md).
- `base/menu` contributes the structural roots (`core`, `admin`, `admin.system.*`, and the
  Domain roots). Attach to them; do not redefine them.
- `portal` (`PORTAL_ROOT`) is the customer portal's menu ([ADR 16](../../../docs/architecture/decisions/16-customer-accounts-and-portal.md)). Items under it route only to `/portal` pages and ask for a portal capability; boot refuses one that routes elsewhere or asks for nothing, a staff item routing into `/portal`, and (Authz) a key of the wrong audience. `visible_tree` never includes it and `visible_portal_tree` returns only it, for the portal layout.

`apps/web/examples/p1-seams.rs` shows cycles, duplicates, dropped parents and
visibility against a disposable database.

## Workplace coverage

`MenuItem::page` defaults to tenant-wide coverage for administration and shared directories. Add `.company_scoped()` when its route declares `CompanyScope::Company`; Web checks that they agree at boot. A company page uses `Scope::current_company()` for its records and keeps its owner authorization. The shell retains the chosen desk on tenant-wide pages as a plain label.

## Make a page section searchable

Type `combo` in the top bar to jump straight to the Combobox card in the Design Library. Add the same kind of link to a card, settings section or report on a page you own.

Submit a `PageSection` through `bilibi_module_registry::contributions::PAGE_SECTIONS` from the module that contributes the route.

```rust
use bilibi_module_registry::{
    contributions::{PageSection, PAGE_SECTIONS},
    Contribution, Provenance,
};

const COMBOBOX: PageSection = PageSection {
    route: "design_library.components",
    title: "Combobox",
    anchor: "component-combobox",
    keywords: &["autocomplete", "combo box"],
    query: None,
};

inventory::submit! {
    Contribution::new(PAGE_SECTIONS, "design_library.component-combobox", Provenance {
        module: "base/design_library", source: file!(), line: line!(),
    }, &COMBOBOX)
}
```

- `route` is the contribution ID of a named, framed staff GET page with a fixed path.
- Use `anchor` as the ID of the heading or card. Use letters, digits, hyphens and underscores. Keep the title and ID in the same declaration the view uses, as the Design Library does.
- Leave `keywords` empty or add other words a person uses for that section. Search matches part of the title or any keyword, ignoring case.
- For a tab that is selected by a query, set `query: Some("tab=summary")`. The page opens that tab and the browser scrolls to the anchor. If tabs are separate routes, name the route of the tab.
- Contribute sections every reader of the page can see. Search uses the page's access rules before showing any of its sections.

The HTTP agent search (`GET /api/v1/search?q=combo`) and the MCP `search` tool use the same page search and return matches in `pages`. A section match has its own title, page breadcrumb and URL with the anchor. [Record search](../search/README.md) covers business records.
