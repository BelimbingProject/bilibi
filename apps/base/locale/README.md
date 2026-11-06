# Locale contract

`base/locale` owns the supported regional catalogue (`SUPPORTED`, 28 codes such
as `en-MY`, `de-CH`) and the `ui.locale` Settings contract. It has no table; the
values and their provenance live in `base_settings` through `base/settings`, so
every read and write carries the sealed tenancy `Scope`. Do not read or write
those rows directly, and do not keep a second locale list in a template or host.

- `Locales::resolve(client, actor, bootstrap)` follows one order: the actor's own
  override → stored global locale → inference from `Bootstrap` → declared default
  (`en-MY`). It returns `Resolved { locale, language, source, inferred_country,
  persisted }`. Resolve once per request and pass the value along; nothing is
  process-global.
- A read never writes. Inference from `Bootstrap` is reported with
  `persisted: false` until an operator-proven transaction calls
  `adopt_inferred`, which stores it once (`source=platform_operator_address`)
  and does nothing if any global row exists. Manual global writes record
  `source=manual` and clear the inferred country; both serialize on one advisory
  lock so a stale read cannot overwrite a manual choice. Global writes need the
  explicit platform-operator proof Settings enforces.
- `Bootstrap` is supplied by Core, which owns companies and addresses (K11):
  explicitly as an argument, or through the `BootstrapSource` Core Address
  binds once at boot (`Locales::bind_bootstrap`, K15), which `resolve` asks on
  its own connection only when the caller supplied none and no global locale
  is stored. Base never queries those tables; only `country_iso` and
  `languages` drive inference.
- Unsupported locales are refused (`Invalid`), not silently replaced; `normalize`
  accepts case, `_`/`-` and bare-language variants.
- `Numbers::new(locale)` formats integers and exact decimal strings with CLDR
  data (grouping, separators and digits per region; `hi-IN` lakh grouping,
  `ar-SA` digits). It is formatting only: Bilibi copy stays English and the
  catalogue is not a translation claim. `language()` is what a future copy
  adapter would use.
- `money(amount, places)` shows an amount with exactly `places` decimals, rounded half away from zero, with comma thousands groups (`-1,234.57`). It is the one money display for pages, documents and exports for people; it is not locale-aware yet.
- The definition leaves `editor` unset. The settings editor (P3) and the System
  localization page (O5) contribute editor metadata and its capability together.

`apps/web/examples/p4-locale-datetime.rs` demonstrates this against a disposable
database (see `base/datetime/README.md`).
