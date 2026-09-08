//! The WebSocket server Claude Code connects to.
//!
//! Design constraints (see the plan doc): a single dedicated OS thread running
//! *blocking* `tungstenite` (no async runtime), bound to loopback only, with the
//! whole thread body wrapped in `catch_unwind` so a panic can never unwind into
//! Helix. Each accepted client gets its own thread and an outgoing channel so the
//! selection emitter (T07) can broadcast to every client.
//!
//! Layering: bind + accept + auth (T05); the `initialize`/`tools/list` handshake
//! (T06, in the `mcp` module); `selection_changed` debounce + broadcast (T07).

use std::net::{TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::json;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::Message;

/// The header Claude Code sends on the WebSocket upgrade; we validate it against
/// the token we wrote into the lock file.
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// How often the per-connection loop wakes to flush queued outgoing messages
/// while otherwise blocked reading. A short timeout keeps latency low without a
/// hot spin.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Trailing-debounce window for selection updates: after the last payload
/// arrives, wait this long for a quieter moment before broadcasting, so a burst
/// of cursor moves collapses into a single notification (design D5).
const DEBOUNCE: Duration = Duration::from_millis(50);

/// One outgoing-message channel per connected client. The emitter clones a
/// `Message` onto every `Sender` to broadcast; a `Sender` whose receiver has
/// hung up (client gone) errors on send, which is how dead clients get pruned.
type Clients = Arc<Mutex<Vec<Sender<Message>>>>;

/// A snapshot of the editor's current selection, handed from the Steel-facing
/// `push_selection` (T08) to the emitter. Coordinates are already 0-indexed
/// (the Steel layer converts) — Rust only assembles and serializes (F3).
pub struct SelectionPayload {
    pub file: String,
    pub text: String,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub is_empty: bool,
}

/// Handle to the running server. Holds the bound `port` (which the lock file
/// advertises) and the channel the Steel layer pushes selection updates into.
pub struct Server {
    pub port: u16,
    selection_tx: Sender<SelectionPayload>,
}

impl Server {
    /// Binds an ephemeral loopback port and spawns the accept thread. Returns as
    /// soon as the socket is bound, so `port` is immediately valid for the caller
    /// to write into the lock file.
    pub fn start(token: String) -> std::io::Result<Server> {
        // Port 0 asks the OS for any free port; we read back which one it chose.
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();

        let clients: Clients = Arc::new(Mutex::new(Vec::new()));

        // The emitter thread owns the receiving end of the selection channel and
        // its own handle to the client list; it debounces and broadcasts.
        let (selection_tx, selection_rx) = mpsc::channel::<SelectionPayload>();
        let emitter_clients = Arc::clone(&clients);
        thread::spawn(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                emitter_loop(selection_rx, emitter_clients);
            }));
        });

        // `Arc` = atomically reference-counted shared ownership. The token and
        // client list are shared between the accept thread (and every connection
        // thread it spawns) and the emitter. Cloning an `Arc` bumps a counter and
        // hands out another handle to the *same* value. Note `clients` isn't
        // stored on `Server`: the two threads' clones keep the `Vec` alive.
        let token = Arc::new(token);
        let accept_clients = Arc::clone(&clients);

        thread::spawn(move || {
            // Contain any panic inside this thread. `AssertUnwindSafe` tells the
            // compiler "trust me, it's fine to catch across these captured vars"
            // (needed because `Mutex` isn't automatically unwind-safe). Note that
            // a panic in a spawned thread already stays within that thread; this
            // is belt-and-suspenders. The hard rule — never unwind across the FFI
            // boundary — is enforced in the `start()`/`push_selection()` fns (T08),
            // which don't call into these threads synchronously.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                accept_loop(listener, token, accept_clients);
            }));
        });

        Ok(Server { port, selection_tx })
    }

    /// Hand a selection snapshot to the emitter. Called from the FFI
    /// `push_selection` (T08). Sending never blocks; if the emitter thread is
    /// gone we silently drop the update (nothing useful to do from the editor).
    pub fn push_selection(&self, payload: SelectionPayload) {
        let _ = self.selection_tx.send(payload);
    }
}

/// Receives selection payloads, debounces bursts, and broadcasts the latest as a
/// `selection_changed` notification to every connected client.
fn emitter_loop(rx: Receiver<SelectionPayload>, clients: Clients) {
    // Block for the first payload of a burst.
    while let Ok(mut latest) = rx.recv() {
        // Coalesce: keep replacing `latest` with any newer payload that arrives
        // within the debounce window. When the window passes quietly, emit.
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(newer) => latest = newer,               // supersede, keep waiting
                Err(RecvTimeoutError::Timeout) => break,   // quiet → emit `latest`
                Err(RecvTimeoutError::Disconnected) => return, // all senders gone
            }
        }
        broadcast(&clients, selection_notification(&latest));
    }
}

/// Sends `message` to every client, dropping any whose receiver has hung up.
fn broadcast(clients: &Clients, message: Message) {
    // Poison-tolerant: if a connection thread panicked mid-lock, we still want
    // broadcasts to keep working rather than cascade-panic every future call.
    let mut guard = clients.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // `retain` keeps only the entries whose closure returns true. A failed
    // `send` means that client's connection thread is gone → prune it.
    guard.retain(|tx| tx.send(message.clone()).is_ok());
}

