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
  ['the panel page', require('./out/panel.js').HTML],
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
      'the panel has no second, pooled copy of what the form introduced');

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


// ---- `pretty`: re-flowing a term to the width it is read at ----

const { pretty } = require('./out/pretty.js');

{
  // The reason the rule is not width alone: the inner
  // conjunction fits comfortably on the head's line and is still laid out
  // down the page, because that is how a list of hypotheses is read.
  const src = '(implies (and (not (consp b)) b) (not (segment-listp (append a b))))';
  const want =
    '(implies (and (not (consp b))\n' +
    '              b)\n' +
    '         (not (segment-listp (append a b))))';
  check(pretty(src, 60) === want, 'a conjunction is laid out down the page');

  // Alignment is under the first argument, not a fixed indent.
  check(
    pretty(src, 60).split('\n')[1].indexOf('b') === '(implies (and '.length,
    'and its arguments line up under the first one'
  );
}

{
  // A second example. Nothing here is an always-break head, so
  // this one is width alone -- and it is the case that showed the panel must
  // cap its width rather than use the whole pane: at the ~115 columns a wide
  // pane measures, a 66-character term never breaks at all.
  const src = '(implies t (equal (segment-listp (append a b)) (segment-listp b)))';
  const want =
    '(implies t\n' +
    '         (equal (segment-listp (append a b))\n' +
    '                (segment-listp b)))';
  check(pretty(src, 60) === want, 'the second example lays out as asked');
  // implies breaks whatever the width; only the inner `equal` is width-driven,
  // which is why a wide pane still splits after the hypothesis.
  check(
    pretty(src, 115) === '(implies t\n         (equal (segment-listp (append a b)) (segment-listp b)))',
    'and at any width the conclusion still starts its own line'
  );
}

check(pretty('(equal x y)', 60) === '(equal x y)', 'a short term is left alone');
// Unconditional, and that is the point: a goal is read as its structure,
// not as text that happens to fit. An earlier version only broke a
// conjunction when one of its arguments was itself a list, which left short
// ones running together.
check(
  pretty('(and p q)', 200) === '(and p\n     q)',
  'a conjunction breaks however much room there is'
);
check(
  pretty('(implies p q)', 200) === '(implies p\n         q)',
  'and so does implies, so the conclusion always starts its own line'
);
check(pretty('(not p)', 200) === '(not p)', 'a one-argument form has nothing to break');
check(
  pretty('(equal (f x) (g y))', 200) === '(equal (f x) (g y))',
  'and a head outside the family is width-driven as before'
);

{
  // A form with no always-break head breaks only when it must.
  const src = '(equal (segment-listp (append a b)) (segment-listp b))';
  check(pretty(src, 80) === src, 'a wide pane leaves a term on one line');
  check(pretty(src, 30).split('\n').length > 1, 'a narrow one breaks it');
  check(
    pretty(src, 30).split('\n').every((l) => l.length <= 30 || !l.includes(' ')),
    'and keeps the lines within the width where it can'
  );
}

// Total: anything unparseable comes back exactly as it arrived, because a
// term this cannot read is one ACL2 printed and we would rather show it.
for (const bad of ['(unbalanced', 'closed)', '(a "unterminated', '', '(a |open']) {
  check(pretty(bad, 40) === bad, `unparseable input is returned unchanged: ${JSON.stringify(bad)}`);
}
// Parens hidden inside quoting forms are not structure.
check(
  pretty('(f "a)b" #\\( |c)d|)', 80) === '(f "a)b" #\\( |c)d|)',
  'strings, char literals and bar symbols keep their parens'
);

{
  // A whole topic. The property that matters is that code blocks come
  // through untouched -- their line structure carries meaning, which is why
  // the daemon transports them verbatim in the first place.
  const { xdocText } = require('./out/names.js');
  const real =
    "<p>The following example shows the usage of @('unless-casematch')\n" +
    "inside a @(see b*) form.</p>\n" +
    "@({\n (b* (((unless-casematch obj (a . b))\n" +
    '       (er hard? \'my-form "nope")))\n' +
    '    (cw "The car is ~x0~%" a))\n })\n' +
    "<p>This expands to:</p>\n";
  const out = xdocText(real);
  check(!out.includes('@('), 'no directive machinery survives');
  check(out.includes('(b* (((unless-casematch obj (a . b))'),
        'the code block keeps its text');
  check(out.includes('~x0'), 'including fmt directives inside it');
  check(out.includes('\n       (er hard?'), 'and its indentation and line breaks');
  check(out.includes('b* form.'), 'a cross-reference becomes its symbol');
  check(/expands to:\s*$/.test(out), 'and paragraphs are separated, not run together');

  check(xdocText('<ul><li>one</li><li>two</li></ul>').includes('- one'),
        'list items become bullets');
  check(xdocText('a &mdash; b') === 'a \u2014 b', 'entities resolve');
  check(xdocText('@(def foo)') === '[def foo]',
        'a directive that reads the world is named, not resolved');
  check(xdocText('') === '', 'and an empty body stays empty');
}

