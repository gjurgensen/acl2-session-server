# ACL2 Session (VSCode)

Live form-by-form checking of ACL2 books, as a thin client of the
`acl2-session-editor` daemon (this repository's third crate). The daemon owns
the session; this extension renders its event stream.

The default rendering is a Lean-style **gutter status column**: a faint bar
over every line still queued for processing (turning bright over the form
being proved), a checkmark on each admitted form, and reds as Problems
entries with the failure envelope -- error line, abort cause, checkpoints --
squiggled on the form's first line. Forms admitted below a red one keep a
background wash in the *other* green: they are conditional on an unproved
claim, and that must be impossible to miss. All statuses also mark the
scrollbar. Set `acl2-session.decorations` to `wash` for whole-form
washes instead, with no diagnostics.

**ACL2: Show Panel** opens a structured view beside the editor that follows
the cursor: the form's status and timings, a failure's checkpoints as terms
with the prover's per-goal output one click away, the rules an admitted
proof used, and a live tail while the form proves.

Edits resync automatically -- the daemon reuses the untouched prefix, rolls
back to the first real change, and re-checks from there.

Zero runtime dependencies: the JSON-RPC framing is ~150 lines in
[src/protocol.ts](src/protocol.ts), deliberately vscode-free so it can be
exercised under plain node (see below).

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
