//! hx-claude-ide — Claude Code editor-awareness for the Steel branch of Helix.
//!
//! This is the native dylib half of the plugin. The Steel layer (`mod.scm`)
//! calls three functions across the FFI boundary:
//!
//! - `claude-ide/start`          — bind the WS port, write the lockfile, spawn the server
//! - `claude-ide/push-selection` — forward the current editor selection to Claude Code
//! - `claude-ide/stop`           — tear the server down and remove the lockfile
//!
//! All three are best-effort and MUST NOT panic across the FFI boundary — a panic
//! unwinding into the Steel VM is undefined behavior. Each entrypoint therefore
//! swallows its errors (there is nothing useful to report back to the editor),
//! and the server's own threads are additionally panic-isolated (see `server`).

use std::sync::Mutex;

// `FFIModule` is the table of functions we expose to Steel.
// `RegisterFFIFn` is the trait that gives `FFIModule` its `.register_fn(...)`
// method; it has to be in scope even though we never name it directly.
use steel::steel_vm::ffi::{FFIModule, RegisterFFIFn};

mod lockfile;
mod mcp;
// Public so the `tests/protocol.rs` integration suite can drive the server
// (`Server`, `SelectionPayload`) as an external consumer would.
pub mod server;

use lockfile::LockFile;
use server::{SelectionPayload, Server};

/// Everything the running plugin owns. Held in a process-global so the FFI
/// entrypoints — free functions with no shared receiver — can reach it. Dropping
/// this stops the emitter (the `Server`'s selection sender goes away) and removes
/// the lockfile (`LockFile`'s `Drop`).
struct App {
    server: Server,
    // Kept alive purely for its `Drop`: while this is held the lockfile stays on
    // disk; dropping `App` removes it. The leading `_` marks "owned, not read".
    _lock: LockFile,
}

/// The one-and-only plugin instance. `None` until `start()` succeeds, back to
/// `None` after `stop()`. `Mutex::new` is `const`, so this needs no lazy init.
static APP: Mutex<Option<App>> = Mutex::new(None);

/// Serializes tests that mutate process-global state (`CLAUDE_CONFIG_DIR`, cwd).
/// Rust runs a crate's unit tests concurrently in one binary, so two tests that
/// each `set_var` the same key would otherwise clobber each other.
#[cfg(test)]
pub(crate) static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Steel entrypoint: `(claude-ide/start)`.
///
/// Binds a loopback WebSocket port, writes `~/.claude/ide/<port>.lock`, and
/// spawns the server thread. Idempotent: if the server is already running (or the
/// lock is poisoned) this is a no-op. Any bind/IO failure is swallowed — the
/// plugin simply stays inactive rather than taking Helix down with it.
fn start() {
    let mut app = match APP.lock() {
        Ok(app) => app,
        Err(_) => return, // a prior panic poisoned the lock; stay inactive
    };
    if app.is_some() {
        return; // already running
    }

    // Reap lockfiles from prior Helix sessions that exited without cleanup — our
    // `Drop` can't run at process exit (the owner is a `static`), so this is the
    // only reliable moment to clear the corpses. Best-effort.
    lockfile::sweep_stale();

    // The workspace folder Claude matches `/ide` against is this process's cwd
    // (i.e. Helix's cwd, decision D4). Without it there's nothing to advertise.
    let workspace = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok());
    let workspace = match workspace {
        Some(w) => w,
        None => return,
    };

    let token = lockfile::generate_token();
    let server = match Server::start(token.clone()) {
        Ok(server) => server,
        Err(_) => return,
    };
    // `server.port` is the OS-assigned port the listener bound to — it names the
    // lockfile and is what Claude connects back to.
    let lock = match LockFile::write(server.port, &workspace, &token) {
        Ok(lock) => lock,
        Err(_) => return, // server threads idle unreferenced; harmless for MVP
    };

    *app = Some(App {
        server,
        _lock: lock,
    });
}

