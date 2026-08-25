// Indentation, measured against books that were indented by Emacs.
//
// "Better auto-indentation" is a matter of taste right up until it is
// checked against the corpus, and then it is a number. Every book in the
// community checkout was indented by the very `lisp-indent-function` model
// `src/indent.ts` transcribes, so reindenting one and counting the lines
// that come back unchanged says exactly how faithful the transcription is.
// A rule proposal that does not move this number is not worth arguing about.
//
// Three things are checked, in order of how much they matter:
//
//   agreement    lines whose indentation we reproduce exactly
//   idempotence  reindent(reindent(x)) === reindent(x)
//   opacity      no line inside a string or block comment is touched
//
// The last two are invariants and must be perfect. Agreement is a score.
//
//   npm run compile   (or tsc -p .)
//   node indent.mjs [file-or-directory ...]
//
// With no arguments it reindents two directories of the community books,
// found under `ACL2_BOOKS`.

import { readFileSync, readdirSync, statSync } from 'fs';
import { join } from 'path';
import { createRequire } from 'module';

import { freshOrDie } from './fresh.mjs';

// Before the first require: this harness tests the compiled module, so a
// stale out/ makes it report on code that is not the code.
freshOrDie();

const require = createRequire(import.meta.url);
const { reindent, reindentRange, topLevelStarts, insideOpaque, plannedIndent, enclosingStart } =
  require('./out/indent.js');

// `ACL2_BOOKS`, the same spelling `source-grammar.mjs` reads, then a
// placeholder as the last resort. Without the env var a fresh clone
// degraded silently to the unit cases with no hint that a corpus was even
// wanted, and the score this harness exists to produce simply did not appear.
const BOOKS = process.env.ACL2_BOOKS ?? '/path/to/acl2/books';
const DEFAULT = [join(BOOKS, 'kestrel/c/syntax'), join(BOOKS, 'kestrel/fty')];

function lispFiles(p) {
  const s = statSync(p, { throwIfNoEntry: false });
  if (!s) return [];
  if (s.isFile()) return p.endsWith('.lisp') ? [p] : [];
  return readdirSync(p)
    .filter((f) => f.endsWith('.lisp'))
    .map((f) => join(p, f));
}

// The cases worth pinning by hand, which run wherever this does. The corpus
// is a score and needs a checkout; these are the shape of the rules and need
// nothing.
let unitBad = 0;
function ind(what, source, want) {
  const got = reindent(source.replace(/^\n/, ''));
  const ok = got === want.replace(/^\n/, '');
  if (!ok) {
    unitBad++;
    console.log(`FAIL: ${what}\n--- got ---\n${got}\n--- want ---\n${want.replace(/^\n/, '')}`);
  } else {
    console.log(`ok: ${what}`);
  }
}

ind('a definition body sits two columns in', `
(defun f (x)
(+ x 1))
`, `
(defun f (x)
  (+ x 1))
`);

ind('an unknown def- macro is a definition too', `
(defprod point
((x natp)
(y natp)))
`, `
(defprod point
  ((x natp)
   (y natp)))
`);

ind('a call aligns under its first argument', `
(append (foo x)
(bar y))
`, `
(append (foo x)
        (bar y))
`);

ind('cond clauses line up under the first clause', `
(cond ((consp x) 1)
((null x) 2)
(t 3))
`, `
(cond ((consp x) 1)
      ((null x) 2)
      (t 3))
`);

ind('a call whose head is alone goes one column in', `
(must-fail
(defthm bad nil))
`, `
(must-fail
 (defthm bad nil))
`);

ind('if distinguishes its test and then from its else', `
(if (consp x)
(car x)
nil)
`, `
(if (consp x)
    (car x)
  nil)
`);

ind('a paren inside a string is not a paren', `
(defun f (x)
"a ( that never closes"
x)
`, `
(defun f (x)
  "a ( that never closes"
  x)
`);

ind('a line inside a string is left exactly alone', `
(defxdoc top
:long "first
   second
 third")
`, `
(defxdoc top
  :long "first
   second
 third")
`);

