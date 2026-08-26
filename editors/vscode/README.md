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

A held target is drawn in every mode, and identically in all three: a small blue
arrow in the text, immediately after the last form that will be checked,
pointing back at it. Everything up to the arrow is in.

In the text rather than in the gutter, and that is not an aesthetic choice. A
gutter cell addresses a *line*, so it cannot say "after this form" on a line
holding two — and a cell holds one icon, so a mark there would have to take it
from the status bar that wants it. A column is what the position actually is,
so the mark goes where the position is. The scrollbar gets a tick as well, at
the only resolution a scrollbar has: two forms on a line share one, which makes
it a "look around here" aid rather than a statement about which of them.

With the target held above everything the arrow leads the file, since there is
no included form for it to follow.

ACL2's own warnings — `Double-rewrite`, `Use`, `Subsume` — are reported in the
**Problems** view too, one entry per kind, on the form that emitted them, as
`ACL2 Warning: double-rewrite`.
`acl2-session.formWarnings` turns them off. Two things worth knowing before
leaving them on: the kind is all there is, because ACL2 records the *set* a
form emitted rather than the individual warnings, so five double-rewrite
warnings look like one and what any of them actually says is in that form's
output (`ctrl+alt+o`); and how many forms carry one varies enormously by book —
1% of forms in `arithmetic-light/mod.lisp`, 40% in `std/lists/sets.lisp`.

Every diagnostic is one short line, because a Problems row is an index entry:
it names what is wrong, and the form view explains it. Where ACL2 has said
something itself, its own error line is used verbatim.

Failed and skipped forms are reported in the **Problems** view in every mode,
which is also where the squiggle comes from — VSCode gives no way to have the
entry without the mark. `acl2-session.problems` turns both off. In `minimal`
that leaves a failure marked by nothing at all, scrollbar included, since in
that mode this *is* the mark.

### Colours

The status colours follow the active theme. Each status has a meaning in
VSCode's own vocabulary — `editorError.foreground` for a failure,
`editorWarning.foreground` for a conditional one, `editorInfo.foreground` for
the held target — and the extension reads those out of the theme,
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

### Pinning

**Pin to This Form** holds the views on one form so it can be watched while the
edit meant to fix it happens somewhere else — hold the failure, change the
lemma above it, watch the checkpoints move. The button is in the title bar of
the form view *and* of the ACL2 Output document, since either may be the one
you are looking at, and **ACL2: Toggle Pin** is in the palette.

It pins **both** views, always together. They are two views of one form — the
structured summary and the raw log — and pinned to different forms they would
show one form's checkpoints beside another form's output.

The pin is conservative: it is anchored on the form's first character and
carried across your edits, and it lets go rather than guess whenever it cannot
be sure the form is still there. Editing the pinned form is the ordinary case
and holds fine; deleting it, or rewriting its head, releases the pin and says
so.

`ctrl+alt+o` **Show Output** re-points the pin at the form under the cursor
rather than refusing, the same way **Target at Cursor** moves the barrier
instead of declining while one is held.

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

A word is the whole symbol. `alpha-of-nil`, `fty::defprod`, `*earmuffs*` and
`1+` are each one word, so a double-click takes the symbol, hover reads the
symbol, and a word motion crosses it in one step rather than stopping at every
hyphen. Parens, brackets, quotes, the comma, the semicolon and the bar are
what separate — which keeps a word motion meaningfully finer than a
whitespace one in code this full of parens.

Vim's `w`, `e` and `b` are the exception, and not by choice: VSCodeVim reads
`editor.wordSeparators` without a document to scope it to, so a per-language
value never reaches it, and its own `vim.iskeyword` is window-scoped. Either
way it is global, so it belongs in your settings rather than in this
extension. `"vim.iskeyword": "()[]{}'\"`,;|"` matches what ACL2 files do
here, at the price of applying everywhere.

VSCode's inline colour swatches are turned off for these files. Its built-in
colour parser runs over any language that has none of its own, and Lisp's
reader syntax collides with CSS's: `#001#` is a label reference, `#d1e-2` is a
decimal float, and `#001` and `#d1e` are perfectly good hex colours. Reader
tests aside, a swatch on a radix prefix is never right. `editor.colorDecorators`
in your own settings overrides this, since a user setting beats an extension's
default.

