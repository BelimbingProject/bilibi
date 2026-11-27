//! What a release is made of, read from the text its own program prints for
//! `bilibi-web status` (`bilibi_web::status`), and what differs between two
//! releases. The installer cannot link the program it installs, so it reads
//! the text; a line it does not understand is skipped.
use serde_json::{json, Value};

/// `{"platform": {"version", "release", "commit"}, "domains": [...],
/// "extensions": [...]}`, each container `{"id", "version", "commit"}`.
pub fn parse(status: &str) -> Value {
    let mut lines = status.lines();
    let platform = lines.next().and_then(platform).unwrap_or(Value::Null);
    let (mut domains, mut extensions) = (Vec::new(), Vec::new());
    let mut section = 0;
    for line in lines {
        if !line.starts_with(' ') {
            section = match line.split_once(':') {
                Some(("Domains", rest)) if rest.trim().is_empty() => 1,
                Some(("Extensions", rest)) if rest.trim().is_empty() => 2,
                _ => 0,
            };
        } else if let Some(container) = container(line) {
            match section {
                1 => domains.push(container),
                2 => extensions.push(container),
                _ => {}
            }
        }
    }
    json!({"platform": platform, "domains": domains, "extensions": extensions})
}

pub fn admit(status: &str, mode: &str) -> Result<Value, String> {
    if !matches!(mode, "shared" | "isolated") {
        return Err("The saved installation.mode is invalid.".into());
    }
    let candidate = parse(status);
    if candidate["platform"].is_null() {
        return Err("The release does not report its Platform composition.".into());
    }
    let (_, extensions) = status
        .split_once("\nExtensions:")
        .ok_or("The release does not report its Extension composition.")?;
    if extensions.trim() == "none mounted" {
        return Ok(candidate);
    }
    if !extensions.starts_with('\n') || extensions.trim().is_empty() {
        return Err("The release reports an invalid Extension composition.".into());
    }
    for line in extensions.lines().filter(|line| !line.is_empty()) {
        let extension = line
            .starts_with("  ")
            .then(|| container(line))
            .flatten()
            .ok_or("The release reports an invalid Extension declaration.")?;
        let id = extension["id"].as_str().unwrap_or("unknown");
        let code = &extension["code"];
        match if code.is_null() { Some("customer") } else { code.as_str() } {
            Some("reviewed") => {}
            Some("customer") if mode == "isolated" => {}
            Some("customer") => return Err(format!("Extension {id} requires an isolated installation; use an isolated installation, or prepare a code-free release.")),
            _ => return Err(format!("Extension {id} must declare reviewed or customer code.")),
        }
    }
    Ok(candidate)
}

/// `Bilibi 0.1.1, release v0.1.1, commit abc` and its shorter forms.
fn platform(line: &str) -> Option<Value> {
    let mut parts = line.strip_prefix("Bilibi ")?.split(", ");
    let version = parts.next()?;
    let (mut release, mut commit) = (None, None);
    for part in parts {
        if let Some(tag) = part.strip_prefix("release ") {
            release = Some(tag);
        } else if let Some(sha) = part
            .strip_prefix("commit ")
            .or_else(|| part.strip_prefix("built from commit "))
        {
            commit = Some(sha);
        }
    }
    Some(json!({"version": version, "release": release, "commit": commit}))
}

/// `  connector 0.1.0, commit abc, code: reviewed, modules: a, b`, `  finance untagged,
/// commit abc, modules: a` or `  finance unpinned, modules: a`.
fn container(line: &str) -> Option<Value> {
    let (declaration, modules) = line.split_once(", modules:")?;
    let (id, rest) = declaration.trim_start().split_once(' ')?;
    let (rest, code) = rest
        .rsplit_once(", code:")
        .map_or((rest, None), |(rest, code)| {
            (rest, Some(code.strip_prefix(' ').unwrap_or("")))
        });
    let token = |value: &str| {
        !value.is_empty()
            && !value.contains(|character: char| {
                character.is_whitespace() || matches!(character, ',' | ':')
            })
    };
    let (version, commit) = match rest {
        "unpinned" => (None, None),
        _ => {
            let (version, commit) = rest.split_once(", commit ")?;
            if !token(version) || !token(commit) {
                return None;
            }
            (Some(version).filter(|v| *v != "untagged"), Some(commit))
        }
    };
    let modules = modules.trim();
    if !token(id) || (!modules.is_empty() && !modules.split(", ").all(token)) {
        return None;
    }
    Some(json!({"id": id, "version": version, "commit": commit, "code": code}))
}

