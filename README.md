# hx-claude-ide

Editor-awareness plugin that makes **Claude Code** aware of the active file and
selection in the [Steel branch of Helix](https://github.com/mattwparas/helix)
(`hx-steel`). When you select text in Helix, Claude Code picks it up as context —
the same lockfile + WebSocket/MCP mechanism the official VS Code and JetBrains
extensions use.

It's a native Steel dylib (`libclaude_ide`) plus a small `mod.scm` glue layer.
On start it binds a loopback WebSocket server, writes a discovery lockfile to
`~/.claude/ide/`, and streams `selection_changed` notifications as you move
around the editor.

## Requirements

- `hx-steel` — the Steel branch of Helix (this does **not** work with stock `hx`).
- The Steel toolchain, which ships `cargo-steel-lib` (used to build + install the
  dylib). If you can run `hx-steel`, you already have it.
- `claude` (Claude Code CLI).

## Install

### With forge (recommended)

`forge`, the Steel package manager (bundled with the Steel toolchain), installs
straight from git — no manual clone:

```sh
forge pkg install --git https://github.com/brandoshmando/hx-claude-ide.git
# pin a commit with:  --rev <sha>
```

This clones the package, builds `libclaude_ide.dylib` into `$STEEL_HOME/native/`,
and installs the `mod.scm` module under `$STEEL_HOME` (as `claude-ide/`, its cog
`package-name`; `forge list` shows what's installed). Then load it from
`~/.config/helix/init.scm`:

```scheme
(require "claude-ide/mod.scm")
(claude-ide/start)
```

### From source

For local development, build and install the dylib directly:

```sh
git clone https://github.com/brandoshmando/hx-claude-ide.git
cd hx-claude-ide
cargo steel-lib   # copies libclaude_ide.dylib into $STEEL_HOME/native/
```

Then require it by absolute path in `~/.config/helix/init.scm`:

```scheme
(require "/absolute/path/to/hx-claude-ide/mod.scm")
(claude-ide/start)
```

Either way, `mod.scm` loads the dylib, registers the selection/document-open
hooks, and provides `claude-ide/start` (and `claude-ide/stop`).

## Usage

1. Launch `hx-steel` from a project directory.
2. Run `claude` from that **same** directory (it must be inside the workspace the
   plugin advertised).
3. In Claude Code, run `/ide` and select **Helix**.
4. Select text in Helix — Claude Code attaches it as context. A bare cursor (no
   selection) attaches nothing.

## Notes & limitations

- **ABI must match your `hx-steel`.** The dylib is linked against a specific
  `steel-core` revision and feature set (pinned in `Cargo.toml` to mirror
  helix's). If loading fails with `abi_stable` "incompatible type size", your
  `hx-steel` was built from a different Steel revision — re-pin `rev`/`features`
  to match your build and re-run `cargo steel-lib`.
- **Columns are codepoint offsets**, which diverge from UTF-16 only on astral
  characters; the selected `text` itself is always exact.
- **Stale lockfiles**: an `hx-steel` that is killed (rather than quit cleanly)
  can leave a lockfile in `~/.claude/ide/`, which may show up as a duplicate
  option in `/ide`. Claude Code usually prunes dead entries by PID.
- MVP scope: selection context only. No tools (`openDiff`, diagnostics, etc.)
  are implemented yet.

## License

[MPL-2.0](./LICENSE) (matching Helix).
