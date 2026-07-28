//! The WebSocket server Claude Code connects to.
//!
//! Design constraints (see the plan doc): a single dedicated OS thread running
//! *blocking* `tungstenite` (no async runtime), bound to loopback only, with the
//! whole thread body wrapped in `catch_unwind` so a panic can never unwind into
//! Helix. Each accepted client gets its own thread and an outgoing channel so the
//! selection emitter (T07) can broadcast to every client.
//!
//! T05 scope: bind + accept + auth. Handling `initialize`/`tools/list` is T06;
//! broadcasting `selection_changed` is T07.

use std::net::{TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::Message;

/// The header Claude Code sends on the WebSocket upgrade; we validate it against
/// the token we wrote into the lock file.
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// How often the per-connection loop wakes to flush queued outgoing messages
/// while otherwise blocked reading. A short timeout keeps latency low without a
/// hot spin.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One outgoing-message channel per connected client. The emitter clones a
/// `Message` onto every `Sender` to broadcast; a `Sender` whose receiver has
/// hung up (client gone) errors on send, which is how dead clients get pruned.
type Clients = Arc<Mutex<Vec<Sender<Message>>>>;

/// Handle to the running server. Holds the bound `port` (which the lock file
/// advertises) and the shared client registry (used by the emitter in T07).
pub struct Server {
    pub port: u16,
    clients: Clients,
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

        // `Arc` = atomically reference-counted shared ownership. The token and
        // client list are shared between the accept thread (and every connection
        // thread it spawns) and the `Server` we return. Cloning an `Arc` bumps a
        // counter and hands out another handle to the *same* value.
        let token = Arc::new(token);
        let accept_clients = Arc::clone(&clients);

        thread::spawn(move || {
            // Contain any panic inside this thread. `AssertUnwindSafe` tells the
            // compiler "trust me, it's fine to catch across these captured vars"
            // (needed because `Mutex` isn't automatically unwind-safe). Note that
            // a panic in a spawned thread already stays within that thread; this
            // is belt-and-suspenders. The hard rule — never unwind across the FFI
            // boundary — is enforced in the `start()`/`push_selection()` fns (T08),
            // which don't call into this thread synchronously.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                accept_loop(listener, token, accept_clients);
            }));
        });

        Ok(Server { port, clients })
    }
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
        thread::spawn(move || handle_client(stream, token, clients));
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
    clients.lock().unwrap().push(tx);

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
            // T06 will parse Text frames as JSON-RPC (initialize, tools/list).
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
    use tungstenite::client::IntoClientRequest;

    /// Attempts a WS handshake to the given port with an optional auth token;
    /// returns whether the handshake succeeded.
    fn handshake_succeeds(port: u16, token: Option<&str>) -> bool {
        let mut request = format!("ws://127.0.0.1:{port}/")
            .into_client_request()
            .unwrap();
        if let Some(t) = token {
            request
                .headers_mut()
                .insert(AUTH_HEADER, t.parse().unwrap());
        }
        tungstenite::connect(request).is_ok()
    }

    #[test]
    fn accepts_valid_token_and_rejects_others() {
        let token = "0123456789abcdef0123456789abcdef".to_string();
        let server = Server::start(token.clone()).expect("server should start");

        assert!(
            handshake_succeeds(server.port, Some(&token)),
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
}
