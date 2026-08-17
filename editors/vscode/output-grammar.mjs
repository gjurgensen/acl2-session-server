// The output grammar, checked against real ACL2 and against the cases that
// would make it dangerous.
//
// Highlighting a transcript is best-effort by nature: ACL2's log is prose
// with Lisp embedded in it, and prose is hostile to a Lisp grammar --
// `Subgoal *1/2''` and `we'll` are apostrophes that open a quote, and one
// stray `"` in prose opens a string. TextMate carries a rule stack across
// lines, so an escape does not end at the line: it ends at the end of the
// file.
//
// So the grammar never runs `source.acl2` over the log. It enters an island
// only at a place a term really starts, and each island is a `begin`/`while`
// block whose `while` is re-tested at the start of every line -- which pops
// the block even when a string is open inside it. That containment is the
// whole design, and it is what this file exists to prove: the adversarial
// cases below all put an unterminated construct inside an island and assert
// that the prose after it comes out clean.
//
//   npm run compile   (not needed -- this reads the grammars directly)
//   node output-grammar.mjs

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
const grammar = await registry.loadGrammar('source.acl2-output');
if (!grammar) {
  console.error('FAIL: the output grammar did not load');
  process.exit(2);
}

/** Tokenize a whole log; returns, per line, the union of its token scopes. */
function scopeLines(text) {
  let stack = vsctm.INITIAL;
  return text.split('\n').map((line) => {
    const r = grammar.tokenizeLine(line, stack);
    stack = r.ruleStack;
    const scopes = new Set();
    for (const t of r.tokens) {
      for (const s of t.scopes) {
        scopes.add(s);
      }
    }
    return { line, scopes };
  });
}
// `include: source.acl2` pulls in that grammar's *patterns*, not its scope
// name -- the enclosing `source.acl2-output` stays on every token. So what
// marks an island is the included grammar's own suffix, which its rules end
// in and the output grammar's (`.acl2-output`) do not.
const isLisp = (e) => [...e.scopes].some((s) => s.endsWith('.acl2'));
const has = (e, prefix) => [...e.scopes].some((s) => s.startsWith(prefix));
const find = (rows, needle) => rows.find((r) => r.line.includes(needle));

// ---- Against a real transcript -------------------------------------------

const rows = scopeLines(readFileSync(join(here, 'test', 'acl2-output.log'), 'utf8'));

check(isLisp(find(rows, '(equal (append x y) (append y x))')), 'a goal term at column zero is Lisp');
check(
  isLisp(find(rows, '(implies (and (consp x)')) &&
    isLisp(find(rows, '(append y x)))')),
  'and so are the indented continuation lines of a multi-line goal'
);
check(has(find(rows, 'Subgoal *1/2\'\''), 'entity.name.section'), 'a goal header is a header');
check(has(find(rows, 'Q.E.D.'), 'markup.inserted'), 'Q.E.D. is marked');
check(has(find(rows, 'ACL2 Error [Failure]'), 'invalid.illegal'), 'an ACL2 Error is marked');
check(has(find(rows, '******** FAILED ********'), 'invalid.illegal'), 'and so is FAILED');
check(has(find(rows, 'Rules: ((:definition'), 'keyword.control'), 'the Rules: label is a label');
check(isLisp(find(rows, '(:elim car-cdr-elim)')), 'and the rune list under it is Lisp');
check(has(find(rows, 'Prover steps counted:'), 'keyword.control'), 'the steps label is a label');
check(isLisp(find(rows, '(defun alpha (x)')), "a form printed by `pe` is Lisp");

// The false positives. Each of these lines is prose that opens like code.
check(
  !isLisp(find(rows, '([ A key checkpoint while proving')),
  'a checkpoint bracket `([` is prose, not a term'
);
check(
  !isLisp(find(rows, 'We will induct according to a scheme suggested by')),
  'prose mentioning a term inline stays prose'
);
check(
  !isLisp(find(rows, "denote *1 above then the induction scheme we'll")),
  "prose containing an apostrophe (`we'll`) stays prose"
);
check(
  !isLisp(find(rows, 'The admission of alpha is trivial')),
  'the admission sentence stays prose'
);
// The nastiest shape in real output: a wrapped sentence whose line happens
// to start with a term. Excluded because it does not end in a close paren.
check(
  !isLisp(find(rows, '(acl2-count x).  We observe')),
  'a sentence that merely begins with a term stays prose'
);
// ... and the bound holds even when something does slip through: a block can
// never outlive the first unindented line after it.
{
  const r = scopeLines('(equal (foo x) bar baz quux and more prose here)\nthe next line is prose.\n');
  check(!isLisp(r[1]), 'a block never survives past the first unindented line');
}

// ---- Our own status messages ----------------------------------------------
//
// The output views show these when there is no log, and they are highlighted
// by the same grammar. `(not checked yet)` is shaped exactly like a term --
// opens at column zero, closes at end of line -- so it came out coloured,
// `not` and all. They are prose now; these mirror the literals in
// extension.ts and outputView.ts, and exist so nobody puts the parens back.

for (const msg of [
  'No form under the cursor has been checked yet.',
  'Nothing evaluated yet.',
  'Not checked yet.',
  'Output unavailable: Error: connection closed',
]) {
  const r = scopeLines(msg + '\n');
  check(!isLisp(r[0]), `the status message ${JSON.stringify(msg)} stays prose`);
}
// The shape that bit us, kept as the reason the wording matters.
check(
  isLisp(scopeLines('(not checked yet)\n')[0]),
  'and a parenthesized one would not have -- which is why they are prose'
);

// ---- The containment cases ------------------------------------------------
//
// Each opens something inside an island and never closes it. The assertion
// is always about the *unindented* line afterwards: it must be clean.

const ADVERSARIAL = [
  [
    'an unterminated string inside a goal term',
    'Goal\n(equal (foo "unterminated\n              bar)\nThis line is prose and must be clean.\n',
  ],
  [
    'an unterminated block comment inside a goal term',
    'Goal\n(equal (foo #| open\n              bar)\nThis line is prose and must be clean.\n',
  ],
  [
    'an unbalanced open paren inside a goal term',
    'Goal\n(equal (foo (((((\n              bar\nThis line is prose and must be clean.\n',
  ],
  [
    'an unterminated string in a rune list',
    'Rules: ((:rewrite "open\n        (:definition bar)\nThis line is prose and must be clean.\n',
  ],
  [
    'an unterminated bar-quoted symbol inside a goal term',
    'Goal\n(equal |open-bar\n       x)\nThis line is prose and must be clean.\n',
  ],
];

for (const [what, text] of ADVERSARIAL) {
  const r = scopeLines(text);
  const after = r.find((x) => x.line.startsWith('This line is prose'));
  check(after !== undefined && !isLisp(after), `${what}: the block pops and the prose is clean`);
}

// A truncated log -- the console shows the tail of one -- must not leave the
// first line half-inside something either.
const truncated = scopeLines('              (append y (cdr x))))\nSummary\n');
check(has(truncated[1], 'keyword.control'), 'a log cut mid-term still finds the next label');

if (bad) {
  console.error(`output-grammar: ${bad} failing`);
  process.exit(1);
}
console.log('output-grammar: all green');
