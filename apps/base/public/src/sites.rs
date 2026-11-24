//! A tenant's public site and the hostnames that name it. The public
//! listener finds a request's tenant by its hostname alone (`resolve`); a
//! site that is off, an unknown hostname and a tenant that is not live are
//! the same 404. Staff manage the site on the Public site page and agents
//! through `base.public.*`, both through the writes here.
use crate::{Public, PublicScope};
use bilibi_authz::authorize_key;
use bilibi_database::{Change, Error, MutationTransaction, Result};
use serde_json::{json, Value};
use std::time::Instant;
use tokio_postgres::{GenericClient, Row};

/// See the Public site page and its hostnames.
pub const VIEW: &str = "admin.public-site.view";
/// Set the site, turn it on or off, add and remove hostnames.
pub const MANAGE: &str = "admin.public-site.manage";
/// The longest title.
pub const TITLE_MAX: usize = 120;
/// Addresses an installation keeps for its own names.
const RESERVED: [&str; 8] = [
    "www", "app", "admin", "api", "mail", "staff", "portal", "public",
];

/// A field the input got wrong, in the words a form shows under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}
/// Either the value, or every field that refused it.
pub type Checked<T> = std::result::Result<T, Vec<FieldError>>;
fn field(field: &'static str, message: impl Into<String>) -> FieldError {
    FieldError {
        field,
        message: message.into(),
    }
}

/// One hostname of a site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hostname {
    pub name: String,
    /// Emailed links use the primary hostname.
    pub primary: bool,
}

/// A tenant's public site.
#[derive(Clone, Debug)]
pub struct Site {
    pub tenant: i64,
    /// Lowercase letters, digits and inner hyphens; unique across the
    /// installation.
    pub address: String,
    /// The name visitors and customers see on the site's pages and emails.
    pub title: String,
    /// Off until staff turn it on; while off every page of it is a 404.
    pub enabled: bool,
    pub logo_asset_id: Option<i64>,
    /// A signed Media link to the logo, filled by the reads a page uses.
    pub logo_url: Option<String>,
    pub version: i64,
    /// Primary first, then by name.
    pub hostnames: Vec<Hostname>,
}
impl Site {
    fn from_row(row: &Row) -> Self {
        Self {
            tenant: row.get("tenant_id"),
            address: row.get("address"),
            title: row.get("title"),
            enabled: row.get("enabled"),
            logo_asset_id: row.get("logo_asset_id"),
            logo_url: None,
            version: row.get("version"),
            hostnames: Vec::new(),
        }
    }
    /// The primary hostname, if one is set.
    pub fn primary(&self) -> Option<&str> {
        self.hostnames
            .iter()
            .find(|hostname| hostname.primary)
            .map(|hostname| hostname.name.as_str())
    }
    fn audit(&self) -> Value {
        json!({"address": self.address, "title": self.title, "enabled": self.enabled, "logo_asset_id": self.logo_asset_id})
    }
    pub fn to_json(&self) -> Value {
        json!({
            "address": self.address, "title": self.title, "enabled": self.enabled,
            "logo_asset_id": self.logo_asset_id, "version": self.version,
            "hostnames": self.hostnames.iter().map(|h| json!({"hostname": h.name, "primary": h.primary})).collect::<Vec<_>>(),
        })
    }
}

/// What a save does to the logo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogoChange {
    #[default]
    Keep,
    Remove,
    /// A PNG, JPEG, GIF or WebP image already in Media as the tenant's own.
    Set(i64),
}
/// A save of the site's own facts.
#[derive(Clone, Debug, Default)]
pub struct SiteInput {
    pub address: String,
    pub title: String,
    /// `None` keeps it as it is; a new site starts off.
    pub enabled: Option<bool>,
    pub logo: LogoChange,
    /// The version read; a site changed since is refused.
    pub version: Option<i64>,
}

const SITE_COLUMNS: &str = "tenant_id,address,title,enabled,logo_asset_id,version";

