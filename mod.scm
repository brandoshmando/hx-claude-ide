;; mod.scm — Steel glue for hx-claude-ide.
;;
;; This is the Scheme half of the plugin. It loads the native dylib, pulls in the
;; Helix editor readers it needs, and registers hooks that snapshot the current
;; selection and forward it to the dylib (which broadcasts it to Claude Code).
;;
;; All the binding names below were verified against the Helix Steel fork source
;; (helix-term/src/commands/engine/steel/{mod.rs,editor.scm,static.scm} and
;; helix-core/src/extensions.rs) and the Steel dylib docs, so no name here is a
;; guess. The one thing still to confirm on a live editor (T11) is whether the
;; `helix/*.scm` require paths resolve as-is from an installed plugin.

;; --- Helix bindings we call --------------------------------------------------
;; register-hook, editor-focus, editor->doc-id, editor->text  (helix/core/editor)
(require "helix/editor.scm")
;; cx->current-file, current-selection-object, current-selection->string,
;; selection->primary-range, range->from, range->to           (helix/core/static)
(require "helix/static.scm")
;; rope-char->line, rope-line->char — 0-indexed, arg order (rope offset).
(require-builtin helix/core/text)

;; --- Our native dylib --------------------------------------------------------
;; The require argument is the *library file* name: `lib` + the Cargo `[lib] name`
;; (`claude_ide`), extension stripped → `libclaude_ide`. This is NOT the
;; `FFIModule::new` path string ("dylib/claude-ide/claude-ide"); `only-in` lists
;; the `register_fn` names instead. (Steel dylib docs: start/dylib.md.)
(#%require-dylib "libclaude_ide"
                 (only-in claude-ide/start
                          claude-ide/push-selection
                          claude-ide/stop))

;; Re-export the lifecycle entrypoints so `init.scm` can drive them:
;;   (require "<...>/mod.scm")
;;   (claude-ide/start)
(provide claude-ide/start
         claude-ide/stop)

;;@doc
;; Snapshot the primary selection of the focused view and push it to the dylib.
;; Coordinates are computed 0-indexed here (via rope char/line math) so the Rust
;; side only has to serialize (F3). Columns are codepoint offsets (D7).
(define (push-current-selection!)
  (define rope (editor->text (editor->doc-id (editor-focus))))
  (define range (selection->primary-range (current-selection-object)))
  (define from (range->from range))
  (define to (range->to range))
  ;; width>1 rule (D6): a single-grapheme range is a Normal-mode cursor, which we
  ;; treat as "no selection" rather than a 1-char selection.
  (define is-empty (<= (- to from) 1))
  (define text (if is-empty "" (current-selection->string)))
  ;; char offset -> (line, column). rope-line->char gives the line's first char
  ;; offset; subtracting yields the 0-indexed column.
  (define sl (rope-char->line rope from))
  (define sc (- from (rope-line->char rope sl)))
  (define el (rope-char->line rope to))
  (define ec (- to (rope-line->char rope el)))
  (define file (cx->current-file))
  ;; `cx->current-file` is #false for scratch buffers — nothing to report then.
  (when file
    (claude-ide/push-selection file text sl sc el ec is-empty)))

;; Fire on cursor/selection movement and on opening a file. Both callbacks are
;; handed an id (ViewId / DocumentId) we don't need — we always read the focused
;; view inside `push-current-selection!`. Event kinds are quoted symbols, matching
;; every example in editor.scm / steel-docs.md. Hooks can't be unregistered, but
;; that's fine: after `(claude-ide/stop)` the dylib drops pushes silently.
(register-hook 'selection-did-change (lambda (_view-id) (push-current-selection!)))
(register-hook 'document-opened (lambda (_doc-id) (push-current-selection!)))
