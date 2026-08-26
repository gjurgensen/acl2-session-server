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

## Installing

Both front-ends need **Rust 1.88+** and an **ACL2 image, version 8.7 or
later**; the VSCode extension additionally needs **Node 22+** to build. They
are independent — install either one without the other.

**[INSTALL.md](INSTALL.md)** has all of it: the prerequisites in full, the MCP
server, the extension, and the tests.

The shortest way to see whether this works on your machine needs neither
front-end:

```sh
cargo build
./target/debug/acl2-session-syncfile <saved_acl2> <book.lisp>
```

That syncs a book and prints the structured report for each top-level form.