/// What differs from one release to another: a Platform change as
/// `{"from", "to"}`, and each added, removed, updated, downgraded or moved
/// (same version, new commit) container. Null when either release's composition is not known.
pub fn diff(from: &Value, to: &Value) -> Value {
    if from["platform"].is_null() || to["platform"].is_null() {
        return Value::Null;
    }
    let platform = (from["platform"] != to["platform"]).then(|| {
        let (was, now) = (label(&from["platform"]), label(&to["platform"]));
        if was == now {
            json!({"from": with_commit(&from["platform"], &was),
                "to": with_commit(&to["platform"], &now)})
        } else {
            json!({"from": was, "to": now})
        }
    });
    let mut containers = Vec::new();
    for layer in ["domains", "extensions"] {
        let before = from[layer].as_array().map_or(&[][..], Vec::as_slice);
        let after = to[layer].as_array().map_or(&[][..], Vec::as_slice);
        for now in after {
            let was = before.iter().find(|old| old["id"] == now["id"]);
            let change = match was {
                None => "added",
                Some(old) if old == now => continue,
                Some(old) if old["version"] == now["version"] => "moved",
                Some(old) if older(&now["version"], &old["version"]) => "downgraded",
                Some(_) => "updated",
            };
            containers.push(json!({"layer": layer, "id": now["id"], "change": change,
                "from": was.map(|old| old["version"].clone()), "to": now["version"],
                "from_commit": was.map(|old| short(&old["commit"])), "to_commit": short(&now["commit"])}));
        }
        for old in before {
            if !after.iter().any(|now| now["id"] == old["id"]) {
                containers.push(json!({"layer": layer, "id": old["id"], "change": "removed",
                    "from": old["version"], "to": null}));
            }
        }
    }
    json!({"platform": platform, "containers": containers})
}

/// The changes in one plain sentence; empty when they are not known.
pub fn describe(changes: &Value) -> String {
    if changes.is_null() {
        return String::new();
    }
    let mut parts = Vec::new();
    if !changes["platform"].is_null() {
        parts.push(format!(
            "Bilibi {} to {}",
            changes["platform"]["from"].as_str().unwrap_or("?"),
            changes["platform"]["to"].as_str().unwrap_or("?")
        ));
    }
    for item in changes["containers"].as_array().into_iter().flatten() {
        let id = item["id"].as_str().unwrap_or("?");
        let at = |key: &str| item[key].as_str().unwrap_or("unreleased");
        let commit = |key: &str| item[key].as_str().unwrap_or("unknown");
        parts.push(match item["change"].as_str() {
            Some("moved") => match item["to"].as_str() {
                Some(version) => format!(
                    "{id} stays at {version}, new build {} (was {})",
                    commit("to_commit"),
                    commit("from_commit")
                ),
                None => format!(
                    "{id} (unreleased) changed from build {} to {}",
                    commit("from_commit"),
                    commit("to_commit")
                ),
            },
            Some("added") => format!("added {id} {}", at("to")),
            Some("removed") => format!("removed {id}"),
            Some("downgraded") => format!("moved {id} back from {} to {}", at("from"), at("to")),
            _ => format!("updated {id} from {} to {}", at("from"), at("to")),
        });
    }
    if parts.is_empty() {
        "The Bilibi and module versions stayed the same.".to_owned()
    } else {
        format!("Changed: {}.", parts.join("; "))
    }
}

/// `v0.1.1`, or `0.1.1 (commit 1a2b3c4)` when the build has no release tag.
fn label(platform: &Value) -> String {
    match platform["release"].as_str() {
        Some(tag) => tag.to_owned(),
        None => with_commit(platform, platform["version"].as_str().unwrap_or("?")),
    }
}

fn with_commit(platform: &Value, label: &str) -> String {
    format!("{label} (build {})", short(&platform["commit"]))
}

/// The first seven characters of a commit, or `unknown`.
fn short(commit: &Value) -> String {
    commit
        .as_str()
        .map_or("unknown", |sha| &sha[..sha.len().min(7)])
        .to_owned()
}

/// Whether `a` is a lower version than `b`, comparing their numbers; a
/// version that is not numbers is never lower.
fn older(a: &Value, b: &Value) -> bool {
    let numbers = |version: &Value| -> Option<Vec<u64>> {
        version
            .as_str()?
            .split('.')
            .map(|part| {
                part.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .collect()
    };
    matches!((numbers(a), numbers(b)), (Some(a), Some(b)) if a < b)
}

/// `bilibi-web status` on a release folder: it proves the program runs on
/// this machine, and its text names what the release holds.
pub fn status(dir: &std::path::Path) -> Result<String, String> {
    let out = crate::layout::release_command("30")?
        .arg(dir.join("bilibi-web"))
        .arg("status")
        .output()
        .map_err(|error| format!("could not run the copied program: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "the copied program does not run on this server: {}",
            crate::backup::tail(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
