//! The ChatGPT plugin download. The settings page is AI connections in
//! `core/agent`. Every signed-in person may download; `AgentPlugins`
//! keeps the self-service gate and the client check.
use super::web::FILES;
use bilibi_module_registry::{
    contributions::{Access, Download, DOWNLOADS},
    Contribution, Provenance,
};
macro_rules! provenance {
    () => {
        Provenance {
            module: "core/agent",
            source: file!(),
            line: line!(),
        }
    };
}

// ChatGPT (`chatgpt.rs`): the operation is the harness key. The ZIP is not a
// generic MCP download. AI connections and the status bar link it.
inventory::submit! { Contribution::new(DOWNLOADS, "core/agent.chatgpt.download", provenance!(), &Download {
    path: crate::PLUGIN_DOWNLOAD, service: FILES, operation: "chatgpt", access: Access::SignedIn,
}) }
