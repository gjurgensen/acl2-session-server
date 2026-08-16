# ACL2 Session (VSCode)

Live form-by-form checking of ACL2 books, as a thin client of the
`acl2-session-editor` daemon (this repository's third crate). The daemon owns
the session; this extension renders its event stream.

The default rendering is a **gutter status column** of bars, and the buffer
text stays entirely unmarked: gray over every line still queued for
processing, blue over the form being proved, green over admitted forms,
yellow over forms admitted below a red one (conditional on its unproved
claim), red over a form admitted only under `skip-proofs` or failed
outright. Every status also marks the scrollbar, and the failure envelope
-- error line, abort cause, checkpoints -- lives in the panel. Set
`acl2-session.decorations` to `wash` for whole-form background washes
instead.

**ACL2: Show Panel** opens a structured view beside the editor that follows
the cursor: the form's status and timings, a failure's checkpoints as terms
with the prover's per-goal output one click away, the rules an admitted
proof used, and a live tail while the form proves.

Edits resync automatically -- the daemon reuses the untouched prefix, rolls
back to the first real change, and re-checks from there.

Zero runtime dependencies: the JSON-RPC framing is ~150 lines in
[src/protocol.ts](src/protocol.ts), deliberately vscode-free so it can be
exercised under plain node (see below).

## Syntax highlighting

The extension contributes an `acl2` language for `.lisp` and `.acl2` with its
own grammar, rather than injecting into a third-party Lisp grammar: VSCode
ships no Lisp support at all, so an injection would silently do nothing for
anyone who has not installed one.

Definer heads are scoped by *family* -- `defthm` and `defrule` as theorems,
`defun`/`defund`/`define` alike as functions, then constants, datatypes,
macros and doc forms -- and the defined name takes the matching scope, so a
theorem name and a function name are distinguishable by any theme that cares
to distinguish them. A head the list does not know (`def-rp-rule`,
`std::deflist`, `def::un`) still reads as a definer and still names what it
defines, which matters because the long tail of book-local definers is real.
Beyond that: events (`local`, `encapsulate`, `include-book`, `in-theory`),
binding forms, `declare`/`xargs`, keywords, package prefixes, `*earmuffed*`
constants, rationals, character literals, and nested `#|` block comments.

The grammar is lexical, so it cannot know that a project's own macro defines
a theorem, or that a name refers to something admitted. That is the semantic
token layer's job, and the daemon already knows the answers; it is not built
yet.

Verified by tokenizing the 10,712 books of a full ACL2 checkout (5.0M lines)
under the real TextMate engine: no file ends with an unterminated string or
comment, and of 244,608 definer heads none went unrecognized outside of the
4,024 that sit inside comments or strings, where silence is correct.

To keep another Lisp extension on `.lisp` instead, override the association:

```json
"files.associations": { "*.lisp": "lisp" }
```

## Build

```sh
cd editors/vscode
npm install
npm run compile
code .   # then F5 ("Run Extension") -> a new [Extension Development Host] window
```

Install the daemon first (`cargo install --path crates/acl2-session-editor`),
or point `acl2-session.daemonPath` at a built binary.

To use it day to day without F5, load it as an unpacked extension:

```sh
ln -s "$(pwd)" ~/.vscode/extensions/gjurgensen.acl2-session-0.0.1
```

and restart VSCode (recompile with `npm run compile` after pulling changes).

## Configure

- `acl2-session.daemonPath` -- the daemon binary (default: on `PATH`).
- `acl2-session.defaultImage` / `acl2-session.trusted` -- the daemon's
  `--default` and `--trusted` trust flags; `ACL2_DEFAULT` and `ACL2_TRUSTED`
  in the environment work too.

## Commands

Start / Stop Live Checking, Interrupt, Retract to Cursor, Target at Cursor,
Clear Target, Show Output for Form at Cursor, Go to Frontier.

The target is Proof-General-shaped: auto-admission goes only as far as the
target, retraction moves the world back and *holds* it there, and clearing
the target checks the whole book.

## Smoke test without an editor

```sh
node smoke.mjs ../../target/debug/acl2-session-editor /path/to/saved_acl2
```

Drives the compiled protocol module against a real daemon and ACL2: open,
a triage that skips a red form and proves its conditional dependent, an
output slice off the red form's log, close. The editor UI is the only layer
this does not cover.
