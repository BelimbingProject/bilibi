//! A published update is a discovery result. Admission and owner approval belong to install.
use crate::{transport::Paths, Version};
use serde_json::{json, Value};
use std::fs;

pub(super) fn version(tag: &str) -> Option<Version> {
    let parsed = Version::parse(tag.strip_prefix("setup-v")?)?;
    (tag == format!("setup-v{parsed}")).then_some(parsed)
}

/// Keep the newest stable setup even when its target file is missing: an older
/// usable file must not turn a missing publication into "up to date".
pub(super) fn latest(releases: &[Value]) -> Value {
    let Some((version, release)) = releases
        .iter()
        .filter(|r| r["draft"] == false && r["prerelease"] == false)
        .filter_map(|r| Some((version(r["tag_name"].as_str()?)?, r)))
        .max_by_key(|(v, _)| *v)
    else {
        return Value::Null;
    };
    let body = release["body"].as_str().unwrap_or_default().trim();
    // App Maker's default publication paragraph describes the archive, not changes.
    // Keep any owner-written notes following it, including on existing releases.
    let boilerplate = "Verify SHA256SUMS and recipe.json. Install the bilibi-setup archive through Software upload. The installer and server tools are separate bootstrap assets. Checksums are not signatures.";
    let owner_notes = body
        .strip_prefix("Built from ")
        .and_then(|rest| {
            let (commit, rest) = rest.split_once(". ")?;
            (commit.len() == 40 && commit.bytes().all(|c| c.is_ascii_hexdigit()))
                .then(|| rest.strip_prefix(boilerplate))
                .flatten()
        })
        .unwrap_or(body)
        .trim();
    let notes: String = owner_notes.chars().take(1600).collect();
    json!({"version": version.to_string(), "tag": release["tag_name"], "notes": notes})
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
    let current = setup["tag"].as_str().and_then(version);
    let mut result = json!({"state":"unavailable","current":current.map(|v|v.to_string()),"checked_at":listing["fetched_at"]});
    let reason = if listing["repository"].as_str().is_none() {
        "source_not_set"
    } else if listing["stale"] == true || listing["available"] != true {
        "check_failed"
    } else if listing["accepted"] != true || listing["identity_changed"] == true {
        "source_not_accepted"
    } else if listing["complete"] != true {
        "incomplete_list"
    } else {
        let latest = &listing["latest"];
        let Some((tag, published)) = latest["tag"]
            .as_str()
            .and_then(|tag| Some((tag, version(tag)?)))
        else {
            result["reason"] = json!("no_stable_release");
            return result;
        };
        result["release"] = latest.clone();
        let Some(current) = current.filter(|_| setup["repository"] == listing["repository"]) else {
            result["reason"] = json!("unknown_installed_release");
            return result;
        };
        if published <= current {
            result["state"] = json!("current");
            return result;
        }
        let mut rows = listing["releases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["tag"] == tag && r["prerelease"] == false);
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
