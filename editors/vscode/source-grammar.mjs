// The ACL2 grammar, against the reader's awkward corners.
//
// A TextMate grammar carries its rule stack from line to line, so a construct
// it fails to recognise does not cost one token -- it costs the rest of the
// file. Every case here is a real one from
// `books/kestrel/acl2pl/reader-tests.lisp`, which exists to exercise ACL2's
// reader and therefore exercises this too: it opens a symbol with a quote in
// it, a string that is not delimited by quotes, and a symbol whose every
// character is backslash-escaped. Each used to swallow everything after it.
//
// The assertion is always the same shape, and it is the one that matters: the
// *next form* is still code.
//
//   node source-grammar.mjs [path-to-reader-tests.lisp]

import { readFileSync } from 'fs';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';
import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const vsctm = require('vscode-textmate');
const oniguruma = require('vscode-oniguruma');

let bad = 0;
function check(ok, what) {
  if (ok) {
    console.log(`ok: ${what}`);
  } else {
    bad++;
    console.error(`FAIL: ${what}`);
  }
}

const wasmPath = join(dirname(require.resolve('vscode-oniguruma')), 'onig.wasm');
const wasm = readFileSync(wasmPath);
await oniguruma.loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength));

const files = {
  'source.acl2': join(here, 'syntaxes', 'acl2.tmLanguage.json'),
  'source.acl2-output': join(here, 'syntaxes', 'acl2-output.tmLanguage.json'),
};
const registry = new vsctm.Registry({
  onigLib: Promise.resolve({
    createOnigScanner: (s) => new oniguruma.OnigScanner(s),
    createOnigString: (s) => new oniguruma.OnigString(s),
  }),
  loadGrammar: (scope) =>
    Promise.resolve(
      files[scope] ? vsctm.parseRawGrammar(readFileSync(files[scope], 'utf8'), files[scope]) : null
    ),
});
const grammar = await registry.loadGrammar('source.acl2');
if (!grammar) {
  console.error('FAIL: the ACL2 grammar did not load');
  process.exit(2);
}

/** Scopes of the first token of each line. */
function firstScopes(text) {
  let stack = vsctm.INITIAL;
  return text.split('\n').map((line) => {
    const r = grammar.tokenizeLine(line, stack);
    stack = r.ruleStack;
    return { line, scopes: r.tokens[0] ? r.tokens[0].scopes : [] };
  });
}

/** Whether every line that opens a form at column 0 is still code. */
function formsSurvive(text) {
  return firstScopes(text)
    .filter((l) => /^\(/.test(l.line))
    .every((l) => !l.scopes.some((s) => /^string/.test(s)));
}

// A bar-quoted symbol is a `begin`/`end` rule, not a match, because its name
// may contain anything at all -- including a newline and a double quote.
check(
  formsSurvive('(eq \'|a\n"b| x)\n(defun after (x) x)\n'),
  'a bar-quoted symbol spanning lines does not open a string'
);
check(formsSurvive("(eq '|a\"b| x)\n(defun after (x) x)\n"), 'nor does one holding a quote');
check(formsSurvive("(eq '|| x)\n(defun after (x) x)\n"), 'an empty bar symbol closes at once');
check(
  formsSurvive("(eq '|a\\|b| x)\n(defun after (x) x)\n"),
  'an escaped bar inside one does not close it'
);

// ACL2's fancy strings are delimited by `#{"""` and `"""}`, so every quote
// between them is content -- which is the entire point of the syntax.
check(
  // An *odd* number of quotes inside, so nothing else could balance them.
  formsSurvive('(equal #{"""a"b"""} x)\n(defun after (x) x)\n'),
  'a fancy string holding an odd quote does not open a string'
);
check(
  formsSurvive('(equal #{"""a\nb"""} x)\n(defun after (x) x)\n'),
  'nor does one spanning lines'
);

// A backslash escapes the next character anywhere, not only inside a string.
check(
  formsSurvive('(equal (symbol-name \'a\\"b) x)\n(defun after (x) x)\n'),
  'a backslash-escaped quote in a symbol does not open a string'
);
check(
  formsSurvive('(equal \'\;a x)\n(defun after (x) x)\n'),
  'nor does an escaped semicolon become a comment'
);

// The constructs these sit next to must still work.
check(
  firstScopes('"unterminated\n(defun after (x) x)\n')[1].scopes.some((s) => /^string/.test(s)),
  'a genuinely unterminated string still runs on, as it must'
);
check(
  firstScopes('#| a\n|#\n(defun after (x) x)\n')[2].scopes.some((s) => /^punctuation/.test(s)),
  'a block comment still ends at its closer'
);
check(
  firstScopes('#| a #| b |# c\n|#\n(defun after (x) x)\n')[2].scopes.some((s) =>
    /^punctuation/.test(s)
  ),
  'and a nested one needs both closers'
);
check(
  formsSurvive('(eql #\\" x)\n(defun after (x) x)\n'),
  'a character literal for a quote is a character, not a string'
);
check(
  firstScopes('(defun f (x) "doc" x)\n')[0].scopes.some((s) => /^punctuation/.test(s)),
  'an ordinary form is still an ordinary form'
);

// And the file that prompted all of this, when it is to hand.
const real =
  process.argv[2] ??
  join(
    process.env.ACL2_BOOKS ?? '/nonexistent',
    'kestrel',
    'acl2pl',
    'reader-tests.lisp'
  );
try {
  // Latin-1: this file is ACL2 source and writes every byte 0-255.
  const text = readFileSync(real, 'latin1');
  check(formsSurvive(text), `every form survives ${real}`);
} catch {
  console.log(`(skipped the real reader-tests.lisp: pass a path or set ACL2_BOOKS)`);
}

console.log(bad === 0 ? 'source-grammar: all green' : `source-grammar: ${bad} failed`);
process.exit(bad === 0 ? 0 : 1);
