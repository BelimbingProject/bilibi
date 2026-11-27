//! Hostname resolution: the public listener finds a request's tenant by its
//! `Host` alone. A site that is off, an unknown hostname and a tenant that
//! is not live are the same 404. Answers are reused for a few seconds per
//! server, and a database lookup on a request first spends its sender's
//! lookup budget, so made-up hostnames are bounded before they reach the
//! database.
use crate::{sites::hostname_shaped, Public, PublicScope};
use bilibi_database::Result;
use std::time::Instant;

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

impl Public {
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
    /// Forgets every answer: a write on this server changed a site.
    pub(crate) fn forget(&self) {
        self.resolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}
