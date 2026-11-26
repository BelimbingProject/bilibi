//! Paste-ready connection prompt.
//! Public installation details only; apps discover their own OAuth identity.
//!
//! Keep the connection contract here without tying it to host-specific commands.
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
    let text = if instance.label.eq_ignore_ascii_case(role.label()) {
        format!("This installation is {}. {}", role.kind(), role.effect())
    } else {
        format!(
            "This installation is called \"{}\" and is {}. {}",
            instance.label,
            role.kind(),
            role.effect()
        )
    };
    text.trim_end().to_owned()
}

/// Sign-in and recovery guidance shared by the copy box and exported setup skill.
pub(super) fn connection_guidance(instance: &Instance) -> String {
    let address = address(instance);
    let settings = format!("{}/settings/ai-connections", instance.origin);
    format!(
        "Reuse the host's saved Bilibi connection first. Keep renewable credentials in the host's secure store, if supported. Renew silently with the same client and resource; save the rotated credentials together. Do not ask for device approval again for an ordinary token expiry, a new chat or a restart when the grant can still be renewed.\n\
For a new connection, use the host's supported sign-in. Prefer device authorization when the host supports it; request it for the exact MCP resource `{address}`, using its registered device client or the built-in client ID `{client}`. Show the short `user_code` and the returned `verification_uri_complete` link so the person can check the code and approve in the browser on the device they are using. Manual entry at {device} is a fallback, not a required step. Keep `device_code` and tokens in the client; do not ask the person for a password or token. If the host requires browser OAuth with PKCE instead, use its supported callback. A callback on this computer is only for a client that can receive it here.\n\
Check with authenticated `hello`; a public handshake does not count. If Bilibi returns `ai_switched_off`, keep the existing connection and direct the person to {settings} to turn it on, then retry. If the saved grant has ended or new access needs consent, explain why and keep the pending work. Require the usual review for writes and operation-specific 2FA for high-risk actions without repeating connection approval.",
        client = crate::CLIENT_ID,
        device = device_page(instance),
    )
}

pub fn prompt(instance: &Instance) -> String {
    let (address, name) = (address(instance), server_name(instance));
    format!(
        "Connect to my business application over MCP.\n\n\
{identity}\n\
Server name: {name}\n\
Server URL: {address}\n\n\
{guidance}\n\n\
Tell me what `hello` returned, or the exact error.",
        identity = identity(instance),
        guidance = connection_guidance(instance),
    )
}
