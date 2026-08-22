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

// The ordinary book: one form per line, or a form per block of lines. A form
// is painted over its own extent and no further, so the line it ends on stops
// at its closing paren rather than running out to the viewport edge.
{
  const p = paintMap([unit(0, 1, 1, 1, 20), unit(1, 3, 1, 7, 2)]);
  check('a form on its own line stops where it ends', p[0].spans, [
    { kind: 'part', line: 1, col: 1, endCol: 20 },
  ]);
  check('and owns its gutter cell', p[0].gutter, { from: 1, to: 1 });
  check('and needs no inline mark', p[0].inlineAt, null);
  // Whole lines for the block, so a multi-line form still reads as one, and
  // the closing line cut to the form.
  check('a multi-line form is a block and a last line', p[1].spans, [
    { kind: 'lines', from: 3, to: 6 },
    { kind: 'part', line: 7, col: 1, endCol: 2 },
  ]);
  check('over all its lines', p[1].gutter, { from: 3, to: 7 });
}

// Two forms side by side: `(defun one (x) x) (defun two (x) x)`.
// The wash changes colour at column 19, the gutter speaks for the first, and
// the second's bar goes between them.
{
  const p = paintMap([unit(0, 2, 1, 2, 18, 'admitted'), unit(1, 2, 19, 2, 36, 'failed')]);
  // Each paints its own half and nothing between them: the space separating
  // two forms belongs to neither, and nothing overlaps.
  check('the first form stops at its own end', p[0].spans, [
    { kind: 'part', line: 2, col: 1, endCol: 18 },
  ]);
  check('and so does the second', p[1].spans, [
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
  check('the form that ends on the line stops at its closing paren', p[0].spans, [
    { kind: 'lines', from: 1, to: 1 },
    { kind: 'part', line: 2, col: 1, endCol: 5 },
  ]);
  check('and the one that starts there begins at its own', p[1].spans, [
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
  // Its opening line runs to the end of the text -- the form carries on, so
  // there is nothing after it on that line to leave alone -- then whole lines,
  // then a last line cut to the form.
  check('it opens at its own column and runs to the end of the line', p[1].spans, [
    { kind: 'part', line: 1, col: 12, endCol: null },
    { kind: 'lines', from: 2, to: 3 },
    { kind: 'part', line: 4, col: 1, endCol: 3 },
  ]);
  check('and the form beside it keeps its own half', p[0].spans, [
    { kind: 'part', line: 1, col: 1, endCol: 10 },
  ]);
  check('the gutter picks it up on the next line', p[1].gutter, { from: 2, to: 4 });
  check('and the mark says where it began', p[1].inlineAt, { line: 1, col: 12 });
}

// Three on one line, which nothing forbids.
{
  const p = paintMap([unit(0, 1, 1, 1, 6), unit(1, 1, 7, 1, 12), unit(2, 1, 13, 1, 18)]);
  check('each paints itself and no more', p.map((x) => x.spans), [
    [{ kind: 'part', line: 1, col: 1, endCol: 6 }],
    [{ kind: 'part', line: 1, col: 7, endCol: 12 }],
    [{ kind: 'part', line: 1, col: 13, endCol: 18 }],
  ]);
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

// The same colour now lives in three places, none of which the type system
// connects: a value in `bars.ts`, a fill in `media/*.svg` for the gutter
// icon, and a contributed theme-colour default in `package.json`. An earlier
// version of this feature drifted apart at exactly the first two -- a
// hand-copied table with no way to notice if the files moved. So compare all
// three.
{
  const { BARS } = require('./out/bars.js');
  const { readFileSync } = require('fs');
  for (const bar of BARS) {
    for (const theme of ['light', 'dark']) {
      const svg = readFileSync(`media/${bar.icon}-${theme}.svg`, 'utf8');
      const fill = /fill="#([0-9a-fA-F]{6})"/.exec(svg);
      const alpha = /fill-opacity="([0-9.]+)"/.exec(svg);
      if (!fill || !alpha) {
        failures++;
        console.log(`FAIL: media/${bar.icon}-${theme}.svg has no single fill to compare`);
        continue;
      }
      const [r, g, b] = [0, 2, 4].map((i) => parseInt(fill[1].slice(i, i + 2), 16));
      check(
        `${bar.icon} (${theme}) matches its gutter SVG`,
        bar[theme],
        `rgba(${r}, ${g}, ${b}, ${alpha[1]})`
      );
    }
  }

  // And the contributed defaults, which are the same colours as #rrggbbaa.
  // Compared channel by channel with a tolerance of one, because the alpha
  // makes a round trip through a byte and 0.5 has no exact eighth-bit form.
  const pkg = JSON.parse(readFileSync('package.json', 'utf8'));
  const byId = new Map(pkg.contributes.colors.map((c) => [c.id, c.defaults]));
  const channels = (hex) =>
    [0, 2, 4, 6].map((i) => parseInt(hex.slice(1 + i, 3 + i), 16)).filter((n) => !isNaN(n));
  const rgbaChannels = (c) => {
    const m = /rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([0-9.]+))?\)/.exec(c);
    return m
      ? [+m[1], +m[2], +m[3], Math.round(parseFloat(m[4] ?? '1') * 255)]
      : channels(c).concat(channels(c).length === 3 ? [255] : []);
  };
  for (const bar of BARS) {
    for (const status of bar.statuses) {
      const d = byId.get(`acl2Session.bar.${status}`);
      if (!d) {
        failures++;
        console.log(`FAIL: package.json contributes no acl2Session.bar.${status}`);
        continue;
      }
      for (const theme of ['light', 'dark']) {
        const want = rgbaChannels(bar[theme]);
        const got = rgbaChannels(d[theme]);
        const near = want.length === got.length && want.every((n, i) => Math.abs(n - got[i]) <= 1);
        if (!near) {
          failures++;
          console.log(`FAIL: acl2Session.bar.${status} (${theme}) is ${d[theme]}, not ${bar[theme]}`);
        }
      }
    }
  }
  check('every bar has a contributed default', true, true);

  // The washes only ever existed as values, but they are contributed now too.
  const { WASHES } = require('./out/bars.js');
  for (const [status, c] of Object.entries(WASHES)) {
    const d = byId.get(`acl2Session.wash.${status}`);
    if (!d) {
      failures++;
      console.log(`FAIL: package.json contributes no acl2Session.wash.${status}`);
      continue;
    }
    for (const theme of ['light', 'dark']) {
      const want = rgbaChannels(c[theme]);
      const got = rgbaChannels(d[theme]);
      const near = want.length === got.length && want.every((n, i) => Math.abs(n - got[i]) <= 1);
      if (!near) {
        failures++;
        console.log(`FAIL: acl2Session.wash.${status} (${theme}) is ${d[theme]}, not ${c[theme]}`);
      }
    }
  }
  check('every wash has a contributed default', true, true);
}

console.log(failures === 0 ? 'paintmap: all green' : `paintmap: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
