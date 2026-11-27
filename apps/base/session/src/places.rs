//! Optional city-and-country lookup from an operator-supplied local MMDB file.
use bilibi_settings::Settings;
use bilibi_tenancy::ReadClient;
use maxminddb::Reader;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio::sync::Mutex;

pub const DATABASE_PATH_KEY: &str = "session.ip_place_database_path";
const MAX_DATABASE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_COMPONENT_BYTES: usize = 128;

#[derive(Clone, Eq, PartialEq)]
struct Fingerprint {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

struct CachedDatabase {
    fingerprint: Fingerprint,
    reader: Reader<Vec<u8>>,
}

pub(crate) struct PlaceLookup {
    settings: Arc<Settings>,
    cached: Mutex<Option<CachedDatabase>>,
}

impl PlaceLookup {
    pub(crate) fn new(settings: Arc<Settings>) -> Self {
        Self {
            settings,
            cached: Mutex::new(None),
        }
    }

    pub(crate) async fn find(
        &self,
        client: &dyn ReadClient,
        address: Option<&str>,
    ) -> Option<String> {
        let address = address?.parse::<IpAddr>().ok()?.to_canonical();
        if !is_public(address) {
            return None;
        }
        let setting = self
            .settings
            .get_global(client, DATABASE_PATH_KEY)
            .await
            .ok()?;
        let path = setting.as_str()?.trim();
        if path.is_empty() || !valid_database_path(path) {
            return None;
        }
        let path = PathBuf::from(path);
        let file_fingerprint = fingerprint(&path).await?;
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_none_or(|database| database.fingerprint != file_fingerprint)
        {
            let bytes = tokio::fs::read(&path).await.ok()?;
            if fingerprint(&path).await? != file_fingerprint {
                return None;
            }
            let reader = Reader::from_source(bytes).ok()?;
            *cached = Some(CachedDatabase {
                fingerprint: file_fingerprint,
                reader,
            });
        }
        place(cached.as_ref()?.reader.lookup(address).ok()?)
    }
}

async fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_DATABASE_BYTES {
        return None;
    }
    Some(Fingerprint {
        path: path.to_owned(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn place(result: maxminddb::LookupResult<'_, Vec<u8>>) -> Option<String> {
    let city: Option<String> = result
        .decode_path(&maxminddb::path!["city", "names", "en"])
        .ok()?
        .and_then(component);
    let country: Option<String> = result
        .decode_path(&maxminddb::path!["country", "names", "en"])
        .ok()?
        .and_then(component);
    match (city, country) {
        (Some(city), Some(country)) if city != country => Some(format!("{city}, {country}")),
        (Some(city), _) => Some(city),
        (_, Some(country)) => Some(country),
        _ => None,
    }
}

fn component(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_COMPONENT_BYTES
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

pub(crate) fn valid_database_path(path: &str) -> bool {
    path.is_empty()
        || (path.len() <= 4_096
            && Path::new(path).is_absolute()
            && !path.chars().any(char::is_control))
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || (first == 100 && (64..=127).contains(&second))
        || (first == 198 && matches!(second, 18 | 19))
        || first >= 240)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || address.is_multicast()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}
