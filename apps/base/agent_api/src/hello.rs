//! The connection check every agent door answers.
use bilibi_system::Instance;
use serde_json::{json, Value};

/// The longest message `hello` echoes.
pub const HELLO_MESSAGE_LIMIT: usize = 200;
/// What `hello` answers at every agent door (`/mcp` tool, `GET
/// /api/v1/hello`, the CLI): the caller's message and which Bilibi this is.
/// It reads no business data and needs no capability, so a working call
/// proves the transport and the session; a later refusal of a business
/// operation is then a permission answer. `None` when the message is longer
/// than [`HELLO_MESSAGE_LIMIT`] characters.
pub fn hello(instance: &Instance, message: Option<&str>) -> Option<Value> {
    let message = message.unwrap_or("hello");
    (message.chars().count() <= HELLO_MESSAGE_LIMIT).then(|| {
        json!({
            "message": message,
            "server_name": instance.server_name(),
            "instance": instance.agent_json(),
        })
    })
}
