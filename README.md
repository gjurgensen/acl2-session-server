# acl2-session-server

A file-anchored, structured interface to a live ACL2 session, exposed over MCP.

Status: **early**, but working end to end: a book syncs, failures come back
structured, and the MCP server speaks the protocol over stdio.

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
back with its checkpoints as terms and a typed reason, not a wall of text.

The complete log is still captured and retrievable by handle, because some
things — the induction scheme the prover chose, per-goal rewriting detail,
arbitrary macro output — exist only in the printed narrative. The structured
result is a fast path over a complete log, never a replacement for it.

## Trying it

Without MCP, to see the structured output directly:

```
cargo build
./target/debug/acl2-session-syncfile <acl2-binary> <book.lisp>
```

Syncs a book and prints the structured report for each top-level form. Add
`--find <text>` or `--goal "Subgoal *1/2"` to slice a form's captured log.

As an MCP server, install it and point it at your ACL2:

```
cargo install --path crates/acl2-session-mcp
```

That puts `acl2-session-mcp` in `~/.cargo/bin`. Configure your client with the
path to your ACL2 image as its argument:

```json
{
  "mcpServers": {
    "acl2": {
      "command": "/home/you/.cargo/bin/acl2-session-mcp",
      "args": ["/path/to/saved_acl2"]
    }
  }
}
```

Ten tools: `session_open`, `session_close`, `sessions`, `sync`, `eval`,
`resume`, `status`, `interrupt`, `health`, `output`.

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
  acl2-session/      core: ACL2 process, file-prefix invariant, form
                     splitting, structured extraction. No transport.
  acl2-session-mcp/  MCP server: wire types and a mapping onto the core.
```

The core is transport-neutral because an editor is a plausible second client.
The split is a workspace boundary rather than a naming convention, so the
compiler enforces it.

## Runtime

Rust 1.85 or later, distributed as a single binary.
