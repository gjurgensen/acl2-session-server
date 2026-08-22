# ACL2 (VSCode)

Live form-by-form checking of ACL2 books, as a thin client of the
`acl2-session-editor` daemon. The daemon owns the session; this extension
renders its event stream.

The extension automatically syncs a background ACL2 session with the file. How
form status is then shown is `acl2-session.decorations`:

- **`gutter`** (the default) — a status column beside every form: grey queued,
  grey *pulsing* for the form being proved, green admitted, yellow conditional,
  red failed. The text itself is left alone.
- **`wash`** — a background wash over the whole of every form, reds included.
- **`minimal`** — only what is happening or wrong. A spinner at the head of the
  form being proved, and nothing else: an admitted form is unremarkable, a
  conditional one follows from the failure above it, and one not reached yet
  follows from sitting below the running one.

Each mode marks each fact once, which is the rule they are built around: never
a bar *and* a wash, never a wash *and* a squiggle.

Failed and skipped forms are reported in the **Problems** view in every mode,
which is also where the squiggle comes from — VSCode gives no way to have the
entry without the mark. `acl2-session.problems` turns both off. In `minimal`
that leaves a failure marked by nothing at all, scrollbar included, since in
that mode this *is* the mark.

### Colours

The status colours follow the active theme. Each status has a meaning in
VSCode's own vocabulary — `editorError.foreground` for a failure,
`editorWarning.foreground` for a conditional one, `editorInfo.foreground` for
the form being proved — and the extension reads those out of the theme,
keeping its own per-status opacity so the column stays quiet. The shipped
values are VSCode's defaults, so on the default light and dark themes nothing
moves; on any other theme the column follows it.

A theme that cannot be read falls back to those shipped values. That is a
normal outcome rather than a failure: most themes define a fraction of the
colour list and let VSCode's built-in defaults carry the rest, and those live
in no file for an extension to read.

Four sources, most specific first: `acl2-session.colors`, then
`workbench.colorCustomizations` on `acl2Session.bar.<status>` or
`acl2Session.wash.<status>`, then the theme, then what is shipped. Whichever
wins supplies the *hue*; the opacity stays this extension's, since a theme
colour carries none and cannot be given one.

A wash covers the form and stops there: whole lines through a multi-line
form's block, and the line it ends on cut at its closing paren rather than
running out to the edge of the viewport.

`acl2-session.washOpacity` is that opacity for the washes, and is the setting
most likely to be worth touching: how far a tint reads depends on the
background under it, so the same wash can be too faint on one theme and too
loud on another.

```jsonc
"acl2-session.washOpacity": 0.22,          // or { "light": 0.2, "dark": 0.16 }
```

It adjusts whatever colour won — including one from
`workbench.colorCustomizations` — but not a wash written out in full under
`acl2-session.colors`, which is taken exactly as given. The bars have no such
dial on purpose: their opacities differ per status because a running form is
the one to look at and an admitted one is a fact you can ignore, which is not
a matter of taste about the background.

The gutter bar is the exception, and it is a platform one: it is drawn as an
image, and an image cannot resolve a theme colour or a CSS variable. That is
why the theme is read into a concrete value rather than referenced — a value
can be baked into the generated icon, and then all four surfaces agree.
`acl2-session.colors` overrides that value directly.

```jsonc
"acl2-session.colors": {
  "failed":  { "bar": "#ff5555" },
  "running": { "bar": { "light": "#0057b7", "dark": "#7fb8ff" } }
}
```

Decoration colours are CSS, so a value here can be more than a literal — a
theme colour, adjusted:

```jsonc
"acl2-session.colors": {
  "failed": {
    "bar":  "color-mix(in srgb, var(--vscode-editorError-foreground) 65%, transparent)",
    "wash": "color-mix(in srgb, var(--vscode-editorError-foreground) 15%, transparent)"
  }
}
```

That follows whatever red the active theme uses, at an opacity of your
choosing — which a plain theme colour cannot express, since a colour
reference carries no alpha. The gutter bar is the exception again and for the
same reason: it is an image, and an image cannot see the workbench's CSS
variables, so it keeps the shipped icon and the other three surfaces take the
value. Only a literal colour reaches all four.

## The form view

The editor title bar shows a "play" icon in the upper right, which starts the
ACL2 session. This automatically opens the ACL2 form view, which displays structured
information for the current form under the user's cursor.

## The console

**ACL2: Show Console** opens a window for evaluating forms.
There is **one per session**, titled after its book, because that is what an
evaluation belongs to: it ran against the world *that* book has built, and its
answer means nothing against another book's world. The form view follows the cursor;
the console does not follow anything.

The console is not quite a REPL, and deliberately so. It is sufficient for forms
such as `:pe`, `:disabledp`, throwaway `thm`s, etc. But the results are
transient: a submitted `defun` is undone after evaluation and unavailable for
the next query. This keeps console forms from derailing the session and
de-synching it from the file.

## Syntax highlighting

The extension contributes an `acl2` language for `.lisp` and `.acl2` with its
own grammar.

Definer heads are scoped by *family* — `defthm` and `defrule` as theorems,
`defun`/`defund`/`define` alike as functions, then constants, datatypes,
macros and doc forms — and the defined name takes the matching scope, so a
theorem name and a function name are distinguishable by any theme that cares
to distinguish them. Beyond that: events (`local`, `encapsulate`,
`include-book`, `in-theory`), binding forms, `declare`/`xargs`, keywords,
package prefixes, `*earmuffed*` constants, rationals, character literals, and
nested `#|` block comments.

