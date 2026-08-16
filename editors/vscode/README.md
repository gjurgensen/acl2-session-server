# ACL2 Session (VSCode)

Live form-by-form checking of ACL2 books, as a thin client of the
`acl2-session-editor` daemon (this repository's third crate). The daemon owns
the session; this extension renders its event stream.

The default rendering is a **gutter status column** of bars, and the buffer
text stays entirely unmarked: gray over every line still queued for
processing, blue over the form being proved, green over admitted forms,
yellow over forms admitted below a red one (conditional on its unproved
claim), red over a form admitted only under `skip-proofs` or failed
outright. Every status also marks the scrollbar, and the failure envelope
-- error line, abort cause, checkpoints -- lives in the panel. Set
`acl2-session.decorations` to `wash` for whole-form background washes
instead.

**ACL2: Show Panel** opens a structured view beside the editor that follows
the cursor: the form's status and timings, a failure's checkpoints as terms
with the prover's per-goal output one click away, the rules an admitted
proof used, and a live tail while the form proves.

## The console

**ACL2: Show Console** opens a window for forms the file does not contain.
There is **one per session**, titled after its book, because that is what an
eval belongs to: it ran against the world *that* book has built, and its
answer means nothing against another book's world. The panel follows the
cursor; the console does not follow anything.

It is deliberately *not* a REPL: the session undoes every event an evaluated
form introduces, so a `defun` typed here is gone by the next form. What it is
for is everything that is not part of the book -- `pe`, `pl`, `pr`,
`disabledp`, a throwaway `thm`, `set-gag-mode` -- where routing through the
file would mean two edits per question and would make the question part of
the prefix the daemon must admit. Settings stick where events do not, on
purpose: a debugging session that turns on accumulated persistence and then
asks for the results would break otherwise.

A form runs against the world the file has built so far, and a query asked
while a proof is running waits for that one form rather than being refused
or jumping the queue. Enter sends a form whose parens are closed and adds a
line to one that is not, the way a Lisp REPL does -- which is also the guard
that keeps a half-typed form from wedging the session. Up and Down walk the
history.

What you type is colored as you type it, by the same grammar and the same
theme the buffer uses -- the input is a transparent textarea over a colored
copy of its own text, and each edit asks the extension host to tokenize,
since a webview can do neither. A second, approximate highlighter local to
the webview would have been cheaper and is exactly what the panel's terms
avoid.

A failed experiment comes back with the same envelope a failed `defthm`
does, rendered by the same code: error line, abort cause, checkpoints as
terms, the events inside a compound form. Output is bounded at three
levels -- older entries fold to one line, the newest shows a scrolling
excerpt of what ACL2 printed, and **full log** puts the whole thing in a
document. A form that changed the world says so, and says whether the
rollback held.

**ACL2: Evaluate Selection** sends the selection to that book's console,
opening it if it was closed; with nothing selected it just opens the console,
since that is already the place to type.

Edits resync automatically -- the daemon reuses the untouched prefix, rolls
back to the first real change, and re-checks from there.

The JSON-RPC framing is ~150 lines in [src/protocol.ts](src/protocol.ts),
dependency-free and deliberately vscode-free so it can be exercised under
plain node (see below). The two runtime dependencies are `vscode-textmate`
and `vscode-oniguruma`, and they exist for the panel alone -- see below.

## Syntax highlighting

The extension contributes an `acl2` language for `.lisp` and `.acl2` with its
own grammar, rather than injecting into a third-party Lisp grammar: VSCode
ships no Lisp support at all, so an injection would silently do nothing for
anyone who has not installed one.

Definer heads are scoped by *family* -- `defthm` and `defrule` as theorems,
`defun`/`defund`/`define` alike as functions, then constants, datatypes,
macros and doc forms -- and the defined name takes the matching scope, so a
theorem name and a function name are distinguishable by any theme that cares
to distinguish them. A head the list does not know (`def-rp-rule`,
`std::deflist`, `def::un`) still reads as a definer and still names what it
defines, which matters because the long tail of book-local definers is real.
Beyond that: events (`local`, `encapsulate`, `include-book`, `in-theory`),
binding forms, `declare`/`xargs`, keywords, package prefixes, `*earmuffed*`
constants, rationals, character literals, and nested `#|` block comments.

ACL2's own functions -- `equal`, `car`, `len`, `implies`, `+`, `<` -- are
marked in call position. The list is deliberately the stable core, 97 names
that ACL2 itself defines and that the community books call thousands of times
apiece, not the 1,700 names ACL2 defines in total, most of which are prover
machinery no book writes and which move between releases. Omitting one costs
nothing: it simply renders as an ordinary symbol. Call position only, because
a macro can put anything at all in an argument slot, but a head is a head.
Ordinary calls are left unmarked for the same reason -- `(let ((x 1)) ...)`
puts a *variable* in head position, and in real books single letters are
among the most frequent things sitting there.

The grammar is lexical, so it cannot know that a project's own macro defines
a theorem, or that a name refers to something admitted. That is the semantic
token layer's job, and the daemon already knows the answers; it is not built
yet.

The panel's terms and runes go through *the same grammar*, so a term in the
panel and the same term in the buffer are tokenized identically rather than
by a second, approximate tokenizer. Coloring them is the harder half: a
webview receives workbench colors but never the editor's token colors, and
there is no API for the active theme's palette. So
[src/highlight.ts](src/highlight.ts) finds the extension contributing the
`workbench.colorTheme` label, reads its JSON off disk, and hands the rules
to `vscode-textmate`, which resolves scopes to colors with VSCode's own
algorithm -- user `editor.tokenColorCustomizations` included. That is what
the two runtime dependencies buy, and why the tokenizing is done in the
extension host rather than the webview.

When the theme cannot be read -- an unparseable file, a tmTheme plist -- the
panel falls back to the `acl2Session.token.*` colors this extension
contributes, which default to the terminal palette and which a theme or a
user can set directly. Lean's infoview solves the same problem the same way,
minus the theme reading: it contributes its handful of colors and does not
attempt to match the editor's syntax palette at all.

Verified by tokenizing the 10,712 books of a full ACL2 checkout (5.0M lines)
under the real TextMate engine: no file ends with an unterminated string or
comment, and of 244,608 definer heads none went unrecognized outside of the
4,024 that sit inside comments or strings, where silence is correct.

To keep another Lisp extension on `.lisp` instead, override the association:

```json
"files.associations": { "*.lisp": "lisp" }
```

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

Start / Stop Live Checking, Interrupt, Show Panel, Show Console, Evaluate
Selection, Retract to Cursor, Target at Cursor, Clear Target, Show Output for
Form at Cursor, Go to Frontier.

The target is Proof-General-shaped: auto-admission goes only as far as the
target, retraction moves the world back and *holds* it there, and clearing
the target checks the whole book.

## Smoke test without an editor

```sh
node smoke.mjs ../../target/debug/acl2-session-editor /path/to/saved_acl2
```

Drives the compiled protocol module against a real daemon and ACL2: open,
a triage that skips a red form and proves its conditional dependent, an
output slice off the red form's log, a query, a failed experiment's
envelope, an evaluated event being undone, an eval sent mid-check waiting
its turn, close. The editor UI is the only layer this does not cover.

```sh
node webview.mjs
```

Needs neither daemon nor ACL2. Both webviews are strings, so nothing in them
is reachable from `tsc` or from the smoke test, and a syntax error in one
shows up only as a window that silently does nothing. This builds each page
and compiles its script, then lifts out the function deciding whether Enter
submits a form and exercises it against strings, character literals,
bar-quoted symbols and both comment syntaxes. That one is tested because
getting it wrong is not cosmetic: an unbalanced form sent to the session
swallows the marker that ends it, and ACL2 waits for the rest of the input
until something interrupts.
