# Installing

Two front-ends are built on the same core, and they are independent. Install
whichever you want; nothing here requires the other.

- **[MCP server](#the-mcp-server)** — drives a session from an agent.
- **[VSCode extension](#the-vscode-extension)** — live form-by-form checking
  while you edit.

There is also a **[command-line demo](#trying-it-without-a-client)** that needs
neither, and is the shortest way to see whether this works on your machine.

## Prerequisites

**Linux or macOS.** Windows is not supported: a session drives ACL2 through a
pseudo-terminal.

**An ACL2 image** — a `saved_acl2` from a built checkout, **version 8.7 or
later**. This is a hard floor rather than a recommendation: the library reads
system internals, and the rule scrape needs a function 8.7 introduced. On an
older image a session silently reports no rules for every form. See
[ACL2's own installation instructions](https://www.cs.utexas.edu/~moore/acl2/)
if you do not have one.

Induction reporting wants a *newer* image still — ACL2 added
`get-event-data`'s induction records after the 8.7 release. Everything else
works without it, and the MCP server's `acl2_health` names what an image is
missing. The version string cannot tell you, since an image with the field and
one without both call themselves 8.7.

**Rust**, for either front-end — both are Rust binaries. Version **1.88 or
later**; the workspace is edition 2024. Install it with
[rustup](https://rustup.rs), which puts `cargo install`'s output in
`~/.cargo/bin` and that directory on your `PATH`.

```sh
cargo --version    # 1.88.0 or later
```

**Node.js** is needed for the **extension only** — skip it if you are here for
the MCP server. Version **22 or later**, and npm comes with it. If your
`npm install` refuses with `Unsupported engine`, that is this requirement
being enforced rather than something being broken; see
[the note below](#why-node-22).

```sh
node --version     # v22 or later
```

## The MCP server

From the repository root:

```sh
cargo install --path crates/acl2-session-mcp
```

That builds and installs `acl2-session-mcp` into `~/.cargo/bin`. Then register
it with your MCP client. With Claude Code, user-scoped:

```sh
claude mcp add -s user acl2 -- ~/.cargo/bin/acl2-session-mcp --default <saved_acl2> --trusted <dir>
```

or with Codex:

```sh
codex mcp add acl2 -- ~/.cargo/bin/acl2-session-mcp --default <saved_acl2> --trusted <dir>
```

`--default` and `--trusted` decide which ACL2 image a given book runs under;
see [Choosing the ACL2 image](README.md#choosing-the-acl2-image). **At least
one is required** — a server with neither has no image it is allowed to run,
and says so on startup rather than failing at the first session. The
environment variables `ACL2_DEFAULT` and `ACL2_TRUSTED` set the same two.

The [MCP server's README](crates/acl2-session-mcp/README.md) has the tool list
and the shape of what comes back.

## The VSCode extension

The extension is a thin client of the `acl2-session-editor` daemon, so the
daemon comes first. From the repository root:

```sh
cargo install --path crates/acl2-session-editor
```

Or, instead of installing it, point the `acl2-session.daemonPath` setting at a
binary you have built.

Then build and package the extension:

```sh
cd editors/vscode
npm install
npm run compile
npx @vscode/vsce package
code --install-extension acl2-session-*.vsix
```

`code` installs into whichever VSCode variant is on your `PATH` — stock,
Insiders, or VSCodium. On macOS that command does not exist until you run
**Shell Command: Install 'code' command in PATH** from VSCode's command
palette. The glob picks up the version `vsce` wrote into the filename, so
nothing is pinned to a version or a fixed extensions path; repackage and
reinstall to pick up later changes.

Finally, tell the extension where ACL2 is, with the `acl2-session.defaultImage`
and `acl2-session.trusted` settings — the same two ideas as the MCP server's
`--default` and `--trusted`. The
[extension's README](editors/vscode/README.md) covers the commands, the
rendering and the rest of the settings.

### Why Node 22

`npm install` will refuse to run on anything older, saying `Unsupported
engine` and naming a package, its requirement and your version. That is
deliberate: the packaging tool is a dependency here rather than an `npx`
download precisely so the floor is checked by the tooling, and npm's default of
installing anyway with a warning is turned off in `.npmrc`.

The package it names is usually an `@azure/*` one rather than `@vscode/vsce`,
since npm reports whichever it reaches first and the requirement comes from
deep in the tree. Nothing is wrong with your setup and nothing here talks to
Azure; the answer is the same either way, which is a newer Node.

The floor is higher than the extension itself needs, and higher than `vsce`
itself asks for — `vsce` wants Node 20, and the extra step to 22 comes from a
transitive dependency on its *publishing* path, which packaging never touches.
Nothing here publishes to a marketplace. If that floor is in your way,
`npx @vscode/vsce@2 package` asks only for Node 16, or drop `engine-strict`
from `.npmrc` and take a warning instead of a refusal.

None of this affects the installed extension, which runs inside VSCode's own
Node rather than yours. Node is a *build* requirement only.

## Trying it without a client

To see the structured output directly, with no MCP client and no editor:

```sh
cargo build
./target/debug/acl2-session-syncfile <saved_acl2> <book.lisp>
```

It syncs a book and prints the structured report for each top-level form. Add
`--find <text>` or `--goal "Subgoal *1/2"` to slice a form's captured log.

## Running the tests

```sh
cargo test
ACL2=/path/to/saved_acl2 cargo test -- --include-ignored
```

The first runs the unit tests, which need no ACL2. The second runs those *and*
the integration tests, which drive a real ACL2: the prefix invariant, deadlines
and interrupts, eval, and the wire of both front-ends end to end. Set
`ACL2_BOOKS` as well and the splitter is checked across every book in that
tree.

The extension has its own suite, which needs no ACL2:

```sh
cd editors/vscode
npm test
```

A failure in the live suite is not by itself a verdict on the library. It also
depends on things ACL2's version does not pin — the community books' layout,
ACL2's own wording, a book directory's `acl2-customization.lsp`.
`crates/acl2-session/tests/live.rs` separates the two.
