//! `bilibi-axi`: see the crate's own notes (`lib.rs`) and `apps/cli/README.md`.
use std::{io::Read, path::PathBuf, process::ExitCode};

/// `BILIBI_AXI_HOME`, else `bilibi-axi` under the account's configuration folder.
fn home() -> PathBuf {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(home) = set("BILIBI_AXI_HOME") {
        return PathBuf::from(home);
    }
    let base = set("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("bilibi-axi")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Standard input is read only when an argument asks for it.
    let stdin = args
        .iter()
        .any(|arg| arg == "-" || arg == "--code-stdin")
        .then(|| {
            let mut text = String::new();
            let _ = std::io::stdin().read_to_string(&mut text);
            text
        });
    let text = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let outcome = bilibi_axi::run(bilibi_axi::Invocation {
        args,
        home: home(),
        self_report: text("BILIBI_AGENT_SELF_REPORT"),
        stdin,
    })
    .await;
    print!("{}", outcome.stdout);
    ExitCode::from(outcome.code)
}
