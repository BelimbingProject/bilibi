# ADR 22: Shared products in Core, selling rules in Domains

**Document Type:** Architecture Decision Record
**Status:** Proposed
**Agents:** codex/gpt-6-astra-high (author)
**Scope:** Shared product identity and selling prices in `apps/core/product`; extensions in CX, Commerce and Finance
**Last Updated:** 2026-11-04

## Context

**A seller should enter a product once, whether selling goods or services.** “Annual support” should be the same product in a CX opportunity, a Commerce order and a Finance invoice. Separate product lists mean repeated entry and reconciliation. Domains cannot depend on each other, so none can own that shared identity.

Core must stay small. Sharing a product does not justify putting every selling rule in the required enterprise layer.

## Decision

**Use `apps/core/product`, owner `core/product`, Rust crate `bilibi-product`, with the menu label “Products”.** The singular noun matches `company`, `person` and `currency`; a catalog is a curated selling list, which remains a Domain concern.

### Core facts

Only these business facts belong in Core. “Every seller needs it” is the admission test, not a requirement to fill every optional field.

| Fact | Why every seller needs it |
| --- | --- |
| Stable product identity, within a tenant | Refer to the same thing across work and after a rename. |
| Code | Distinguish products reliably when names look alike; a generated code avoids extra setup. |
| Name | Tell people what is being sold. |
| Goods or service | Identify whether the sale supplies a thing or work; delivery rules remain elsewhere. |
| Base unit | Say what one priced quantity means, such as each, hour or year. |
| Optional category | Group and find products as the list grows; a seller with a short list can leave it empty. |
| Active or retired | Stop new selection without losing references from earlier work. |
| Dated selling price: amount per base unit, currency, selling company and effective date | State how much that seller charges, in which money and when, without replacing earlier prices. |

Products are shared within the tenant; a price explicitly names the selling company. Use [Core Currency](../../../apps/core/currency/README.md) for enabled currencies and precision. Select the latest price effective on the requested date for that product, company and currency; refuse duplicate effective dates. A missing price is reported as missing, never zero or another company's price. Authorize the chosen selling company; a desk selection alone is not permission.

Stored prices are optional for negotiated work. Core stores amounts, not formulas, costs, tax rates, price books or customer discounts. A Domain may calculate a price and save it through the same Core operation as a person. Core never invokes that calculation. Once a transaction uses a product, the transaction owner freezes the description, unit and agreed money facts; later product changes do not rewrite them.

### Domain facts

Each Domain keeps additional facts in records linked to the Core product identity, without copying the shared facts into another product master.

| Owner | Facts and rules kept outside Core |
| --- | --- |
| CX | Price formulas and factors, costs and cost visibility, service and renewal terms, favourites, and opportunity estimates. Restricted costs and factors stay protected even when a selling price is visible. |
| Commerce | Fulfilment details, stock and shipping requirements, packaging and unit conversions, company or channel availability, curated catalogs, discounts, quotations and orders. |
| Finance | Account mappings and tax rules evaluated for the seller and transaction context. A product-level “taxable” flag cannot determine the tax due. |

Anything beyond the Core table stays in a Domain unless a later decision passes the same admission test. Finance may still accept descriptive invoice lines without a product.

### One product page

Core provides one Products list, one record page and one create operation, following [DESIGN.md](../../../DESIGN.md). Domains register product panels through [Module Registry](../../../apps/base/module_registry/README.md) record embeds. The host mounts authorized panels; each Domain reads and writes its own facts. Core never calls a Domain, and the basic page and price lookup work with none installed.

All Domains use the same product picker through [Records](../../../apps/base/records/README.md) and the shared UI. Selection and saving recheck access. Retirement removes a product from new choices while earlier references remain readable. Company filters and Domain panels do not create another product list or menu entry.

## Rejected alternatives

- **Separate CX and Commerce products with synchronization:** duplicate identity and reconciliation inside one Platform.
- **Products owned by a Domain:** other Domains would need a forbidden dependency or an optional service for basic product facts.
- **All selling features in Core:** every installation would inherit specialist pricing, fulfilment and accounting rules.

## Consequences

This decision is proposed; no product module ships in this documentation change. Implementation replaces the shared product and dated-price parts of [the CX catalog proposal](https://github.com/cx2027/bilibi-dom-cx/pull/28) and the independent Commerce items master with `core/product`. CX retains the extensions listed above; Commerce item references become Core product references. Move every affected caller, picker and operation together, with companion changes for official Domains and upgrade instructions for other owners.

With no production users before 2027-01-01, replace or drop obsolete tables in new migrations and recreate disposable databases. Build no synchronization, imports of old records, backfills, dual writes or compatibility APIs. If implementation starts after that date, obtain the owner's renewed decision before relying on the no-users rule.

## Prior art

The 2026-11-04 catalog and quote-to-cash research supports shared identity, not a larger Core:

- [Odoo](https://www.odoo.com/documentation/19.0/applications/inventory_and_mrp/inventory/product_management/configure/type.html) shares products across apps; [Dynamics AX](https://www.microsoft.com/en-us/dynamics-365/blog/no-audience/2012/02/13/product-number-and-item-number-in-dynamics-ax-2012/) separates shared definitions from company-specific setup.
- [Salesforce](https://help.salesforce.com/s/articleView?id=products_def.htm&language=en_US&type=5) reuses Products and Price Books across selling records. Bilibi keeps advanced pricing in Domains.
- [Shopify](https://help.shopify.com/en/manual/fulfillment/managing-orders/create-orders) connects products to draft orders; [OpenMage](https://devdocs-openmage.org/guides/m1x/api/soap/sales/sales.html) keeps orders, invoices and shipments in commerce. These illustrate transaction ownership, not a requirement to put those workflows in Core.
