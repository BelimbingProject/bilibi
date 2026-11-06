//! The public lookup API. Every method is a global reference-data read: no
//! tenant, no scope. Capability checks belong to the screens and operations
//! that call these (`admin.geonames.list` for the indexes), never to an
//! address form's lookup.
use crate::model::*;
use crate::query::*;
use bilibi_database::{Db, MutationBoundary, Result};
use bilibi_tenancy::ProofClient;
use std::sync::Arc;
use tokio_postgres::GenericClient;

const POSTCODE_SEARCH_LIMIT: i64 = 10;
const CITY_SEARCH_LIMIT: i64 = 15;
const POSTCODE_LOOKUP_LIMIT: i64 = 100;
const MAX_POSTCODE: usize = 20;

pub struct Geonames {
    pub(crate) db: Db,
    /// Operator corrections (`corrections`) are business writes and go
    /// through mutation capture; lookups and the import do not.
    pub(crate) boundary: Arc<MutationBoundary>,
}

/// A two-letter ISO country code, trimmed and uppercased.
/// Whether `iso` names an imported country, on the caller's connection or
/// transaction. Company validates a jurisdiction with it inside its own
/// write, where `Geonames::get_country` (a pooled read) cannot join.
pub async fn country_known(client: &impl GenericClient, iso: &str) -> Result<bool> {
    let Some(iso) = normalize_iso(iso) else {
        return Ok(false);
    };
    Ok(client
        .query_opt("SELECT 1 FROM geonames_countries WHERE iso=$1", &[&iso])
        .await?
        .is_some())
}
/// One imported country by ISO code, on the caller's connection or
/// transaction. Use it where the caller already holds a connection and
/// `Geonames::get_country` would take a second one.
pub async fn country_summary(
    client: &(impl ProofClient + ?Sized),
    iso: &str,
) -> Result<Option<CountrySummary>> {
    let Some(iso) = normalize_iso(iso) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {} FROM geonames_countries WHERE iso=$1",
        CountrySummary::COLUMNS
    );
    let row = client.query_opt(&sql, &[&iso]).await?;
    Ok(row.as_ref().map(CountrySummary::from_row))
}
pub fn normalize_iso(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic()))
        .then(|| value.to_ascii_uppercase())
}

impl Geonames {
    pub fn new(db: Db, boundary: Arc<MutationBoundary>) -> Self {
        Self { db, boundary }
    }

    /// What this installation holds, including the migration's reference snapshot.
    pub async fn presence(&self) -> Result<Presence> {
        let row = self
            .db
            .get()
            .await?
            .query_one(
                "SELECT (SELECT count(*) FROM geonames_countries) AS countries,\
                 (SELECT count(*) FROM geonames_admin1) AS admin1,\
                 EXISTS (SELECT 1 FROM geonames_postcodes) AS postcodes,\
                 EXISTS (SELECT 1 FROM geonames_cities) AS cities",
                &[],
            )
            .await?;
        Ok(Presence {
            countries: row.get("countries"),
            admin1: row.get("admin1"),
            postcodes: row.get("postcodes"),
            cities: row.get("cities"),
        })
    }

    /// Every country, by name. At most `MAX_LIST`.
    pub async fn list_countries(&self) -> Result<Vec<CountrySummary>> {
        let sql = format!(
            "SELECT {} FROM geonames_countries ORDER BY country, iso LIMIT {MAX_LIST}",
            CountrySummary::COLUMNS
        );
        let rows = self.db.get().await?.query(&sql, &[]).await?;
        Ok(rows.iter().map(CountrySummary::from_row).collect())
    }

