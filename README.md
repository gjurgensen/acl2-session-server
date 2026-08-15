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

As an MCP server, install it:

```
cargo install --path crates/acl2-session-mcp
```

Then configure your client. The server is started as a subprocess, so it is
configured through `args` (or `env`). With a single ACL2, that is one path:

```json
{
  "mcpServers": {
    "acl2": {
      "command": "/home/you/.cargo/bin/acl2-session-mcp",
      "args": ["/home/you/acl2/saved_acl2"]
    }
  }
}
```

If you keep several ACL2 builds side by side — a release and a development
tree, or a set of git worktrees — name the directory holding them instead:

```json
"args": ["--trusted", "/home/you/acl2-builds",
         "--default", "/home/you/acl2-builds/release/saved_acl2"]
```

`--trusted` vouches for the ACL2 checkouts under a directory. A book inside one
runs under **its own checkout's image**, which is usually what you want, since a
book was certified against the checkout it lives in. One entry covers every
build under that directory, including ones added later. `--default` is used for
books outside every trusted directory. `ACL2_TRUSTED` (colon-separated) and
`ACL2_DEFAULT` work too, for clients where editing `args` is awkward.

The trusted set is an allowlist, not just a search path: an image is run only if
it is the default or lives under a trusted directory. That holds for images
named explicitly in `acl2_session_open` as well, so a repository cannot supply
its own "ACL2" by shipping an `acl2.lisp` next to an executable, and neither
can a client that was told to. `acl2_images` lists what is permitted, and
`acl2_session_open` reports which image the session actually got. Note that
trust is subtree-wide and forward-open: it covers every checkout under the
directory, including ones cloned there *after* you granted it — and
innermost-checkout-wins means anything checked out inside a trusted root can
carry its own image and be selected. Trust a directory whose contents you
control, not one others can write into.

Twelve tools: `session_open`, `session_close`, `sessions`, `images`, `sync`,
`eval`, `certify`, `resume`, `status`, `interrupt`, `health`, `output`.

`certify` runs `cert.pl` on the session's book, with the same ACL2 image the
session runs and the `cert.pl` from that image's own checkout. It runs outside
the session, so the world is untouched — and because certification chases
dependencies, the reply says *which* book failed, which is usually not the one
you asked for.

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

Rust 1.88 or later, distributed as a single binary.
