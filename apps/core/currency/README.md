# Core Currency

The one currency list, the currencies each tenant has used, a base currency, dated exchange rates and the one conversion every module shares. It is Core master data because a Domain may depend on Core but never on another Domain, so a list kept in Finance could not serve CRM or Factory. Callers use `Currencies` (registry service `core/currency.currencies`); table layout and SQL stay private. Decision: [ADR 08](../../../docs/architecture/decisions/08-currencies-as-core-master-data.md).

## What a module does

| I need | Call |
| --- | --- |
| A currency dropdown | `currencies.combobox(client, scope, id, name, label)`, or `choices` for the options: every current currency, the used ones first |
| Show a stored code, even a withdrawn one | `currencies.get(client, scope, code)` (`name`, `minor_units`, `withdrawn`, `used`) |
| Check a code for a **new** record | `currencies.check(&mut tx, scope, code)` in the commit, on the write's own transaction; refuses `Unknown` and `Withdrawn`, and marks the currency used |
| Convert an amount | `currencies.convert(client, scope, &Request::new(amount, "USD", "MYR", date))` |
| Convert to the base currency | `currencies.convert_to_base(client, scope, amount, "USD", date)` |
| The base currency | `currencies.base(client, scope, company)` |

Never keep a free-text currency code, your own minor-units list or your own rate table. Never store `f64`: amounts are `rust_decimal::Decimal` (re-exported as `bilibi_currency::Decimal`), and JSON carries them as strings.

## The list

- **ISO 4217 ships.** `currencies` is global, with code, numeric code, name, decimal places (`minor_units`, 0 to 4) and a `withdrawn` flag. Production seed `currencies-iso4217-v1` inserts the list by code and never updates a row, so a rerun changes nothing. It leaves out the precious-metal and test codes (XAU, XTS, XXX…). Seed `currencies-iso4217-v2` sets common names. A later seed version adds new codes and marks withdrawn ones.
- **Common names.** `common_names` holds the names people use that ISO does not (CNY: RMB, Renminbi, 人民币), most common first. Search in every currency control matches them, case-insensitively; labels read `CNY — Chinese yuan (RMB)` with the first one, and the others show muted after it. A name whose words already sit together in the ISO name (`Yen` for `Japanese yen`) is left out of labels but still searched. The ISO code is still what is stored and sent. Use `Currency::choice()`, never `Choice::new(currency.label(), ..)`, so a control keeps the names searchable. Printing a common name on a document is not done here.
- **Used currencies come first; nothing is switched on.** A currency becomes used by the tenant the first time it is used: as the tenant's or a company's base, in a new rate, or on a new record `check` accepts. `currency_tenant_used` keeps that set, audited as `currency_used`; a used currency stays used. `current` and every currency control list the used ones first, then every other current one, each group by code. Tenants never add a code or change decimal places.
- **Withdrawn codes.** A withdrawn currency stays readable (`get`) for records that hold it, but `check`, a base and a new rate refuse it.

## Base currency

- Setting `currency.base` (tenant, and company to override). `null` until set: `base` answers `None` and `convert_to_base` refuses with `NoBase`. The pages and `core.currency.base.set` are its operator UI; they accept any currency that is not withdrawn and mark it used in the same write.
- When it is unset, the Currencies page suggests the primary company's country currency from Geonames. It never saves the suggestion unasked.
- Conversion through the base uses the tenant's base. A company's own base is for reports and forms; `base(.., Some(company))` reads it.

## Rates

- `currency_rates`: one row per tenant, pair and effective date. 1 `from` = `rate` `to` from that date until a later row. `rate` is `numeric(30,12)`, more than zero, at most 12 decimal places. Entered by hand; nothing is fetched. An Extension may later write rates through the same reviewed operation.
- Adding one marks both currencies used. A rate is changed in place with its `version`, deleted with its `version`; both are audited with old and new values (`currency_rate`). Past conversions are unchanged because callers store what they used.
- Setting `currency.rates.max_age_days` (tenant, default 7, `0` means no limit): a rate older than this, counted back from the conversion date, is not used.

## Conversion

`rate(from, to, on)` and `convert` look for the latest rate on or before the date, in this order: the pair itself (`Direct`), one over the opposite pair (`Inverse`), then two rates through the base currency (`ViaBase`, the older date is reported). Two equal codes answer `Same` with rate 1 and no row. Nothing found, or only a stale rate, is `NoRate`: never an assumed 1 and never a silent old rate.

`Conversion { amount, from, to, quote: Quote { rate, rate_date, path, rate_ids } }`. Store the `quote` with the result; a later rate change rewrites nothing.

- The result is rounded to the target's decimal places, half away from zero, unless the request is `.unrounded()`.
- An amount with more places than its own currency allows is refused (`TooManyPlaces`).
- `Refusal::code()` is the agent-facing name: `unknown_currency`, `too_many_places`, `out_of_range`, `no_rate`, `no_base`; `message()` is a sentence to show.

## Access

| Capability | Gives |
| --- | --- |
| `admin.currency.view` | The pages, `catalog`, `rates` and the read operations |
| `admin.currency.manage` | Choose the base currency, the `currency.rates.max_age_days` editor |
| `admin.currency.rates.manage` | Add, change and delete rates |

All three are tenant capabilities contributed to `tenant_owner`. `current`, `choices`, `combobox`, `get`, `check`, `rate` and `convert` need only the sealed scope, since the calling module authorises its own operation. Every write rechecks its capability inside its transaction, and every statement carries the tenant predicate.

## Pages and agents

- `/currencies`: one page, one menu item, two tabs kept in the address.
  - **Base currency** (the default): the tenant's base currency and each company's own, chosen in place from every current currency, the used ones first.
  - **Exchange rates** (`?tab=rates`): the rates, edited in place, a dialog to add one, the row's delete icon that opens a confirm, and a currency conversion calculator: two rows of an amount and its currency that answer each other through the same `convert` modules use, with the rate's date and source on one line and "Add this rate" when none is recent enough. The tab's guidance says how long a rate applies and the oldest rate a conversion uses. The date fields start as today in the reader's display zone; in browser-local display mode they start empty, because only the browser knows the day.
- Operations: `core.currency.list`, `.rates.list` and `.convert` (reads), and the reviewed writes `.base.set`, `.rates.set` and `.rates.delete`. The pages call these same contracts (`PageWrites`).

## Runtime check

On a disposable database, run `migrate` and `seed-production` first, then:

```sh
BILIBI_CURRENCY_DEMO=disposable cargo run -p bilibi-currency --example currency-walkthrough --locked
```

It prints a person without the capability refused, the base and rates marking currencies used, each conversion path, rounding at 0 and 3 places, stale and missing rates, a second tenant keeping its own, and `check` marking a new currency used while refusing a withdrawn or unknown code.