/// A site address: lowercase letters, digits and inner hyphens, 1 to 63.
pub fn address_shaped(address: &str) -> bool {
    let bytes = address.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes.first() != Some(&b'-')
        && bytes.last() != Some(&b'-')
}
/// A hostname as a browser sends it, lowercased and without a port: at
/// least two dot-separated labels of letters, digits and inner hyphens, at
/// most 253 characters, and not an IP address.
pub fn hostname_shaped(hostname: &str) -> bool {
    let labels: Vec<&str> = hostname.split('.').collect();
    hostname.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| address_shaped(label))
        && !labels
            .last()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}
/// The hostname of a `Host` header, lowercased, when its port is `port`
/// (or absent where `port` is `None`). A port that is not a number is no
/// port at all: the header names nothing.
pub fn host_name(host: &str, port: Option<u16>) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    let (name, given) = match host.rsplit_once(':') {
        Some((name, given)) if !name.contains(']') => {
            (name.to_owned(), Some(given.parse::<u16>().ok()?))
        }
        _ => (host.clone(), None),
    };
    (given == port && hostname_shaped(&name)).then_some(name)
}

/// What a request's `Host` reached ([`Public::resolve_from`]).
#[derive(Clone, Debug)]
pub enum Reached {
    Site(PublicScope),
    /// No live site has this hostname.
    Nothing,
    /// Its sender spent the lookup budget, and the answer was not at hand.
    Busy,
}

async fn site_row(client: &(impl GenericClient + Sync), tenant: i64) -> Result<Option<Site>> {
    let Some(row) = client
        .query_opt(
            &format!("SELECT {SITE_COLUMNS} FROM public_sites WHERE tenant_id=$1"),
            &[&tenant],
        )
        .await?
    else {
        return Ok(None);
    };
    let mut site = Site::from_row(&row);
    site.hostnames = hostnames(client, tenant).await?;
    Ok(Some(site))
}
async fn hostnames(client: &(impl GenericClient + Sync), tenant: i64) -> Result<Vec<Hostname>> {
    Ok(client
        .query(
            "SELECT hostname,is_primary FROM public_site_hostnames WHERE tenant_id=$1 ORDER BY is_primary DESC, hostname",
            &[&tenant],
        )
        .await?
        .iter()
        .map(|row| Hostname {
            name: row.get(0),
            primary: row.get(1),
        })
        .collect())
}

impl Public {
    /// The scope's tenant's site, if one is set: `VIEW`.
    pub async fn site_of(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
    ) -> Result<Option<Site>> {
        authorize_key(client, scope, VIEW).await?;
        site_row(client, scope.tenant()).await
    }

