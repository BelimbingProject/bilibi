//! Paste-ready connection prompt.
//! Public installation details only; apps discover their own OAuth identity.
use bilibi_system::Instance;

/// The MCP address of this Bilibi.
pub fn address(instance: &Instance) -> String {
    format!("{}/mcp", instance.origin)
}

/// The name a tool saves the connection under, as the server calls itself
/// (`bilibi-<slug>`), so Dev, Staging and Production never collide.
pub fn server_name(instance: &Instance) -> String {
    format!("bilibi-{}", instance.slug())
}

/// One sentence naming this Bilibi once, then what a change here does. The
/// role is spoken only once: a label that is just the role's name (`Dev` on a
/// Dev copy) is not repeated.
pub fn identity(instance: &Instance) -> String {
    let role = instance.role;
    if instance.label.eq_ignore_ascii_case(role.label()) {
        format!("This Bilibi is {}. {}", role.kind(), role.effect())
    } else {
        format!(
            "This Bilibi is called \"{}\" and is {}. {}",
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
        "Connect my AI app to my Bilibi business application using MCP and OAuth.\n\n\
{}\n\
Server name: {name}\n\
Server URL: {address}\n\n\
Follow these steps:\n\
1. Check whether `{name}` is already in your MCP settings.\n\
2. If it is absent, add it with the server URL above.\n\
3. If sign-in is required, or the saved sign-in is missing or invalid, check your app's MCP management commands and settings, then start MCP sign-in for `{name}`. Give me the authorization link to open. Never ask for my password or a token. Do not say that you cannot start sign-in until you have checked both.\n\
4. After I approve sign-in, try the Bilibi `search` tool with `company`. If Bilibi returns `ai_switched_off`, tell me to open {settings} and switch on this connection. Wait for me to confirm, then try the search again.\n\
5. Report the actual search result. OAuth approval and the AI switch are separate steps.\n\n\
If the tools still do not appear, report the exact error. Tell me whether I need to start a new chat or reload the app, then verify again.",
        identity(instance)
    )
}