/// Steel entrypoint: `(claude-ide/push-selection file text sl sc el ec is-empty)`.
///
/// Assembles a `SelectionPayload` from the primitives Steel hands us and forwards
/// it to the emitter. If the server isn't running the update is dropped silently.
///
/// Steel's native integer is `isize`; the payload and wire protocol use `i64`. On
/// every platform we target `isize` is 64-bit, so the cast is lossless.
fn push_selection(
    file: String,
    text: String,
    start_line: isize,
    start_col: isize,
    end_line: isize,
    end_col: isize,
    is_empty: bool,
) {
    let app = match APP.lock() {
        Ok(app) => app,
        Err(_) => return,
    };
    if let Some(app) = app.as_ref() {
        app.server.push_selection(SelectionPayload {
            file,
            text,
            start_line: start_line as i64,
            start_col: start_col as i64,
            end_line: end_line as i64,
            end_col: end_col as i64,
            is_empty,
        });
    }
}

/// Steel entrypoint: `(claude-ide/stop)`.
///
/// Drops the running instance: the lockfile is removed (so Claude stops
/// discovering us) and the emitter thread winds down as its channel closes. The
/// blocking accept thread lingers on its bound port until the process exits — a
/// known MVP limitation, harmless because nothing advertises that port anymore.
fn stop() {
    if let Ok(mut app) = APP.lock() {
        *app = None;
    }
}

/// Builds the module Steel loads. The name string is the module path the Steel
/// `require` refers to; `register_fn` pairs a scheme-visible name with a Rust fn
/// and returns `&mut FFIModule`, so the calls chain.
fn build_module() -> FFIModule {
    let mut module = FFIModule::new("dylib/claude-ide/claude-ide");
    module
        .register_fn("claude-ide/start", start)
        .register_fn("claude-ide/push-selection", push_selection)
        .register_fn("claude-ide/stop", stop);
    module
}

// Generates the C-ABI entrypoint the dylib loader looks for, wrapping
// `build_module`. This is the one required piece of macro boilerplate.
steel::declare_module!(build_module);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tungstenite::client::IntoClientRequest;

    /// Test-only view into `APP`: the bound port of the running server, if any.
    fn running_port() -> Option<u16> {
        APP.lock().ok()?.as_ref().map(|app| app.server.port)
    }

    #[test]
    fn start_writes_lockfile_then_push_broadcasts() {
        // `start()` reads CLAUDE_CONFIG_DIR (via the lockfile writer); serialize
        // against other env-mutating tests so config dirs don't clobber.
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());

        let base = std::env::temp_dir().join(format!("hxcc-lib-{}", std::process::id()));
        std::env::set_var("CLAUDE_CONFIG_DIR", &base);

        start();
        start(); // second call must be a no-op (idempotent)
        let port = running_port().expect("server should be running after start()");

        // The lockfile Claude would read: <base>/ide/<port>.lock.
        let lock_path = base.join("ide").join(format!("{port}.lock"));
        assert!(lock_path.exists(), "lock file should exist at {lock_path:?}");
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        let token = body["authToken"].as_str().unwrap().to_string();

        // Connect a mock client with the advertised token, then drive one push.
        let addr = format!("127.0.0.1:{port}");
        let stream = std::net::TcpStream::connect(&addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = format!("ws://{addr}/").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("x-claude-code-ide-authorization", token.parse().unwrap());
        let (mut ws, _resp) = tungstenite::client(request, stream).expect("handshake");

        // Give the accept thread a moment to register this client before pushing.
        std::thread::sleep(Duration::from_millis(150));

        push_selection(
            "/tmp/x.rs".to_string(),
            "hello".to_string(),
            3,
            2,
            3,
            7,
            false,
        );

        let msg = ws.read().unwrap().into_text().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.as_str()).unwrap();
        assert_eq!(v["method"], "selection_changed");
        assert_eq!(v["params"]["text"], "hello");
        assert_eq!(v["params"]["filePath"], "/tmp/x.rs");
        assert_eq!(v["params"]["selection"]["start"]["line"], 3);
        assert_eq!(v["params"]["selection"]["start"]["character"], 2);
        assert_eq!(v["params"]["selection"]["end"]["line"], 3);
        assert_eq!(v["params"]["selection"]["end"]["character"], 7);

        // `stop()` clears the instance and removes the lockfile.
        stop();
        assert!(running_port().is_none(), "stop() should clear the running app");
        assert!(!lock_path.exists(), "stop() should remove the lock file");
        let _ = std::fs::remove_dir_all(&base);
    }
}
