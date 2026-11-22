use std::{
    path::Path,
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

use bilibi_base::{datetime::DisplayContext, system::Role};
use bilibi_ui::VersionBadge;

pub(super) fn application_version(
    role: Role,
    platform_version: &str,
    display: &DisplayContext,
) -> Option<VersionBadge> {
    if role == Role::Dev {
        let mut text = format!("v{platform_version}");
        let mut title = "Development build".to_owned();
        if working_tree_dirty() {
            text.push_str(" *");
            title.push_str(" · uncommitted changes");
        }
        return Some(VersionBadge { text, title });
    }

    let exe = std::env::current_exe().ok()?;
    let bytes = std::fs::read(exe.parent()?.join("release.json")).ok()?;
    let record: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let setup = &record["setup"];
    let version = setup["tag"].as_str()?.strip_prefix("setup-")?;
    let date = setup["released"]
        .as_str()?
        .parse::<jiff::civil::Date>()
        .ok()?;
    Some(VersionBadge {
        text: version.to_owned(),
        title: format!("Application release · {}", display.render_date(date)),
    })
}

fn working_tree_dirty() -> bool {
    static DIRTY: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut cache = DIRTY.lock().expect("dirty state cache");
    if let Some((checked, dirty)) = *cache {
        if checked.elapsed() < Duration::from_secs(2) {
            return dirty;
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dirty = |path: &Path| {
        Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(path)
            .args(["status", "--porcelain"])
            .output()
            .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
    };
    let changed = dirty(&root)
        || ["domains", "extensions"].iter().any(|layer| {
            std::fs::read_dir(root.join("apps").join(layer))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .any(|path| path.join(".git").exists() && dirty(&path))
        });
    *cache = Some((Instant::now(), changed));
    changed
}
