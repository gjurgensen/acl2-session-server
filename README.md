# acl2-session-server

A file-anchored, structured interface to a live ACL2 session — for agents (over
MCP) and for people (in VSCode).

Status: **early**, but working end to end: a book syncs, failures come back
structured, and both front-ends drive it.

## What it is

A server that keeps an ACL2 process in sync with a `.lisp` file on disk and
reports what happened as structured data rather than as terminal text.

Two ideas carry the design.

**The file is the source of truth.** The session's logical world is a cache of a
prefix of a real file, never a parallel universe, so rollback is a consequence
of editing rather than something a client has to request and track:

> the session world equals the result of admitting file forms `1..N` in a plain
> interactive ACL2 session started in the book's own directory

A client edits the file with ordinary tools and asks the server to sync. The
server reuses the unchanged prefix, rolls back to the first difference, and
admits forward from there. Reindenting a form or editing a comment inside it
costs nothing.

**Read ACL2's state, don't scrape its output.** Key checkpoints, the rules a
proof used, warnings, timings and abort causes all come from `get-event-data`
and `gag-state`, without adding anything to the ACL2 world. A failed proof comes
back with its checkpoints as terms and a typed reason, not a wall of text — and
a checkpoint's goal name is exactly the handle for pulling what the prover
printed under it, so there is nothing to guess out of a log. The concrete
failure envelope is shown field by field in the
[MCP README](crates/acl2-session-mcp/README.md#tools).

The complete log is still captured and retrievable by handle, because some
things — the induction scheme the prover chose, per-goal rewriting detail,
arbitrary macro output — exist only in the printed narrative. The structured
result is a fast path over a complete log, never a replacement for it.

## Interfaces

Two front-ends over one transport-neutral core:

- **[MCP server](crates/acl2-session-mcp/README.md)** (`crates/acl2-session-mcp`)
  — an agent drives the session as a set of tools. Its README has the install,
  the client configuration, the tool list, and the structured failure envelope.
- **[VSCode extension](editors/vscode/README.md)** (`editors/vscode`) — live
  form-by-form checking for a person, as a client of the editor daemon
  (`crates/acl2-session-editor`) over JSON-RPC. Its README has the commands,
  the rendering, and the settings.

The core is transport-neutral, and the two front-ends are its two clients.
Both resolve their ACL2 image the same way.

## Choosing the ACL2 image

Both front-ends run a book under a specific ACL2, and which one is a matter of
**trust**, because a book's image is the image it was certified against and
running it means running whatever that checkout's code does.

The set of runnable images is an **allowlist**, not a search path. An image
runs only if it is the configured default or lives under a trusted directory,
and that holds for images named explicitly at open time too — so neither a
repository nor a client can supply its own "ACL2".

- **`--trusted <dir>`** (repeatable) vouches for the ACL2 checkouts under a
  directory: a book inside one runs under **its own checkout's image**, and one
  entry covers every build there, including ones added later. Trust is
  subtree-wide and forward-open — it covers checkouts cloned under the root
  *after* you granted it, and an inner checkout wins over the one above it — so
  trust a directory whose contents you control.
- **`--default <image>`** is used for books outside every trusted directory.
- `ACL2_TRUSTED` (colon-separated) and `ACL2_DEFAULT` set the same two from the
  environment.

The MCP server takes these as `args`; the VSCode extension takes them as the
`acl2-session.trusted` and `acl2-session.defaultImage` settings. Each front-end
reports which image a session actually got, and the MCP `acl2_images` tool
lists what is permitted.

## Trying it without a client

To see the structured output directly, with no MCP or editor in the loop:

```
cargo build
./target/debug/acl2-session-syncfile <acl2-binary> <book.lisp>
```

Syncs a book and prints the structured report for each top-level form. Add
`--find <text>` or `--goal "Subgoal *1/2"` to slice a form's captured log.

## Tests

```
cargo test
ACL2=/path/to/saved_acl2 cargo test -- --ignored --test-threads=1
```

The first runs the unit tests. The second adds the integration tests, which
drive a real ACL2 — the prefix invariant, deadlines and interrupts, eval, and
the splitter across every book in a tree if `ACL2_BOOKS` is also set.

## Layout

```
crates/
  acl2-session/         core: ACL2 process, file-prefix invariant, form
                        splitting, structured extraction. No transport.
  acl2-session-mcp/     MCP server: wire types and a mapping onto the core.
  acl2-session-editor/  editor daemon: JSON-RPC over stdio, one stepping
                        driver per book.
editors/
  vscode/               VSCode client of the daemon: live coloring,
                        diagnostics from failure envelopes, an output view
                        following the cursor. Two small deps, for the panel.
```

The split is a workspace boundary rather than a naming convention, so the
compiler enforces it. Each interface has its own README beside its code
(`crates/acl2-session-mcp`, `editors/vscode`).

## Runtime

Rust 1.88 or later, distributed as a single binary.
