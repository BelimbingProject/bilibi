use std::{error::Error, path::Path, process::Command};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let [mode, instance] = args.as_slice() {
        if mode == "--worker" {
            bilibi_installer::Installer::open(Path::new(instance))?
                .run()
                .await?;
            return Ok(());
        }
    }
    let status = Command::new("bash")
        .args(["-c", include_str!("scenario.sh"), "legacy-recovery"])
        .arg(std::env::current_exe()?)
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("legacy recovery walkthrough: {status}").into())
    }
}
