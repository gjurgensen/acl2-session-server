# acl2-session-server

A file-anchored, structured interface to a live ACL2 session — for agents (over
MCP) and for people (in VSCode).

## Overview

`acl2-session` is a library for maintaining ACL2 sessions synced against
files. It supports minimal re-syncing: when a form in the book is modified and
then the entire file re-synced, the session is undone exactly through the
modified form, then the subsequent forms resubmitted. It also tracks
`include-book` dependencies. If a dependency is modified, the book will undo and
resubmit appropriately on the next sync (undoing back to the first place the
book is included).

The library provides structured information for synced forms. This includes key
checkpoints, rules used, warnings, timings, etc., retrieved from functions like
`get-event-data` and `gag-state`. The traditional ACL2 output is also available,
divided by corresponding form.

## Interfaces

Two front-ends are built on top of the core `acl2-session` library:

- **[MCP server](crates/acl2-session-mcp/README.md)** (`crates/acl2-session-mcp`)
  — for agents. Its README has the install, the client configuration, the tool
  list, and the structured failure envelope.
- **[VSCode extension](editors/vscode/README.md)** (`editors/vscode`)
  — live form-by-form checking, as a client of the editor daemon
  (`crates/acl2-session-editor`) over JSON-RPC. Its README has the commands, the
  rendering, and the settings.

The core is transport-neutral, and the two front-ends are its two clients.
Both resolve their ACL2 image the same way.

## Choosing the ACL2 image

Both front-ends and the core library take a file path for the default ACL2
image. However, many ACL2 users maintain several copies of ACL2 and the
community books, so there is also a mechanism for identifying trusted
directories from which the system may run ACL2 images. If the synced book
appears to be in the community books, an upstream ACL2 image is used when
found in a trusted directory. Otherwise, the default image is used.

The set of runnable images is an **allowlist**, not a search path. An image
runs only if it is the configured default or lives under a trusted directory.

- **`--default <image>`** is used for books outside every trusted directory.
- **`--trusted <dir>`** (repeatable) vouches for the ACL2 checkouts under a
  directory: a book inside one runs under **its own checkout's image**.
- `ACL2_DEFAULT` and `ACL2_TRUSTED` (colon-separated) set the same two from the
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