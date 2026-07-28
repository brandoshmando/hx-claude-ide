//! Minimal MCP (Model Context Protocol) handshake over JSON-RPC 2.0.
//!
//! Claude Code connects as an MCP *client*. It won't keep streaming selection
//! context unless the `initialize` handshake looks the way it expects, so we
//! mirror the shape the battle-tested `coder/claudecode.nvim` reference returns
//! (see the plan doc, decision D8).
//!
//! MVP scope: we advertise **no tools** (`tools/list` returns an empty array).
//! The selection feature rides entirely on the pushed `selection_changed`
//! notification (T07), so no tool is needed for it — and advertising a tool we
//! don't implement would risk Claude *calling* it and getting an error. Any
//! `tools/call` (or unknown method) gets a clean JSON-RPC "method not found"
//! instead of a hang. Real tools (openDiff, getDiagnostics, …) get added here
//! later, and only if the T11 live test shows Claude requires any tool at all.

use serde_json::{json, Value};

/// MCP protocol version we advertise. Fixed value the reference uses (not echoed
/// from the client's request).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Name reported in `serverInfo` (matches the lock file's `ideName`).
const SERVER_NAME: &str = "Helix";

/// Handles one inbound JSON-RPC message. Returns the serialized reply to send
/// back, or `None` when no reply is due — a notification (no `id`), or a message
/// we can't parse (we can't answer without an `id` to echo).
pub fn handle_message(text: &str) -> Option<String> {
    let request: Value = serde_json::from_str(text).ok()?;
    let method = request.get("method")?.as_str()?;

    // Notifications omit `id` (or send null) and expect no response — e.g.
    // `notifications/initialized`. Nothing to do.
    let id = request.get("id").cloned().filter(|v| !v.is_null())?;

    let response = match method {
        "initialize" => success(id, initialize_result()),
        // Minimal MVP: advertise no tools (see module doc). Selection works via
        // the pushed `selection_changed` notification, not a tool.
        "tools/list" => success(id, json!({ "tools": [] })),
        // Advertised-but-unimplemented tool calls, and anything else, get a
        // standard "method not found". Keeps the connection healthy.
        other => error(id, -32601, &format!("method not found: {other}")),
    };
    Some(response.to_string())
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "logging": {},
            "prompts": { "listChanged": true },
            "tools": { "listChanged": true }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            // Compiled-in crate version, e.g. "0.1.0".
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_has_expected_shape() {
        let reply = handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .expect("initialize should get a reply");
        let v: Value = serde_json::from_str(&reply).unwrap();

        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1); // echoed
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "Helix");
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_is_empty_for_mvp() {
        let reply = handle_message(r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list"}"#)
            .expect("tools/list should get a reply");
        let v: Value = serde_json::from_str(&reply).unwrap();

        assert_eq!(v["id"], "abc"); // string ids echo too
        let tools = v["result"]["tools"].as_array().unwrap();
        assert!(tools.is_empty(), "MVP advertises no tools");
    }

    #[test]
    fn notification_gets_no_reply() {
        // No `id` → notification → no response.
        assert!(handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let reply = handle_message(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{}}"#)
            .expect("a request always gets a reply");
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], -32601);
        assert!(v.get("result").is_none());
    }
}