ind('a lone semicolon comment keeps the column it was typed in', `
(defun f (x)
; deliberately at the left margin
x)
`, `
(defun f (x)
; deliberately at the left margin
  x)
`);

ind('a double semicolon comment takes the code indent', `
(defun f (x)
;; explains the body
x)
`, `
(defun f (x)
  ;; explains the body
  x)
`);

ind('a char literal paren is not a paren either', `
(defun f (x)
(eql x #\\()
)
`, `
(defun f (x)
  (eql x #\\()
  )
`);

ind('an unclosed form still indents what follows it', `
(defun f (x
(g x)
`, `
(defun f (x
          (g x)
`);

// What the editor's providers ask, checked here rather than through a
// stubbed editor: `formatting.ts` holds no decision of its own, so these are
// the decisions it makes.
function plan(what, source, line, want, opts) {
  const text = source.replace(/^\n/, '');
  const starts = topLevelStarts(text);
  let off = 0;
  for (let i = 0; i < line; i++) off = text.indexOf('\n', off) + 1;
  const got = plannedIndent(text, off, starts, opts);
  const ok = got === want;
  if (!ok) unitBad++;
  console.log(`${ok ? 'ok' : 'FAIL'}: ${what}${ok ? '' : ` (got ${got}, want ${want})`}`);
}

const BOOK = `(in-package "ACL2")

(defun f (x)
  ;; a comment
; left margin on purpose
  (+ x 1))

(defxdoc top
  :long "one
   two")
`;

plan('a body line is planned at the body indent', BOOK, 5, 2);
plan('a double-semicolon comment is planned like code', BOOK, 3, 2);
plan('a lone-semicolon comment is not planned at all', BOOK, 4, undefined);
plan('a blank line is not planned', BOOK, 6, undefined);
plan('a line inside a string is not planned', BOOK, 9, undefined);
plan('a top-level form is planned at column zero', BOOK, 2, 0);

// Pressing Enter. This is what the buffer holds at the instant the on-type
// provider is asked: `editor.autoIndent: keep` has copied the previous
// line's whitespace onto the new line, and there is nothing else on it. The
// whole-buffer pass wants such a line emptied; this caller wants it
// indented, and getting that backwards makes Enter do nothing whatsoever.
const AFTER_ENTER = `(defun f (x)
  (+ x 1)
  `;
plan('Enter inside a form lands the cursor at the body indent',
     AFTER_ENTER, 2, 2, { blankLines: 'indent' });
plan('and the same line is left alone by a whole-buffer pass',
     AFTER_ENTER, 2, undefined);
plan('Enter after a finished form lands at column zero',
     `(defun f (x) 1)\n`, 1, 0, { blankLines: 'indent' });

// The reader's own escapes. A construct the lexer fails to recognise does not
// cost one token, it costs the rest of the form: the open-stack desynchronises
// and every line below is indented against the wrong depth. The grammar and the
// README both advertise fancy strings, and the indenter did not know them.
ind('a fancy string hides its quotes and parens', `
(defun f (x)
#{"""a ) " ( lone quote and parens"""}
x)
`, `
(defun f (x)
  #{"""a ) " ( lone quote and parens"""}
  x)
`);

// The point of the syntax is spanning lines, so the state has to cross them --
// and a line inside one is content, exactly like a line inside a string.
ind('a multi-line fancy string is left exactly as written', `
(defun f (x)
#{"""
   deliberately
     aligned ) prose
"""}
x)
`, `
(defun f (x)
  #{"""
   deliberately
     aligned ) prose
"""}
  x)
`);

ind('and the form after one is indented as if it were not there', `
(defun f (x)
#{"""((("""}
(g x))
`, `
(defun f (x)
  #{"""((("""}
  (g x))
`);