/// Builds the F2 `selection_changed` JSON-RPC notification for a payload.
fn selection_notification(p: &SelectionPayload) -> Message {
    let body = json!({
        "jsonrpc": "2.0",
        "method": "selection_changed",
        "params": {
            "text": p.text,
            "filePath": p.file,
            "fileUrl": format!("file://{}", p.file),
            "selection": {
                "start": { "line": p.start_line, "character": p.start_col },
                "end": { "line": p.end_line, "character": p.end_col },
                "isEmpty": p.is_empty,
            }
        }
    });
    Message::text(body.to_string())
}

/// Blocks accepting connections; hands each off to its own thread.
fn accept_loop(listener: TcpListener, token: Arc<String>, clients: Clients) {
    // `incoming()` yields a `Result<TcpStream>` per connection, forever.
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue, // transient accept error: skip this one
        };
        // Each connection thread needs its own handle to the shared state.
        let token = Arc::clone(&token);
        let clients = Arc::clone(&clients);
        // Isolate each connection: a panic here must not escape the thread (and,
        // via a poisoned lock, take down the others). Matches the emitter/accept
        // threads' panic containment.
        thread::spawn(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| handle_client(stream, token, clients)));
        });
    }
}

/// Runs one client: authenticate the upgrade, then loop reading requests and
/// writing any queued broadcast messages.
fn handle_client(stream: TcpStream, token: Arc<String>, clients: Clients) {
    // The handshake callback: inspect the upgrade request's headers and either
    // accept (return the response) or reject (return an error response). This
    // closure runs once, during `accept_hdr` below.
    let authenticate = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let presented = request
            .headers()
            .get(AUTH_HEADER)
            .and_then(|value| value.to_str().ok());

        if presented == Some(token.as_str()) {
            Ok(response)
        } else {
            let deny = tungstenite::http::Response::builder()
                .status(tungstenite::http::StatusCode::UNAUTHORIZED)
                .body(Some("unauthorized".to_string()))
                .expect("build 401 response");
            Err(deny)
        }
    };

    // Perform the WS upgrade with the auth check. On rejection or any handshake
    // error, just drop the connection.
    let mut websocket = match tungstenite::accept_hdr(stream, authenticate) {
        Ok(ws) => ws,
        Err(_) => return,
    };

    // Register this client so the emitter can push to it. `tx`/`rx` are the two
    // ends of a queue: the emitter holds clones of `tx`; we own the `rx`.
    let (tx, rx): (Sender<Message>, Receiver<Message>) = mpsc::channel();
    clients
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(tx);

    // A read timeout turns the otherwise-blocking `read()` into a poll: it
    // returns a WouldBlock/TimedOut error every POLL_INTERVAL, giving us a chance
    // to flush outgoing messages even when the client is silent.
    let _ = websocket
        .get_ref()
        .set_read_timeout(Some(POLL_INTERVAL));

    loop {
        // 1. Flush anything the emitter has queued for this client.
        while let Ok(message) = rx.try_recv() {
            if websocket.send(message).is_err() {
                return; // client write failed → connection is dead
            }
        }

        // 2. Read one inbound message (or time out and loop).
        match websocket.read() {
            Ok(Message::Close(_)) => return,
            // Parse text frames as JSON-RPC and reply if a response is due.
            Ok(Message::Text(text)) => {
                if let Some(response) = crate::mcp::handle_message(text.as_str()) {
                    if websocket.send(Message::text(response)).is_err() {
                        return;
                    }
                }
            }
            // Ping/Pong are handled by tungstenite; ignore other frame types.
            Ok(_message) => {}
            // Read timed out: no data this interval, just loop back to step 1.
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            // Clean or abrupt close, or any other error: end the connection.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Socket-level behavior (auth, MCP handshake, debounced broadcast) is
    // exercised end-to-end in `tests/protocol.rs`. This unit test covers the one
    // thing that suite can't reach: the private `selection_notification`
    // serializer (F2 wire shape).

    #[test]
    fn notification_has_f2_shape() {
        let payload = SelectionPayload {
            file: "/a/b.rs".to_string(),
            text: "hi".to_string(),
            start_line: 10,
            start_col: 5,
            end_line: 12,
            end_col: 3,
            is_empty: false,
        };
        let msg = selection_notification(&payload);
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();

        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "selection_changed");
        assert!(v.get("id").is_none(), "notifications carry no id");
        assert_eq!(v["params"]["text"], "hi");
        assert_eq!(v["params"]["filePath"], "/a/b.rs");
        assert_eq!(v["params"]["fileUrl"], "file:///a/b.rs");
        assert_eq!(v["params"]["selection"]["start"]["line"], 10);
        assert_eq!(v["params"]["selection"]["start"]["character"], 5);
        assert_eq!(v["params"]["selection"]["end"]["line"], 12);
        assert_eq!(v["params"]["selection"]["end"]["character"], 3);
        assert_eq!(v["params"]["selection"]["isEmpty"], false);
    }
}
