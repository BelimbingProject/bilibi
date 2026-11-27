//! Native reference walkthrough: one release trust policy's tag channel and
//! its replay record, through the private rules the server applies.
//!
//! A policy pinned to `setup-vX.Y.Z` tags accepts only setup releases, and one
//! pinned to `nightly-YYYY-MM-DD` accepts only nightlies: no tag is ever read
//! as newer or older across the two. The replay record keeps one highest
//! admitted tag and digest per channel, so an older nightly is refused even
//! after a setup release was admitted, and the reverse.
//!
//! Run with an empty disposable DATABASE_URL, migrated:
//!   cargo run -p bilibi-software --example release-proof-walkthrough --locked
#[path = "../src/release_proof.rs"]
#[expect(
    dead_code,
    reason = "the walkthrough uses the tag and replay rules, not the whole module"
)]
mod release_proof;
// `release_proof` reads the instance folder through this, so it is here too.
#[path = "../src/transport.rs"]
#[expect(
    dead_code,
    reason = "the walkthrough uses the instance paths, not the whole module"
)]
mod transport;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The tag rule: one channel per policy, never mixed, ordered inside it.
    assert_eq!(
        release_proof::policy_order("setup-v1.0.0", "setup-v1.0.1"),
        Ok(release_proof::Order::Setup(1, 0, 1))
    );
    assert!(
        release_proof::policy_order("setup-v1.0.0", "setup-v0.9.9").unwrap()
            < release_proof::Order::Setup(1, 0, 0)
    );
    let mixed = release_proof::policy_order("setup-v1.0.0", "nightly-2026-11-25").unwrap_err();
    assert!(mixed.contains("accepts only setup-vX.Y.Z"), "{mixed}");
    assert_eq!(
        release_proof::policy_order("nightly-2026-11-01", "nightly-2026-11-02"),
        Ok(release_proof::Order::Nightly(2026, 11, 2))
    );
    let mixed = release_proof::policy_order("nightly-2026-11-01", "setup-v9.9.9").unwrap_err();
    assert!(mixed.contains("accepts only nightly-YYYY-MM-DD"), "{mixed}");
    // A nightly date must be real, and the tag canonical.
    assert!(release_proof::policy_order("nightly-2026-11-01", "nightly-2026-02-30").is_err());
    assert!(release_proof::policy_order("nightly-2026-11-01", "nightly-2026-1-01").is_err());
    assert!(release_proof::policy_order("setup-v1.0.0", "setup-v1.0").is_err());
    println!("tag channel: ok");

    // An uploaded archive's file name carries its tag and target, in either
    // channel, and only a canonical one.
    let name = |file: &str| release_proof::setup_identity_of(file);
    assert_eq!(
        name("bilibi-setup-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"),
        Some(("setup-v1.2.3".into(), "x86_64-unknown-linux-gnu".into()))
    );
    assert_eq!(
        name("bilibi-nightly-2026-11-25-x86_64-unknown-linux-gnu.tar.gz"),
        Some((
            "nightly-2026-11-25".into(),
            "x86_64-unknown-linux-gnu".into()
        ))
    );
    assert_eq!(
        name("bilibi-nightly-2026-02-30-x86_64-unknown-linux-gnu.tar.gz"),
        None
    );
    assert_eq!(
        name("bilibi-nightly-2026-1-01-x86_64-unknown-linux-gnu.tar.gz"),
        None
    );
    println!("upload name: ok");

    // The replay record: per channel, from the helper the server runs.
    let root =
        std::env::temp_dir().join(format!("release-proof-walkthrough-{}", std::process::id()));
    let instance = root.join("instance");
    std::fs::create_dir_all(&instance)?;
    let paths = transport::Paths::open(&instance)?;
    let helper = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/lib/release-proof.py");
    let python = |tag: &str, digest: &str, record: bool| -> std::process::Output {
        std::process::Command::new("python3")
            .arg(&helper)
            .arg(if record {
                "record-replay"
            } else {
                "check-replay"
            })
            .arg("--state")
            .arg(release_proof::replay_path(&paths))
            .args(["--tag", tag, "--digest", digest])
            .output()
            .expect("run the release proof helper")
    };
    let digest = |byte: char| byte.to_string().repeat(64);
    let (a, b) = (digest('a'), digest('b'));
    let ok = |output: &std::process::Output| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let refused = |output: &std::process::Output, text: &str| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(text),
            "{stderr}"
        );
    };
    ok(&python("nightly-2026-11-05", &a, true));
    refused(
        &python("nightly-2026-11-04", &b, false),
        "older than the highest admitted release nightly-2026-11-05",
    );
    refused(
        &python("nightly-2026-11-05", &b, false),
        "cannot replace the admitted bytes",
    );
    ok(&python("setup-v1.0.0", &b, true));
    refused(
        &python("setup-v0.9.0", &a, false),
        "older than the highest admitted release setup-v1.0.0",
    );
    // A setup admission leaves the nightly floor where it was.
    refused(
        &python("nightly-2026-11-04", &b, false),
        "older than the highest admitted release nightly-2026-11-05",
    );
    ok(&python("nightly-2026-11-06", &b, true));
    refused(
        &python("nightly-2026-11-05", &a, false),
        "older than the highest admitted release nightly-2026-11-06",
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(release_proof::replay_path(&paths))?)?;
    assert_eq!(state["highest"]["nightly"]["tag"], "nightly-2026-11-06");
    assert_eq!(state["highest"]["setup"]["tag"], "setup-v1.0.0");
    let _ = std::fs::remove_dir_all(&root);
    println!("replay record: ok");
    Ok(())
}
