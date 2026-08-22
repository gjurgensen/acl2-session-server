// Where an explicit target stands, and where it goes when the file moves.
//
// A target names a form, and the only handle on a form that survives editing
// it is where it starts -- so the barrier is an anchor, tracked exactly as a
// lock is. What is tested here is the half that differs: what happens when the
// anchor stops naming a form. A lock lets go, and showing nothing is the safe
// end of that. Letting go here means "no barrier", which is "check the whole
// book" -- so it rounds up instead, and every case below is a case about which
// direction wrong goes in.
//
//   npm run compile   (or tsc -p .)
//   node barrier.mjs

import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const { barrierIndex, stepBarrier } = require('./out/policy.js');

let failures = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) {
    failures++;
  }
  console.log(
    `${ok ? 'ok' : 'FAIL'}: ${what}` +
      (ok ? '' : ` (got ${JSON.stringify(got)}, want ${JSON.stringify(want)})`)
  );
};

// A book with a form on each of lines 1, 5 and 9, plus a second form sharing
// line 9 -- which is the case a line number cannot express and a column can.
const units = [
  { index: 0, line_start: 1, col_start: 1 },
  { index: 1, line_start: 5, col_start: 1 },
  { index: 2, line_start: 9, col_start: 1 },
  { index: 3, line_start: 9, col_start: 20 },
];
const at = (line, column) => ({ line, column, character: column - 1 });

// The ordinary case: the anchor is a form's head, and names that form.
check('an anchor on a form head names it', barrierIndex(at(5, 1), units), 1);
check('and the column tells two forms on a line apart',
  barrierIndex(at(9, 20), units), 3);
check('holding before the first form is a real place, not a missing one',
  barrierIndex(null, units), -1);

// The anchor no longer starts a form. Every one of these rounds *up*: to the
// last form starting strictly above it. Rounding down would admit a form
// nobody pointed at, and admitting is the half that is expensive to undo.
check('an anchor adrift in the middle of the book rounds up',
  barrierIndex(at(7, 1), units), 1);
// An anchor can end up *inside* a form, which is what a merge looks like from
// here: delete a closing paren and two forms become one whose head is the
// earlier of the two, leaving the anchor pointing into its middle. The form it
// lands in is the one to take -- it contains the text the barrier was set
// around, so taking it checks what was asked and a little above, where
// refusing it would retract the whole book over one paren.
check('an anchor inside a form takes that form', barrierIndex(at(5, 8), units), 1);

// Which is the invariant underneath all of these, and the only one that
// matters: whatever the anchor has become, the barrier never reaches a form
// that starts below it. Checking too little is a keystroke away from being
// fixed; checking too much has already run.
for (const [line, column] of [[1, 0], [3, 4], [5, 1], [5, 8], [7, 1], [9, 10], [9, 20], [40, 1]]) {
  const i = barrierIndex(at(line, column), units);
  const below = units.filter(
    (u) => u.line_start > line || (u.line_start === line && u.col_start > column)
  );
  check(`nothing below (${line},${column}) is inside the barrier`,
    below.every((u) => u.index > i), true);
}
check('an anchor above every form holds before the first',
  barrierIndex(at(1, 0), units), -1);
check('and one below every form lands on the last',
  barrierIndex(at(40, 1), units), 3);
check('an anchor between two forms sharing a line rounds up to the first',
  barrierIndex(at(9, 10), units), 2);

// An empty book has nowhere to stand but the top, whatever is held.
check('no forms at all: the top', barrierIndex(at(3, 1), []), -1);

// Stepping. `-1` is the floor and a real position, so a step back from the
// top stays at the top rather than going out of range.
check('forward one', stepBarrier(1, 1, 4), 2);
check('back one', stepBarrier(1, -1, 4), 0);
check('back from the first form holds before it', stepBarrier(0, -1, 4), -1);
check('and back from there stays there', stepBarrier(-1, -1, 4), -1);
check('forward from the top takes the first form', stepBarrier(-1, 1, 4), 0);
check('forward from the last form stays on it', stepBarrier(3, 1, 4), 3);
check('an empty book has only the top', stepBarrier(-1, 1, 0), -1);

// Which form carries the mark: the first one *beyond* the barrier, never the
// last one inside it. The gutter beside an included form is fully spoken for
// -- green, yellow and red each say something nothing else says -- so a mark
// there would displace a verdict. Beyond it every form is the same grey, so
// the first is free to be special.
const marked = (anchor) => {
  const i = barrierIndex(anchor, units);
  return i + 1 < units.length ? i + 1 : undefined;
};
check('the mark goes on the form after the barrier', marked(at(5, 1)), 2);
check('held above everything, it marks the first form -- "none of this is in"',
  marked(null), 0);
check('held on the last form there is nothing beyond, so nothing is drawn',
  marked(at(9, 20)), undefined);
check('a rounded-up barrier marks the form after where it landed',
  marked(at(7, 1)), 2);

// The invariant that pairs with the one above: the marked form is never a
// form the run will reach, so the mark can never cover a verdict.
for (const [line, column] of [[1, 0], [3, 4], [5, 1], [5, 8], [7, 1], [9, 10]]) {
  const i = barrierIndex(at(line, column), units);
  check(`the mark for (${line},${column}) is outside the barrier`, marked(at(line, column)) > i, true);
}

console.log(failures === 0 ? 'barrier: all green' : `barrier: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