    fn check_site(address: &str, title: &str) -> Checked<(String, String)> {
        let address = address.trim().to_lowercase();
        let title = title.trim().to_owned();
        let mut errors = Vec::new();
        if address.is_empty() {
            errors.push(field("address", "can't be blank"));
        } else if !address_shaped(&address) {
            errors.push(field(
                "address",
                "use 1 to 63 lowercase letters, digits and hyphens, not starting or ending with a hyphen",
            ));
        } else if RESERVED.contains(&address.as_str()) {
            errors.push(field("address", "is kept for the installation's own names"));
        }
        if title.is_empty() {
            errors.push(field("title", "can't be blank"));
        } else if title.chars().count() > TITLE_MAX || title.contains('\0') {
            errors.push(field(
                "title",
                format!("should be at most {TITLE_MAX} character(s)"),
            ));
        }
        if errors.is_empty() {
            Ok((address, title))
        } else {
            Err(errors)
        }
    }
    async fn address_taken(
        client: &(impl GenericClient + Sync),
        tenant: i64,
        address: &str,
    ) -> Result<bool> {
        Ok(client
            .query_opt(
                "SELECT 1 FROM public_sites WHERE address=$1 AND tenant_id<>$2",
                &[&address, &tenant],
            )
            .await?
            .is_some())
    }
    /// The logo a save leaves the site with, after checking a new image is the
    /// tenant's own raster image (an agent may name any asset id).
    async fn checked_logo(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        change: LogoChange,
        before: Option<&Site>,
    ) -> Result<Checked<Option<i64>>> {
        let current = before.and_then(|site| site.logo_asset_id);
        let id = match change {
            LogoChange::Keep => return Ok(Ok(current)),
            LogoChange::Remove => return Ok(Ok(None)),
            LogoChange::Set(id) => id,
        };
        let refused = || {
            vec![field(
                "logo",
                "must be a PNG, JPEG, GIF or WebP image of this workspace",
            )]
        };
        let asset = match self.media.get_asset(client, scope, id).await {
            Ok(asset) => asset,
            Err(bilibi_media::Error::Database(error)) => return Err(error),
            Err(_) => return Ok(Err(refused())),
        };
        let image = asset.mime_type.as_deref().is_some_and(|mime| {
            ["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&mime)
        });
        // A private attachment is refused by the stream check, as is a file
        // that is not readable (quarantined or still being scanned).
        let servable = self
            .media
            .tenant_stream_url(client, scope.tenant(), id)
            .await
            .is_ok();
        Ok(if image && servable {
            Ok(Some(id))
        } else {
            Err(refused())
        })
    }

    /// The site as it would be saved: `MANAGE`, the same checks, no write.
    pub async fn preview_site(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        input: &SiteInput,
    ) -> Result<Checked<(Option<Site>, Site)>> {
        authorize_key(client, scope, MANAGE).await?;
        let (address, title) = match Self::check_site(&input.address, &input.title) {
            Ok(checked) => checked,
            Err(errors) => return Ok(Err(errors)),
        };
        if Self::address_taken(client, scope.tenant(), &address).await? {
            return Ok(Err(vec![field("address", "is used by another site")]));
        }
        let before = site_row(client, scope.tenant()).await?;
        let logo = match self
            .checked_logo(client, scope, input.logo, before.as_ref())
            .await?
        {
            Ok(logo) => logo,
            Err(errors) => return Ok(Err(errors)),
        };
        let after = Site {
            tenant: scope.tenant(),
            address,
            title,
            enabled: input
                .enabled
                .unwrap_or(before.as_ref().is_some_and(|site| site.enabled)),
            logo_asset_id: logo,
            logo_url: None,
            version: before.as_ref().map_or(1, |site| site.version + 1),
            hostnames: before
                .as_ref()
                .map(|site| site.hostnames.clone())
                .unwrap_or_default(),
        };
        Ok(Ok((before, after)))
    }

    /// Sets the tenant's site address, title and logo, and turns it on or
    /// off: `MANAGE`. A site changed since `version` was read is refused.
    pub async fn set_site_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &bilibi_tenancy::Scope,
        input: &SiteInput,
    ) -> Result<Checked<Site>> {
        authorize_key(tx.sql(), scope, MANAGE).await?;
        let (address, title) = match Self::check_site(&input.address, &input.title) {
            Ok(checked) => checked,
            Err(errors) => return Ok(Err(errors)),
        };
        let tenant = scope.tenant();
        let before = tx
            .sql()
            .query_opt(
                &format!("SELECT {SITE_COLUMNS} FROM public_sites WHERE tenant_id=$1 FOR UPDATE"),
                &[&tenant],
            )
            .await?
            .map(|row| Site::from_row(&row));
        if let (Some(before), Some(version)) = (&before, input.version) {
            if before.version != version {
                return Ok(Err(vec![field(
                    "version",
                    "has changed since this edit began",
                )]));
            }
        }
        if Self::address_taken(tx.sql(), tenant, &address).await? {
            return Ok(Err(vec![field("address", "is used by another site")]));
        }
        let enabled = input
            .enabled
            .unwrap_or(before.as_ref().is_some_and(|site| site.enabled));
        let logo = match self
            .checked_logo(tx.sql(), scope, input.logo, before.as_ref())
            .await?
        {
            Ok(logo) => logo,
            Err(errors) => return Ok(Err(errors)),
        };
        let old = before.as_ref().map_or(json!({}), Site::audit);
        let saved = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    let row = sql
                        .query_one(
                            &format!("INSERT INTO public_sites(tenant_id,address,title,enabled,logo_asset_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT (tenant_id) DO UPDATE SET address=excluded.address,title=excluded.title,enabled=excluded.enabled,logo_asset_id=excluded.logo_asset_id,version=public_sites.version+1,updated_at=now() RETURNING {SITE_COLUMNS}"),
                            &[&tenant, &address, &title, &enabled, &logo],
                        )
                        .await
                        .map_err(|error| match error.code() {
                            Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => Error::Conflict,
                            _ => Error::from(error),
                        })?;
                    let site = Site::from_row(&row);
                    let change = Change {
                        kind: "public.site".into(),
                        id: tenant,
                        old,
                        new: site.audit(),
                    };
                    Ok((site, vec![change]))
                })
            })
            .await;
        let mut site = match saved {
            Ok(site) => site,
            Err(Error::Conflict) => {
                return Ok(Err(vec![field("address", "is used by another site")]))
            }
            Err(error) => return Err(error),
        };
        site.hostnames = hostnames(tx.sql(), tenant).await?;
        self.forget();
        Ok(Ok(site))
    }

    async fn check_hostname(
        &self,
        client: &(impl GenericClient + Sync),
        tenant: i64,
        hostname: &str,
    ) -> Result<Checked<String>> {
        let hostname = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
        if hostname.is_empty() {
            return Ok(Err(vec![field("hostname", "can't be blank")]));
        }
        if !hostname_shaped(&hostname) {
            return Ok(Err(vec![field(
                "hostname",
                "must be a name such as shop.example.com, without https:// or a port",
            )]));
        }
        if self.staff_host.as_deref() == Some(hostname.as_str()) {
            return Ok(Err(vec![field(
                "hostname",
                "is the staff site's own name; the public site needs another",
            )]));
        }
        if client
            .query_opt("SELECT 1 FROM public_sites WHERE tenant_id=$1", &[&tenant])
            .await?
            .is_none()
        {
            return Ok(Err(vec![field(
                "hostname",
                "needs the site's address and title first",
            )]));
        }
        if client
            .query_opt(
                "SELECT 1 FROM public_site_hostnames WHERE hostname=$1 AND tenant_id<>$2",
                &[&hostname, &tenant],
            )
            .await?
            .is_some()
        {
            return Ok(Err(vec![field("hostname", "is used by another site")]));
        }
        Ok(Ok(hostname))
    }

    /// The hostname as an add would leave it: `MANAGE`, the same checks.
    pub async fn preview_hostname(
        &self,
        client: &(impl GenericClient + Sync),
        scope: &bilibi_tenancy::Scope,
        hostname: &str,
        primary: bool,
    ) -> Result<Checked<Hostname>> {
        authorize_key(client, scope, MANAGE).await?;
        let name = match self
            .check_hostname(client, scope.tenant(), hostname)
            .await?
        {
            Ok(name) => name,
            Err(errors) => return Ok(Err(errors)),
        };
        let first = hostnames(client, scope.tenant())
            .await?
            .iter()
            .all(|existing| existing.name == name);
        Ok(Ok(Hostname {
            name,
            primary: primary || first,
        }))
    }

    /// Adds a hostname to the tenant's site, or makes one it has primary:
    /// `MANAGE`. The first hostname is primary; a new primary one takes
    /// over from the old.
    pub async fn add_hostname_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &bilibi_tenancy::Scope,
        hostname: &str,
        primary: bool,
    ) -> Result<Checked<Hostname>> {
        authorize_key(tx.sql(), scope, MANAGE).await?;
        let tenant = scope.tenant();
        // Writes to one site's hostnames wait for each other.
        tx.sql()
            .query_opt(
                "SELECT 1 FROM public_sites WHERE tenant_id=$1 FOR UPDATE",
                &[&tenant],
            )
            .await?;
        let name = match self.check_hostname(tx.sql(), tenant, hostname).await? {
            Ok(name) => name,
            Err(errors) => return Ok(Err(errors)),
        };
        let before = hostnames(tx.sql(), tenant).await?;
        let primary = primary || before.iter().all(|existing| existing.name == name);
        let existing = before.iter().find(|h| h.name == name).cloned();
        if existing.as_ref().is_some_and(|h| h.primary || !primary) {
            return Ok(Ok(existing.expect("found")));
        }
        let old = json!({"hostnames": before.iter().map(|h| json!({"hostname": h.name, "primary": h.primary})).collect::<Vec<_>>()});
        let added = name.clone();
        let saved = tx
            .mutate(move |sql| {
                Box::pin(async move {
                    if primary {
                        sql.execute(
                            "UPDATE public_site_hostnames SET is_primary=false WHERE tenant_id=$1 AND is_primary",
                            &[&tenant],
                        )
                        .await?;
                    }
                    sql.execute(
                        "INSERT INTO public_site_hostnames(hostname,tenant_id,is_primary) VALUES($1,$2,$3) ON CONFLICT (hostname) DO UPDATE SET is_primary=excluded.is_primary WHERE public_site_hostnames.tenant_id=excluded.tenant_id",
                        &[&added, &tenant, &primary],
                    )
                    .await
                    .map_err(|error| match error.code() {
                        Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => Error::Conflict,
                        _ => Error::from(error),
                    })?;
                    let after = hostnames(sql, tenant).await?;
                    let change = Change {
                        kind: "public.site".into(),
                        id: tenant,
                        old,
                        new: json!({"hostnames": after.iter().map(|h| json!({"hostname": h.name, "primary": h.primary})).collect::<Vec<_>>()}),
                    };
                    Ok((Hostname { name: added, primary }, vec![change]))
                })
            })
            .await;
        self.forget();
        match saved {
            Ok(hostname) => Ok(Ok(hostname)),
            Err(Error::Conflict) => Ok(Err(vec![field("hostname", "is used by another site")])),
            Err(error) => Err(error),
        }
    }

    /// Removes a hostname from the tenant's site: `MANAGE`. The primary one
    /// goes only when it is the last, so emailed links always have an
    /// address while the site has any.
    pub async fn remove_hostname_in(
        &self,
        tx: &mut MutationTransaction<'_>,
        scope: &bilibi_tenancy::Scope,
        hostname: &str,
    ) -> Result<Checked<Hostname>> {
        authorize_key(tx.sql(), scope, MANAGE).await?;
        let tenant = scope.tenant();
        tx.sql()
            .query_opt(
                "SELECT 1 FROM public_sites WHERE tenant_id=$1 FOR UPDATE",
                &[&tenant],
            )
            .await?;
        let name = hostname.trim().to_ascii_lowercase();
        let before = hostnames(tx.sql(), tenant).await?;
        let Some(found) = before.iter().find(|h| h.name == name).cloned() else {
            return Err(Error::NotFound);
        };
        if found.primary && before.len() > 1 {
            return Ok(Err(vec![field(
                "hostname",
                "is the primary one: make another primary first",
            )]));
        }
        let old = json!({"hostnames": before.iter().map(|h| json!({"hostname": h.name, "primary": h.primary})).collect::<Vec<_>>()});
        let removed = found.clone();
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "DELETE FROM public_site_hostnames WHERE hostname=$1 AND tenant_id=$2",
                    &[&removed.name, &tenant],
                )
                .await?;
                let after = hostnames(sql, tenant).await?;
                let change = Change {
                    kind: "public.site".into(),
                    id: tenant,
                    old,
                    new: json!({"hostnames": after.iter().map(|h| json!({"hostname": h.name, "primary": h.primary})).collect::<Vec<_>>()}),
                };
                Ok(((), vec![change]))
            })
        })
        .await?;
        self.forget();
        Ok(Ok(found))
    }

    /// The site a request's `Host` names, as the scope the host seals for
    /// it: only a hostname of a site that is on, of a live tenant, with the
    /// public listener's `port` (`None`: the scheme's default). Anything else
    /// is `None`, one 404. Answers are reused for a few seconds; a write on
    /// this server forgets them.
    pub async fn resolve(&self, host: &str, port: Option<u16>) -> Result<Option<PublicScope>> {
        let Some(name) = host_name(host, port) else {
            return Ok(None);
        };
        if let Some(tenant) = self.cached(&name) {
            return Ok(tenant.map(|tenant| PublicScope { tenant }));
        }
        self.look_up(name).await
    }
    /// The public listener's lookup of a request's `Host` from `sender`:
    /// an answer read in the last few seconds is free; a database lookup
    /// first spends one of the sender's lookups
    /// (`public.lookups_per_minute`, `public.lookups_per_hour`), so a
    /// flood of made-up hostnames from one sender reaches the database a
    /// bounded number of times, whatever its path.
    pub async fn resolve_from(
        &self,
        host: &str,
        port: Option<u16>,
        sender: &str,
    ) -> Result<Reached> {
        let Some(name) = host_name(host, port) else {
            return Ok(Reached::Nothing);
        };
        let tenant = match self.cached(&name) {
            Some(tenant) => tenant,
            None => {
                if !self.charge_lookup(sender, self.lookup_policy().await?) {
                    return Ok(Reached::Busy);
                }
                return Ok(match self.look_up(name).await? {
                    Some(site) => Reached::Site(site),
                    None => Reached::Nothing,
                });
            }
        };
        Ok(match tenant {
            Some(tenant) => Reached::Site(PublicScope { tenant }),
            None => Reached::Nothing,
        })
    }
    /// A hostname's answer read in the last few seconds.
    fn cached(&self, name: &str) -> Option<Option<i64>> {
        self.resolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .filter(|(read, _)| read.elapsed() < crate::POLICY_FRESH)
            .map(|(_, tenant)| *tenant)
    }
    async fn look_up(&self, name: String) -> Result<Option<PublicScope>> {
        let client = self.db.get().await?;
        let tenant: Option<i64> = client
            .query_opt(
                "SELECT h.tenant_id FROM public_site_hostnames h JOIN public_sites s ON s.tenant_id=h.tenant_id JOIN tenants t ON t.id=s.tenant_id WHERE h.hostname=$1 AND s.enabled AND t.deleted_at IS NULL AND t.status='active'",
                &[&name],
            )
            .await?
            .map(|row| row.get(0));
        let mut resolved = self.resolved.lock().unwrap_or_else(|p| p.into_inner());
        if resolved.len() >= crate::RESOLVED_MAX {
            resolved.clear();
        }
        resolved.insert(name, (Instant::now(), tenant));
        Ok(tenant.map(|tenant| PublicScope { tenant }))
    }
    fn forget(&self) {
        self.resolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// The site a request reached, for its pages' brand and logo: public,
    /// since anyone on the site reads its title.
    pub async fn site(&self, scope: &PublicScope) -> Result<Option<Site>> {
        let client = self.db.get().await?;
        let Some(mut site) = site_row(&**client, scope.tenant).await? else {
            return Ok(None);
        };
        if !site.enabled {
            return Ok(None);
        }
        if let Some(asset) = site.logo_asset_id {
            site.logo_url = self
                .media
                .tenant_stream_url(&**client, site.tenant, asset)
                .await
                .ok();
        }
        Ok(Some(site))
    }

    /// Whether `asset` is the logo of the site a request reached: the one
    /// image the public site streams from Media.
    pub async fn logo_matches(&self, scope: &PublicScope, asset: i64) -> Result<bool> {
        let client = self.db.get().await?;
        Ok(client
            .query_opt(
                "SELECT 1 FROM public_sites WHERE tenant_id=$1 AND logo_asset_id=$2 AND enabled",
                &[&scope.tenant, &asset],
            )
            .await?
            .is_some())
    }

    /// The address of `tenant`'s public site for a link sent by email:
    /// `https://` and its primary hostname (with the listener's port when it
    /// is not the default). `None` while the site is off, has no hostname,
    /// or the installation serves no public site.
    pub async fn origin(
        &self,
        client: &dyn bilibi_tenancy::ReadClient,
        tenant: i64,
    ) -> Result<Option<String>> {
        let Some(scheme) = self.public_scheme.as_deref() else {
            return Ok(None);
        };
        let host: Option<String> = client
            .query(
                "SELECT h.hostname FROM public_site_hostnames h JOIN public_sites s ON s.tenant_id=h.tenant_id WHERE h.tenant_id=$1 AND h.is_primary AND s.enabled",
                &[&tenant],
            )
            .await?
            .first()
            .map(|row| row.get(0));
        Ok(host.map(|host| match self.public_port {
            Some(port) => format!("{scheme}://{host}:{port}"),
            None => format!("{scheme}://{host}"),
        }))
    }
}