The reader's escapes are handled, which matters more than it sounds: a
grammar carries its state from line to line, so a construct it fails to
recognise costs the rest of the file rather than one token. A bar-quoted
symbol may span lines and hold a quote (`'|a"b|`), a fancy string is
delimited by `#{"""` and `"""}` so every quote inside it is content, and a
backslash escapes the next character outside a string as well as in.
`source-grammar.mjs` holds each of those against the file that motivated
them, `books/kestrel/acl2pl/reader-tests.lisp`.

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

## Indentation

Indentation follows Emacs' `lisp-indent-function` model, with the tables from
`emacs-acl2.el`, `acl2-indent.el` and `lisp-mode` transcribed rather than
invented — and then checked, because "better indentation" is a matter of taste
right up until it is a number. `indent.mjs` reindents the community books and
counts the lines that come back unchanged: 95.7% over 271 files. A macro whose
body sits at the wrong column is a table entry away, and the measurement says
whether the entry helped.

A book's own definers outnumber any table, so an unknown head beginning `def`
is indented like a definition. Anything else that is not in the table is a
plain call, with its arguments under the first one.

Three ways in, all the same rules:

- **As you type.** Enter indents the new line; a closing delimiter re-indents
  the line it finished. This is `editor.formatOnType`, which the extension
  turns on for `acl2` files only.
- **On request.** Format Document and Format Selection. Both change *leading
  whitespace and nothing else* — no re-wrapping, no reflowing, no normalising
  of anything you aligned by hand. Format-on-paste is on for the same reason:
  it can only move a pasted form to the right column. Lines inside a string or
  a block comment are never touched, and neither is a line whose `;` comment
  column you chose yourself.
- **Tab**, as in Emacs, re-indenting the current line or the selected lines —
  including an empty one, which is where it is wanted most. On, and
  `acl2-session.tabIndents` turns it off if you would rather Tab inserted. It
  is also the repair for a line that arrived by command rather than by typing,
  which as-you-type formatting is never asked about — Vim's `o` opens one.
  **ACL2: Indent Line or Selection** is in the command palette either way.

Each of the three switches off on its own, and they are VSCode's own settings
rather than ones invented here, so they behave the way you already expect a
per-language setting to behave:

```json
"[acl2]": {
  "editor.formatOnType": false,   // stop indenting as you type
  "editor.formatOnPaste": false   // stop indenting what you paste
},
"acl2-session.tabIndents": false  // give Tab back to VSCode
```

Format Document and Format Selection stay, because you have to ask for those.

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

The scopes it marks with are chosen for what a *theme* will do with them,
which is not the same as what they mean to us. Errors are `markup.deleted`
and warnings `markup.changed` rather than the `invalid.*` family: nothing in
a log is ill-formed source, and themes read `invalid` literally — a
background, a strikethrough, or in one case a purple `deprecated`. The
summary labels are `support.type.property-name`, since that is what they are,
and `Summary` itself heads its block the way a goal name does. They used to
be `keyword.control`, which is the loudest colour most themes have: thirteen
of them in one short log, shouting over an `ACL2 Error` painted a quieter
red.

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

The extension is a client of the `acl2-session-editor` daemon, so that comes
first — from the **repository root**, not from this directory:

```sh
cargo install --path crates/acl2-session-editor
```

Or point `acl2-session.daemonPath` at a built binary instead of installing it.

Then build and install the extension itself:

```sh
cd editors/vscode
npm install
npm run compile
npx @vscode/vsce package
code --install-extension acl2-session-*.vsix
```

`code` installs into whatever VSCode variant is on your `PATH` — stock,
Insiders, or VSCodium — and the glob takes the version `vsce` wrote into the
`.vsix` name, so nothing is pinned to a version or a fixed extensions path.
Repackage and reinstall to pick up later changes.

## Commands

**Starting is one action**: `ctrl+alt+a` (`cmd+alt+a` on a Mac), or the
&#9654; button in the editor's title bar. Both run **ACL2: Start Live
Checking**, which opens the form view too — there is no moment where you want
the session but not the place its failures explain themselves. The form view
opens without taking the cursor. Clicking the status bar brings it back if you
close it — that item is an icon and a count, `$(check) ACL2 40/40`, with the
rest of what it knows in its tooltip.

