//! Paste-ready connection prompt.
//! Public installation details only; apps discover their own OAuth identity.
//!
//! The prompt states facts (which Bilibi, its name and address, how sign-in
//! works, where the switch is, what to report) and leaves the steps to the
//! app: a capable model knows how to add a server, and each step it was told
//! went stale the moment an app changed its commands.
use bilibi_system::Instance;

/// The MCP address of this Bilibi.
pub fn address(instance: &Instance) -> String {
    format!("{}/mcp", instance.origin)
}

/// The name a tool saves the connection under (`Instance::server_name`): dots
/// become hyphens and existing hyphens double, so two deployments never share
/// one saved sign-in.
pub fn server_name(instance: &Instance) -> String {
    instance.server_name()
}

/// Where a person types the code a device-flow app shows them.
pub fn device_page(instance: &Instance) -> String {
    format!("{}{}", instance.origin, crate::DEVICE_PAGE)
}

/// One sentence naming this Bilibi once, then what a change here does. The
/// role is spoken only once: a label that is just the role's name (`Dev` on a
/// Dev copy) is not repeated.
pub fn identity(instance: &Instance) -> String {
    let role = instance.role;
    if instance.label.eq_ignore_ascii_case(role.label()) {
        format!("This installation is {}. {}", role.kind(), role.effect())
    } else {
        format!(
            "This installation is called \"{}\" and is {}. {}",
            instance.label,
            role.kind(),
            role.effect()
        )
    }
}

pub fn prompt(instance: &Instance) -> String {
    let (address, name) = (address(instance), server_name(instance));
    let settings = format!("{}/settings/ai-connections", instance.origin);
    format!(
        "Connect to my business application over MCP.\n\n\
{identity}\n\
Server name: {name}\n\
Server URL: {address}\n\
Sign-in: OAuth, approved by me in my browser. An app that cannot receive a browser callback on this computer (a cloud app or a bot) uses the device flow, as its own registered client or as the built-in client ID `{client}`: it shows me a code and I approve it at {device}\n\
AI switch: each connection is switched on or off at {settings}. The error `ai_switched_off` means it is off.\n\
Check: call the `hello` tool. Tell me what it returned, or the exact error.",
        identity = identity(instance),
        client = crate::CLIENT_ID,
        device = device_page(instance),
    )
}