impl Public {
    /// The deployment command's binding (`bilibi-web public-hostname`): gives
    /// `tenant`'s public site `hostname` as its primary one, making the site
    /// first when it has none (address `site-<tenant>`, the tenant's name as
    /// its title, off) and turning it on when `on`. Audited as the system
    /// principal `public-site`. Staff change everything else on the page.
    pub async fn bind_hostname(
        &self,
        mutations: &bilibi_database::MutationBoundary,
        tenant: i64,
        hostname: &str,
        on: bool,
    ) -> Result<Checked<Site>> {
        let mut client = self.db.get().await?;
        let name = match self
            .check_hostname_for_binding(&**client, tenant, hostname)
            .await?
        {
            Ok(name) => name,
            Err(errors) => return Ok(Err(errors)),
        };
        let mut tx = mutations
            .begin(
                &mut client,
                bilibi_database::Context::system(
                    tenant,
                    "public-site",
                    "base.public.hostname.bind",
                ),
            )
            .await?;
        let before = site_row(tx.sql(), tenant).await?;
        let title: String = tx
            .sql()
            .query_opt(
                "SELECT name FROM tenants WHERE id=$1 AND deleted_at IS NULL",
                &[&tenant],
            )
            .await?
            .map(|row| row.get(0))
            .ok_or(Error::NotFound)?;
        let old = before.as_ref().map_or(json!({}), |site| {
            json!({"site": site.audit(), "hostnames": site.hostnames.iter().map(|h| &h.name).collect::<Vec<_>>()})
        });
        let address = format!("site-{tenant}");
        let title: String = title.chars().take(TITLE_MAX).collect();
        tx.mutate(move |sql| {
            Box::pin(async move {
                sql.execute(
                    "INSERT INTO public_sites(tenant_id,address,title,enabled) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id) DO UPDATE SET enabled=public_sites.enabled OR excluded.enabled,version=public_sites.version+1,updated_at=now()",
                    &[&tenant, &address, &title, &on],
                )
                .await?;
                sql.execute(
                    "UPDATE public_site_hostnames SET is_primary=false WHERE tenant_id=$1 AND hostname<>$2 AND is_primary",
                    &[&tenant, &name],
                )
                .await?;
                sql.execute(
                    "INSERT INTO public_site_hostnames(hostname,tenant_id,is_primary) VALUES($1,$2,true) ON CONFLICT (hostname) DO UPDATE SET is_primary=true WHERE public_site_hostnames.tenant_id=excluded.tenant_id",
                    &[&name, &tenant],
                )
                .await?;
                let after = site_row(sql, tenant).await?.ok_or(Error::Internal)?;
                let change = Change {
                    kind: "public.site".into(),
                    id: tenant,
                    old,
                    new: json!({"site": after.audit(), "hostnames": after.hostnames.iter().map(|h| &h.name).collect::<Vec<_>>()}),
                };
                Ok(((), vec![change]))
            })
        })
        .await?;
        let site = site_row(tx.sql(), tenant).await?.ok_or(Error::Internal)?;
        tx.commit().await?;
        self.forget();
        Ok(Ok(site))
    }
    async fn check_hostname_for_binding(
        &self,
        client: &(impl GenericClient + Sync),
        tenant: i64,
        hostname: &str,
    ) -> Result<Checked<String>> {
        let name = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
        if !hostname_shaped(&name) {
            return Ok(Err(vec![field(
                "hostname",
                "must be a name such as shop.example.com, without https:// or a port",
            )]));
        }
        if self.staff_host.as_deref() == Some(name.as_str()) {
            return Ok(Err(vec![field("hostname", "is the staff site's own name")]));
        }
        if client
            .query_opt(
                "SELECT 1 FROM public_site_hostnames WHERE hostname=$1 AND tenant_id<>$2",
                &[&name, &tenant],
            )
            .await?
            .is_some()
        {
            return Ok(Err(vec![field("hostname", "is used by another site")]));
        }
        Ok(Ok(name))
    }
}
