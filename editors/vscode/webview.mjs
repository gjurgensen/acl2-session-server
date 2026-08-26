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

import { freshOrDie } from './fresh.mjs';

// Before the first require: this harness tests the compiled module, so a
// stale out/ makes it report on code that is not the code.
freshOrDie();

const require = createRequire(import.meta.url);

// Both modules import `vscode`, which exists only inside the editor. Nothing
// they do at *load* time touches it, so an empty stub is enough to get at
// the page builders.
const { Module } = require('module');
const resolve = Module._resolveFilename;
Module._resolveFilename = (request, ...rest) =>
  request === 'vscode' ? 'vscode' : resolve.call(Module, request, ...rest);
// Enough of `vscode` to post a model: the form view reads the fold limit
// from configuration on its way out. `get` hands back the caller's own
// default, which is what a harness should say about an unset setting.
const vscodeStub = {
  workspace: { getConfiguration: () => ({ get: (_key, fallback) => fallback }) },
  Uri: { joinPath: (base, ...parts) => `${base}/${parts.join('/')}` },
};
require.cache.vscode =
  { id: 'vscode', filename: 'vscode', loaded: true, exports: vscodeStub };

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
  const m = /<script[^>]*>([\s\S]*)<\/script>/.exec(page);
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
//
// The nonce is that layer, and it is checked in both halves because half of
// it is worse than neither: a policy naming a nonce with no nonce on the
// script tag is a page whose script does not run at all, and a nonce on the
// tag with `'unsafe-inline'` still in the policy runs everything, which is
// the state this replaced. `pageHtml` fills both placeholders in one pass, so
// the pairing is what there is to check.
for (const [name, page] of pages) {
  check(/http-equiv="Content-Security-Policy"/.test(page)
        && /default-src 'none'/.test(page),
        `${name} declares a content security policy`);
  check(/script-src 'nonce-%%NONCE%%'/.test(page),
        `${name}'s policy admits scripts by nonce alone`);
  check(!/script-src[^;"]*'unsafe-inline'/.test(page),
        `${name} does not also admit every inline script`);
  check(/<script nonce="%%NONCE%%">/.test(page),
        `${name}'s own script carries the nonce`);
}

// And the substitution itself, which is what turns those placeholders into a
// policy. A page rendered twice must not reuse a nonce: one an attacker saw
// on an earlier render is one they can write into a later one.
{
  const { pageHtml } = require('./out/webviewParts.js');
  const stub = { asWebviewUri: (u) => u, cspSource: 'vscode-webview:' };
  const page = pages[0][1];
  const once = pageHtml(page, stub);
  const twice = pageHtml(page, stub);
  const nonceOf = (p) => (/<script nonce="([^"]+)">/.exec(p) ?? [])[1];
  check(!once.includes('%%NONCE%%'), 'no nonce placeholder survives a render');
  check(nonceOf(once) !== undefined && once.includes(`script-src 'nonce-${nonceOf(once)}'`),
        'and the policy names the very nonce the script carries');
  check(nonceOf(once) !== nonceOf(twice), 'a second render gets a nonce of its own');
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
function parts(names) {
  const out = [];
  for (const name of names) {
    // Declarations end at a closing brace in column zero; arrow constants and
    // mutable page state at the first `;` that ends a line. All three
    // spellings are in use -- the fold map is a `let` because the page
    // rewrites it when the cursor moves to another form.
    const m =
      new RegExp(`function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`).exec(consoleScript) ??
      new RegExp(`const ${name} = [\\s\\S]*?;$`, 'm').exec(consoleScript) ??
      new RegExp(`let ${name} = [\\s\\S]*?;$`, 'm').exec(consoleScript);
    if (!m) {
      console.error(`FAIL: ${name} is not in the console script`);
      process.exit(2);
    }
    out.push(m[0]);
  }
  return out.join('\n');
}

function lift(...names) {
  return new Function(`${parts(names)}; return ${names[names.length - 1]};`)();
}

/**
 * Several names at once, for a group that only means anything together.
 *
 * `fold` is the case: it reads and writes page state that `bindFolds`
 * records, so testing either one alone tests nothing.
 */
function liftAll(names, expr) {
  return new Function(`${parts(names)}; return ${expr};`)();
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

// ---- the icon font, and the two holes a page cannot fill for itself ----
//
// A webview gets no icons for free, so the chevrons on the folds are
// VSCode's own codicon font shipped with the extension. Where it lives and
// what the webview will load it from are both per-webview, so the pages
// carry placeholders and `pageHtml` fills them. A page that forgot either
// one renders with no chevrons and a console full of CSP violations, which
// is exactly the kind of thing nothing else here would notice.

{
  const { pageHtml, setExtensionUri, SHARED_CSS } = require('./out/webviewParts.js');

  check(SHARED_CSS.includes('font-family: "codicon"'),
        'the shared stylesheet asks for the codicon font');
  check(SHARED_CSS.includes('\\eab6') && SHARED_CSS.includes('\\eab4'),
        'and uses chevron-right closed, chevron-down open');
  check(/details\[data-fold\]/.test(SHARED_CSS),
        'scoped to the folds, so the console keeps its own summaries');

  // The shading has one job: marking a block of ACL2's own text you might
  // select as a unit. Left on every inline `code`, a rule list became a
  // stack of grey pills and the mark stopped marking anything.
  check(/ul\.rules code,[\s\S]*?\{ background: none/.test(SHARED_CSS),
        'the places made of code carry no chip behind them');
  check(/\.body pre \{[^}]*background: var\(--vscode-textCodeBlock-background\)/
          .test(SHARED_CSS) || /^pre \{[^}]*background:/m.test(SHARED_CSS),
        'while a pre block keeps its own, which is what the shading is for');

  for (const [what, page] of pages) {
    check(page.includes('%%CODICON%%'), `${what} carries the font placeholder`);
    check(/font-src %%CSPSOURCE%%/.test(page), `${what} allows the font in its CSP`);
  }

  setExtensionUri('ext:/root');
  const filled = pageHtml(pages[1][1], {
    asWebviewUri: (u) => ({ toString: () => `https://webview/${u}` }),
    cspSource: 'https://webview',
  });
  check(!filled.includes('%%'), 'and pageHtml leaves no placeholder behind');
  check(filled.includes('https://webview/ext:/root/node_modules/@vscode/codicons/dist/codicon.ttf'),
        'the font URL points at the file the extension ships');
  check(filled.includes('font-src https://webview'),
        'and the CSP names the origin that will serve it');
}

// ---- the running form: the tail takes the pane ----
//
// While a form runs the log is the only thing on the page, so it grows into
// the pane rather than sitting in a fixed box. Grown and not stretched: the
// button belongs under the output, and `flex: 1` would fill the pane even
// when two lines had been printed and strand the button at the foot of an
// empty box. Both `min-height: 0` rules are load-bearing -- without them a
// flex child will not shrink below its content and the *pane* scrolls
// instead of the log, which looks like the tail simply not working.

{
  const page = pages[1][1];
  check(/#root\.running \{[^}]*display: flex/.test(page),
        'a running form lays its page out as a column');
  check(/#root\.running details\.tail\[open\] \{[^}]*flex: 0 1 auto;[^}]*min-height: 0/.test(page),
        'the open tail fold grows into the leftover room without claiming it');
  check(!/#root\.running[^}]*\{[^}]*flex: 1;/.test(page),
        'and nothing stretches, which would strand the button under empty space');
  check(/#root\.running details\.tail\[open\] #tailtext \{[^}]*min-height: 0/.test(page),
        'and so does the log inside it, or the pane scrolls instead');
  check(/details\.tail\[open\]/.test(page) && !/details\.tail \{/.test(page),
        'only while open, so a collapsed fold lets the button rise');
  check(page.indexOf("fold('tail'") < page.indexOf('id="openoutput"'),
        'the button sits under the output, not above it');
}

// ---- `statusLabel`: the word on the chip ----
//
// The chips stopped echoing the wire because `admitted` is wrong for a form
// that was evaluated rather than defined, and the console had already needed
// a second word for the same state. Two things matter. The map is total:
// every status the stylesheet paints comes out as something, and one it has
// never heard of comes out as itself. And the words agree with each other --
// they are verbs, so a finished form reads as a past participle and an
// unfinished one as a present participle, rather than mixing a noun in among
// them.

{
  const statusLabel = lift('statusLabel');
  check(statusLabel('admitted') === 'succeeded', 'an admitted form reads as succeeded');
  check(statusLabel('ok') === 'succeeded', 'and so does an eval, which the wire calls ok');
  check(statusLabel('failed') === 'failed', 'the rest keep the word they already had');
  // The inconsistency this replaced: a noun beside a participle.
  for (const [st, want] of [['admitted', 'succeeded'], ['failed', 'failed'],
                            ['skipped', 'skipped'], ['running', 'running'],
                            ['queued', 'queued'], ['pending', 'pending']]) {
    check(statusLabel(st) === want, `${st} reads as a verb (${want})`);
  }
  check(statusLabel('gubbins') === 'gubbins',
        'and a status from a newer daemon renders as itself, not as nothing');
  for (const st of ['admitted', 'ok', 'conditional', 'skipped', 'failed',
                    'running', 'queued', 'pending', 'beyond']) {
    const got = statusLabel(st);
    check(!!got && got === got.toLowerCase(),
          `${st} has a lowercase label (${got})`);
  }
}

// ---- folds: what the reader opened, surviving the next render ----
//
// A render replaces the page's markup, so `open` on a `<details>` is gone
// with the element that carried it. Everything here is about the three
// states being distinct -- opened, closed, and never touched -- because a
// fold the reader deliberately shut must not spring back open just because
// its section's default says open.

{
  const { fold, foldScopeIs, bindFolds, foldKindsAre, foldsForget } =
    liftAll(['esc', 'folds', 'foldMemory', 'foldOrder', 'foldScope', 'FOLD_SCOPES',
             'kindFolds', 'foldKindsAre', 'foldsForget', 'foldScopeIs', 'fold', 'bindFolds'],
            '({ fold, foldScopeIs, bindFolds, foldKindsAre, foldsForget })');

  // The page binds one listener on the document; this stands in for it, and
  // records how it was bound.
  let toggled;
  let capture;
  bindFolds({
    addEventListener: (type, fn, useCapture) => {
      if (type === 'toggle') { toggled = fn; capture = useCapture; }
    },
  });
  check(capture === true,
    'folds are recorded in the capture phase, since toggle does not bubble');

  const details = (id, open, kind) =>
    ({ tagName: 'DETAILS', dataset: kind ? { fold: id, kind } : { fold: id }, open });
  const open = (id, byDefault, kind) =>
    fold(id, kind, byDefault, 'S', 'B').includes(' open>');

  check(open('a', true), 'an untouched fold takes its section default');
  check(!open('b', false), 'and a closed default stays closed');

  toggled({ target: details('a', false) });
  check(!open('a', true), 'a fold the reader shut stays shut through the next render');
  toggled({ target: details('b', true) });
  check(open('b', false), 'and one they opened stays open');

  foldScopeIs('book:4');
  toggled({ target: details('c', false) });
  foldScopeIs('book:4');
  check(!open('c', true), 'a re-render of the same form keeps what they left');
  foldScopeIs('book:5');
  check(open('c', true), 'while another form starts from the section default');
  // The point of keying by form: coming back finds it as it was left.
  foldScopeIs('book:4');
  check(!open('c', true), 'and coming back to the first finds it as they left it');

  check(fold('4:event:my-app:introduced', null, true, 'S', 'B')
          .includes('data-fold="4:event:my-app:introduced"'),
    'the id is carried on the element, so the next render can find it again');
  check(fold('a&b', null, true, 'S', 'B').includes('data-fold="a&amp;b"'),
    'and escaped, since an ACL2 name is not HTML');

  // Neither of these is a fold; neither should be recorded or throw.
  toggled({ target: { tagName: 'DETAILS', dataset: {} } });
  toggled({ target: { tagName: 'DIV', dataset: { fold: 'a' } } });
  check(open('a', true), 'a details with no id, and a non-details, are both ignored');

  foldsForget();
}

// ---- a fold's kind carries the gesture to the ones not touched ----
//
// The generalising rule. What makes a reader collapse a fold is usually a
// property of its kind -- a `used` list is always long -- so one gesture
// speaks for all of them. Being wrong costs a click; not generalising costs a
// click per instance per form, forever.
{
  const { fold, foldScopeIs, bindFolds, foldKindsAre, foldsForget } =
    liftAll(['esc', 'folds', 'foldMemory', 'foldOrder', 'foldScope', 'FOLD_SCOPES',
             'kindFolds', 'foldKindsAre', 'foldsForget', 'foldScopeIs', 'fold', 'bindFolds'],
            '({ fold, foldScopeIs, bindFolds, foldKindsAre, foldsForget })');
  let toggled;
  let sent;
  bindFolds(
    { addEventListener: (type, fn) => { if (type === 'toggle') { toggled = fn; } } },
    (kinds) => { sent = { ...kinds }; }
  );
  const details = (id, open, kind) =>
    ({ tagName: 'DETAILS', dataset: { fold: id, kind }, open });
  const open = (id, byDefault, kind) =>
    fold(id, kind, byDefault, 'S', 'B').includes(' open>');

  foldScopeIs('book:form-a');
  check(open('ev:foo:used', true, 'ev:used'), 'to begin with, the default rules');
  toggled({ target: details('ev:foo:used', false, 'ev:used') });
  check(!open('ev:bar:used', true, 'ev:used'),
        'collapsing one carries to another of the same kind, untouched');
  check(open('ev:foo:introduced', true, 'ev:introduced'),
        'and not to a different kind');

  // Explicit beats inherited: a fold set by hand keeps what it was set to
  // even when the kind default moves under it afterwards.
  toggled({ target: details('ev:baz:used', true, 'ev:used') });
  check(open('ev:baz:used', false, 'ev:used'), 'the one just opened is open');
  check(!open('ev:foo:used', true, 'ev:used'),
        'and the one explicitly closed earlier stays closed');
  check(open('ev:qux:used', false, 'ev:used'),
        'while an untouched one follows the newest gesture');

  // The kind default is not per form -- that is the whole point of it.
  foldScopeIs('book:form-b');
  check(open('ev:new:used', false, 'ev:used'),
        'a different form inherits the kind default, not the section default');
  check(!open('ev:foo:used', true, 'ev:used') === false,
        'though the explicit entry belonged to the form it was set on');

  // What leaves for the host, so it can be stored and fed to the other pane.
  check(sent && sent['ev:used'] === true, 'the kind map is handed out on change');

  // The host feeding stored state back in.
  foldKindsAre({ 'ev:used': false, events: false });
  check(!open('ev:brand-new:used', true, 'ev:used') && !open('events', true, 'events'),
        'stored defaults arrive from workspace state and apply at once');

  foldsForget();
  check(open('events', true, 'events') && open('ev:brand-new:used', true, 'ev:used'),
        'and the reset command puts every kind back to its shipped default');
}

// ---- fold memory is bounded ----
//
// Browsing a large book end to end would otherwise keep a map per form for
// the life of the window.
{
  const { fold, foldScopeIs, foldsForget, scopes } =
    liftAll(['esc', 'folds', 'foldMemory', 'foldOrder', 'foldScope', 'FOLD_SCOPES',
             'kindFolds', 'foldKindsAre', 'foldsForget', 'foldScopeIs', 'fold', 'bindFolds'],
            '({ fold, foldScopeIs, foldsForget, scopes: () => foldOrder.length })');
  foldsForget();
  for (let i = 0; i < 250; i++) { foldScopeIs('book:f' + i); }
  check(scopes() === 100, `at most FOLD_SCOPES forms are remembered (${scopes()})`);
  foldsForget();
}

// ---- `envelope`: what a form left behind, per event ----
//
// A productive form is its events, each carrying the facts the buffer does
// not have. Rendered from the shape the daemon actually sends: a
// guard-verified function with derived rules, one of them disabled, and a
// theorem whose left-hand side is written nowhere in the source.

globalThis.toks = {};
const envelope = lift('esc', 'strip', 'causes', 'spans', 'hl', 'rat',
                      'fmtSecs', 'timeParts', 'proveSecs', 'fmtTime', 'fmtChars', 'guardNote',
                      'folds', 'foldMemory', 'foldOrder', 'foldScope', 'FOLD_SCOPES',
                      'kindFolds', 'foldKindsAre', 'foldsForget', 'foldScopeIs', 'fold',
                      'factRow', 'nameFacts', 'introducedHtml', 'inductionHtml', 'usedHtml',
                      'summaryHtml',
                      'withCode', 'TITLE_NAMES', 'cliqueTitle', 'memberHtml', 'eventHtml', 'docCard', 'docsHtml', 'productiveHtml', 'failureHtml', 'warningsHtml',
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
      { name: 'true-listp-of-dfn-app', kind: 'defthm', proverSteps: 12,
        rules: ['(:definition dfn-app)'], detail: thm },
      { name: 'dfn-app', kind: 'defun', detail: fn },
      { name: 'undetailed' },
    ],
  }, 'g-');

  check(h.includes('<code class="ename">dfn-app</code> <code class="lisp">(x y)</code>'),
        'a function shows its formals beside its name, as a signature');
  check(h.includes('guards verified'),
        'and says in the summary that its guards were verified');
  check(h.includes('2 rules, 1 off'),
        'and counts the rules it wrote, and how many of those are switched off');
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
  check(h.includes('introduced (1)') && h.includes('used (1)'),
        'a theorem counts what it introduced apart from what its proof used, ' +
        'in the (N) style the section headings use');
  check(h.includes('data-fold="g-ev:true-listp-of-dfn-app:introduced"') &&
        h.includes('data-fold="g-ev:true-listp-of-dfn-app:used"'),
        'and the two are separate folds, so opening one does not open the other');
  check(h.indexOf('Events (') !== -1 && h.indexOf('dfn-app') > h.indexOf('Events ('),
        'the events are the section, not a table appended under one');
  check(h.indexOf('<code class="ename">dfn-app</code>') <
        h.indexOf('<code class="ename">true-listp-of-dfn-app</code>'),
        'and run in source order, not the newest-first order the wire sends');
  check(/lhs<\/td><td><code[^>]*>\(true-listp \(dfn-app x y\)\)/.test(h),
        'the left-hand side a rewrite rule will match is shown, inline');
  // The value sits beside its label either way: a one-liner inline, a
  // multi-line term as a pre still in the value cell, never promoted to its
  // own line. Neither is marked.
  {
    const factRow = lift('esc', 'spans', 'hl', 'factRow');
    const one = factRow('lhs', '(true-listp x)');
    check(/<td class="meta">lhs<\/td><td><code/.test(one),
          'a one-line term is inline in the value column');
    const many = factRow('corollary', '(or (consp x)\n    (equal x y))');
    check(/<td class="meta">corollary<\/td><td><pre class="field">/.test(many),
          'a multi-line term stays in the value cell, beside its label');
    check(!many.includes('colspan'),
          'and is not promoted onto a line of its own');
  }
  // A field value carries no rule down its edge, at any length: the label is
  // the structure. The rule is for a standalone block -- a checkpoint, the
  // value, the tail -- which this asserts the stylesheet still marks.
  {
    const { SHARED_CSS } = require('./out/webviewParts.js');
    const preRule = /\npre \{[^}]*\}/.exec(SHARED_CSS)[0];
    check(/pre\.field \{[^}]*border: 0/.test(SHARED_CSS),
          'a field value carries no box; the label beside it is its structure');
    check(/border: 1px solid/.test(preRule) && /border-radius/.test(preRule),
          'while a standalone block is boxed, the same idiom as an event card');
  }
  check((h.match(/data-fold="g-ev:[^":]*"/g) || []).length === 2,
        'an event with nothing to show gets no fold');
  check(h.includes('detail (kind, time, steps, rules) is probed for the first 2'),
        'and is counted as unprobed, so a truncated probe still says so');
}
check(!pages[1][1].includes('Introduced ('),
      'the form view has no second, pooled copy of what the form introduced');

// The value a form evaluated to. Whether there is one to show is ACL2's
// decision, made in the core; the page's whole job is to put it above the
// events and not invent a section when there is nothing.
{
  const withValue = envelope(
    { checkpoints: [], rules: [], value: '(1 2 3)', inner: [] }, 'v-');
  check(withValue.includes('Value'), 'a form that evaluated to something shows it');
  check(withValue.includes('(1 2 3)'), 'and shows the value itself');
  check(!withValue.includes('Events ('),
        'a form with a value and no events grows no events section');

  const both = envelope({
    checkpoints: [], rules: [], value: '7',
    inner: [{ name: 'e', kind: 'defthm', rules: ['(:definition endp)'] }],
  }, 'w-');
  check(both.indexOf('Value') < both.indexOf('Events ('),
        'and when a form did both, the value comes first');

  const neither = envelope({ checkpoints: [], rules: [], inner: [] }, 'y-');
  check(!neither.includes('Value'),
        'while a form ACL2 printed no value for gets no section at all');
}

// A failure is a different page, not the same one with a red chip. What is
// pinned here is the *order*: the abort cause is nearly always
// induction-depth-limit-exceeded and the fix is nearly never the depth
// limit, so the goals come first and the cause sinks to the bottom.
{
  const h = envelope({
    failed: true,
    abortCause: '(ACL2::EMPTY-CLAUSE)',
    errorLine: 'ACL2 Error [Failure] in ( PROGN (defthm true-listp-of-app ...) ...)',
    time: '(1/100 0 0 0)',
    rules: ['(:definition endp)', '(:induction app)'],
    checkpoints: [
      { kind: 'top', goal: 'Goal', term: '(true-listp (app x y))' },
      { kind: 'sub', goal: "Subgoal *1/1''", term: '(implies (not (consp x)) (true-listp y))' },
    ],
    nTop: 1, nSub: 1,
    inner: [
      { name: 'true-listp-of-app', kind: 'defthm', abortCauses: '(EMPTY-CLAUSE)' },
      { name: 'app', kind: 'defun' },
    ],
  }, 'f-');

  const at = (t) => h.indexOf(t);
  check(at('true-listp-of-app') < at('Checkpoints'),
        'a failure names what failed before it shows a single goal');
  check(at('Checkpoints') < at('Summary ('),
        'the goals come before what the proof drew on');
  check(at('Rules (') < at('abort cause'), 'and the cause comes after both');
  check(at('abort cause') < at('Admitted before the failure'),
        'while what already worked is last of all');
  check(!/>top</.test(h) && !/>sub</.test(h),
        'checkpoints carry no pool tag -- the goal name already says which they are');
  check(h.includes("Subgoal *1/1''"), 'and the goal name is there to say it');
  check(!h.includes('<button'), 'no per-goal button, which the first version leaves out');
  check(h.includes('data-fold="f-before"'),
        'the events that got in before the failure are folded');
  check((h.match(/class="ename">app</g) || []).length === 1,
        'and the failed event is not repeated among them');
}

// A form that never reached the prover has no goals at all. The name and the
// error line are then the whole page, which is why the name is the top slot
// rather than the cause.
{
  const h = envelope({
    failed: true,
    errorLine: 'ACL2 Error in ( DEFUN app ...):  The name APP is in use.',
    checkpoints: [],
    inner: [{ name: 'app', kind: 'defun' }],
  }, 'x-');
  check(h.indexOf('class="ename">app<') < h.indexOf('The name APP is in use'),
        'a translate-level failure still says what failed, then why');
  check(!h.includes('Checkpoints'), 'and shows no empty checkpoint section');
}

// Per-checkpoint prover output has to come from a model, not from the DOM.
//
// The console re-renders on a tail tick, on another eval's state, and on a
// theme refresh, and each render rebuilds these elements. Written straight
// into the DOM, the text a reader had just asked for was wiped by whichever
// came first -- and a render landing between the click and the reply left the
// reply arriving into a pre that was still hidden, so the button looked dead.
{
  const cps = [{ goal: "Subgoal *1/2'", term: '(implies (consp x) (natp (f x)))' }];
  const bare = envelope({ checkpoints: cps, inner: [] }, 'egoal-1-', 'goal');
  check(bare.includes('<pre id="egoal-1-0" hidden></pre>'),
        'a checkpoint with nothing fetched renders an empty, hidden slot');
  check(bare.includes('prover output</button>'), 'and the button that fetches it');

  const filled = envelope({ checkpoints: cps, inner: [] }, 'egoal-1-', 'goal',
                          { 0: 'Subgoal *1/2\u2019 simplifies to T.' });
  check(filled.includes('<pre id="egoal-1-0">Subgoal'),
        'and one already fetched renders its text, not hidden');
  check(!filled.includes('id="egoal-1-0" hidden'), 'the slot is shown');

  // Book-controlled text reaches this slot -- it is what ACL2 printed under a
  // goal -- so it is escaped like everything else in these pages.
  const nasty = envelope({ checkpoints: cps, inner: [] }, 'egoal-1-', 'goal',
                         { 0: '<script>alert(1)</script>' });
  check(!nasty.includes('<script>'), 'and is escaped on the way in');
  check(nasty.includes('&lt;script&gt;'), 'as text');

  // Without the fetch command there are no buttons and no slots at all, which
  // is the form view's spelling of the same envelope.
  const noOn = envelope({ checkpoints: cps, inner: [] }, 'goal-');
  check(!noOn.includes('<pre id="goal-0"'), 'no slots where nothing can fetch');
}

// Open by default only while there are few enough to read past. The defines
// in kestrel/c/syntax/validator.lisp records 620 events.
{
  const events = (n) => {
    const out = [];
    for (let i = 0; i < n; i++) {
      out.push({ name: 'e' + i, kind: 'defthm', rules: ['(:definition endp)'] });
    }
    return out;
  };
  const render = (n, eventsOpenLimit) =>
    envelope({ checkpoints: [], rules: [], inner: events(n), eventsOpenLimit }, 'c-');
  // The cards, not the section that holds them: the Events section folds too
  // and is open by default, so asking the whole envelope would always find an
  // `open` and never see the cards close.
  const cards = (h) => [...h.matchAll(/<details[^>]*class="card"[^>]*>/g)].map((m) => m[0]);
  check(cards(render(3, 4)).every((c) => c.includes(' open>')),
    'a handful of events open by default');
  check(cards(render(6, 4)).every((c) => !c.includes(' open>')),
    'and past the limit every one of them starts collapsed');
  check(render(6, 99).includes(' open>'), 'the limit is a setting, not a constant');
}

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


// ---- reading the theme: the two pure functions in highlight.ts ----
//
// The form view's promise is that a term in it and the same term in the buffer
// are coloured identically, from the same grammar. That rests on reading the
// theme the way VSCode reads it, and neither half of that reading was checked
// by anything.
{
  const { stripJsonc, simpleRules } = require('./out/highlight.js');

  // JSONC, which is what a theme file is. The comment and trailing-comma
  // stripping has to be string-aware in both directions.
  // Replaced by its newline rather than removed, so line numbers survive for
  // anything that reports a parse error by position.
  check(stripJsonc('{"a": 1} // trailing') === '{"a": 1} \n', 'a line comment goes');
  check(stripJsonc('{/* a */ "a": 1}') === '{ "a": 1}', 'and a block comment');
  check(stripJsonc('{"a": 1,}') === '{"a": 1}', 'a trailing comma before }');
  check(stripJsonc('{"a": [1,2,]}') === '{"a": [1,2]}', 'and before ]');
  check(stripJsonc('{"a": 1, "b": 2}') === '{"a": 1, "b": 2}', 'a real comma stays');
  // The one that was wrong: the character loop knew about strings and the
  // trailing-comma pass did not, so a comma inside a theme's own value was
  // taken out.
  check(stripJsonc('{"a": "x, }"}') === '{"a": "x, }"}',
        'a comma inside a string is content, not punctuation');
  check(stripJsonc('{"a": "// not a comment"}') === '{"a": "// not a comment"}',
        'and so is a slash pair');
  check(stripJsonc('{"a": "q\\" , }"}') === '{"a": "q\\" , }"}',
        'an escaped quote does not end the string');
  check(JSON.parse(stripJsonc('{"a": 1, /* c */ "b": [2,],}')).b[0] === 2,
        'and the result parses');

  // The simple `tokenColorCustomizations` keys, which reached nothing: only
  // `textMateRules` was read, so a reader who set `comments` in the settings
  // UI saw one colour in the buffer and another in the form view.
  const rules = simpleRules({ comments: '#ff0000', keywords: { fontStyle: 'bold' } });
  check(rules.length === 2, 'a colour and a style each become a rule');
  const byScope = (s) => rules.find((r) => r.scope.includes(s));
  check(byScope('comment')?.settings.foreground === '#ff0000',
        'a bare string is a foreground');
  check(byScope('keyword')?.settings.fontStyle === 'bold', 'an object is taken as written');
  check(simpleRules({ textMateRules: [] }).length === 0,
        'and the key that already worked is not translated twice');
  check(simpleRules({ comments: undefined, strings: null }).length === 0,
        'nothing set is no rule');
  check(simpleRules({}).length === 0, 'and an empty customization is none');
}

// ---- xdoc markup to HTML: the escaping layer ----
//
// The harness `xdoc.ts`'s own header claimed and did not have. It matters
// more here than anywhere else in the client, and for a reason the module
// states itself: a topic body is *book-controlled* text rendered into a
// script-enabled webview. `esc` and `xdocHtml` are what stand between "open
// somebody's book" and "run somebody's script" -- the nonce above is the
// second layer, not the first.
//
// Escaping is checked by its output, never by reading the source: the claim
// is that no book text reaches the page as markup, and the way to check a
// claim about the output is to look at the output.
{
  const { xdocHtml, esc } = require('./out/xdoc.js');
  const html = (src) => xdocHtml(src).html;

  check(esc('<a href="x">&</a>') === '&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;',
        'esc handles all four of the characters that matter');
  // `'` is not among them, which is safe only because every attribute this
  // module writes is double-quoted. Pinned, so a single-quoted one added
  // later fails here rather than in the field.
  check(!/'/.test(esc("won't")) === false && esc("won't") === "won't",
        "esc leaves ' alone, which every attribute here is written to allow");
  check(!/ '/.test(html('@(see x)')) && / [a-z-]+="/.test(html('@(see x)')),
        'and every attribute it emits is double-quoted');

  // Prose. An unknown tag is dropped and its text kept -- that is the
  // documented behaviour -- so the test is that the *text* arrives inert.
  check(html('<script>alert(1)</script>') === 'alert(1)',
        'a script tag is dropped and its body arrives as prose');
  check(html('a < b > c').includes('a &lt; b &gt; c'),
        'and comparison operators in prose survive as text');
  check(!html('<img src=x onerror="alert(1)">').includes('onerror'),
        'an unknown tag takes its attributes with it');

  // Entities are resolved before escaping so `&lt;` round-trips. That order
  // is the one that could be got wrong in the dangerous direction: resolve
  // after escaping and `&amp;lt;script&amp;gt;` becomes a tag.
  check(html('&lt;script&gt;alert(1)&lt;/script&gt;')
          === '&lt;script&gt;alert(1)&lt;/script&gt;',
        'an escaped tag written as entities stays escaped');
  check(html('a &amp; b') === 'a &amp; b', 'and an ampersand stays one ampersand');

  // Attributes. Two paths write one: `<see topic="...">` from a book, and
  // `@(see ...)`, whose whole argument is book text.
  check(html('<see topic="a<b">x</see>').includes('data-topic="a&lt;b"'),
        'a topic attribute is escaped on its way into the page');
  {
    // Single quotes are not the spelling the topic regex reads, so such an
    // attribute reaches the page as an empty topic and its contents as prose
    // -- a dead link, never a live tag.
    const odd = html('<see topic=\'"><script>alert(1)</script>\'>x</see>');
    check(odd.includes('data-topic=""') && !odd.includes('<script>'),
          'a topic in the other quotes yields an empty topic, not a tag');
  }
  check(!html('@(see "><script>alert(1)</script>)').includes('<script>'),
        'nor can a see directive');
  check(html('@(url "><b>)').includes('&quot;&gt;&lt;b&gt;'),
        'nor a url directive, whose argument is written as text');
  check(html('@(def "><b>)').includes('&quot;&gt;&lt;b&gt;'),
        'nor an unresolved directive, which is shown rather than obeyed');

  // Code. Blocks and inline code are lifted out before anything else and
  // handed to the host separately -- the host colours them with the editor's
  // grammar -- so both the copy in the page and the copy in `code` matter.
  // A code block's entities are decoded, like an inline one's: it is lifted
  // out ahead of the pass that would do it, and every `define` writes a
  // signature line reading `(f x) &rarr; y`.
  const arrow = xdocHtml('<code>(f x) &rarr; y</code>');
  check(/\u2192/.test(arrow.html), 'an entity in a code block becomes its character');
  check(arrow.code[0].text.includes('\u2192'),
    'and the lifted copy the host colours has it too, so the two agree');

  const withCode = xdocHtml('@({ <script>alert(1)</script> })');
  check(withCode.html.includes('&lt;script&gt;') && !withCode.html.includes('<script>'),
        'a code block reaches the page escaped');
  check(withCode.code.length === 1 && withCode.code[0].text.includes('<script>'),
        'and reaches the host as the text it actually is, for the grammar');
  check(!html("@('<script>alert(1)</script>')").includes('<script>'),
        'inline code is escaped the same way');
  check(!html('<v><script>alert(1)</script></v>').includes('<script>'),
        'and so is its tag spelling');
  check(!html('@([\\href{javascript:alert(1)}])').includes('<a '),
        'math is shown as its source rather than rendered');

  // The slot sentinel is U+0001 on the grounds that nothing turns into one.
  // A book that contains one anyway must not be able to address a slot.
  const smuggle = xdocHtml('\u00010\u0001 @(\'x\')');
  check(!smuggle.html.includes('<script>') && smuggle.code.length === 1,
        'a stray sentinel in book text cannot conjure a slot that is not ours');

  check(html('') === '', 'an empty body renders as nothing');
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

// ---- an induction sits on the event that performed it ----
//
// The grouping is the point, and it is the thing the deleted log parser could
// not do. Two theorems in one form, both inducting, both calling their goal
// `*1`: the schemes have to land on their own cards or they say nothing.
{
  globalThis.toks = {};
  const h = envelope({
    checkpoints: [], rules: [], inner: [
      { name: 'tl-rev', kind: 'defthm', inductions: [
        { goal: '*1.1', suggested: '(app rv (list x1))',
          accommodating: ['(true-listp rv)'],
          runes: ['(:induction true-listp)', '(:induction app)'],
          pFormula: '(:p rv x1)', scheme: '(and (implies (not (consp rv)) (:p rv x1)))' },
        { goal: '*1', suggested: '(rev x)', runes: ['(:induction rev)'],
          pFormula: '(:p x)', scheme: '(and (implies (not (consp x)) (:p x)))' },
      ] },
      { name: 'app-assoc', kind: 'defthm', inductions: [
        { goal: '*1', suggested: '(app x y)', accommodating: ['(app x (app y z))'],
          runes: ['(:induction app)'], pFormula: '(:p x y z)',
          scheme: '(and (implies (not (consp x)) (:p x y z)))' },
      ] },
      { name: 'never-inducted', kind: 'defthm' },
    ],
  }, 'g-');

  check(h.includes('inducted (2)') && h.includes('inducted (1)'),
        'each event counts its own inductions, in the (N) style');
  check(h.includes('data-fold="g-ev:tl-rev:inducted"') &&
        h.includes('data-fold="g-ev:app-assoc:inducted"'),
        'and they are separate folds, keyed by the event that performed them');
  check(h.split('inducted (').length === 3,
        'the event that inducted nothing gets no fold at all');
  // Both goals are called *1. What tells them apart is whose card they are on.
  // Sliced between the two folds without assuming which comes first: the
  // events render in *source* order, which is the reverse of the order the
  // wire sends them in, and pinning the test to one of those tests the
  // ordering rather than the grouping.
  const tl = h.indexOf('g-ev:tl-rev:inducted');
  const aa = h.indexOf('g-ev:app-assoc:inducted');
  check(tl !== -1 && aa !== -1, 'both events drew their fold');
  const first = h.slice(Math.min(tl, aa), Math.max(tl, aa));
  const second = h.slice(Math.max(tl, aa));
  const [tlPart, aaPart] = tl < aa ? [first, second] : [second, first];
  check(tlPart.includes('(rev x)') && !tlPart.includes('(app x y)'),
        'tl-rev keeps its own suggesting term');
  check(aaPart.includes('(app x y)') && !aaPart.includes('(rev x)'),
        'and app-assoc keeps its own, though both goals are named *1');
  check(h.includes('(:induction true-listp)') && h.includes('(:induction rev)'),
        'the runes are runes, not ACL2’s English about them');
  check(h.includes('1 induction') === false && h.includes('2 inductions'),
        'the closed card still says an event inducted, and how often');
}

// ---- the event that FAILED shows its scheme too ----
//
// The regression this exists for: `failureHtml` never draws a card for the
// failing event -- it leads with the name and spreads the detail across its
// own sections -- so the fold on `eventHtml` covered every event except the
// one a reader opens the panel for. A bare failing `defthm` has exactly one
// event, and it is that one.
{
  globalThis.toks = {};
  const failed = {
    status: 'failed',
    checkpoints: [{ goal: 'Subgoal *1/2', term: '(implies (not (consp x)) (not x))' }],
    rules: ['(:definition d-app)'],
    abortCause: '(empty-clause)',
    inner: [
      { name: 'app-nil-is-identity', kind: 'defthm', abortCauses: '(empty-clause)',
        inductions: [
          { goal: '*1', suggested: '(d-app x nil)', runes: ['(:induction d-app)'],
            pFormula: '(:p x)', scheme: '(and (implies (not (consp x)) (:p x)))' },
        ] },
    ],
  };
  const h = envelope(failed, 'g-');
  check(h.includes('inducted (1)'),
        'a failing form shows what it inducted on');
  check(h.includes('(d-app x nil)') && h.includes('(:induction d-app)'),
        'with the suggesting term and the runes behind it');
  check(h.indexOf('Checkpoints (') < h.indexOf('Summary ('),
        'in the block directly under the checkpoints, whose *1 names it explains');
  check(h.indexOf('Summary (') < h.indexOf('inducted ('),
        'and leading that block, for the same reason');

  // A form that failed before the prover ran has no scheme and must not
  // grow an empty section for one.
  const rejected = {
    status: 'failed', checkpoints: [], errorLine: 'ACL2 Error [Translate]',
    inner: [{ name: 'n-bogus', kind: 'defthm' }],
  };
  check(!envelope(rejected, 'g-').includes('Induction ('),
        'and a form that never reached the prover grows no section for one');
}

// ---- hints and splits sit on the event that had them ----
//
// Read per event because ACL2 reports neither on the outer event of a
// compound form: a progn of hinted theorems reports no hint events at all, so
// a form-level list would be empty for most real code.
{
  globalThis.toks = {};
  const h = envelope({
    checkpoints: [], rules: [], inner: [
      { name: 'p-one', kind: 'defthm', hintEvents: ['(:use p-assoc)'] },
      { name: 'p-two', kind: 'defthm', hintEvents: ['(:by p-nil)'],
        splitterRules: ['(:rewrite p-split)'] },
      { name: 'p-three', kind: 'defthm' },
    ],
  }, 'g-');

  check(h.includes('supplied (1)'), 'an event shows what its proof was handed');
  check(h.includes('splits (1)'), 'and what made it branch');
  check(h.includes('data-fold="g-ev:p-one:supplied"') &&
        h.includes('data-fold="g-ev:p-two:supplied"'),
        'keyed by the event, so one event’s hints are not another’s');
  check(h.split('supplied (').length === 3 && h.split('splits (').length === 2,
        'and an event with neither grows no fold');
  // Each event's own: p-one was handed a :use, p-two a :by.
  const one = h.indexOf('g-ev:p-one:supplied');
  const two = h.indexOf('g-ev:p-two:supplied');
  const first = h.slice(Math.min(one, two), Math.max(one, two));
  const second = h.slice(Math.max(one, two));
  const [oneP, twoP] = one < two ? [first, second] : [second, first];
  check(oneP.includes('(:use p-assoc)') && !oneP.includes('(:by p-nil)'), 'p-one keeps its own');
  check(twoP.includes('(:by p-nil)'), 'and p-two keeps its own');
}

// ---- the failing form's Summary comes from the failing event ----
//
// The regression this exists for: the three sections it replaces read
// form-level fields, and `get-event-data` leaves those empty for every
// compound form -- so a failing `define` showed no rules, no hints and no
// splitters while its own event had all three.
{
  globalThis.toks = {};
  const summaryHtml = lift('esc', 'strip', 'spans', 'hl', 'folds', 'foldMemory', 'foldOrder',
                           'foldScope', 'FOLD_SCOPES', 'kindFolds', 'foldKindsAre',
                           'foldsForget', 'foldScopeIs', 'fold', 'factRow', 'fmtChars',
                           'inductionHtml', 'usedHtml', 'summaryHtml');
  const block = summaryHtml({
    supplied: ['(:use d-assoc)'],
    splits: ['(:rewrite d-split)'],
    inducted: [{ goal: '*1', suggested: '(d-app x nil)', runes: ['(:induction d-app)'],
                 pFormula: '(:p x)', scheme: '(and (:p x))' }],
    used: ['(:definition d-app)'],
  }, 'g-', 'g-');
  check(block.includes('Summary (4)'), 'the section counts every part');
  check(block.includes('inducted (1)') && block.includes('supplied (1)') &&
        block.includes('splits (1)') && block.includes('used (1)'),
        'and labels each one');
  // Each part is its own fold, so a reader can shut the long one and keep
  // the short ones.
  for (const k of ['inducted', 'supplied', 'splits', 'used']) {
    check(block.includes('data-fold="g-' + k + '"'), k + ' is separately collapsible');
  }
  // Order is not ACL2's: the long reference list goes last, so the parts a
  // reader came for are not pushed below a wall of runes.
  const at = (t) => block.indexOf(t);
  check(at('inducted (') < at('supplied (') && at('supplied (') < at('splits (') &&
        at('splits (') < at('used ('),
        'inducted, supplied, splits, then used');

  check(summaryHtml({}, 'g-', 'g-') === '',
        'and a proof with none of them grows no section');
}

// ---- a failing form's Summary comes from the failing event ----
//
// The regression this exists for: the sections it replaced read form-level
// fields, and `get-event-data` leaves those empty for every compound form --
// so a failing `define` showed no rules, no hints and no splitters while its
// own event had all three. `failureHtml` draws no card for that event, so this
// block is its card's Summary, flattened.
{
  globalThis.toks = {};
  const compound = envelope({
    status: 'failed',
    checkpoints: [{ goal: 'Subgoal *1/2', term: '(implies (not (consp x)) (not x))' }],
    rules: [], hintEvents: [], splitterRules: [],
    abortCause: '(empty-clause)',
    inner: [
      { name: 'd-thm', kind: 'defthm', abortCauses: '(empty-clause)',
        rules: ['(:definition d-app)'],
        hintEvents: ['(:use d-assoc)'],
        splitterRules: ['(:rewrite d-split)'] },
    ],
  }, 'g-');
  check(compound.includes('Summary (3)'),
        'the failing event supplies the block when the form level is empty');
  check(compound.includes('(:use d-assoc)') && compound.includes('(:definition d-app)') &&
        compound.includes('(:rewrite d-split)'),
        'all three of its parts, none of which the outer event knows');

  // A failure with nothing to attribute to -- a translate error names no
  // event -- still renders whatever the form level had.
  const bare = envelope({
    status: 'failed', checkpoints: [], errorLine: 'ACL2 Error [Translate]',
    rules: ['(:definition b-app)'], hintEvents: [], splitterRules: [],
    inner: [],
  }, 'g-');
  check(bare.includes('Summary (1)') && bare.includes('used (1)'),
        'with no event to attribute to, the form-level values still render');
}

// ---- a cut scheme says which cut, because there are two ----
//
// A scheme that simply stops reads as a scheme with no base case. The two
// cuts happen for different reasons and at different layers, so a reader who
// sees only one note draws the wrong conclusion about the other.
{
  globalThis.toks = {};
  const inductionHtml = lift('esc', 'strip', 'spans', 'hl', 'fmtChars', 'factRow',
                             'inductionHtml');
  const whole = inductionHtml([
    { goal: '*1', suggested: '(app x y)', pFormula: '(:p x)', scheme: '(and (:p x))' },
  ]);
  check(!whole.includes('case 1 of') && !whole.includes('of 6.8 MB'),
        'a scheme that arrived whole is not marked as anything else');

  const cut = inductionHtml([
    { goal: '*1', suggested: '(app x y)', pFormula: '(:p x)', scheme: '(and (:p x))',
      schemeCases: 47, schemeChars: 6831709 },
  ]);
  check(cut.includes('case 1 of 47'),
        'a clique scheme says it is one case of many');
  check(cut.includes('6.8 MB'),
        'and separately says how much text there really was');
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
  // Letting go of a pin is followed immediately by rendering whatever the
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

// ---- a pin follows its form, or lets go ----
{
  const { keepAnchor, pinnedIndex } = require('./out/policy.js');
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

  // Inside the form: the first character has not moved, so the pin holds --
  // this is the whole point, since editing the pinned form is what you do.
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

  // Resolving: a pin is good only where a form actually starts.
  const units = [
    { index: 0, line_start: 1, col_start: 1 },
    { index: 1, line_start: 3, col_start: 1 },
    { index: 2, line_start: 3, col_start: 20 },
  ];
  const at = (line, column) => ({ line, column, character: column - 1 });
  check(pinnedIndex(at(3, 20), units) === 2, 'the second form on a shared line is found by its column');
  check(pinnedIndex(at(3, 1), units) === 1, 'and the first by its own');
  check(pinnedIndex(at(3, 9), units) === undefined,
        'a position merely *inside* a form is not that form starting -- let go');
  check(pinnedIndex(at(4, 1), units) === undefined, 'and neither is a line with no form on it');
}

// ---- a clique is one event, and every function in it has a record ----
{
  const mem = (n) => ({ name: n, formals: '(x)', measure: '(acl2-count x)',
    symbolClass: ':common-lisp-compliant',
    runes: [{ rune: '(:definition ' + n + ')', class: ':definition',
              enabled: true, fields: [] }] });
  const clique = envelope({ checkpoints: [], rules: [], inner: [{
    name: 'zz-m1', kind: 'mutual-recursion', proverSteps: 220,
    rules: ['(:definition zp)'], detail: mem('zz-m1'),
    members: [mem('zz-m1'), mem('zz-m2')],
  }] }, 'p:');

  // ACL2 records the whole clique as one event keyed under its first
  // definition. Naming that one reads as though the form defined it alone,
  // and the others' rules appeared nowhere at all -- half the rules a
  // two-function clique writes were simply missing from the page.
  check(/zz-m1, zz-m2/.test(clique), 'the card names every function in the clique');
  check(/:definition zz-m2/.test(clique), "and the last one's rules are there too");
  check(/introduced \(1\)[\s\S]*introduced \(1\)/.test(clique),
    'each member introducing its own');

  // The termination proof is the clique's, not any member's: one argument
  // admits all of them, so it stays on the event above rather than being
  // copied under each.
  check(clique.split('(:definition zp)').length === 2,
    'the proof it used is said once, by the event');

  // The record filed under the event's own name is the first member's, and
  // that member has a row of its own now -- so the event must not also show
  // it as though it were the event's.
  check(clique.split('acl2-count').length === 3,
    'the first member is a row, not the card and a row both');
}

// ---- the sections fold too, not only what is inside them ----
{
  const m = { checkpoints: [], rules: [], inner: [
    { name: 'f', kind: 'defun', rules: [] },
  ], docs: [{ name: 't', short: 's' }] };
  const h = envelope(m, 'p:');
  // A form recording six hundred events is a page to scroll past before
  // reaching anything below it, and the count in the heading is often the
  // whole of what a reader wanted from it.
  for (const [id, what] of [['p:events', 'Events'], ['p:docs', 'Documented']]) {
    check(h.includes('data-fold="' + id + '"'), `the ${what} section folds`);
  }
  // Open to begin with: folding it is for getting past it, not for hiding it.
  check(/data-fold="p:events"[^>]*\bopen\b/.test(h), 'and starts open');
  // Keyed by what it is, not where it sits, so it survives a re-render -- the
  // rule every fold on this page follows.
  check(!/data-fold="p:events:\d/.test(h), 'and is keyed by what it holds');
}

// ---- an event with no name, and a clique with too many ----
{
  const one = (e) => envelope({ checkpoints: [], rules: [], inner: [e] }, 'p:');

  // A `thm` proves a goal and introduces nothing, so ACL2 records it under no
  // name. Its kind carries the line; an empty name chip would be the box that
  // marks a name, drawn around nothing. What it cost and the rules it used
  // are the whole of what it has to say, and they are worth saying.
  const thm = one({ name: '', kind: 'thm', proverSteps: 40,
                    rules: ['(:rewrite car-cons)'] });
  check(/>thm</.test(thm), 'a nameless event still says what kind it is');
  check(!/class="ename"/.test(thm), 'and draws no chip where the name would be');
  check(/40 steps/.test(thm) && /car-cons/.test(thm), 'and says what it cost and used');

  // And what it proved. A `defthm` gets that row from the world under its
  // name; a `thm` writes no name, so the form ACL2 recorded is the only place
  // its statement survives -- without it the card said a proof had run and
  // not what it was about.
  const goal = one({ name: '', kind: 'thm', rules: [],
                     goal: '(equal (zz-f (cons a b)) (zz-f b))' });
  check(/proved/.test(goal), 'a thm says what it proved');
  check(/zz-f \(cons a b\)/.test(goal), 'and that is the term it was given');

  // Only a thm. Every other event writes a name, and its statement is a fact
  // about that name rather than about the event.
  const df = one({ name: 'zz-t', kind: 'defthm', rules: [] });
  check(!/proved/.test(df), 'a named event has no such row');

  // Two of them in one form must not share a fold: keyed by name, they would
  // both key on the empty string and open and close together.
  const two = envelope({ checkpoints: [], rules: [], inner: [
    { name: '', kind: 'thm', rules: [] }, { name: '', kind: 'thm', rules: [] },
  ] }, 'p:');
  const ids = [...two.matchAll(/data-fold="([^"]*)"/g)].map((m) => m[1]);
  check(new Set(ids).size === ids.length, 'two nameless events get folds of their own');

  // A clique's title is its members -- but a dozen long names is not a title.
  // Capped, and the cap says how many it left out, because the rest are rows
  // of this very card rather than something dropped.
  const mem = (n) => ({ name: n, runes: [] });
  const few = one({ name: 'a', kind: 'mutual-recursion', rules: [],
                    members: ['a', 'b', 'c', 'd'].map(mem) });
  check(/a, b, c, d/.test(few), 'a short clique lists every name');
  const many = one({ name: 'a', kind: 'mutual-recursion', rules: [],
                     members: ['a', 'b', 'c', 'd', 'e', 'f'].map(mem) });
  check(/a, b, c/.test(many) && !/a, b, c, d/.test(many), 'a long one lists the first few');
  check(/\+3 more/.test(many), 'and says how many it did not');
  check(/>f</.test(many), 'the ones it did not name are still rows of the card');
}

// ---- an event that did not define the name shows none of its record ----
{
  const one = (kind) =>
    envelope({ checkpoints: [], rules: [], inner: [{
      name: 'foo', kind, proverSteps: 120, rules: ['(:rewrite natp-of-foo)'],
      detail: { name: 'foo', formals: '(x)', guard: '(natp x)',
                measure: '(acl2-count x)', short: 'does a thing',
                symbolClass: ':common-lisp-compliant',
                runes: [{ rune: '(:definition foo)', class: ':definition',
                          enabled: true, fields: [] }] },
    }] }, 'p:');

  // `verify-guards` proves an obligation about a function some *earlier*
  // event defined. The record under that name is the definition's, so every
  // one of these was the defun's card repeated under a heading that had not
  // done any of it.
  const vg = one('verify-guards');
  for (const [re, what] of [
    [/\(x\)/, 'the arguments'],
    [/does a thing/, 'the documentation'],
    [/natp x/, 'the guard'],
    [/acl2-count/, 'the measure'],
    [/introduced/, 'what the definition introduced'],
    [/1 rule/, 'and the count of it'],
    [/guards verified/, 'and the class it left the name in'],
  ]) {
    check(!re.test(vg), `a verify-guards card does not repeat ${what}`);
  }

  // What is left is what the event did: its own cost, and the rules its own
  // proof consumed.
  check(/120 steps/.test(vg), 'it keeps what the proof cost');
  check(/used \(1\)/.test(vg) && /natp-of-foo/.test(vg),
    'and the rules that proof used, which are its own');

  // The same detail on the event that *did* define the name is all shown --
  // this is about which event a record belongs to, not about hiding records.
  const df = one('defun');
  for (const re of [/\(x\)/, /does a thing/, /natp x/, /acl2-count/, /introduced/]) {
    check(re.test(df), 'a defun card still carries its own record');
  }
}

// ---- documentation lands on the name it belongs to ----
{
  // Through `envelope`, like every other rendering test here: it is the entry
  // the page actually calls, so this exercises the real path rather than a
  // function lifted out of the middle of it.
  const doc = (extra) =>
    envelope({ checkpoints: [], rules: [], inner: [], ...extra }, 'p:');

  // A name that carries a topic says so once, above its facts -- not in a
  // "documented" row *and* a fold underneath, which said the same sentence
  // twice on one card.
  const onEvent = doc({
    inner: [{ name: 'shown', kind: 'define',
              detail: { name: 'shown', short: 'counts the scopes',
                        guard: '(natp x)', runes: [] } }],
  });
  check(/counts the scopes/.test(onEvent), 'a documented name says what it is');
  check(onEvent.indexOf('counts the scopes') < onEvent.indexOf('guard'),
    'and it leads, being the only line written by a person');
  check(onEvent.split('counts the scopes').length === 2, 'and says it once');

  const quiet = doc({
    inner: [{ name: 'quiet', kind: 'defun',
              detail: { name: 'quiet', guard: '(natp x)', runes: [] } }],
  });
  check(!/dsum/.test(quiet), 'an undocumented name says nothing at all');

  // A topic belonging to no event of the form. A standalone defxdoc records no
  // event at all, so it has nothing above to attach to -- and a form view that
  // only ever decorated events would show it as empty.
  const lone = doc({ docs: [{ name: 'lone-topic', short: 'documented by itself' }] });
  check(/lone-topic/.test(lone), 'a form with no events still shows its topic');
  check(/documented by itself/.test(lone), 'with its summary');

  // A documented topic reads as a card, the shape the events use: the name
  // and its one-line summary on the face, the body inside.
  const card = doc({ docs: [{ name: 'topic', short: 'a summary',
                              long: '<p>the whole document</p>' }] });
  check(/<details[^>]*class="card"/.test(card), 'a documented topic is a card');
  check(/<summary>[\s\S]*topic[\s\S]*a summary[\s\S]*<\/summary>/.test(card),
    'with the name and the summary on its face');
  check(/the whole document/.test(card), 'and the body inside');
  check(!/<details[^>]*class="card"[^>]*\bopen\b/.test(card),
    'closed to begin with: a body is prose, and several open at once is a wall');

  // No body, no chevron -- the rule the events already follow. A `defxdoc`
  // with a short and no long is an ordinary thing to write.
  const bare = doc({ docs: [{ name: 'terse', short: 'all there is' }] });
  check(!/<details[^>]*class="card"/.test(bare), 'a topic with no body gets no fold');
  check(/all there is/.test(bare), 'and still says what it is');

  // A documented name that *is* an event keeps its body under its own card,
  // rather than only in the section for topics belonging to no event.
  const onCard = doc({
    inner: [{ name: 'shown', kind: 'define',
              detail: { name: 'shown', short: 'counts', long: '<p>at length</p>',
                        runes: [] } }],
  });
  // The same face the standalone card has: a label beside the chevron, the
  // short on its own line beneath, and both of them in the *summary* -- so
  // the sentence reads without opening anything, and the chevron still has a
  // word beside it rather than pointing at a blank line.
  check(/<summary><span class="meta">documentation<\/span><div class="dsum">counts/
    .test(onCard), 'the label is beside the chevron and the short below it');
  check(/counts[\s\S]*<\/summary>/.test(onCard), 'the short is readable collapsed');
  check(/at length/.test(onCard), 'and the body is there to open');
  check(/class="card"/.test(onCard), 'the whole of it is boxed');
  // Once as a heading, and that is the event card's own. Fold ids carry it
  // too, which is not the reader seeing it twice.
  check(onCard.split('class=\"ename\"').length === 2,
    'and the name is written once, by the card it sits in');

  // ...and it comes last when the form did do something as well.
  const both = doc({
    inner: [{ name: 'ev', kind: 'defun' }],
    docs: [{ name: 'lone-topic', short: 'documented by itself' }],
  });
  check(both.indexOf('lone-topic') > both.indexOf('ev'),
    'documentation comes after what the form did');

  // Code lifted out of a topic is put back coloured, from the same token map
  // every other term on this page is coloured from -- a term is a term
  // wherever it is written.
  const held = doc({ docs: [{ name: 'x', short: 'a summary',
    long: 'see <pre data-code="0">(f x)</pre>',
    longCode: [{ slot: 0, text: '(f x)' }] }] });
  check(!/data-code/.test(held), 'a filled hole keeps none of its scaffolding');
  check(/\(f x\)/.test(held), 'and the code is there');

  // A hole with nothing to fill it keeps the escaped source the renderer left
  // inside it, so it reads rather than coming up blank.
  const unfilled = doc({ docs: [{ name: 'x', short: 's',
    long: 'see <pre data-code="7">(g y)</pre>' }] });
  check(/\(g y\)/.test(unfilled), 'an unfilled hole still shows its source');

  // Nothing documented, nothing said: not an empty heading.
  check(!/Documented/.test(doc({ inner: [{ name: 'ev' }], docs: [] })),
    'no topics, no section');

  // A short arrives as *rendered* HTML: xdoc markup is turned into a page
  // fragment by the host, in the same pass the doc view uses, because 59% of
  // real shorts carry markup and raw ones read as `@(tsee foo)` mid-sentence.
  // So the page inserts rather than escapes -- and what it inserts has to
  // survive.
  const rendered = doc({ docs: [{ name: 'x', short: 'about <code>foo</code>' }] });
  check(/<code>foo<\/code>/.test(rendered), 'rendered markup reaches the page');
  check(!/&lt;code&gt;/.test(rendered), 'and is not escaped a second time');
}

// ---- a failure the extension can name, it can also offer to fix ----
{
  const { readFileSync } = require('fs');
  const src = readFileSync(new URL('./src/extension.ts', import.meta.url), 'utf8');
  const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));

  // Three ways to fail before a session exists, and each used to hand back a
  // sentence with a setting's name in it. The worst was the middle one: with
  // no image configured the daemon *starts* and then refuses, so the reader
  // was told `daemon exited` -- the symptom -- while the cause and the fix,
  // "no ACL2 to run", sat in an output channel nobody had been told to open.
  const spawn = src.slice(src.indexOf("c.on('spawn-error'"));
  check(/act\(/.test(spawn.slice(0, 300)), 'a daemon that will not spawn offers actions');
  check(/Set Path/.test(spawn.slice(0, 300)) && /Install Steps/.test(spawn.slice(0, 300)),
    'and they are the two that fix it');

  const exit = src.slice(src.indexOf("c.on('exit'"));
  const body = exit.slice(0, exit.indexOf("c.on('state'"));
  check(/dying/.test(body), "a daemon that dies before any session says what it said");
  check(/Set Image/.test(body), 'and offers the setting when the image is what it wanted');
  check(/Show Log/.test(body), 'with the log a click away either way');

  // One notification per death: a spawn failure ends in an exit too, and being
  // told twice about one event is the thing the guidance calls out by name.
  check(/told = true/.test(spawn.slice(0, 200)) && /if \(told\)/.test(body),
    'and a spawn failure is not announced twice on its way out');

  // The steps come from the copy that shipped with this extension, not a URL:
  // they are version-specific, they must work with no network, and a link
  // rots when files move.
  const steps = src.slice(src.indexOf('function showInstallSteps'));
  check(/README\.md/.test(steps.slice(0, 400)), 'the install steps are the bundled README');
  check(!/https?:\/\//.test(steps.slice(0, 400)), 'and not a link to one');

  // Both settings the actions open have to exist, or the button opens Settings
  // filtered to nothing.
  for (const id of ['acl2-session.daemonPath', 'acl2-session.defaultImage']) {
    check(id in pkg.contributes.configuration.properties, `${id} is a real setting`);
  }
}

// ---- one writer for the status bar ----
{
  const { readFileSync } = require('fs');
  const src = readFileSync(new URL('./src/extension.ts', import.meta.url), 'utf8');
  const writes = (prop) =>
    (src.match(new RegExp(`status\\.${prop}\\s*=`, 'g')) || []).length;

  // The bar is one widget shared by every book and by VSCode itself, so who
  // may write it has to be decided in one place. It was not: a certification's
  // clock set the text, tooltip *and* command straight onto the widget from a
  // timer, so with two books live and one certifying the bar flickered between
  // the other's counts and the clock twice a second -- and for up to half a
  // second the interrupt command sat under a tooltip offering something else.
  //
  // `setStatus` is that place, and these counts are how a second writer gets
  // noticed. One text write, and one each of tooltip and command beyond the
  // defaults set at activation before any book exists.
  check(writes('text') === 1, `only setStatus writes status.text (found ${writes('text')})`);
  check(writes('tooltip') === 2, `and status.tooltip (found ${writes('tooltip')})`);
  check(writes('command') === 2, `and status.command (found ${writes('command')})`);

  // The command travels with the text, because it belongs to what the bar is
  // showing: a finished certification must not leave its interrupt behind.
  const set = src.slice(src.indexOf('function setStatus'));
  const body = set.slice(0, set.indexOf('\nfunction '));
  check(/if \(command !== undefined\)/.test(body),
    'setStatus sets the command it was given');
  check(!/s\.certifying/.test(body),
    'and nothing outranks the book in front, a certification included');
}

// ---- every fold indent comes from one number ----
{
  const { readFileSync } = require('fs');
  const css = readFileSync(new URL('./src/webviewParts.ts', import.meta.url), 'utf8');
  // The selector followed by its own brace: several of these names also
  // appear inside *other* selectors (`.card.bare .csum code,` styles code),
  // and matching the first occurrence reads the wrong rule.
  const rule = (sel) => {
    const at = css.indexOf(sel + ' {');
    return at === -1 ? null : css.slice(at, css.indexOf('}', at) + 1);
  };

  // A chevron sits in a slot; everything the fold contains lines up with the
  // *label* beside that slot, not with the slot. Four places need the same
  // number to agree on that, and they are only correct together -- which is
  // exactly the kind of thing that drifts. It had: the slot was 14px and the
  // body's indent 20px, so a card's title and its contents missed by two
  // pixels. Two is the worst possible amount: too small to read as
  // indentation, too large to read as aligned.
  check(/:root\s*\{\s*--twist:/.test(css), 'the indent is a single declared number');
  // And a *relative* one. The slot holds a glyph, the glyph is sized by the
  // font, and every indent on the page derives from this: fixed in pixels it
  // would keep its width while the text around it grew.
  check(/--twist:\s*[\d.]+rem/.test(css), 'and it scales with the text');
  // The text scales because one number does, on the root. Anything written
  // in pixels here would sit still while the rest of the page moved.
  check(/html\s*\{[^}]*font-size:\s*var\(--vscode-font-size/.test(css),
    'and the text follows the size the reader configured');
  // Two settings, because there are two kinds of text here. Labels and prose
  // are workbench text and take the workbench size; terms are the buffer's
  // own -- a statement, a rune, a checkpoint -- and take the editor's, as
  // they already take its family. Anchored to the workbench alone they came
  // out smaller than the buffer they came from, which is the default setting.
  const codeRule = rule('pre, code');
  check(codeRule !== null && /var\(--vscode-editor-font-family/.test(codeRule),
    'terms are set in the editor\u2019s font');
  check(codeRule !== null && /font-size:\s*var\(--vscode-editor-font-size/.test(codeRule),
    'and at the editor\u2019s size, so they match the buffer they came from');
  // Every stylesheet, not only the shared one: the console and the doc view
  // add rules of their own on top of it, and the console's had the same fault
  // this set out to fix -- a code block pinned at 12px while the buffer it
  // echoes followed the reader's setting.
  for (const f of ['webviewParts', 'console', 'docView']) {
    const sheet = readFileSync(new URL(`./src/${f}.ts`, import.meta.url), 'utf8');
    const sizes = [...sheet.matchAll(/font-size:\s*([^;}]+)[;}]/g)].map((m) => m[1].trim());
    const px = sizes.filter((v) => /^\d+px$/.test(v));
    check(px.length === 0, `${f} writes no size in pixels (${px})`);
  }

  // The terms take all three of the editor's font settings, because they are
  // the buffer's own text and a reader compares them against it.
  const codeRule2 = rule('pre, code');
  check(codeRule2 !== null && /font-weight:\s*var\(--vscode-editor-font-weight/.test(codeRule2),
    'and the editor\u2019s weight, so a light or bold editor font is matched too');
  // The one place a var has no fallback is a var that has to resolve anyway;
  // everywhere else names what to do without it.
  const bodyRule = rule('body');
  check(bodyRule !== null && /var\(--vscode-font-family,\s*[a-z-]+\)/.test(bodyRule),
    'the workbench font names a fallback, as the editor font already did');
  for (const [sel, what] of [
    ['details[data-fold] > summary::before', "the chevron's slot"],
    ['.cinside', "a card's body"],
    ['.efacts', "the facts line inside a summary"],
    ['.dsum', "a topic's summary inside one"],
        ['.card.bare > .csum', "a card with no chevron at all"],
    ['details[data-fold]:not(.card) > *:not(summary):not(.card)',
     "a nested fold's body"],
  ]) {
    const r = rule(sel);
    check(r !== null, `${what} has a rule`);
    check(r !== null && /var\(--twist\)/.test(r),
      `${what} is derived from it, not written out`);
  }

  // A card is not indented by the fold that holds it. The indent says what a
  // fold contains; a card says that with its outline, and saying it twice
  // spends a chevron's width of a narrow pane on nothing -- which the terms
  // inside the card would rather have.
  const nested = rule('details[data-fold]:not(.card) > *:not(summary):not(.card)');
  check(nested !== null && /var\(--twist\)/.test(nested),
    'a fold indents what it holds, except a card, which is delimited already');

  // The other half of the same number: where there is *no* chevron the slot
  // is reserved by the container's padding, so a block inside must not indent
  // again. A bare event's facts line did, putting it a slot right of the name
  // it is about -- 8+14 from the box and 14 more of its own, against a name
  // at 8+14. Both rules reference `--twist`, so the check above passed while
  // the sum was wrong; only the pairing catches it.
  for (const [box, block, what] of [
    ['.card.bare > .csum', '.card.bare > .csum > .efacts', "a bare card's facts"],
    ['.card.bare > .csum', '.card.bare > .csum > .dsum',
     "a bare documentation card's short"],
  ]) {
    const outer = rule(box);
    check(outer !== null && /calc\(8px \+ var\(--twist\)\)/.test(outer),
      `${what} sits in a box that already reserves the slot`);
    const inner = rule(block);
    check(inner !== null && /padding-left:\s*0/.test(inner),
      `${what} does not then indent by it a second time`);
  }

  // Vertically, the chevron must not be baseline-aligned. The codicon font is
  // unitsPerEm 300 with ascender 300 and descender 0 -- its baseline is the
  // bottom of the em box -- so a baseline-aligned glyph puts its ink centre
  // 0.498em above the text baseline, where lowercase text's optical centre is
  // nearer half an x-height. That is about three pixels of ride at this size,
  // which reads as the chevron sitting high beside its own word.
  const chev = rule('details[data-fold] > summary::before');
  check(/vertical-align:\s*middle/.test(chev),
    'the chevron is centred on the text, not sat on its baseline');
  check(!/vertical-align:\s*baseline/.test(chev),
    'and not left at the icon font\u2019s own default');

  // The two that must be equal rather than merely related: a fold's body and
  // the label above it. Both are the summary's padding plus one slot.
  const pad = (sel) => (rule(sel) || '').match(/calc\(([^)]*\))?[^)]*\)/)?.[0];
  check(pad('.einside') === pad('.event.bare .esum'),
    "a fold's body and a chevronless event start at the same column");
}

// ---- one editor action, in every state the editor can be in ----
{
  const { readFileSync } = require('fs');
  const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));

  // VSCode's UX guidance asks for at most one action in the editor title bar.
  // Ours is play/pause/resume: one slot, three commands, and the `when`
  // clauses are the only thing keeping two of them from showing at once. That
  // is not a property anyone can see by reading three separate entries, and it
  // breaks by *addition* -- someone puts a fourth useful button in
  // `navigation` and nothing complains.
  //
  // So enumerate the states. `editorLangId == acl2` is a given here; the rest
  // of the vocabulary is two booleans.
  const truth = (when, active, paused) => {
    const src = when
      .replace(/editorLangId == acl2/g, 'true')
      .replace(/acl2Session\.active/g, String(active))
      .replace(/acl2Session\.paused/g, String(paused));
    return Function(`"use strict"; return (${src});`)();
  };

  // Only the editor's own toolbar: the form view's pin and unpin live on the
  // webview panel's title bar, which is a different bar with its own slot.
  const editorBar = pkg.contributes.menus['editor/title'].filter((m) =>
    /editorLangId == acl2/.test(m.when)
  );
  const nav = editorBar.filter((m) => (m.group ?? '').startsWith('navigation'));

  for (const active of [false, true]) {
    for (const paused of [false, true]) {
      const shown = nav.filter((m) => truth(m.when, active, paused));
      check(shown.length <= 1,
        `at most one editor action with active=${active} paused=${paused}` +
        ` (got ${shown.map((m) => m.command).join(', ') || 'none'})`);
    }
  }
  // And exactly one whenever there is anything to say, or the toolbar goes
  // empty and the feature loses its only visible affordance.
  for (const [active, paused] of [[false, false], [true, false], [true, true]]) {
    check(nav.filter((m) => truth(m.when, active, paused)).length === 1,
      `exactly one editor action with active=${active} paused=${paused}`);
  }

  // The rest are reachable, just not as icons: a non-navigation group is what
  // puts them in the overflow menu rather than the title bar.
  const overflow = editorBar.filter((m) => !(m.group ?? '').startsWith('navigation'));
  check(overflow.some((m) => m.command === 'acl2-session.stop'),
    'stop is in the overflow menu rather than the title bar');
  check(overflow.some((m) => m.command === 'acl2-session.certify'),
    'and so is certify, which used to be a second icon');

  // Every command the toolbar names has to exist, and carry an icon -- a
  // navigation entry without one renders as its title, which is a sentence.
  const byId = new Map(pkg.contributes.commands.map((c) => [c.command, c]));
  for (const m of nav) {
    check(byId.has(m.command), `${m.command} is a contributed command`);
    check(!!byId.get(m.command)?.icon, `${m.command} has an icon to draw`);
  }

  // One category, declared as one. VSCode composes "ACL2: Title" from the
  // `category` field itself and groups the palette by it; spelling the prefix
  // into every title produces the same line and none of the grouping, which
  // is what these used to do.
  for (const c of pkg.contributes.commands) {
    check(c.category === 'ACL2', `${c.command} is in the ACL2 category`);
    check(!/^ACL2\s*:/.test(c.title),
      `${c.command}'s title does not repeat the category`);
  }
}

// ---- one switch for code inside a topic, and both views obey it ----
{
  const { readFileSync } = require('fs');
  const ext = readFileSync(new URL('./src/extension.ts', import.meta.url), 'utf8');
  const fv = readFileSync(new URL('./src/formView.ts', import.meta.url), 'utf8');
  const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));
  const key = 'acl2-session.highlightDocCode';

  const decl = pkg.contributes.configuration.properties[key];
  check(!!decl, 'the setting is declared');
  check(decl && decl.default === true,
    'and on by default: a book\u2019s code blocks are its own forms nearly always');

  // The failure this guards, which the pin already made once: two views show
  // the same thing, and only one of them consults the setting that governs
  // it. The form view colours from a token map it builds; the doc view
  // colours slots it fills itself. Neither reads the other's code.
  check(fv.includes('highlightDocCode'), 'the form view reads it');
  check(ext.includes("get<boolean>('highlightDocCode'"), 'and so does the doc view');

  // Changing it has to repaint what is already on screen. Both views hold
  // their content, so neither needs the daemon -- but both need telling.
  const at = ext.indexOf(`affectsConfiguration('${key}')`);
  check(at !== -1, 'a change is noticed');
  const arm = ext.slice(at, ext.indexOf('\n      }', at));
  check(/formView\.refresh\(\)/.test(arm), 'and repaints the form view');
  check(/paintDocs\(\)/.test(arm), 'and the documentation view');
}