That button is the editor's **only** title-bar action, which is what VSCode's
own guidance asks for, so it carries all three states: a green &#9654; to
start, then a blue &#10074;&#10074; to pause, then a blue &#9654;&#10074; to
resume &mdash; start and resume are different icons because they are different
things, one beginning a session and one carrying on from where it stopped.
`ctrl+alt+a` follows it — on a paused session it resumes,
because from your side that is the same gesture. **Stop** and **Certify Book** are in the `…` overflow beside it, and
in the command palette.

The four session controls all use VSCode's debug-family icons, which is a
colour decision as well as a shape one: VSCode colours those codicons globally
by name, so start is green, pause and resume are blue, and stop is red without
this extension choosing anything. The sequence reads green to begin, blue while
you are steering it, red to end it &mdash; and blue is what this extension
already uses for *the reader's own instruction* rather than a verdict, which is
exactly what pausing is.

### Pausing

A paused session stops admitting forms, and stops doing nothing else. It still
retracts as you edit — so the gutter never claims more than the text supports —
still answers hover, completion and go-to-definition, and still evaluates forms
in the console. Only the proving waits.

That is the difference from **Interrupt** (`ctrl+alt+i`), which aborts the form
in flight and then carries on at your next keystroke. Interrupt means "stop,
I'm about to fix that form". Pause means "leave me alone for a while", and
survives every edit until you press play.

Unchecked forms stay *pending* while paused, and are not reported as *beyond*.
A pause is a moment, not a barrier: those forms will be reached the instant you
press play, which is what pending says. *Beyond* is for what a **target**
excludes, which stays excluded until you move the target.

| key | command |
| --- | --- |
| `ctrl+alt+a` | Start Live Checking (and open the form view) |
| `ctrl+alt+i` | Interrupt |
| `ctrl+alt+n` | Target at Cursor — check up to here |
| `ctrl+alt+]` | Target Forward One Form |
| `ctrl+alt+[` | Target Back One Form |
| `ctrl+alt+u` | Retract to Cursor |
| `ctrl+alt+b` | Clear Target — check the whole book |
| `ctrl+alt+p` | Show Form view |
| `ctrl+alt+c` | Show Console |
| `ctrl+alt+e` | Evaluate Selection |
| `ctrl+alt+o` | Show Output — and re-point the pin here |
| `ctrl+alt+g` | Go to Next Attention Point — the reds, then the frontier |
| `ctrl+alt+r` | Go to Frontier |

All are scoped to `editorLangId == acl2`, so they are inert everywhere else.
Four commands have no key and live in the command palette: **Stop Live
Checking** (the title-bar button covers it), **Target Before First Form**,
**Certify Book**, and **Show Documentation**.

## The target

The target is a barrier: check as far as this form and no further. Setting one
is how you stop a book racing ahead of you, and how you walk it a form at a
time.

It names a **form**, not a line. The distinction shows up the moment you edit:
a line number is invalidated by every insertion above it, where a form is still
the same form, so the barrier is anchored on the target form's first character
and carried across edits from there. Anchored on its *first* character
specifically, because editing the target form is the ordinary thing to be
doing and its end moves on every keystroke.

When an edit destroys that anchor outright the barrier **rounds up**, to the
nearest form above that the same edit left alone, and to the top of the file
when there is none. Never down. Checking less than you asked for is a keypress
from being fixed; checking more has already run.

Four things follow from being a barrier rather than a bookmark:

- **Moving it back retracts.** The target is the whole statement about how far
  the world should extend, so pulling it back above the frontier rolls the
  world back to meet it, with no edit required.
- **The step commands take hold of a running book.** With no target held there
  is no barrier to step from, so the first step plants one at the frontier —
  which stops the run where it has actually got to, and moves from there.
- **Any of them will start the session**, which is how you start one that is
  already held somewhere. **Start Live Checking** itself is unchanged and holds
  nothing: it checks the whole book, as it always has. But `ctrl+alt+n` on a
  book with no session starts one held at the cursor's form, and **Target
  Before First Form** starts one that has checked nothing at all — "check as
  far as here" being most useful said *before* anything has been checked. Such
  a session comes up held above the first form and places the real barrier
  once the book has been split, so nothing is admitted and then undone in
  between.
- **Forms past it say so.** They are `beyond`, not queued: nothing will reach
  them until the barrier moves, and **Go to Next Attention Point** and **Go to
  Frontier** both decline to send you to one.