// `|a\|b|` is one symbol whose name holds a bar. Read as a plain search for
// the next `|`, it was two symbols with a stray `b` between them.
// `|a\\|b ( |` is one symbol whose name holds a bar, a space and an open paren.
// Read as a plain search for the next `|`, the escaped one ended the symbol,
// the `(` after it was counted as a real open paren, and the second `|` opened
// a symbol that ran to end of line -- swallowing the `)` that would have closed
// it. Everything below then indented against a stack two too deep.
ind('a backslash escapes a bar inside a bar symbol', `
(defun f (x)
(|a\\|b ( |)
(g x))
`, `
(defun f (x)
  (|a\\|b ( |)
  (g x))
`);

// The whole-buffer and single-line paths must answer the same, since a reader
// meets one through Tab and the other through Format Document.
plan('a line inside a fancy string is not planned', `
(defxdoc top
  :long #{"""
one
  two"""})
`, 3, undefined);

plan('and the line after one is planned normally', `
(defxdoc top
  :long #{"""x"""}
:more nil)
`, 2, 2);

// Formatting part of a buffer. The rule is `reindent`'s -- top down, against
// the running result -- and the reason it has to be is that a line's target
// depends on the columns of the lines above it, which this call is in the
// middle of changing. Planning each line against the text as found is the
// bug these pin: it plans a child at the column its parent has *now*, then
// moves the parent out from under it.
function ranged(what, source, first, last, want, opts) {
  const text = source.replace(/^\n/, '');
  const got = reindentRange(text, topLevelStarts(text), first, last, opts);
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) unitBad++;
  console.log(
    `${ok ? 'ok' : 'FAIL'}: ${what}` +
      (ok ? '' : `\n--- got ---\n${JSON.stringify(got)}\n--- want ---\n${JSON.stringify(want)}`)
  );
}

// The demonstration case. `(+ x` moves from column 0 to column 2, so `1`
// belongs under `x` at column 5 -- and the per-line plan said 3, because it
// measured against the `(+` that was about to move.
ranged('a child follows its parent to where the parent is going', `
  (defun f (x)
(+ x
1))
`, 0, 2, ['(defun f (x)', '  (+ x', '     1))']);

// The invariant, and the strongest thing here: over the whole buffer this
// must be `reindent`, line for line. If the two ever disagree, Format
// Document and Format Selection have started formatting differently.
{
  const text = `(in-package "ACL2")

(defun f (x)
(let ((y (g x
(h x))))
(+ y
1)))
`;
  const want = reindent(text).split('\n');
  const got = reindentRange(text, topLevelStarts(text), 0, want.length - 1);
  // Blank lines are the one deliberate difference: a whole-buffer pass clears
  // their trailing whitespace, a range pass leaves them alone (`blankLines`).
  const ok = got.every((l, i) => l === undefined || l === want[i]);
  if (!ok) unitBad++;
  console.log(`${ok ? 'ok' : 'FAIL'}: over the whole buffer it agrees with reindent line for line`);
}

// Lines above the range are not the call's to change, and are what the range
// is measured against: selecting only the body of a wrongly-indented form
// must align it under the form as it actually sits, not as it ought to.
ranged('a selection measures against the lines above it, unfixed', `
    (defun f (x)
(+ x 1))
`, 1, 1, ['      (+ x 1))']);

// The three answers that are not ours, exactly as `plannedIndent` gives them.
ranged('a lone-semicolon comment, a blank line and a string line are left alone', `
(defxdoc top
; left margin on purpose

  :long "one
two")
`, 1, 5, [undefined, undefined, '  :long "one', undefined, undefined]);

{
  const starts = [0, 100, 250];
  const cases = [[0, 0], [99, 0], [100, 100], [249, 100], [250, 250], [9999, 250]];
  let ok = true;
  for (const [at, want] of cases) if (enclosingStart(starts, at) !== want) ok = false;
  if (!ok) unitBad++;
  console.log(`${ok ? 'ok' : 'FAIL'}: the enclosing form is found on either side of every boundary`);
}

const targets = (process.argv.slice(2).length ? process.argv.slice(2) : DEFAULT).flatMap(lispFiles);
if (!targets.length) {
  console.log(`\nno corpus at ${BOOKS} -- unit cases only (pass a path or set ACL2_BOOKS)`);
  process.exit(unitBad ? 1 : 0);
}

