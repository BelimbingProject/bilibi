//! A published update is a discovery result. Admission and owner approval belong to install.
use crate::{transport::Paths, Software, Version};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::fs;

/// Keep the newest release in the chosen channel even when its target file is missing.
pub(super) fn latest<'a>(releases: &'a [Value], minimum: &str) -> Option<&'a Value> {
    releases
        .iter()
        .filter(|r| r["draft"] == false)
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            if minimum.starts_with("setup-") && r["prerelease"] != false {
                return None;
            }
            Some((crate::release_proof::policy_order(minimum, tag).ok()?, r))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, release)| release)
}

fn version(tag: &str) -> Option<Version> {
    let parsed = Version::parse(tag.strip_prefix("setup-v")?)?;
    (tag == format!("setup-v{parsed}")).then_some(parsed)
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
        if version(&policy.minimum_tag).is_none() {
            return Err(Error::refused(
                "Terminal updates require a stable setup release source.",
            ));
        }
        let listing = self
            .github_listing(&**client, Some(scope), true, false, &policy)
            .await?;
        let asset = select(paths, &listing, wanted).map_err(Error::refused)?;
        Ok((listing, asset))
    }
}

fn select(
    paths: &Paths,
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

pub(super) fn discover(paths: Option<&Paths>, listing: &Value) -> Value {
    let Some(paths) = paths else {
        return json!({"state":"source_checkout"});
    };
    let record = paths
        .current()
        .and_then(|n| fs::read(paths.release(n).join("release.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let setup = &record.as_ref().unwrap_or(&Value::Null)["setup"];
    let current = setup["tag"]
        .as_str()
        .and_then(crate::release_proof::tag_order);
    let mut result = json!({"state":"unavailable","current":setup["tag"].as_str().map(|tag| tag.strip_prefix("setup-").unwrap_or(tag)),"checked_at":listing["fetched_at"]});
    let Some(repository) = listing["repository"].as_str() else {
        result["reason"] = json!("source_not_set");
        return result;
    };
    let reason = if listing["stale"] == true || listing["available"] != true {
        "check_failed"
    } else if listing["accepted"] != true || listing["identity_changed"] == true {
        "source_not_accepted"
    } else if listing["complete"] != true {
        "incomplete_list"
    } else {
        let latest = &listing["latest"];
        let Some((tag, (channel, published))) = latest
            .as_str()
            .and_then(|tag| Some((tag, crate::release_proof::tag_order(tag)?)))
        else {
            result["reason"] = json!("no_stable_release");
            return result;
        };
        let mut release = listing["releases"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| matches_release(row, tag))
            .cloned()
            .unwrap_or_else(|| json!({"tag": tag}));
        release["notes"] = listing["latest_notes"].clone();
        result["release"] = release;
        let Some(current) = current.filter(|_| {
            setup["repository"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(repository))
        }) else {
            result["reason"] = json!("unknown_installed_release");
            return result;
        };
        let (current_channel, current) = current;
        if current_channel != channel {
            result["reason"] = json!("unknown_installed_release");
            return result;
        }
        if published <= current {
            result["state"] = json!("current");
            return result;
        }
        let mut rows = listing["releases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| matches_release(row, tag));
        match (rows.next(), rows.next()) {
            (None, _) => "no_matching_file",
            (_, Some(_)) => "ambiguous_files",
            (Some(row), None) => {
                result["state"] = json!("ready");
                result["asset"] = json!({"repository_id":listing["repository_id"],"release_id":row["release_id"],"asset":row["asset_id"],"tag":tag,"target":row["target"],"digest":row["digest"]});
                return result;
            }
        }
    };
    result["reason"] = json!(reason);
    result
}

fn matches_release(row: &Value, tag: &str) -> bool {
    row["tag"] == tag && (tag.starts_with("nightly-") || row["prerelease"] == false)
}
