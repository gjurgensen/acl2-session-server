# acl2-session-mcp

The MCP server: a wire layer that exposes the [core](../acl2-session) over the
Model Context Protocol, so an agent drives a live ACL2 session as a set of
tools. It is a thin mapping onto the core's `Book`/`Registry` and adds no logic
of its own beyond the protocol.

For what a session *is* — the file-prefix invariant, why failures come back as
structure rather than text — see the [top-level README](../../README.md). This
document is the interface: how to install it, how to point it at an ACL2, and
what the tools are.

## Install

```
cargo install --path crates/acl2-session-mcp
```

The server is started as a subprocess by the client, so it is configured
entirely through `args` (or the environment) — there is no config file of its
own. With a single ACL2, that is one path:

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
tree, or a set of git worktrees — name the directory holding them instead of a
single image:

```json
"args": ["--trusted", "/home/you/acl2-builds",
         "--default", "/home/you/acl2-builds/release/saved_acl2"]
```

`--trusted` and `--default` are the shared image-trust flags, described under
[Choosing the ACL2 image](../../README.md#choosing-the-acl2-image) in the
top-level README; `ACL2_TRUSTED` (colon-separated) and `ACL2_DEFAULT` work too,
for clients where editing `args` is awkward. The short version: a book runs
under its own trusted checkout's image, `--default` covers books outside every
trusted directory, and `acl2_images` reports what is permitted.

**Restarting matters.** `cargo install ... --force` does not restart a running
server — the client keeps talking to the old subprocess until the client is
restarted, which looks exactly like a fix that did not take.

## Tools

Thirteen, each returning structured JSON:

| tool | what it does |
| --- | --- |
| `acl2_session_open` | Open a session anchored to a `.lisp` book. Returns the `session_id` every other tool takes. |
| `acl2_session_close` | Close a session and kill its ACL2. Its output handles stop working. |
| `acl2_sessions` | List open sessions. |
| `acl2_images` | List the ACL2 images this server may run (the trust allowlist). |
| `acl2_sync` | Bring the session into agreement with the file: reuse the unchanged prefix, roll back to the first difference, admit forward. |
| `acl2_eval` | Evaluate a one-off form without changing the logical world — the event it introduces is undone. |
| `acl2_resume` | Keep waiting for a form whose deadline expired; the session was left running it. |
| `acl2_certify` | Certify the session's book with `cert.pl`, outside the session. |
| `acl2_status` | Progress on whatever is still running — a form in flight, a certification. |
| `acl2_interrupt` | Abort the form in flight and make the session usable again. |
| `acl2_health` | Whether the session is answering: ready, busy, unresponsive, dead. |
| `acl2_output` | Read part of a form's captured output, by handle. |
| `acl2_induction` | Why the prover inducted, for a form whose `rules` carry an `:induction` rune. |

Three of these carry caveats worth stating once.

**`acl2_sync` stops at the first failure** and reports it as structure. The
reply's `failed` object, for a theorem whose proof reverts to induction and
runs out of depth (abridged — the induction-step term and ten more runes are
elided):

```json
"failed": {
  "status": "proof_failure",
  "abort_cause": "induction-depth-limit-exceeded",
  "failed_event": "car-of-my-rev",
  "failed_kind": "defthm",
  "error_line": "ACL2 Error [Failure] in ( defthm car-of-my-rev ...):  See :DOC failure.",
  "index": 2, "line_start": 8, "line_end": 9,
  "prover_steps": 1178,
  "checkpoints": {
    "pre_revert": true, "forced": false, "n_top": 1, "n_sub": 1,
    "top": [ { "goal": "Goal",
               "term": "(equal (car (my-rev x)) (car x))" } ],
    "sub": [ { "goal": "Subgoal *1/2''",
               "term": "(implies (and (consp x) ..." } ]
  },
  "inner": [ { "name": "car-of-my-rev", "kind": "defthm",
               "abort_causes": "(induction-depth-limit-exceeded revert)",
               "prove_ms": 2, "prover_steps": 1178,
               "rules": [ "(:definition my-rev)",
                          "(:induction my-rev)", "..." ] } ],
  "inner_detail_through": 1,
  "output_ref": "s653-0:f3", "output_lines": 94
}
```

A checkpoint's `goal` is exactly what `acl2_output mode:"goal"` takes, so
"show me what the prover did under `Subgoal *1/2''`" is one call, with no names
guessed out of a log. The complete log is still captured and retrievable by
handle, because some things — the induction scheme the prover chose, per-goal
rewriting detail, arbitrary macro output — exist only in the printed narrative.
The structured result is a fast path over a complete log, never a replacement
for it.

**`acl2_induction`** is alone among the tools in being a *parse of ACL2's
prose*: the scheme it returns is printed and recorded nowhere — not in
`get-event-data`, not in `gag-state` — so it is best effort. Fields go missing
rather than wrong, and a reworded ACL2 returns nothing at all. Call it only for
a form whose `rules` carry an `:induction` rune.

**`acl2_certify`** runs `cert.pl` on the session's book, with the same ACL2
image the session runs and the `cert.pl` from that image's own checkout. It
runs outside the session, so the world is untouched — and because certification
chases dependencies, the reply says *which* book failed, which is usually not
the one you asked for.

## The PROVE-output disclosure

`acl2_session_open` may return a `prove_output_overridden` field. The session
inherits the user's `acl2-customization.lsp`, and a common thing to put there
is `prove` on `inhibit-output-lst`, which silences the waterfall at a terminal
— but it also disables output *saving*, which would cost every proof its
checkpoints and induction narrative. So the server removes `prove` (and only
`prove`) from the inherited list, and reports what it inherited in this field.

It is present only when an override happened. When set, prover output in the
captured log is output the user's own interactive session would have
suppressed — worth knowing if you are comparing the two. The file admits
identically either way; only what the log captures differs.

## Tests

The wire is covered end to end by
[`tests/wire_live.rs`](tests/wire_live.rs), which spawns the built binary and
drives it over real stdio JSON against a real ACL2. Run it with the workspace
live suite:

```
ACL2=/path/to/saved_acl2 cargo test -p acl2-session-mcp -- --ignored --test-threads=1
```