// ---- the pin governs both views, and both can take it ----
{
  const { readFileSync } = require('fs');
  const src = readFileSync(new URL('./src/extension.ts', import.meta.url), 'utf8');
  const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'));

  // The bug this exists for: the pin was consulted by one of the two
  // cursor-following views. Both branches sit in the same selection handler,
  // and only the form view's checked it -- so a pinned session showed one
  // form's checkpoints beside another form's log, silently.
  const handler = src.slice(src.indexOf('onDidChangeTextEditorSelection'));
  const follows = handler.slice(0, handler.indexOf('}),'));
  for (const call of ['followOutputView', 'followFormView']) {
    const at = follows.indexOf(call);
    check(at !== -1, `the selection handler still drives ${call}`);
    // The guard is on the line above the call, within the same `if`.
    const guard = follows.slice(Math.max(0, at - 200), at);
    check(/pinned && pinned\.book === s\.id/.test(guard),
      `${call} is skipped while pinned`);
  }

  // Both views offer the action, because either may be the one in front. The
  // output document is gated by *scheme* and not language: the eval view is
  // the same class with the same language, and the language is `plaintext`
  // whenever `highlightOutput` is off -- so a language gate would put the
  // button on a view that follows nothing, and lose it on a setting.
  const bar = pkg.contributes.menus['editor/title'];
  for (const cmd of ['acl2-session.pinFormView', 'acl2-session.unpinFormView']) {
    const on = bar.filter((m) => m.command === cmd);
    // The form view is a webview panel, so its gate is the panel id rather
    // than a language; the output view is a document, so its gate is a scheme.
    check(on.some((m) => /activeWebviewPanelId == 'acl2SessionFormView'/.test(m.when)),
      `${cmd} is offered on the form view`);
    const out = on.filter((m) => /acl2-session-output/.test(m.when));
    check(out.length === 1, `${cmd} is offered on the output document`);
    check(out.every((m) => /resourceScheme/.test(m.when) && !/editorLangId/.test(m.when)),
      `${cmd} gates the output document by scheme, not language`);
  }

  // The icon shows the state the click *lands you in*, not the state you are
  // in: the filled pin pins, the outline pin unpins. Which is the same
  // convention as the play/pause button on the book -- the triangle both means
  // "resume" and depicts where resuming leaves you -- so the two buttons agree
  // even though they never appear together.
  //
  // Worth pinning because the other reading is so tempting: an outline pin
  // shown *while pinned* looks like a state indicator that is simply wrong.
  // It is not a state indicator.
  const icon = (c) => pkg.contributes.commands.find((x) => x.command === c)?.icon;
  check(icon('acl2-session.pinFormView') === '$(pinned)',
    'the pinning action shows the pinned state it produces');
  check(icon('acl2-session.unpinFormView') === '$(pin)',
    'and the unpinning action shows the unpinned state it produces');
  // Both are 2020-era codicons. `$(unpin)` would be the honest glyph for the
  // action but was added in 2026, far past this extension's `engines`, and an
  // unrecognised codicon renders as nothing rather than falling back.
  check(!/\$\(unpin\)/.test(JSON.stringify(pkg.contributes.commands)),
    'and neither uses $(unpin), which is newer than the engine we declare');

  // The four session controls are all one family, and that is a colour
  // decision as much as a shape one. VSCode colours certain codicons globally
  // -- `debugColors.ts` adds a rule on `.monaco-workbench .codicon-debug-pause`
  // and friends -- so the colour arrives with the *name*, anywhere in the
  // workbench, including an editor title bar that has nothing to do with
  // debugging.
  //
  // Before this, two of the four were `debug-*` and two were not, so stop was
  // red and pause was blue while start and resume were the default foreground
  // -- a tint on half the set, for a reason invisible to anyone reading the
  // manifest. Whole family or none; mixed is the only wrong answer.
  const control = {
    'acl2-session.start': '$(debug-start)',
    'acl2-session.pause': '$(debug-pause)',
    'acl2-session.resume': '$(debug-continue)',
    'acl2-session.stop': '$(debug-stop)',
  };
  for (const [cmd, want] of Object.entries(control)) {
    check(icon(cmd) === want, `${cmd} uses ${want}`);
  }
  check(Object.values(control).every((i) => /^\$\(debug-/.test(i)),
    'and every session control is in the debug family, so all four are coloured');

  // One word for it, everywhere a reader sees one.
  for (const c of pkg.contributes.commands) {
    check(!/\b(lock|hold|held)\b/i.test(c.title),
      `${c.command}'s title says pin, not lock or hold (${c.title})`);
  }
}

// ---- no command handler may take an argument it did not ask for ----
{
  const { readFileSync } = require('fs');
  const src = readFileSync(new URL('./src/extension.ts', import.meta.url), 'utf8');

  // VSCode hands an `editor/title` command the editor's resource URI as its
  // first argument, and a palette invocation hands it nothing. A handler
  // registered bare therefore receives a parameter it never asked for, and
  // TypeScript cannot see it: `registerCommand` takes `(...args: any[])`, so
  // every arity type-checks.
  //
  // That is not hypothetical. `acl2-session.start` was registered bare and
  // then grew an optional "start held at this barrier" parameter. The URI
  // landed in it, had no `anchor`, and read as the barrier before the first
  // form -- so the title-bar play button started a session that checked
  // nothing, while the keybinding for the same command was fine.
  const bare = [...src.matchAll(/registerCommand\(\s*'([^']+)'\s*,\s*([A-Za-z_$][\w$]*)\s*\)/g)];
  for (const [, command, handler] of bare) {
    const decl = new RegExp(`function ${handler}\\s*\\(([^)]*)\\)`).exec(src);
    check(decl !== null, `${command}'s handler ${handler} is declared in this file`);
    check(decl !== null && decl[1].trim() === '',
      `${command} is registered bare, so ${handler} must take no arguments`);
  }
}

// ---- certifications run one at a time ----
//
// cert.pl already parallelises with -j, and two builds whose graphs share a
// library race on the same .cert files -- so the serializer must never let two
// jobs overlap, must run them in order, and must not wedge on a failure.
{
  const { makeSerializer } = require('./out/policy.js');
  const run = makeSerializer();
  const log = [];
  const job = (id, ms, fail) => () =>
    new Promise((res, rej) => {
      log.push('start ' + id);
      setTimeout(() => {
        log.push('end ' + id);
        fail ? rej(new Error(id)) : res(id);
      }, ms);
    });
  // Queue three at once; the first is the slowest. If they overlapped, a
  // later "start" would appear before the earlier "end".
  const a = run(job('a', 20));
  const b = run(job('b', 1, true)); // b rejects
  const c = run(job('c', 1));
  const settled = await Promise.allSettled([a, b, c]);
  check(log.join(',') === 'start a,end a,start b,end b,start c,end c',
    `serialized, in order, no overlap (got ${log.join(',')})`);
  check(settled[0].value === 'a' && settled[2].value === 'c',
    'each caller gets its own job’s result');
  check(settled[1].status === 'rejected',
    'a failing job rejects to its caller');
  check(log.includes('start c'),
    'and does not wedge the chain: c ran after b failed');
}

// ---- a Problems row names what is wrong, tersely, from the status alone ----
{
  const { diagnosticMessage, warningMessage } = require('./out/policy.js');
  // No "ACL2" and no "Error/Warning" in the text: the source column and the
  // severity icon carry both, so leading with them stutters. The failure is
  // that the event failed; skip-proofs is only how checking carried on.
  check(diagnosticMessage('skipped') === 'Event failed; proofs skipped',
    'a skipped form led with the failure, not the skip');
  check(diagnosticMessage('failed') === 'Event failed', 'a hard failure is terse');
  check(diagnosticMessage('conditional') === 'Conditional on a skipped proof',
    'and a conditional form says what it rests on');
  for (const m of ['skipped', 'failed', 'conditional'].map(diagnosticMessage)) {
    check(!/ACL2|Error|Warning/.test(m), `no prefix stutter in "${m}"`);
  }
  // Warnings: a phrase per known kind, ACL2's own bracket label otherwise.
  check(warningMessage('Uncertified') === 'Includes an uncertified book',
    'uncertified names the included book, not just the word');
  check(warningMessage('Double-rewrite') === 'A rule variable may need double-rewrite',
    'double-rewrite says what to consider');
  check(warningMessage('Theory') === 'Theory expression may have no effect',
    'theory is elaborated');
  check(warningMessage('Skip-proofs') === 'Includes skipped proofs',
    'skip-proofs, the sibling of uncertified, is elaborated too');
  check(warningMessage('Non-rec').startsWith('Rule unlikely to fire'),
    'non-rec says why the rule may never trigger');
  check(warningMessage('Use') === 'Unusual to :use an enabled rule; consider disabling it',
    'use says what is unusual about it, not just the word "Use"');
  check(warningMessage('Guards') === 'Includes a book with guards unverified',
    'guards is about the book it came in with, like uncertified');
  // The line has to be true of *every* instance, which is the test for
  // whether a kind gets one at all. `Equiv` is emitted about the particular
  // rule it names, so no single line covers it: it stays as its label.
  check(warningMessage('Equiv') === 'Equiv',
    'a kind whose point is the rule it names stays a label');
  check(warningMessage('Free') === 'Free variables limit where this rule applies',
    'free says what the free variables cost');
  check(warningMessage('Subsume') === 'Probably subsumes an existing rule',
    'subsume names the overlap');
  check(warningMessage('Ignored-variables') === 'Unused formal variable',
    'ignored-variables is plain English');
  check(warningMessage('Irrelevant-formals') === 'Irrelevant formal variable',
    'and so is irrelevant-formals');
  check(warningMessage('Equiv') === 'Equiv',
    'a kind whose meaning varies falls through to ACL2’s own label');
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

// A form view that renders the same form again must not re-post it.
//
// `rules` and `induction` have always guarded their posts; `render` did not,
// and it is the expensive one -- it carries the model, the narrative and the
// rules records in one message and tokenizes all of them together. `onState`
// follows the form view on every state event, and a form landing produces a
// burst of them describing a form that has not changed, so the guard is the
// difference between one whole-page render and fifteen.
{
  const { FormView } = require('./out/formView.js');
  const fv = new FormView();
  let posts = 0;
  // `panel` is private to TypeScript only; at runtime this is the seam that
  // lets the count be taken without an editor.
  fv.panel = { webview: { postMessage: () => { posts++; } } };
  // As if the page had said so. Without it every post below would be held,
  // which is the subject of the block after this one.
  fv.ready = true;

  const model = () => ({
    book: 's1', index: 69, total: 77,
    lineStart: 2775, lineEnd: 7748, colStart: 1, colEnd: 2,
    status: 'admitted', skipped: false,
    checkpoints: [{ term: '(implies (natp x) (natp (foo x)))' }],
    rules: ['(:definition foo)', '(:rewrite bar)'],
  });

  fv.render(model());
  check(posts === 1, 'the first render of a form posts it');

  fv.render(model());
  check(posts === 1, 'an identical model, freshly built, does not post again');

  fv.render(model());
  fv.render(model());
  check(posts === 1, 'nor does a burst of them -- this is the state-event case');

  const changed = model();
  changed.status = 'conditional';
  fv.render(changed);
  check(posts === 2, 'a model that differs anywhere does post');

  // The unconditional callers have to stay unconditional: a theme change
  // re-sends the same model precisely because the page must redraw it.
  fv.refresh();
  check(posts === 3, 'refresh re-posts the current model regardless');

  fv.render(changed);
  check(posts === 3, 'and the guard still holds after it');
}

// ---- and nothing is sent into a page that is not listening yet ----
//
// A webview's script runs asynchronously after its HTML is set, so a message
// posted before it registers its listener is dropped with no error anywhere.
// The form view posted straight into the fresh panel *and recorded what it
// sent*, so the dedup above then suppressed every identical re-post: check a
// book, close the form-view tab, click the status bar, and the view sat on
// its placeholder until the cursor moved to a different form.
{
  const { FormView } = require('./out/formView.js');
  const fv = new FormView();
  const sent = [];
  fv.panel = { webview: { postMessage: (m) => { sent.push(m); } } };

  const model = {
    book: 's1', index: 0, total: 1,
    lineStart: 1, lineEnd: 1, colStart: 1, colEnd: 2,
    status: 'admitted', skipped: false, checkpoints: [], rules: [],
  };

  fv.render(model);
  fv.note('held too');
  check(sent.length === 0, 'nothing reaches a page that has not said it is listening');

  // What `open`'s `ready` branch does when the page finally speaks.
  fv.ready = true;
  const held = fv.queued;
  fv.queued = [];
  for (const q of held) fv.panel.webview.postMessage(q);
  check(sent.length === 2, 'and everything held arrives once it has, in order');
  check(sent[0].kind === 'model' && sent[1].kind === 'note', 'in the order it was queued');

  // The dedup must not outlive the page it was about. A new panel is a fresh
  // page that has been told nothing.
  fv.render(model);
  check(sent.length === 2, 'the guard still holds while this page is up');
  fv.postedModel = undefined;
  fv.render(model);
  check(sent.length === 3, 'and lets go when the panel does');
}

if (bad) {
  console.error(`webview: ${bad} failing`);
  process.exit(1);
}
console.log('webview: all green');
