// The webviews' pure logic, tested outside a webview.
//
// Both pages are strings assembled at runtime, so nothing in them is
// reachable from tsc, from smoke.mjs, or from any test that stops at the
// module boundary. Two failure modes hide in there:
//
//   - a syntax error anywhere in either script, which surfaces only as a
//     window that silently does nothing; and
//   - `complete`, which decides whether Enter submits a form or inserts a
//     newline. That one is not cosmetic: an unbalanced form sent to the
//     session swallows the marker that ends it, and ACL2 then waits for the
//     rest of the input until something interrupts it.
//
// Neither a daemon nor an ACL2 is needed here.
//
//   npm run compile   (or tsc -p .)
//   node webview.mjs

import { createRequire } from 'module';

const require = createRequire(import.meta.url);

// Both modules import `vscode`, which exists only inside the editor. Nothing
// they do at *load* time touches it, so an empty stub is enough to get at
// the page builders.
const { Module } = require('module');
const resolve = Module._resolveFilename;
Module._resolveFilename = (request, ...rest) =>
  request === 'vscode' ? 'vscode' : resolve.call(Module, request, ...rest);
require.cache.vscode = { id: 'vscode', filename: 'vscode', loaded: true, exports: {} };

let bad = 0;
function check(ok, what) {
  if (ok) {
    console.log(`ok: ${what}`);
  } else {
    bad++;
    console.error(`FAIL: ${what}`);
  }
}

const pages = [
  ['the console page', require('./out/console.js').html('demo.lisp')],
  ['the form view page', require('./out/formView.js').HTML],
  ['the docs page', require('./out/docView.js').HTML],
];

/** The script a page will actually run. */
function scriptOf(page) {
  const m = /<script>([\s\S]*)<\/script>/.exec(page);
  return m ? m[1] : undefined;
}

// `new Function` parses without running, so the browser globals these
// scripts reach for are never touched.
for (const [name, page] of pages) {
  const body = scriptOf(page);
  if (!body) {
    check(false, `${name} has a script`);
    continue;
  }
  try {
    new Function(body);
    check(true, `${name}'s script parses`);
  } catch (e) {
    check(false, `${name}'s script parses: ${e.message}`);
  }
}

// Every page runs with scripts enabled, and the docs page renders markup
// derived from a book. A page that ships without a policy loses the layer
// that stands behind the escaping, and nothing else here would notice --
// the docs page went without one for as long as it existed.
for (const [name, page] of pages) {
  check(/http-equiv="Content-Security-Policy"/.test(page)
        && /default-src 'none'/.test(page),
        `${name} declares a content security policy`);
}

const consoleScript = scriptOf(pages[0][1]);

/**
 * Compile named functions out of a page's script.
 *
 * Lifted from the *assembled* page rather than from the module text on disk,
 * which still carries the template literal's own level of escaping -- read
 * that instead and every character class in the function under test is
 * quietly wrong. Names are given in dependency order; the last is returned.
 */
function lift(...names) {
  const parts = [];
  for (const name of names) {
    // Declarations end at a closing brace in column zero; arrow constants at
    // the first `;` that ends a line. Both spellings are in use.
    const m =
      new RegExp(`function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`).exec(consoleScript) ??
      new RegExp(`const ${name} = [\\s\\S]*?;$`, 'm').exec(consoleScript);
    if (!m) {
      console.error(`FAIL: ${name} is not in the console script`);
      process.exit(2);
    }
    parts.push(m[0]);
  }
  return new Function(`${parts.join('\n')}; return ${names[names.length - 1]};`)();
}

// ---- `complete`: does Enter send this, or give the user another line? ----

const complete = lift('complete');