const leading = (l) => l.length - l.trimStart().length;

let lines = 0;
let agree = 0;
let bad = unitBad;
const perFile = [];
const disagreements = new Map(); // "head-ish context" -> count

for (const path of targets) {
  const text = readFileSync(path, 'latin1');
  const t0 = Date.now();
  let out;
  try {
    out = reindent(text);
  } catch (e) {
    console.log(`ERROR ${path}: ${e.message}`);
    bad++;
    continue;
  }
  const ms = Date.now() - t0;

  const a = text.split('\n');
  const b = out.split('\n');
  if (a.length !== b.length) {
    console.log(`ERROR ${path}: line count changed ${a.length} -> ${b.length}`);
    bad++;
    continue;
  }
  let fileLines = 0;
  let fileAgree = 0;
  const starts = topLevelStarts(text);
  let offset = 0;
  for (let i = 0; i < a.length; i++) {
    const orig = a[i];
    const got = b[i];
    const lineStart = offset;
    offset += orig.length + 1;
    if (!orig.trim()) continue;
    // A line inside a string or block comment is not ours to score; it is
    // ours to leave alone, which is the opacity check below.
    let from = 0;
    for (let k = starts.length - 1; k >= 0; k--) {
      if (starts[k] <= lineStart) { from = starts[k]; break; }
    }
    if (insideOpaque(text, from, lineStart)) {
      if (orig !== got) {
        console.log(`OPACITY ${path}:${i + 1} moved a line inside a string/comment`);
        bad++;
      }
      continue;
    }
    fileLines++;
    if (leading(orig) === leading(got)) {
      fileAgree++;
    } else {
      const key = orig.trim().split(/\s+/)[0].slice(0, 24);
      disagreements.set(key, (disagreements.get(key) || 0) + 1);
    }
  }

  // Format Selection and Format Document must be the same formatter. Over the
  // whole buffer the range pass is `reindent` by construction -- but by
  // construction is what the range path used to be too, and it planned every
  // line against columns it was about to change. Held here against 271 real
  // books rather than the unit case alone.
  const ranged = reindentRange(text, starts, 0, a.length - 1);
  for (let i = 0; i < ranged.length; i++) {
    // Blank lines are the one deliberate difference: a whole-buffer pass
    // clears their trailing whitespace, a range pass leaves them alone.
    if (ranged[i] !== undefined && ranged[i] !== b[i]) {
      console.log(`RANGE ${path}:${i + 1} range pass differs from reindent`);
      bad++;
      break;
    }
  }

  // Idempotence: a second pass must be a no-op, or the providers would fight
  // the reader every time they ran.
  const twice = reindent(out);
  if (twice !== out) {
    let where = 0;
    const x = out.split('\n'), y = twice.split('\n');
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) { where = i + 1; break; }
    console.log(`IDEMPOTENCE ${path}: differs at line ${where}`);
    bad++;
  }

  lines += fileLines;
  agree += fileAgree;
  perFile.push([fileAgree / (fileLines || 1), fileLines, ms, path]);
}

perFile.sort((p, q) => p[0] - q[0]);
console.log('\nworst files:');
for (const [rate, n, ms, path] of perFile.slice(0, 10)) {
  console.log(`  ${(rate * 100).toFixed(1)}%  ${String(n).padStart(6)} lines  ${String(ms).padStart(5)}ms  ${path.replace(BOOKS + '/', '')}`);
}

const top = [...disagreements].sort((p, q) => q[1] - p[1]).slice(0, 15);
console.log('\nmost disagreed-with line heads:');
for (const [head, n] of top) console.log(`  ${String(n).padStart(6)}  ${head}`);

const slow = perFile.reduce((m, p) => Math.max(m, p[2]), 0);
console.log(`\n${targets.length} files, ${lines} scored lines`);
console.log(`agreement ${((agree / (lines || 1)) * 100).toFixed(2)}%   slowest file ${slow}ms`);
if (bad) {
  console.error(`indent: ${bad} invariant failures`);
  process.exit(1);
}
