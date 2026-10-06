//! Black-box protocol suite.
//!
//! Drives the server the way Claude Code does — over a real loopback WebSocket —
//! so protocol regressions surface here rather than only against the opaque real
//! client (design D10). Pure-serialization, lockfile/token, and JSON-RPC parsing
//! checks live as unit tests next to their modules; this file owns the
//! socket-level behavior: auth, the MCP handshake, and debounced broadcast.

use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use claude_ide::server::{SelectionPayload, Server};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

/// Header Claude Code presents on the upgrade; must equal the lockfile token.
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
/// A representative 32-char hex token (the shape the real writer produces).
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Connects an authenticated client over a raw `TcpStream` with a short read
/// timeout, so a missing broadcast fails the test fast instead of hanging.
fn connect_authed(port: u16) -> WebSocket<TcpStream> {
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(AUTH_HEADER, TOKEN.parse().unwrap());
    let (ws, _resp) = tungstenite::client(request, stream).expect("handshake should succeed");
    ws
}

/// Whether a handshake succeeds with the given (optional) token.
fn handshake_succeeds(port: u16, token: Option<&str>) -> bool {
    let mut request = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .unwrap();
    if let Some(t) = token {
        request.headers_mut().insert(AUTH_HEADER, t.parse().unwrap());
    }
    tungstenite::connect(request).is_ok()
}

/// Reads one text frame and parses it as JSON.
fn read_json(ws: &mut WebSocket<TcpStream>) -> serde_json::Value {
    let text = ws.read().unwrap().into_text().unwrap();
    serde_json::from_str(text.as_str()).unwrap()
}

fn payload(text: &str) -> SelectionPayload {
    SelectionPayload {
        file: "/tmp/example.rs".to_string(),
        text: text.to_string(),
        start_line: 1,
        start_col: 0,
        end_line: 1,
        end_col: 4,
        is_empty: text.is_empty(),
    }
}

#[test]
fn accepts_valid_token_and_rejects_others() {
    let server = Server::start(TOKEN.to_string()).expect("server should start");
    assert!(
        handshake_succeeds(server.port, Some(TOKEN)),
        "correct token should be accepted"
    );
    assert!(
        !handshake_succeeds(server.port, Some("wrong-token")),
        "incorrect token should be rejected"
    );
    assert!(
        !handshake_succeeds(server.port, None),
        "missing token should be rejected"
    );
}

#[test]
fn selects_mcp_subprotocol_when_offered() {
    // Claude Code offers `Sec-WebSocket-Protocol: mcp`; a compliant client fails
    // the handshake unless the server selects it.
    let server = Server::start(TOKEN.to_string()).expect("server should start");
    let mut request = format!("ws://127.0.0.1:{}/", server.port)
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert(AUTH_HEADER, TOKEN.parse().unwrap());
    headers.insert("sec-websocket-protocol", "mcp".parse().unwrap());

    let (_ws, response) = tungstenite::connect(request).expect("handshake should succeed");
    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "mcp"
    );
}

#[test]
fn initialize_and_tools_list_are_well_formed() {
    let server = Server::start(TOKEN.to_string()).expect("server should start");
    let mut ws = connect_authed(server.port);

    ws.send(Message::text(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.to_string(),
    ))
    .unwrap();
    let init = read_json(&mut ws);
    assert_eq!(init["id"], 1, "id should be echoed");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "Helix");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    ws.send(Message::text(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string(),
    ))
    .unwrap();
    let list = read_json(&mut ws);
    assert_eq!(list["id"], 2, "id should be echoed");
    assert!(
        list["result"]["tools"].as_array().unwrap().is_empty(),
        "MVP advertises no tools"
    );
}

#[test]
fn push_selection_broadcasts_debounced_selection_changed() {
    let server = Server::start(TOKEN.to_string()).expect("server should start");
    let mut ws = connect_authed(server.port);

    // Let the accept thread register this client before pushing.
    thread::sleep(Duration::from_millis(150));

    // A burst within the debounce window collapses to a single broadcast of the
    // latest payload, in the F2 shape...
    for text in ["first", "second", "final"] {
        server.push_selection(payload(text));
    }
    let v = read_json(&mut ws);
    assert_eq!(v["method"], "selection_changed");
    assert_eq!(v["params"]["text"], "final", "burst should collapse to the latest");
    assert_eq!(v["params"]["filePath"], "/tmp/example.rs");
    assert_eq!(v["params"]["fileUrl"], "file:///tmp/example.rs");

    // ...and a later, separate update produces its own notification.
    server.push_selection(payload("later"));
    let v2 = read_json(&mut ws);
    assert_eq!(v2["params"]["text"], "later");
}
