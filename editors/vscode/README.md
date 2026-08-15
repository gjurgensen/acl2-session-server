# ACL2 Session (VSCode)

Live form-by-form checking of ACL2 books, as a thin client of the
`acl2-session-editor` daemon (this repository's third crate). The daemon owns
the session; this extension renders its event stream: admitted forms get a
green wash, a form admitted only under `skip-proofs` is red with a Problems
entry, forms admitted below it are the *other* green (conditional on the
unproved claim), and the form being proved is highlighted while its output
streams. Edits resync automatically -- the daemon reuses the untouched
prefix, rolls back to the first real change, and re-checks from there.

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
