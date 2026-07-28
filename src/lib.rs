//! hx-claude-ide — Claude Code editor-awareness for the Steel branch of Helix.
//!
//! This is the native dylib half of the plugin. Right now it only wires up the
//! two functions the Steel layer (`mod.scm`) will call; both are no-op stubs so
//! we can prove the FFI/build path end-to-end before adding real logic.
//!
//! - `claude-ide/start`          — will bind the WS port + write the lockfile (T08)
//! - `claude-ide/push-selection` — will forward editor state to Claude Code (T08)

// `FFIModule` is the table of functions we expose to Steel.
// `RegisterFFIFn` is the trait that gives `FFIModule` its `.register_fn(...)`
// method; it has to be in scope even though we never name it directly.
use steel::steel_vm::ffi::{FFIModule, RegisterFFIFn};

// The lock-file writer and WS server. Their public items are used starting in
// T08 (the FFI `start()` wiring), so allow dead_code until then to keep the
// build warning-free.
#[allow(dead_code)]
mod lockfile;
#[allow(dead_code)]
mod mcp;
#[allow(dead_code)]
mod server;

/// Steel entrypoint: `(claude-ide/start)`.
///
/// Stub for now. In T08 this binds a 127.0.0.1 WebSocket port, writes the
/// `~/.claude/ide/<port>.lock` file, and spawns the server thread.
fn start() {
    // no-op stub
}

/// Steel entrypoint: `(claude-ide/push-selection file text sl sc el ec is-empty)`.
///
/// The Steel side hands us plain primitives and Rust owns assembling the
/// protocol payload (T07/T08). `isize` matches Steel's native integer type, so
/// the FFI layer can convert the line/column args without a fuss.
///
/// The leading `_` on each name tells the compiler "unused on purpose" — without
/// it we'd get an unused-variable warning on every parameter.
fn push_selection(
    _file: String,
    _text: String,
    _start_line: isize,
    _start_col: isize,
    _end_line: isize,
    _end_col: isize,
    _is_empty: bool,
) {
    // no-op stub
}

/// Builds the module Steel loads. The name string is the module path the Steel
/// `require` refers to. `register_fn` takes (scheme-visible name, rust fn) and
/// returns `&mut FFIModule`, so the calls chain.
fn build_module() -> FFIModule {
    let mut module = FFIModule::new("dylib/claude-ide/claude-ide");
    module
        .register_fn("claude-ide/start", start)
        .register_fn("claude-ide/push-selection", push_selection);
    module
}

// Generates the C-ABI entrypoint the dylib loader looks for, wrapping
// `build_module`. This is the one required piece of macro boilerplate.
steel::declare_module!(build_module);
