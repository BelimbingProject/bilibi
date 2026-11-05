# ADR 25: An application links only the Core modules it uses

**Document Type:** Architecture Decision Record
**Status:** Accepted; implemented
**Agents:** claude-code/opus-5.5-high (author)
**Scope:** How `scripts/discover-modules.py` composes `apps/core`; what the host, Core modules, Domains and Extensions declare
**Last Updated:** 2026-11-05

## Context

**Core grew shared records that only some applications need.** Products, documents, activities and work calendars are Core so that two Domains can share one record, but a People-only application has no use for a product list. It still got the product tables, menu items, settings and agent operations, because the generated `apps/core` composition linked every Core module.

Every module already names the Core modules it uses, twice: as a Cargo dependency (`bilibi-product.workspace = true`) and as a descriptor dependency (`"core/product"`). Commerce sales, for example, declares Company, Currency and Product.

## Decision

**Every Core module is on demand. An application links the Core modules that the host and its mounted Domains and Extensions declare, and everything those need in turn.** Nobody chooses Core modules by hand, and no module carries an "always" label.

| Who | Declares |
| --- | --- |
| The host (`apps/web`, `apps/installer`, `fixtures/*`) | The Core modules every application runs (sign-in, accounts, the operator's tenants, the shell), as Cargo `[dependencies]` in `apps/web/Cargo.toml` |
| A Core module | The Core modules it depends on, as Cargo dependencies |
| A mounted Domain or Extension module | The Core modules it uses, as Cargo dependencies (and descriptor dependencies when it looks up their services) |

- **Composition.** `discover-modules.py` reads those Cargo `[dependencies]`, follows Core-to-Core dependencies, and writes only that closure into `apps/core/Cargo.toml`. A Core module left out has no tables, migrations, seeds, menu items, settings, panels or agent operations in that application: it is not compiled.
- **The Platform alone links all of Core.** With nothing mounted, the Platform's own build composes every Core module, so Platform CI and Platform development compile, lint and run all of them. The committed composition is that unmounted form.
- **Undeclared use is refused before Cargo runs.** A Core, host, Domain or Extension crate whose `src/` names a Core module outside its own declared closure stops the build with one line naming the file, the module and the dependency to add. "Names" means a service ID (`"core/<module>.<service>"`), a descriptor dependency, a `bilibi_core::<module>` path or the module's own crate. A mounted module may not depend on `bilibi-core`: that crate holds the computed result, so depending on it declares nothing.
- **Optional couplings stay optional.** A bare `"core/<module>"` record kind or slot owner (Documents' panel on Company pages) is not a use: the embed simply has nothing to fill when either side is absent. Calls into a Core module that may be absent go through `base/ports`, as for an unmounted Domain.
- `python3 scripts/discover-modules.py --explain` prints each Core module with who declares it.

### What the host resolves to

The host declares `address`, `agent`, `company`, `geonames`, `portal`, `tenant` and `user`. Their closure adds `affiliation`, `party` and `person`. Every application therefore links these ten:

`address`, `affiliation`, `agent`, `company`, `geonames`, `party`, `person`, `portal`, `tenant`, `user`

`activity`, `currency`, `document`, `product` and `work_calendar` come only with a module that declares them. Measured with each catalogue Domain mounted alone:

| Application | Extra Core modules |
| --- | --- |
| People, People connector, Supply Chain | none |
| Finance | `currency` |
| Factory | `currency`, `product` |
| Commerce | `currency`, `product` |

Geonames is in every application because Company and Address depend on it. A new Core module (Consent, later) needs no entry anywhere: it is linked when something declares it, as Activities ([ADR 24](24-shared-activities-in-core.md)) already is.

## Consequences

- A module that reached a Core module only through `bilibi-core`, or used one its Cargo manifest did not name, must declare it. The refusal says which crate to add.
- Migrations of a Core module that later becomes used run the first time it is composed. A database never shrinks when a module is no longer composed: its tables and ledger rows stay, unused.
- The composition depends on what is mounted, so a Domain's CI (which mounts it alone) proves the Domain declares everything it uses.

## Rejected alternatives

- **An always/on-demand label on each Core module.** It duplicates what Cargo dependencies already say, and a label drifts: a module marked "always" that nothing uses still costs every application its tables.
- **Picking Core modules in the application recipe.** The person building the application cannot know which shared records a Domain's code calls; a wrong pick fails at boot or at run time.
- **Composing only the host's closure on the unmounted Platform too.** Platform CI would then never compile Activities, Products, Documents or Work calendars.
