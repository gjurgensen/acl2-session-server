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
  const { fold, foldScopeIs, bindFolds } =
    liftAll(['esc', 'folds', 'foldScope', 'foldScopeIs', 'fold', 'bindFolds'],
            '({ fold, foldScopeIs, bindFolds })');

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

  const details = (id, open) => ({ tagName: 'DETAILS', dataset: { fold: id }, open });
  const open = (id, byDefault) => fold(id, byDefault, 'S', 'B').includes(' open>');

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
  check(open('c', true), 'while moving to another form forgets it');

  check(fold('4:event:my-app:introduced', true, 'S', 'B')
          .includes('data-fold="4:event:my-app:introduced"'),
    'the id is carried on the element, so the next render can find it again');
  check(fold('a&b', true, 'S', 'B').includes('data-fold="a&amp;b"'),
    'and escaped, since an ACL2 name is not HTML');

  // Neither of these is a fold; neither should be recorded or throw.
  toggled({ target: { tagName: 'DETAILS', dataset: {} } });
  toggled({ target: { tagName: 'DIV', dataset: { fold: 'a' } } });
  check(open('a', true), 'a details with no id, and a non-details, are both ignored');
}

// ---- `envelope`: what a form left behind, per event ----
//
// A productive form is its events, each carrying the facts the buffer does
// not have. Rendered from the shape the daemon actually sends: a
// guard-verified function with derived rules, one of them disabled, and a
// theorem whose left-hand side is written nowhere in the source.

globalThis.toks = {};
const envelope = lift('esc', 'strip', 'causes', 'spans', 'hl', 'rat',
                      'fmtSecs', 'timeParts', 'proveSecs', 'fmtTime', 'guardNote',
                      'folds', 'foldScope', 'foldScopeIs', 'fold',
                      'factRow', 'nameFacts', 'introducedHtml', 'usedHtml',
                      'eventHtml', 'productiveHtml', 'failureHtml', 'warningsHtml',
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
  check(at('Checkpoints') < at('Rules ('), 'the goals come before the rules');
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
  check(render(3, 4).includes(' open>'), 'a handful of events open by default');
  check(!render(6, 4).includes(' open>'),
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

// ---- every fold indent comes from one number ----
{
  const { readFileSync } = require('fs');
  const css = readFileSync(new URL('./src/webviewParts.ts', import.meta.url), 'utf8');
  // The selector followed by its own brace: several of these names also
  // appear inside *other* selectors (`.event.bare .esum code,` styles code),
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
  for (const [sel, what] of [
    ['details[data-fold] > summary::before', "the chevron's slot"],
    ['.einside', "a fold's body"],
    ['.efacts', "the facts line inside a summary"],
    ['.event.bare .esum', "an event with no chevron at all"],
    ['details[data-fold]:not(.event) > *:not(summary)', "a nested fold's body"],
  ]) {
    const r = rule(sel);
    check(r !== null, `${what} has a rule`);
    check(r !== null && /var\(--twist\)/.test(r),
      `${what} is derived from it, not written out`);
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

if (bad) {
  console.error(`webview: ${bad} failing`);
  process.exit(1);
}
console.log('webview: all green');
