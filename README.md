# acl2-session-server

A file-anchored, structured interface to a live ACL2 session, exposed over MCP.

Status: **early.** The core library works; the MCP interface is not written yet.

## What it is

A server that keeps an ACL2 process in sync with a `.lisp` file on disk and
reports what happened as structured data rather than as terminal text.

Two ideas carry the design.

**The file is the source of truth.** The session's logical world is a cache of a
prefix of a real file, never a parallel universe. Rollback is a consequence of
editing rather than an operation the client has to request and track. The
invariant is:

> the session world equals the result of admitting file forms `1..N` in a plain
> interactive ACL2 session started in the book's own directory

A client edits the file with ordinary tools, asks the server to sync, and the
server works out the minimum to do: reuse the unchanged prefix, roll back to the
first difference, admit forward from there.

**Read ACL2's state, don't scrape its output.** Key checkpoints, the rules a
proof used, warnings, timings, and abort causes are all obtainable as data —
via `get-event-data` and `gag-state`, without adding anything to the ACL2
world. A failed proof comes back with its checkpoints as terms and a typed
reason, not as a wall of text to be parsed.

The complete output log is always captured and retrievable by handle, because
some things — the induction scheme the prover chose, per-goal rewriting detail,
arbitrary macro output — exist only in the printed narrative. The structured
result is a fast path over a complete log, never a replacement for it.

## Layout

```
crates/
  acl2-session/        core: ACL2 process, file-prefix invariant, form
                       splitting, structured extraction. No transport.
  acl2-session-mcp/    MCP server: a thin mapping onto the core.
```

The first client is MCP, for coding agents. The core is deliberately
transport-neutral because an editor is a plausible second client, and the split
is a workspace boundary rather than a naming convention so the compiler enforces
it. The core does not depend on the MCP SDK, and interface crates own their own
wire types.

## Runtime

Rust, distributed as a single binary. The server is essentially a process
supervisor with a concurrent state machine — a session admits one form at a
time, output pumps in the background, and deadlines and interrupts race against
both — so the invariants are worth making compile-time obligations. A binary
also beats a virtualenv when the install step is "put a command path in a JSON
config".