ACL2's own functions — `equal`, `car`, `len`, `implies`, `+`, `<` — are
marked in call position.

The form view's terms and runes go through *the same grammar*, so a term in the
form view and the same term in the buffer are tokenized identically.

## Language features

- **Hover** shows what a name is: a function's signature and
  guard, a theorem's statement, where it came from (this file, a named book, or
  built into ACL2), and an xdoc one-liner when there is one. A name the world
  does not know has no hover, which is itself the answer.
- **Go to Definition** jumps to a name defined in this book, and says whether
  the target is exact — read there by the daemon — or a lexical best guess.
- **Include-book links** turn each `(include-book "...")` path into a link to
  the file it resolves to, under the same `:dir` rules ACL2 uses.
- **Completion** offers names the world holds, so it proposes what you can
  actually call rather than every token that has appeared in the buffer.
- **Outline** lists the book's events in order, the names macros generated
  included.

Most of these never touch the session: the outline, the include-book links, and
a jump to a name defined *in this file* are read from the buffer, so they work
while a proof is grinding. Hover and completion answer from what the daemon
scraped as it admitted each form. Only a lookup of a name from *outside* this
file — a built-in, or one an included book defines — needs the live session,
and that is the one case **declined cleanly** while it is busy, rather than
blocked or answered falsely absent.

**ACL2: Show Documentation** opens the full xdoc topic for the name at the
cursor in a read-only view, where the hover shows only its first sentence.

## Highlighting the output log

The log is not a Lisp file. It is prose with Lisp embedded in it, and prose
is hostile to a Lisp grammar: `Subgoal *1/2''` and `we'll` are apostrophes
that open a quote, and one stray `"` opens a string. TextMate carries a rule
stack across lines, so an escape does not end at the line — it ends at the
end of the file.

So `syntaxes/acl2-output.tmLanguage.json` never runs the ACL2 grammar over
the log. It marks what is unambiguous from the line alone — goal headers,
`Summary`/`Form:`/`Rules:`/`Time:` labels, `ACL2 Error`, `FAILED`, `Q.E.D.`
— and enters the real grammar only inside *islands*: the term under a goal
header, the rune list after `Rules:`, the form printed by `pe`.

Each island is a `begin`/`while` block. `while` is re-tested at the start of
every line and pops the block **regardless of what is open inside it**, so an
unterminated string in a truncated term costs one block rather than the rest
of the log. That is the whole containment argument, and `output-grammar.mjs`
proves it: five adversarial inputs open a string, a block comment, an
unbalanced paren and a bar-quoted symbol inside islands, and assert the prose
after each comes out clean.

Two known limits, both deliberate. A term whose first line ends in a bare
symbol is not highlighted — the opening line must close a paren, which is
what keeps a wrapped sentence like `(acl2-count x).  We observe that ...`
from opening a block. And nothing highlights inside prose, including terms
mentioned inline. Under-highlighting is the right way to be wrong here.

`acl2-session.highlightOutput` turns it off; the log is then plain text.
The form view and console still colour *terms* the daemon sends as structure,
which is exact rather than best-effort, and is unaffected by this setting.

## Certifying

**ACL2: Certify Book** (no key) runs `cert.pl` on the book *outside* the
session, so the world it has built is left untouched. The status bar shows a
running clock and becomes an interrupt button while it works. Because
certification chases dependencies, a failure names *which* book failed.
`acl2-session.certifyJobs` sets the `cert.pl -j` parallelism (default 4).

## Installation

```sh
cd editors/vscode
npm install
npm run compile
```

Install the daemon first (`cargo install --path crates/acl2-session-editor`),
or point `acl2-session.daemonPath` at a built binary.

Then package and install it:

```sh
npx @vscode/vsce package
code --install-extension acl2-session-*.vsix
```

`code` installs into whatever VSCode variant is on your `PATH` — stock,
Insiders, or VSCodium — and the glob takes the version `vsce` wrote into the
`.vsix` name, so nothing is pinned to a version or a fixed extensions path.
Repackage and reinstall to pick up later changes.

## Commands

**Starting is one action**: `ctrl+alt+a` (`cmd+alt+a` on a Mac), or the
&#9654; button in the editor's title bar, which becomes a stop button once
the session is live. Both run **ACL2: Start Live Checking**, which opens the
form view too — there is no moment where you want the session but not the place
its failures explain themselves. The form view opens without taking the cursor.
Clicking the status bar brings it back if you close it.

| key | command |
| --- | --- |
| `ctrl+alt+a` | Start Live Checking (and open the form view) |
| `ctrl+alt+i` | Interrupt |
| `ctrl+alt+n` | Target at Cursor — check up to here |
| `ctrl+alt+u` | Retract to Cursor |
| `ctrl+alt+b` | Clear Target — check the whole book |
| `ctrl+alt+p` | Show Form view |
| `ctrl+alt+c` | Show Console |
| `ctrl+alt+e` | Evaluate Selection |
| `ctrl+alt+o` | Show Output for Form at Cursor |
| `ctrl+alt+g` | Go to Next Attention Point — the reds, then the frontier |
| `ctrl+alt+r` | Go to Frontier |

All are scoped to `editorLangId == acl2`, so they are inert everywhere else.
Three commands have no key and live in the command palette: **Stop Live
Checking** (the title-bar button covers it), **Certify Book**, and **Show
Documentation**.