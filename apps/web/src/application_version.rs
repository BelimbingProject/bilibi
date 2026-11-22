//! The identity displayed in the shell.

/// The package beside the running executable describes the assembled application.
pub(super) fn application_version(
    role: bilibi_base::system::Role,
    display: &bilibi_base::datetime::DisplayContext,
) -> Vec<bilibi_ui::VersionBadge> {
    let composition = crate::composition();
    let mut badges = bilibi_ui::version_badges(&composition);
    let badge = &mut badges[0];
    let setup = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("release.json")))
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|record| record.get("setup").cloned());
    let tag = setup.as_ref().and_then(|setup| setup["tag"].as_str());
    let version = tag.and_then(|tag| tag.strip_prefix("setup-"));
    if role == bilibi_base::system::Role::Dev {
        badge.title = "Development build".into();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        static DIRTY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DIRTY.get_or_init(|| {
            let dirty = |path: &std::path::Path| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(path)
                    .args(["status", "--porcelain"])
                    .output()
                    .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
            };
            dirty(&root)
                || ["domains", "extensions"].iter().any(|layer| {
                    std::fs::read_dir(root.join("apps").join(layer))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|entry| entry.path())
                        .any(|path| path.join(".git").exists() && dirty(&path))
                })
        }) {
            badge.text.push_str(" *");
            badge.title.push_str(" · uncommitted changes");
        }
        return badges;
    }
    if let Some(version) = version {
        badge.text = version.to_owned();
        badge.title = "Application release".into();
        if let Some(date) = setup
            .as_ref()
            .and_then(|setup| setup["released"].as_str())
            .and_then(|date| date.parse::<jiff::civil::Date>().ok())
        {
            badge
                .title
                .push_str(&format!(" · {}", display.render_date(date)));
        }
    } else {
        badge.text = composition.short_commit().unwrap_or("dev").into();
        badge.title = "Unreleased application build".into();
    }
    badges
}
