# Products

Products give goods and services one shared identity and dated selling prices across Bilibi.

For example, create “Annual support” with the base unit “year”. CX, Commerce and Finance can all refer to that product through the `product` record kind. A selling company can add MYR 1,200.00 from 1 January, then MYR 1,350.00 from 1 July. A June lookup still returns MYR 1,200.00.

## Shared facts

Core stores only the facts in [ADR 22](../../../docs/architecture/decisions/22-shared-products-in-core.md): code, name, goods or service, base unit, optional category, active or retired status, and dated selling prices. An omitted code is generated. Codes are stable; other facts use version-checked edits.

- Products belong to the tenant. There is one Products list, one record page and one create operation.
- Each price names a selling company, currency and effective date. The amount is per base unit.
- Prices use [Core currencies](../currency/README.md) (any current one; the first price in a currency marks it used) and their decimal precision. Negative amounts, excess precision and duplicate effective dates for the same product, company and currency are refused.
- Lookup returns the latest effective date on or before the requested date for exactly that product, company and currency. **A missing price is missing, never zero or a fallback to another seller.** Disabled currencies remain readable in earlier prices.
- Stored prices are optional. Domains keep formulas, costs, terms, availability, fulfilment, discounts, account mappings and taxes.
- Transaction owners freeze the product description, unit and agreed money facts when used. Later product edits never update transactions.

## Public facade

Resolve `Arc<Products>` from `bilibi_product::SERVICE` (`core/product.products`). All calls take the caller's sealed `Scope` and connection; writes take the caller's `MutationTransaction`.

| Method | Result |
| --- | --- |
| `get`, `list` | Tenant-wide shared facts under `products.view`; list queries are bounded. |
| `preview_create`, `create` | Validated facts or field errors; `products.manage`. |
| `preview_update`, `update` | Patch of shared facts with the version read; `products.manage`. |
| `preview_price`, `add_price` | Append a dated selling price; `products.prices.manage` plus live work access to the named company. |
| `price` | Latest applicable price or `None`, with `products.view` plus live work access to the named company. |
| `prices` | Bounded price history for the authorized selling company. |

Writes recheck authorization inside the mutation transaction and return each affected record through the shared audit boundary. Product updates and price inserts serialize on the product row, so a price cannot slip past a concurrent retirement. Earlier price rows cannot be replaced through this API.

## Picker and panels

Use [Base Records](../../base/records/README.md) with kind `product`, plus the shared `RecordPicker` and `RelatedRecord` UI. New selections and saves call `RecordOwner::select`; retired products are refused. Earlier references resolve through `authorize` and keep their names and page links. Selection is checked again by the holder on save.

Domains contribute a Module Registry `Embed` with `EmbedContext::Record { kind: "core/product" }`, conventionally in slot `record.product`. The record page discovers these contributions and mounts only authorized panels. Each panel receives `id`, `product_id` and `kinds=product`; the host rechecks access on mount and every event. A Domain owns its additional facts and writes. Core imports no Domain, and the basic page and lookup need none installed.

## Pages and operations

`/products` searches by code or name and filters by category or status. Creation opens one dialog. `/products/{id}` reads first and edits in place. Selling prices follow the selected work company; switching desk reloads the page. An archived company's prices read and take no new price. The submitted price explicitly carries the displayed selling company and is authorized again.

| Agent operation | Capability |
| --- | --- |
| `core.product.list`, `core.product.get`, `core.product.price` | `products.view` |
| `core.product.create`, `core.product.update` | `products.manage` |
| `core.product.price.add` | `products.prices.manage` |

All writes are reviewed operation contracts. The browser calls those same contracts through `PageContracts`. The tenant owner role receives these capabilities; grants and direct denials follow Authz.

## Where to go next

- [ADR 22](../../../docs/architecture/decisions/22-shared-products-in-core.md) defines the boundary.
- [Records](../../base/records/README.md) defines reference selection and resolution.
- [Module Registry](../../base/module_registry/README.md) defines contributed panels.

## Runtime check

On a migrated disposable database with production seeds installed, run:

```sh
BILIBI_PRODUCT_WALKTHROUGH=disposable cargo run -p bilibi-product --example product-walkthrough --locked
```

The walkthrough installs `fixtures/walkthrough` and prints creation, denied writes, currency precision refusals, duplicate dates, an unassigned selling company, date selection, missing prices, stale edits, retirement, picker exclusion and readable earlier references. Use the running browser to check the create dialog, inline edits, price entry, filters, column memory and phone layout as well; [runtime parity](../../../docs/runtime-parity.md) defines the evidence record.
