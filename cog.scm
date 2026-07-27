(define package-name 'claude-ide)
(define version "0.1.0")
(define dependencies '())
;; Native dylib built from the root Cargo.toml. `#:name` must match the Cargo
;; `[lib] name` — forge derives `libclaude_ide.<ext>` from it.
(define dylibs '((#:name "claude_ide" #:workspace-root ".")))
