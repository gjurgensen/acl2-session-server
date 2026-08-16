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

if (bad) {
  console.error(`webview: ${bad} failing`);
  process.exit(1);
}
console.log('webview: all green');
