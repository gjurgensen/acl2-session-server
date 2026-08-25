// The byte/UTF-16 column conversion, checked outside VSCode.
//
// Pure string arithmetic, so it needs neither an editor nor a daemon -- but
// it is the one place a coordinate is translated on its way across the wire,
// and getting it wrong puts a cursor on the wrong form silently.
//
//   npm run compile   (or tsc -p .)
//   node positions.mjs

import { createRequire } from 'module';

import { freshOrDie } from './fresh.mjs';

// Before the first require: this harness tests the compiled module, so a
// stale out/ makes it report on code that is not the code.
freshOrDie();

const require = createRequire(import.meta.url);
const { byteColumn, charOffset } = require('./out/positions.js');

let failures = 0;

function check(what, got, want) {
  const ok = got === want;
  if (!ok) {
    failures++;
  }
  console.log(`${ok ? 'ok' : 'FAIL'}: ${what}${ok ? '' : ` (got ${got}, want ${want})`}`);
}

// ASCII, which is nearly every line of nearly every book: the column is the
// offset, off by the one that makes it 1-based.
const plain = '(defun foo (x) x) (defun bar (x) x)';
check('column 1 at the start', byteColumn(plain, 0), 1);
check('the second form starts at column 19', byteColumn(plain, 18), 19);
check('and converts back', charOffset(plain, 19), 18);
check('past the end clamps', charOffset(plain, 500), plain.length);
check('column 0 is the start', charOffset(plain, 0), 0);

// Two bytes per character: the daemon counts four bytes where the editor
// counts two code units.
const latin = '(defun café (x) x) (defun bar (x) x)';
check('a two-byte char widens the column', byteColumn(latin, 11), 13);
check('the second form starts at column 21', byteColumn(latin, 19), 21);
check('and converts back', charOffset(latin, 21), 19);

// Astral: one code point, two UTF-16 code units, four bytes.
const astral = '(f "\u{1F600}") (g)';
check('a surrogate pair is four bytes', byteColumn(astral, 6), 9);
check('and converts back to the code-unit offset', charOffset(astral, 9), 6);
check('the second form', charOffset(astral, byteColumn(astral, 9)), 9);

// Round trip over every offset of a mixed line, which is the property that
// actually matters: whatever the client sends, it can read back.
//
// Every offset except the inside of a surrogate pair, which is half of one
// character and so has no byte column of its own. Neither side ever produces
// one -- a VSCode position does not split a pair, and the daemon's columns
// land on parens -- so there is nothing to agree about there.
const mixed = ';; éè (a) \u{1F600} (b)';
let roundTrips = 0;
for (let i = 0; i <= mixed.length; i++) {
  if (i > 0 && mixed.codePointAt(i - 1) > 0xffff) {
    continue;
  }
  roundTrips++;
  const back = charOffset(mixed, byteColumn(mixed, i));
  if (back !== i) {
    failures++;
    console.log(`FAIL: round trip at offset ${i} gave ${back}`);
  }
}
check('every offset of a mixed line round trips', roundTrips, mixed.length);

console.log(failures === 0 ? 'positions: all green' : `positions: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