    /// Every country as a `("Name (ISO)", iso)` select option, ordered like
    /// `list_countries`.
    pub async fn country_options(&self) -> Result<Vec<(String, String)>> {
        let rows = self
            .db
            .get()
            .await?
            .query(
                &format!("SELECT country, iso FROM geonames_countries ORDER BY country, iso LIMIT {MAX_LIST}"),
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let (name, iso): (String, String) = (row.get("country"), row.get("iso"));
                (format!("{name} ({iso})"), iso)
            })
            .collect())
    }

    pub async fn get_country(&self, iso: &str) -> Result<Option<CountrySummary>> {
        country_summary(&**self.db.get().await?, iso).await
    }

    /// A bounded, searchable, sortable page of the Countries index. Invalid
    /// query values normalise to the defaults.
    pub async fn page_countries(&self, query: &IndexQuery) -> Result<Page<CountryIndex>> {
        let n = Normal::<CountrySort>::of(query);
        let pattern = n.search.as_deref().map(contains_pattern);
        let filter = "($1::text IS NULL OR c.country ILIKE $1 OR c.iso ILIKE $1)";
        let client = self.db.get().await?;
        let total: i64 = client
            .query_one(
                &format!("SELECT count(*) FROM geonames_countries c WHERE {filter}"),
                &[&pattern],
            )
            .await?
            .get(0);
        let sql = format!(
            "SELECT c.id,c.iso,c.country,c.capital,c.phone,c.currency_code,c.population,c.updated_at \
             FROM geonames_countries c WHERE {filter} ORDER BY {} {}, c.iso ASC LIMIT $2 OFFSET $3",
            n.sort.sql(),
            n.dir.sql()
        );
        let (limit, offset) = window(&n);
        let rows = client.query(&sql, &[&pattern, &limit, &offset]).await?;
        Ok(Page::new(
            rows.iter().map(CountryIndex::from_row).collect(),
            n.page,
            n.page_size,
            total as u64,
        ))
    }

    /// A page of the Admin1 index. `country_iso` filters to one country; a
    /// malformed value matches nothing rather than widening the query.
    pub async fn page_admin1(&self, query: &IndexQuery) -> Result<Page<Admin1Index>> {
        let n = Normal::<Admin1Sort>::of(query);
        let pattern = n.search.as_deref().map(contains_pattern);
        let (filter_iso, impossible) = match query.country_iso.as_deref().map(str::trim) {
            None | Some("") => (None, false),
            Some(value) => match normalize_iso(value) {
                Some(iso) => (Some(iso), false),
                None => (None, true),
            },
        };
        if impossible {
            return Ok(Page::new(Vec::new(), n.page, n.page_size, 0));
        }
        let from = "FROM geonames_admin1 a LEFT JOIN geonames_countries c ON c.iso = a.country_iso \
                    WHERE ($1::text IS NULL OR a.name ILIKE $1 OR a.code ILIKE $1 OR c.country ILIKE $1) \
                    AND ($2::text IS NULL OR a.country_iso = $2)";
        let client = self.db.get().await?;
        let total: i64 = client
            .query_one(&format!("SELECT count(*) {from}"), &[&pattern, &filter_iso])
            .await?
            .get(0);
        let sql = format!(
            "SELECT a.id,a.country_iso,c.country AS country_name,a.code,a.name,a.alt_name,a.updated_at {from} \
             ORDER BY {} {}, a.id DESC LIMIT $3 OFFSET $4",
            n.sort.sql(),
            n.dir.sql()
        );
        let (limit, offset) = window(&n);
        let rows = client
            .query(&sql, &[&pattern, &filter_iso, &limit, &offset])
            .await?;
        Ok(Page::new(
            rows.iter().map(Admin1Index::from_row).collect(),
            n.page,
            n.page_size,
            total as u64,
        ))
    }

    /// Only the countries that have imported Admin1 divisions, by name.
    pub async fn admin1_filter_countries(&self) -> Result<Vec<CountryOption>> {
        let rows = self
            .db
            .get()
            .await?
            .query(
                &format!(
                    "SELECT c.iso, c.country FROM geonames_countries c \
                     WHERE EXISTS (SELECT 1 FROM geonames_admin1 a WHERE a.country_iso = c.iso) \
                     ORDER BY c.country, c.iso LIMIT {MAX_LIST}"
                ),
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| CountryOption {
                iso: row.get("iso"),
                country: row.get("country"),
            })
            .collect())
    }

    /// One division by its stored code; blank or unknown is `None`. Prefer
    /// this to listing a country when the code is already known.
    pub async fn get_admin1(&self, code: &str) -> Result<Option<Admin1Summary>> {
        let code = code.trim();
        if code.is_empty() || code.len() > 20 {
            return Ok(None);
        }
        let sql = format!(
            "SELECT {} FROM geonames_admin1 WHERE code=$1",
            Admin1Summary::COLUMNS
        );
        let row = self.db.get().await?.query_opt(&sql, &[&code]).await?;
        Ok(row.as_ref().map(Admin1Summary::from_row))
    }

    /// A country's divisions by name; an invalid country is empty.
    pub async fn list_admin1(&self, country_iso: &str) -> Result<Vec<Admin1Summary>> {
        let Some(iso) = normalize_iso(country_iso) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {} FROM geonames_admin1 WHERE country_iso=$1 ORDER BY name, code LIMIT {MAX_LIST}",
            Admin1Summary::COLUMNS
        );
        let rows = self.db.get().await?.query(&sql, &[&iso]).await?;
        Ok(rows.iter().map(Admin1Summary::from_row).collect())
    }

    /// Division options for one country, valued by the stored code. The label
    /// is the name, or `Name (code)` with `name_and_code` (the postcode form).
    pub async fn admin1_options(
        &self,
        country_iso: &str,
        name_and_code: bool,
    ) -> Result<Vec<(String, String)>> {
        Ok(self
            .list_admin1(country_iso)
            .await?
            .into_iter()
            .map(|admin1| {
                let label = if name_and_code {
                    format!("{} ({})", admin1.name, admin1.code)
                } else {
                    admin1.name
                };
                (label, admin1.code)
            })
            .collect())
    }

    /// A page of the Postcodes index (search over code, place, ISO and
    /// country), each row with its provenance and revision.
    pub async fn page_postcodes(&self, query: &IndexQuery) -> Result<Page<PostcodeIndex>> {
        let n = Normal::<PostcodeSort>::of(query);
        let pattern = n.search.as_deref().map(contains_pattern);
        let from = format!(
            "{} WHERE ($1::text IS NULL OR p.postcode ILIKE $1 OR p.place_name ILIKE $1 \
             OR p.country_iso ILIKE $1 OR c.country ILIKE $1)",
            PostcodeIndex::FROM
        );
        let client = self.db.get().await?;
        let total: i64 = client
            .query_one(&format!("SELECT count(*) {from}"), &[&pattern])
            .await?
            .get(0);
        let sql = format!(
            "SELECT {} {from} ORDER BY {} {}, p.id DESC LIMIT $2 OFFSET $3",
            PostcodeIndex::COLUMNS,
            n.sort.sql(),
            n.dir.sql()
        );
        let (limit, offset) = window(&n);
        let rows = client.query(&sql, &[&pattern, &limit, &offset]).await?;
        Ok(Page::new(
            rows.iter().map(PostcodeIndex::from_row).collect(),
            n.page,
            n.page_size,
            total as u64,
        ))
    }

    /// Imported-postcode totals per country: unpaginated, unaffected by the
    /// index search, bounded by the imported-country set.
    pub async fn list_postcode_country_summaries(
        &self,
        query: &IndexQuery,
    ) -> Result<Vec<PostcodeCountrySummary>> {
        let n = Normal::<SummarySort>::of(query);
        let sql = format!(
            "SELECT p.country_iso, COALESCE(c.country, p.country_iso) AS country_name, count(*) AS record_count \
             FROM geonames_postcodes p LEFT JOIN geonames_countries c ON c.iso = p.country_iso \
             GROUP BY p.country_iso, c.country ORDER BY {} {}, p.country_iso ASC LIMIT {MAX_LIST}",
            n.sort.sql(),
            n.dir.sql()
        );
        let rows = self.db.get().await?.query(&sql, &[]).await?;
        Ok(rows
            .iter()
            .map(|row| PostcodeCountrySummary {
                country_iso: row.get("country_iso"),
                country_name: row.get("country_name"),
                record_count: row.get("record_count"),
            })
            .collect())
    }

    /// Distinct postcodes for an address form: a literal case-insensitive
    /// prefix within one country, ascending, at most ten. An empty prefix is
    /// the country's first ten; an invalid country is empty.
    pub async fn search_postcodes(&self, country_iso: &str, prefix: &str) -> Result<Vec<String>> {
        let prefix = prefix.trim();
        let Some(iso) = normalize_iso(country_iso) else {
            return Ok(Vec::new());
        };
        if prefix.chars().count() > MAX_POSTCODE {
            return Ok(Vec::new());
        }
        let pattern = format!("{}%", escape_like(prefix));
        let rows = self
            .db
            .get()
            .await?
            .query(
                &format!(
                    "SELECT DISTINCT postcode FROM geonames_postcodes \
                     WHERE country_iso=$1 AND upper(postcode) LIKE upper($2) \
                     ORDER BY postcode LIMIT {POSTCODE_SEARCH_LIMIT}"
                ),
                &[&iso, &pattern],
            )
            .await?;
        Ok(rows.iter().map(|row| row.get("postcode")).collect())
    }

    /// City names for an address form. Canonical and ASCII names match a
    /// literal prefix, alternate names a literal substring. The candidate set
    /// is the 15 most populous rows before exact duplicate names are removed.
    /// `admin1_code` is GeoNames' raw value or the full `CC.value` identity of
    /// the same country; another country's identity matches nothing.
    pub async fn search_city_names(
        &self,
        country_iso: &str,
        query: &str,
        admin1_code: Option<&str>,
    ) -> Result<Vec<String>> {
        let query = query.trim();
        let Some(iso) = normalize_iso(country_iso) else {
            return Ok(Vec::new());
        };
        if query.chars().count() > MAX_SEARCH {
            return Ok(Vec::new());
        }
        let admin1 = match admin1_code.map(str::trim).filter(|code| !code.is_empty()) {
            None => None,
            Some(code) => match city_admin1(code, &iso) {
                Some(raw) => Some(raw),
                None => return Ok(Vec::new()),
            },
        };
        let escaped = escape_like(query);
        let (prefix, contains) = if query.is_empty() {
            (None, None)
        } else {
            (Some(format!("{escaped}%")), Some(format!("%{escaped}%")))
        };
        let rows = self
            .db
            .get()
            .await?
            .query(
                &format!(
                    "SELECT name FROM geonames_cities WHERE country_iso=$1 \
                     AND ($2::text IS NULL OR admin1_code=$2) \
                     AND ($3::text IS NULL OR name ILIKE $3 OR ascii_name ILIKE $3 OR alternate_names ILIKE $4) \
                     ORDER BY population DESC, geoname_id ASC LIMIT {CITY_SEARCH_LIMIT}"
                ),
                &[&iso, &admin1, &prefix, &contains],
            )
            .await?;
        let mut names: Vec<String> = Vec::new();
        for row in &rows {
            let name: String = row.get("name");
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// Exact postcode lookup, the address workflow's locality source. At most
    /// 100 places by name.
    pub async fn lookup_postcode(
        &self,
        country_iso: &str,
        postcode: &str,
    ) -> Result<Vec<PostcodeSummary>> {
        let postcode = postcode.trim();
        let Some(iso) = normalize_iso(country_iso) else {
            return Ok(Vec::new());
        };
        if postcode.is_empty() || postcode.chars().count() > MAX_POSTCODE {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {} FROM geonames_postcodes WHERE country_iso=$1 AND postcode=$2 \
             ORDER BY place_name, id LIMIT {POSTCODE_LOOKUP_LIMIT}",
            PostcodeSummary::COLUMNS
        );
        let rows = self.db.get().await?.query(&sql, &[&iso, &postcode]).await?;
        Ok(rows.iter().map(PostcodeSummary::from_row).collect())
    }

    pub async fn get_city_by_geoname_id(&self, geoname_id: i64) -> Result<Option<CitySummary>> {
        let Ok(geoname_id) = i32::try_from(geoname_id) else {
            return Ok(None);
        };
        if geoname_id <= 0 {
            return Ok(None);
        }
        let sql = format!(
            "SELECT {} FROM geonames_cities WHERE geoname_id=$1",
            CitySummary::COLUMNS
        );
        let row = self.db.get().await?.query_opt(&sql, &[&geoname_id]).await?;
        Ok(row.as_ref().map(CitySummary::from_row))
    }
}

/// `LIMIT`/`OFFSET` for a normalised page; both fit i64 by the page bounds.
fn window<S>(n: &Normal<S>) -> (i64, i64) {
    (n.page_size as i64, ((n.page - 1) * n.page_size) as i64)
}

fn city_admin1(code: &str, iso: &str) -> Option<String> {
    match code.split_once('.') {
        None => Some(code.to_ascii_uppercase()),
        Some((country, raw)) if !raw.is_empty() && country.eq_ignore_ascii_case(iso) => {
            Some(raw.to_ascii_uppercase())
        }
        _ => None,
    }
}
