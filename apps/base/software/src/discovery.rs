//! Catalogue guidance for local composition.
//! Both the Software page and the MCP card read these same facts.
use crate::catalogue::{requirement_holds, Catalogue, Version};
use bilibi_module_registry::Composition;
use serde_json::{json, Value};

pub const NEXT_STEP: &str =
    "Run the installation commands in a Bilibi code folder, test the build, then install its release here.";

/// Official modules live under this organisation. Releases of these are
/// checked against every Platform release, so they only need a plain status.
const OFFICIAL: &str = "https://github.com/BelimbingProject/";

pub fn catalogue(catalogue: &Catalogue, made: &Composition) -> Value {
    let installed = crate::review::installed_json(made);
    let mut value = catalogue.json();
    value["running_platform"] = json!({"version": made.version, "commit": made.commit});
    value["next_step"] = json!(NEXT_STEP);
    for kind in ["domains", "extensions"] {
        for item in value[kind].as_array_mut().into_iter().flatten() {
            let id = item["id"].as_str().unwrap_or_default().to_owned();
            let repository = item["repository"].as_str().unwrap_or_default().to_owned();
            let mut choices = Vec::new();
            let mut newest_fit: Option<(Version, String)> = None;
            let mut newest_any: Option<(Version, String)> = None;
            for release in item["versions"].as_array_mut().into_iter().flatten() {
                let version = release["version"].as_str().unwrap_or_default().to_owned();
                let commit = release["commit"].as_str().unwrap_or_default().to_owned();
                let fits = release["platform"].as_str().and_then(|requirement| {
                    Version::parse(made.version).and_then(|v| requirement_holds(requirement, v))
                });
                // A CI pair proves these commits, not just two version labels.
                let tested = kind == "domains"
                    && catalogue.tested.iter().any(|set| {
                        set.domain.id == id
                            && set.domain.commit == commit
                            && set.domain.version.as_deref() == Some(&version)
                            && set.platform.version.as_deref() == Some(made.version)
                            && made.commit.is_some_and(|c| c == set.platform.commit)
                    });
                release["fits_running"] = json!(fits);
                release["tested_with_running"] = json!(tested);
                if let Some(parsed) = Version::parse(&version) {
                    if newest_any.as_ref().is_none_or(|(n, _)| parsed > *n) {
                        let needs = release["platform"].as_str().unwrap_or_default().to_owned();
                        newest_any = Some((parsed, needs));
                    }
                    if fits == Some(true) && newest_fit.as_ref().is_none_or(|(n, _)| parsed > *n) {
                        newest_fit = Some((parsed, version.clone()));
                    }
                }
                if fits == Some(true) {
                    if let Some(parsed) = Version::parse(&version) {
                        choices.push((tested, parsed, version, commit));
                    }
                }
            }
            choices.sort();
            let chosen = choices.pop();
            item["recommended_version"] = json!(chosen.as_ref().map(|c| &c.2));
            // Why nothing fits, when nothing does: the newest release's own
            // requirement, or that no release names this Bilibi.
            let unfit = match &newest_any {
                Some((_, needs)) if !needs.is_empty() => format!("Needs Bilibi {needs}"),
                _ => format!("No published version supports Bilibi {}", made.version),
            };
            let running = installed[kind]
                .as_array()
                .into_iter()
                .flatten()
                .find(|d| d["id"] == id.as_str());
            item["compatibility"] = json!(match (running, &chosen) {
                (Some(d), _) if d["version"].is_null() && d["branch"] == "main" =>
                    "Built from main for this release".to_owned(),
                (_, Some((true, ..))) => format!("Tested with Bilibi {}", made.version),
                (_, Some((false, ..))) => format!("Works with Bilibi {}", made.version),
                _ => unfit.clone(),
            });
            // Official modules are kept in step with every Platform release,
            // so they also get one plain status.
            if repository.starts_with(OFFICIAL) {
                let now = installed[kind]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|d| d["id"] == id.as_str());
                let status = match (now, &newest_fit) {
                    (None, _) => "not_installed",
                    (Some(d), Some((latest, _)))
                        if d["version"]
                            .as_str()
                            .and_then(Version::parse)
                            .is_some_and(|have| have < *latest) =>
                    {
                        "update_available"
                    }
                    _ => "installed",
                };
                let warning = newest_fit.is_none().then_some(unfit);
                item["status"] = json!(status);
                item["latest_version"] = json!(newest_fit.map(|(_, name)| name));
                item["warning"] = json!(warning);
            }
            item["official"] = json!(repository.starts_with(OFFICIAL));
            item["next_step"] = json!(NEXT_STEP);
            item["local_steps"] = json!(chosen
                .and_then(|(_, _, _, commit)| { local_steps(&repository, &id, kind, &commit) }));
        }
    }
    value
}

fn local_steps(repository: &str, id: &str, kind: &str, commit: &str) -> Option<Vec<String>> {
    // The catalogue is remote data. Only official repositories, safe folder
    // names and full commit IDs may enter commands an operator will run.
    let repo = repository.strip_prefix("https://github.com/BelimbingProject/")?;
    if repo.is_empty()
        || !repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        || id.is_empty()
        || id.len() > 64
        || id == "src"
        || !id.as_bytes()[0].is_ascii_lowercase()
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        || commit.len() != 40
        || !commit.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(vec![
        "(set -e".into(),
        format!("./bilibi add {id} --kind {kind} --source {repository} --commit {commit}"),
        "python3 scripts/discover-modules.py".into(),
        "./scripts/build-assets.sh --check".into(),
        "cargo metadata --format-version 1 > /dev/null".into(),
        "python3 scripts/discover-modules.py --pin".into(),
        "cargo build --release --locked -p bilibi-web".into(),
        "./target/release/bilibi-web status".into(),
        ")".into(),
    ])
}

/// Installed containers absent from the selected catalogue. An unavailable
/// catalogue has no entries to match; composition still proves installation.
pub fn uncatalogued(catalogue: &Value, made: &Composition) -> Value {
    let installed = crate::review::installed_json(made);
    let mut result = json!({});
    for kind in ["domains", "extensions"] {
        result[kind] = json!(installed[kind]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|container| !catalogue[kind]
                .as_array()
                .into_iter()
                .flatten()
                .any(|entry| entry["id"] == container["id"]))
            .collect::<Vec<_>>());
    }
    result
}