{
  // The HTML renderer. The properties that matter: text is escaped, tags are
  // not, and neither can happen to the inside of a code block.
  const { xdocHtml } = require('./out/xdoc.js');
  const r = xdocHtml(
    "<p>Compare @('a &lt; b') and @(see b*).</p>\n" +
    '@({\n (cw "~x0 <not a tag>" a)\n })\n' +
    '<p>An <unknown-tag>odd</unknown-tag> one.</p>'
  );
  check(r.html.includes('<p>'), 'block tags become HTML');
  check(r.html.includes('class="see" data-topic="b*"'), 'a cross-reference becomes a link');
  check(r.html.includes('>a &lt; b</code>'), 'inline code is escaped, not interpreted');
  check(r.code.length === 2, 'both the block and the inline code are lifted out');
  check(r.code.some((c) => c.text === 'a < b'),
        'inline code travels as source, for the tokenizer, not as escaped HTML');
  check(r.html.includes('<code data-code='), 'and gets a slot the page can colour');
  check(r.code.some((c) => c.text === '(cw "~x0 <not a tag>" a)'),
        'and kept verbatim, tags and fmt directives and all');
  check(r.html.includes('<pre data-code="0">'), 'leaving a slot where it goes');
  check(r.html.includes('&lt;not a tag&gt;'),
        'with the escaped text inside it, so a page with no tokenizer still shows code');
  check(!r.html.includes('<unknown-tag>'), 'an unknown tag is dropped');
  check(r.html.includes('odd'), 'but its text is kept');

  // Prose that merely looks like markup must survive as prose.
  // Prose containing what used to be a placeholder must survive verbatim.
  const p0 = xdocHtml('<p>See section I0 and @(\'x\') here.</p>');
  check(p0.html.includes('section I0 and'),
        'prose that looks like a placeholder is not eaten');

  const p2 = xdocHtml('<p>if a &lt; b &amp;&amp; c then "x"</p>');
  check(p2.html.includes('a &lt; b'), 'a less-than in prose stays escaped');
  check(!p2.html.includes('&amp;amp;'), 'and an entity is not double-escaped');

  const m = xdocHtml('@([\\sum_{i} x_i]) and @($x^2$)');
  check(m.html.includes('class="math"'), 'math is marked');
  check(m.html.includes('\\sum'), 'and shown as its source, pending KaTeX');

  // Code blocks must arrive as *text* for the host to tokenize, not as
  // pre-escaped HTML: the tokenizer needs the source, and the page escapes.
  const c2 = xdocHtml('@({\n (defun f (x)\n   (+ x 1))\n })');
  check(c2.code[0].text === '(defun f (x)\n  (+ x 1))',
        'a code block keeps its shape after the source indent is removed');
  check(c2.html.includes('(defun f (x)'), 'and is escaped into the pre as a fallback');

  const d = xdocHtml('@(def foo)');
  check(d.html.includes('unresolved') && d.html.includes('[def foo]'),
        'a world-reading directive is named, not resolved');
}

// ---- `pretty` stays linear on deeply nested terms ----
{
  const deep = (n) => {
    let t = '(equal x y)';
    for (let i = 0; i < n; i++) {
      t = `(and ${t} (implies (natp x${i}) (equal (f x${i}) (g x${i}))))`;
    }
    return t;
  };
  // Indentation used to grow with nesting depth, so lines grew while their
  // number grew: 43 KB of term laid out to 4.9 MB, and 88 KB took 13.5 s.
  // A `defines` clique is exactly that shape, and a few of them built an
  // extension host heap of 1.8 GB that spent its life in GC while every
  // other extension stopped answering keystrokes.
  const small = deep(200);
  const big = deep(800);
  const t0 = Date.now();
  const outSmall = pretty(small, 60);
  const mid = Date.now();
  const outBig = pretty(big, 60);
  const t1 = Date.now();
  check(
    outBig.length / big.length < 6,
    `layout stays proportional to the term (${(outBig.length / big.length).toFixed(1)}x)`
  );
  // Four times the input must not be sixteen times the work. Generous
  // against a loaded machine; the failure it guards against was 20x.
  const growth = (t1 - mid + 1) / (mid - t0 + 1);
  check(growth < 10, `and the time with it (${growth.toFixed(1)}x for 4x the input)`);
  check(
    pretty('(and (natp x) (natp y))', 60).split('\n').length === 2,
    'while a small conjunction still breaks as before'
  );
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
    { unit: 7, sameBook: true, shownUnit: 3, want: 'panel-only' },
    { unit: 7, sameBook: false, shownUnit: 7, want: 'panel-only' },
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