const cases = [
  ["(pe 'alpha)", true, 'a closed form'],
  ["(pe 'alpha", false, 'an unclosed form'],
  ['', true, 'nothing (the caller checks for blank separately)'],
  ['(thm (equal (append x y)\n            (append y x)))', true, 'a form over several lines'],
  ['(thm (equal (append x y)', false, 'the same form, half typed'],
  ['(f "a (b")', true, 'a paren inside a string'],
  ['(f "a (b"', false, 'and the same, still open'],
  ['(f "a\\")")', true, 'an escaped quote inside a string'],
  ['(f #\\()', true, 'a paren as a character literal'],
  ['(f #\\( ', false, 'and the same, still open'],
  ['(f |a(b|)', true, 'a paren inside a bar-quoted symbol'],
  ['(f |a(b|', false, 'and the same, still open'],
  ['(f) ; (a comment', true, 'a paren in a trailing line comment'],
  ['(f ; a comment\n)', true, 'a line comment inside a form'],
  ['(f ; (\n', false, 'a line comment hiding nothing that closes'],
  ['#| ( |# (g)', true, 'a paren inside a block comment'],
  ['#| ( |# (g', false, 'and the same, still open'],
  ['#| #| ( |# |# (g)', true, 'nested block comments'],
  ['(f))', true, 'a stray close paren -- a typo for ACL2 to report, not an unfinished form'],
  [':pe alpha', true, 'a keyword command with no parens at all'],
];

for (const [src, want, what] of cases) {
  const got = complete(src);
  check(got === want, `${what}${got === want ? '' : `: got ${got}, want ${want}`}`);
}

// ---- `spans`: tokens to colored HTML ----
//
// The input overlay must line up character-for-character with the textarea
// beneath it, so what this emits has to contain the source text exactly --
// no dropped characters, and every one of `& < > "` escaped.

const spans = lift('esc', 'spans');

