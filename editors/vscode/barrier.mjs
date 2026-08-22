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

// Which form the mark follows: the last one *inside* the barrier. It sits in
// the text just after that form, pointing back at it, so what it marks is what
// it says -- everything up to here is in.
//
// It marked the form *beyond* while it lived in the gutter, and only because a
// gutter cell holds one icon: marking an included form would have displaced
// that form's verdict. In the text nothing is displaced, and the placement
// gains the case the other one had no answer for -- held on the last form in
// the book, "the first form beyond" is nothing at all.
const marked = (anchor) => barrierIndex(anchor, units);
check('the mark follows the last included form', marked(at(5, 1)), 1);
check('held on the last form in the book it still has somewhere to be',
  marked(at(9, 20)), 3);
check('held above everything there is no form to follow', marked(null), -1);
check('a rounded-up barrier follows the form it landed on', marked(at(7, 1)), 1);

// The invariant: the mark never follows a form the run will not reach, so it
// can never claim more is checked than is.
for (const [line, column] of [[1, 0], [3, 4], [5, 1], [5, 8], [7, 1], [9, 10], [40, 1]]) {
  const i = barrierIndex(at(line, column), units);
  check(`the mark for (${line},${column}) is inside the barrier`, marked(at(line, column)) <= i, true);
}

console.log(failures === 0 ? 'barrier: all green' : `barrier: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
