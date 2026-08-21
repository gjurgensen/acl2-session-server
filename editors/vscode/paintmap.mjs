// What gets painted where, checked outside VSCode.
//
// The decisions this covers are invisible in a screenshot and wrong in ways
// nobody reports: a form silently painting over its neighbour, or two forms
// competing for one gutter cell. The layout is pure, so it is checked here
// rather than by looking at it.
//
//   npm run compile   (or tsc -p .)
//   node paintmap.mjs

import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const { paintMap } = require('./out/paintmap.js');

let failures = 0;

function check(what, got, want) {
  const a = JSON.stringify(got);
  const b = JSON.stringify(want);
  const ok = a === b;
  if (!ok) {
    failures++;
  }
  console.log(`${ok ? 'ok' : 'FAIL'}: ${what}${ok ? '' : `\n  got  ${a}\n  want ${b}`}`);
}

const unit = (index, line_start, col_start, line_end, col_end, status = 'admitted') => ({
  index,
  line_start,
  col_start,
  line_end,
  col_end,
  status,
});

// The ordinary book: one form per line, or a form per block of lines. Every
// unit is one whole-line run and owns its own gutter, exactly as before the
// paint map existed -- this is the case that must not have changed.
{
  const p = paintMap([unit(0, 1, 1, 1, 20), unit(1, 3, 1, 7, 2)]);
  check('a form on its own line is whole lines', p[0].spans, [{ kind: 'lines', from: 1, to: 1 }]);
  check('and owns its gutter cell', p[0].gutter, { from: 1, to: 1 });
  check('and needs no inline mark', p[0].inlineAt, null);
  check('a multi-line form is one run', p[1].spans, [{ kind: 'lines', from: 3, to: 7 }]);
  check('over all its lines', p[1].gutter, { from: 3, to: 7 });
}

// Two forms side by side: `(defun one (x) x) (defun two (x) x)`.
// The wash changes colour at column 19, the gutter speaks for the first, and
// the second's bar goes between them.
{
  const p = paintMap([unit(0, 2, 1, 2, 18, 'admitted'), unit(1, 2, 19, 2, 36, 'failed')]);
  check('the first form is painted to its columns', p[0].spans, [
    { kind: 'part', line: 2, col: 1, endCol: 18 },
  ]);
  check('the second picks up where it left off', p[1].spans, [
    { kind: 'part', line: 2, col: 19, endCol: 36 },
  ]);
  check('the gutter speaks for the first', p[0].gutter, { from: 2, to: 2 });
  check('and the second has no cell to claim', p[1].gutter, null);
  check('so its bar goes between the two', p[1].inlineAt, { line: 2, col: 19 });
  check('while the first needs none', p[0].inlineAt, null);
}

// The other way a line is shared: one form ends where the next begins.
// `(defun a (x)\n  x) (defun b (y) y)` -- the first owns line 2 up to its
// closing paren, the second from there on.
{
  const p = paintMap([unit(0, 1, 1, 2, 5), unit(1, 2, 6, 2, 24)]);
  check('the form that ends on the line keeps its whole first line', p[0].spans, [
    { kind: 'part', line: 2, col: 1, endCol: 5 },
    { kind: 'lines', from: 1, to: 1 },
  ]);
  check('the one that starts there is painted from its column', p[1].spans, [
    { kind: 'part', line: 2, col: 6, endCol: 24 },
  ]);
  check('the earlier form owns both gutter cells', p[0].gutter, { from: 1, to: 2 });
  check('the later one owns none', p[1].gutter, null);
  check('and is marked where it starts', p[1].inlineAt, { line: 2, col: 6 });
}

// A form that opens on a line it does not own *and* runs on past it: it has
// both an inline mark, where it begins, and gutter cells for the rest.
{
  const p = paintMap([unit(0, 1, 1, 1, 10), unit(1, 1, 12, 4, 3)]);
  check('its opening line is painted to the end of the line', p[1].spans, [
    { kind: 'part', line: 1, col: 12, endCol: null },
    { kind: 'lines', from: 2, to: 4 },
  ]);
  check('the gutter picks it up on the next line', p[1].gutter, { from: 2, to: 4 });
  check('and the mark says where it began', p[1].inlineAt, { line: 1, col: 12 });
}

// Three on one line, which nothing forbids.
{
  const p = paintMap([unit(0, 1, 1, 1, 6), unit(1, 1, 7, 1, 12), unit(2, 1, 13, 1, 18)]);
  check('only the first owns the cell', p.map((x) => x.gutter), [
    { from: 1, to: 1 },
    null,
    null,
  ]);
  check('and the other two are marked in place', p.map((x) => x.inlineAt), [
    null,
    { line: 1, col: 7 },
    { line: 1, col: 13 },
  ]);
}

check('an empty book paints nothing', paintMap([]), []);

console.log(failures === 0 ? 'paintmap: all green' : `paintmap: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