const line = [
  { t: '(defun ', c: '#569cd6' },
  { t: 'f', c: '#dcdcaa', i: 1 },
  { t: ' <a & "b">', c: '' },
];
const out = spans([line]);
check(out.includes('color:#569cd6'), 'spans carries a token color through');
check(out.includes('font-style:italic'), 'and italics');
check(!out.includes('color:;'), 'and emits no color for a token that has none');
check(out.includes('&lt;a &amp; &quot;b&quot;&gt;'), 'and escapes the markup characters');
check(
  out.replace(/<span[^>]*>|<\/span>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"').replace(/&amp;/g, '&') === '(defun f <a & "b">',
  'and reproduces the source text exactly, so the overlay stays in register'
);
check(spans([[{ t: 'a', c: '' }], [{ t: 'b', c: '' }]]) === 'a\nb', 'and joins lines with newlines');

// ---- `envelope`: what a form left behind, per event ----
//
// The scrape hangs off the events under "inside this form", which is the
// only place a macro-generated name has a row of its own. Rendered from the
// shape the daemon actually sends: a guard-verified function with derived
// runes, and a theorem whose left-hand side is nowhere in the buffer.

globalThis.toks = {};
const envelope = lift('esc', 'strip', 'causes', 'spans', 'hl', 'codes', 'rat',
                      'fmtSecs', 'timeParts', 'proveSecs', 'guardNote', 'nameDetail',
                      'envelope');
{
  const fn = {
    name: 'dfn-app', symbolClass: ':common-lisp-compliant', formals: '(x y)',
    guard: '(if (true-listp x) (true-listp y) (quote nil))',
    measure: '(acl2-count x)', relation: 'l<',
    runes: [
      { rune: '(:definition dfn-app)', fields: [{ key: ':clique', value: '(dfn-app)' }] },
      { rune: '(:type-prescription dfn-app)', enabled: false, fields: [] },
    ],
  };
  const thm = {
    name: 'true-listp-of-dfn-app', statement: '(true-listp (dfn-app x y))',
    runes: [{ rune: '(:rewrite true-listp-of-dfn-app)',
              fields: [{ key: ':lhs', value: '(true-listp (dfn-app x y))' }] }],
  };
  const h = envelope({
    checkpoints: [], rules: [], inner: [
      { name: 'dfn-app', kind: 'defun', detail: fn },
      { name: 'true-listp-of-dfn-app', kind: 'defthm', proverSteps: 12,
        rules: ['(:definition dfn-app)'], detail: thm },
      { name: 'undetailed' },
    ],
  }, 'g-', 'goal');

  check(h.includes('<code>dfn-app</code> <code class="lisp">(x y)</code>'),
        'a function shows its formals beside its name, as a signature');
  check(h.includes('guards verified · 2 runes'),
        'and says in the fold summary that its guards were verified');
  check(h.includes('guards not verified') === false,
        'and does not claim otherwise');
  check(/guard<\/td><td><code[^>]*>\(if \(true-listp x\)/.test(h),
        'the guard is a labelled row');
  check(h.includes('(:type-prescription dfn-app)') && h.includes('>disabled<'),
        'a rune that exists but is off says so');
  check(h.includes('clique</td>'), 'and a rune field keeps ACL2’s own key');
  check(/measure<\/td><td><code[^>]*>\(acl2-count x\)/.test(h),
        'the measure is the term, not the justification record around it');
  check(/decreasing under<\/td><td><code[^>]*>l&lt;/.test(h),
        'and a relation that is not o< is named, since it was chosen');
  check(h.includes('1 rune · 1 rule used'),
        'a theorem counts what it introduced apart from what its proof used');
  check(h.includes('rules the proof used'), 'and the two are labelled apart');
  check(/lhs<\/td><td><code[^>]*>\(true-listp \(dfn-app x y\)\)/.test(h),
        'the left-hand side a rewrite rule will match is shown');
  check((h.match(/<summary class="meta">/g) || []).length === 2,
        'an event with nothing to show gets no fold');
  check(h.includes('detail (kind, time, steps, rules) is probed for the first 2'),
        'and is counted as unprobed, so a truncated probe still says so');
}
check(!pages[1][1].includes('Introduced ('),
      'the form view has no second, pooled copy of what the form introduced');

// The link range, which is where a coordinate bug would hide: the daemon
// sends no columns, so the client places them by finding the quoted name on
// the line. Non-ASCII before the include is the case that would break a
// byte-offset scheme and must not break this one.
const { linkRangeOnLine, hoverMarkdown, bookLabel } = require('./out/names.js');
{
  const line = '(include-book "std/util/bstar" :dir :system)';
  const r = linkRangeOnLine(line, 'std/util/bstar');
  check(r && line.slice(r[0], r[1]) === 'std/util/bstar',
        'the link range covers exactly the book name');
  check(line[r[0] - 1] === '"' && line[r[1]] === '"',
        'and excludes the quotes, so the underline is the path');
  const wide = '  ; \u00e9\u00e9\u00e9 note\n(local (include-book "a/b"))';
  const w = linkRangeOnLine(wide, 'a/b');
  check(w && wide.slice(w[0], w[1]) === 'a/b',
        'and stays right when the line holds non-ASCII');
  check(linkRangeOnLine('(include-book "other")', 'a/b') === undefined,
        'and declines when the name is not on the line');
}
{
  // A built-in must never render as local.
  const builtin = { available: true, found: true, name: 'symbolp',
                    kind: 'function', origin: 'builtin', formals: '(x)' };
  const md = hoverMarkdown(builtin);
  check(!/this file/i.test(md), 'a built-in is not "defined in this file"');
  check(/built into acl2/i.test(md), 'it says where it really comes from');
  const local = { available: true, found: true, name: 'alpha',
                  kind: 'function', origin: 'file', formals: '(x)' };
  check(/this file/i.test(hoverMarkdown(local)), 'while a local name still says so');
  const fromBook = { available: true, found: true, name: 'b*', kind: 'macro',
                     origin: 'book', book: '(:system . "std/util/bstar.lisp")' };
  check(hoverMarkdown(fromBook).includes('std/util/bstar.lisp'),
        'and a book name is shown the way it is written');
  check(bookLabel('(:system . "a/b.lisp")') === 'a/b.lisp', 'bookLabel unwraps :system');
  check(hoverMarkdown({ available: false, found: false, name: 'x' }) === undefined,
        'a declined hover renders nothing at all');
}

{
  // The completion prefix. It must agree with what the daemon will accept as
  // a symbol, or the client asks about something that can never be found.
  const { prefixAt } = require('./out/names.js');
  const line = '(defthm foo (equal (alph';
  check(prefixAt(line, line.length) === 'alph', 'the prefix is the token being typed');
  check(prefixAt('(foo bar', 4) === 'foo', 'and ends where the cursor is, mid-line');
  check(prefixAt('(foo bar', 1) === '', 'and is empty right after an open paren');
  check(prefixAt('(foo bar', 5) === '', 'and empty right after a space');
  check(prefixAt('  :in-th', 8) === ':in-th', 'a leading colon is part of it');
  check(prefixAt('(a b)c', 6) === 'c', 'a close paren ends a token');
  check(prefixAt('x "quoted', 9) === 'quoted', 'and so does a quote');
  check(prefixAt('fty::defpro', 11) === 'fty::defpro', 'a package prefix stays attached');
}

{
  // xdoc summaries arrive as raw markup, because the machinery that expands
  // it is not in the world. Left alone a tooltip reads "@(see b*) binder...".
  const { xdocInline } = require('./out/names.js');
  const real = "@(see b*) binder for accessing record structure fields introduced\n  with ACL2's @('defrec').";
  const out = xdocInline(real);
  check(!out.includes('@('), 'no directive machinery survives into the tooltip');
  check(out.includes('`b*`'), 'a cross-reference becomes the symbol it names');
  check(out.includes('`defrec`'), 'and inline code keeps its text');
  check(!out.includes('\n'), 'a wrapped summary becomes one line');
  check(xdocInline('<p>A <b>bold</b> and <i>slanted</i> thing.</p>')
          === 'A **bold** and *slanted* thing.',
        'inline tags become markdown and block tags are dropped');
  check(xdocInline('a &amp; b &lt; c &mdash; d') === 'a & b < c \u2014 d',
        'entities resolve, including the ones with codepoints');
  check(xdocInline('@({ (foo bar)\n  (baz) })') === '`(foo bar) (baz)`',
        'a code block collapses to inline code in a one-line summary');
  check(xdocInline('') === '', 'and an empty summary stays empty');

  // The summary reaches the hover, above the provenance line.
  const { hoverMarkdown } = require('./out/names.js');
  const md = hoverMarkdown({
    available: true, found: true, name: 'b*', kind: 'macro', origin: 'book',
    book: '(:system . "std/util/bstar.lisp")',
    short: 'A @(see binder) for @(\'let*\') forms.',
  });
  check(md.includes('`binder`'), 'the hover shows the rendered summary');
  check(md.indexOf('`binder`') < md.indexOf('bstar.lisp'),
        'above where it came from, since what it *is* comes first');
}


// ---- a term too big to read is never worked on ----
{
  const { MAX_TERM } = require('./out/policy.js');
  const { tokenizeAll } = require('./out/formView.js');
  // The shape that took the editor down. ACL2 prints an induction scheme
  // with nothing abbreviated, so for a mutual recursion the scheme *is* the
  // clique: the `defines` at line 2775 of kestrel/c/syntax/validator.lisp
  // printed 6.83 MB of it, three times over in one reply. Laying that out
  // grew it to 15.3 MB and coloring it made 563,184 spans, per render, on
  // every state event. The daemon now cuts the scheme; this is the guard for
  // every other term that could take the same shape.
  const huge = '(and\n' + ' (implies (natp x) (equal (f x) (g x)))\n'.repeat(3000) + ')';
  check(huge.length > MAX_TERM, `the sample really is oversized (${huge.length} chars)`);
  // Tokenizing is declined by leaving the entry out, and that *is* the
  // fallback: `hl` renders any string it cannot find as plain escaped text.
  const toks = tokenizeAll([huge, '(natp x)'], (text) => [[{ t: text, c: '#fff' }]]);
  check(!(huge in toks), 'an oversized term is not tokenized');
  check('(natp x)' in toks, 'while an ordinary one still is');
  globalThis.toks = toks;
  const plain = lift('esc', 'spans', 'hl')(huge);
  check(!plain.includes('<span'), 'so the page renders it as plain escaped text');
  check(plain.includes('&lt;') === false && plain.startsWith('(and'),
        'still escaped, and still the term that arrived');
}

// ---- saying how much of a scheme is on show ----
{
  const fmtChars = lift('fmtChars');
  // Decimal, so the unit matches the number a reader would quote back: the
  // scheme that started all this is 6,831,709 characters, which is 6.8 MB in
  // the same arithmetic the rest of these notes use.
  check(fmtChars(6831709) === '6.8 MB', 'a scheme too big to show is said in MB');
  check(fmtChars(16365) === '16 KB', 'and the head of it in KB');
  check(fmtChars(412) === '412 characters',
        'while a small one is said exactly, since rounding it would say 0');
}

// ---- the form view can speak for itself ----
{
  const page = require('./out/formView.js').HTML;
  // Letting go of a lock is followed immediately by rendering whatever the
  // cursor is in, so the message has to outlive that render or it is never
  // read. These pin the two halves of that: the page keeps a note across a
  // model, and drops it when the form view is held again.
  check(/kind === 'note'/.test(page), 'the page takes a note from the extension');
  const modelBranch = /if \(m\.kind === 'model'\) \{([\s\S]*?)\n  \} else if/.exec(page);
  check(modelBranch !== null, 'the page has a model branch');
  check(!/note = undefined/.test(modelBranch[1]),
        'and a model does not silently wipe a note the reader has not seen');
  check(/class="notice"/.test(page), 'a note renders in its own neutral style');
  check(!/note \? '<div class="note">/.test(page),
        'not the red one, which in here means a form went red');
  check(/\.notice \{/.test(page), 'and that style is defined');
}

// ---- a lock follows its form, or lets go ----
{
  const { keepAnchor, lockedIndex } = require('./out/policy.js');
  // A form starting at line 10, column 1 (byte) / 0 (UTF-16).
  const anchor = { line: 10, column: 1, character: 0 };
  // An edit, in the editor's coordinates with 1-based lines: what it replaced
  // and how many lines went back.
  const edit = (startLine, startChar, endLine, endChar, newLines) =>
    ({ startLine, startChar, endLine, endChar, newLines });
  const lineOf = (r) => (r === undefined ? undefined : r.line);

  // Whole lines added or removed above: the form slides, which is the case
  // the feature exists for.
  check(lineOf(keepAnchor(anchor, [edit(3, 0, 3, 0, 3)])) === 12, 'two lines added above push the form down');
  check(lineOf(keepAnchor(anchor, [edit(3, 0, 5, 0, 1)])) === 8, 'two lines removed above pull it up');
  check(lineOf(keepAnchor(anchor, [edit(3, 4, 3, 9, 1)])) === 10, 'an edit above of the same height moves nothing');
  check(lineOf(keepAnchor(anchor, [edit(20, 0, 25, 0, 1)])) === 10, 'an edit below moves nothing');

  // Inside the form: the first character has not moved, so the lock holds --
  // this is the whole point, since editing the locked form is what you do.
  check(lineOf(keepAnchor(anchor, [edit(10, 8, 10, 12, 1)])) === 10, 'editing within the line, after the form starts, holds');
  check(lineOf(keepAnchor(anchor, [edit(11, 2, 11, 6, 1)])) === 10, 'editing a later line of the form holds');
  check(lineOf(keepAnchor(anchor, [edit(10, 8, 10, 8, 4)])) === 10, 'and so does splitting the form across new lines');

  // Anything touching where we point gives up.
  check(keepAnchor(anchor, [edit(10, 0, 10, 0, 1)]) === undefined, 'inserting exactly at the form start lets go');
  check(keepAnchor(anchor, [edit(8, 0, 11, 0, 1)]) === undefined, 'an edit swallowing the line lets go');
  check(keepAnchor(anchor, [edit(10, 0, 12, 0, 1)]) === undefined, 'and so does deleting the form itself');

  // An edit finishing to our left on our own line would move our column, and
  // columns are not tracked -- so let go rather than guess.
  const second = { line: 10, column: 20, character: 19 };
  check(keepAnchor(second, [edit(10, 0, 10, 4, 1)]) === undefined,
        'an edit before a form on its own line lets go');
  check(lineOf(keepAnchor(second, [edit(10, 25, 10, 30, 1)])) === 10,
        'while one after it holds');

  // Several ranges in one event are all measured against the document they
  // were reported against, not against each other.
  check(lineOf(keepAnchor(anchor, [edit(1, 0, 1, 0, 3), edit(30, 0, 30, 0, 5)])) === 12,
        'ranges in one event compose, and only those above count');

  // The byte column rides along untouched: it is what tells two forms on one
  // line apart, and it is what has to match the wire.
  check(keepAnchor(second, [edit(1, 0, 1, 0, 2)]).column === 20, 'the byte column is carried, not recomputed');

  // Resolving: a lock is good only where a form actually starts.
  const units = [
    { index: 0, line_start: 1, col_start: 1 },
    { index: 1, line_start: 3, col_start: 1 },
    { index: 2, line_start: 3, col_start: 20 },
  ];
  const at = (line, column) => ({ line, column, character: column - 1 });
  check(lockedIndex(at(3, 20), units) === 2, 'the second form on a shared line is found by its column');
  check(lockedIndex(at(3, 1), units) === 1, 'and the first by its own');
  check(lockedIndex(at(3, 9), units) === undefined,
        'a position merely *inside* a form is not that form starting -- let go');
  check(lockedIndex(at(4, 1), units) === undefined, 'and neither is a line with no form on it');
}

// ---- tails are pushed, never polled ----
{
  const { planTail } = require('./out/policy.js');
  // Every shape a tail can arrive in, including the ones that route it
  // somewhere: none of them may ask the daemon for more. A tail arrives twice
  // a second for as long as a form runs, so a request here is a request
  // forever, and its reply is only as small as the output's lines happen to
  // be. Four hundred lines of kestrel/c/syntax/validator.lisp came back as
  // ~117 KB a time: 245 MB pushed in eleven minutes against ~1.5 MB of real
  // events, which stalled the daemon's writer and froze the extension host
  // that every other extension shares.
  const shapes = [
    { unit: null, sameBook: true, shownUnit: null, want: 'console' },
    { unit: 7, sameBook: true, shownUnit: 7, want: 'output-view' },
    { unit: 7, sameBook: true, shownUnit: 3, want: 'form-view-only' },
    { unit: 7, sameBook: false, shownUnit: 7, want: 'form-view-only' },
    { unit: 0, sameBook: true, shownUnit: 0, want: 'output-view' },
  ];
  let asked = 0;
  for (const c of shapes) {
    const plan = planTail(c);
    check(plan.target === c.want,
      `a tail with unit=${c.unit} shown=${c.shownUnit}` +
      `${c.sameBook ? '' : ' (other book)'} goes to ${c.want}`);
    if (plan.requestsMore) { asked++; }
  }
  check(asked === 0, 'and no tail, in any shape, asks the daemon for more');
}

if (bad) {
  console.error(`webview: ${bad} failing`);
  process.exit(1);
}
console.log('webview: all green');
