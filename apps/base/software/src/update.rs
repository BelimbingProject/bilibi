//! A published update is a discovery result. Admission and owner approval belong to install.
use crate::{Software, Version};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::fs;

fn version(tag: &str) -> Option<Version> {
    let parsed = Version::parse(tag.strip_prefix("setup-v")?)?;
    (tag == format!("setup-v{parsed}")).then_some(parsed)
}

/// Keep the newest stable setup even when its target file is missing: an older
/// usable file must not turn a missing publication into "up to date".
pub(super) fn latest(releases: &[Value]) -> Option<String> {
    releases
        .iter()
        .filter(|r| r["draft"] == false && r["prerelease"] == false)
        .filter_map(|r| version(r["tag_name"].as_str()?))
        .max()
        .map(|version| format!("setup-v{version}"))
}

impl Software {
    /// Select the terminal's exact stable release asset before install review.
    pub async fn terminal_update(
        &self,
        scope: &Scope,
        wanted: Option<&str>,
    ) -> Result<(Value, Value)> {
        let client = self.db.get().await?;
        crate::authorize_operator(&**client, scope, crate::VIEW).await?;
        let paths = self.paths().map_err(Error::refused)?;
        let policy = crate::release_proof::Policy::load(paths).map_err(Error::refused)?;
        let listing = self
            .github_listing(&**client, scope, true, false, &policy)
            .await?;
        let asset = select(paths, &listing, wanted).map_err(Error::refused)?;
        Ok((listing, asset))
    }
}

fn select(
    paths: &crate::transport::Paths,
    listing: &Value,
    wanted: Option<&str>,
) -> std::result::Result<Value, &'static str> {
    let repository = listing["repository"]
        .as_str()
        .ok_or("Release trust is not set.")?;
    if listing["stale"] == true || listing["available"] != true {
        return Err("A fresh release listing is unavailable.");
    }
    if listing["accepted"] != true || listing["identity_changed"] == true {
        return Err("The release repository identity is not accepted.");
    }
    if listing["complete"] != true {
        return Err("GitHub returned an incomplete release listing.");
    }
    let tag = if let Some(wanted) = wanted {
        let parsed = Version::parse(wanted)
            .filter(|parsed| parsed.to_string() == wanted)
            .ok_or("Enter a stable setup version in X.Y.Z format.")?;
        format!("setup-v{parsed}")
    } else {
        let tag = listing["latest"]
            .as_str()
            .ok_or("There is no published stable setup release.")?;
        let record = paths
            .current()
            .and_then(|n| fs::read(paths.release(n).join("release.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let setup = &record.as_ref().unwrap_or(&Value::Null)["setup"];
        let current = setup["tag"]
            .as_str()
            .and_then(version)
            .filter(|_| setup["repository"].as_str() == Some(repository))
            .ok_or("The running setup version for the pinned repository is unknown. Specify --to X.Y.Z.")?;
        if version(tag).is_none_or(|published| published <= current) {
            return Err("There is no newer release to install.");
        }
        tag.to_owned()
    };
    let mut rows = listing["releases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["tag"] == tag && row["prerelease"] == false);
    match (rows.next(), rows.next()) {
        (Some(row), None) => Ok(json!({
            "repository_id": listing["repository_id"],
            "release_id": row["release_id"],
            "asset": row["asset_id"],
            "tag": tag,
            "target": row["target"],
            "digest": row["digest"],
            "switch": "auto",
        })),
        (None, _) => Err("The requested stable setup has no compatible release file."),
        (_, Some(_)) => {
            Err("The requested stable setup has more than one compatible release file.")
        }
    }
}
