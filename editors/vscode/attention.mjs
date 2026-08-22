// Where `Go to Next Attention Point` stops, and in what order.
//
// The bug this exists for was invisible except at the wrong moment: a form is
// `running` only while ACL2 is actually working on it, and the driver reports
// state far more often than that, so most of the time during a run nothing is
// running while plenty is pending. Treating that as "nothing left" sent the
// cursor to the end of the file mid-run, seemingly at random -- which is
// exactly the kind of thing nobody files a bug about and nobody can reproduce
// on demand.
//
//   npm run compile   (or tsc -p .)
//   node attention.mjs

import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const { attentionPoints } = require('./out/policy.js');

let failures = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) {
    failures++;
  }
  console.log(
    `${ok ? 'ok' : 'FAIL'}: ${what}` +
      (ok ? '' : `\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`)
  );
};

const book = (...statuses) => statuses.map((status, index) => ({ index, status }));

// Mid-run, with a form actually in flight.
check('the reds in order, then the running form',
  attentionPoints(book('admitted', 'failed', 'running', 'pending')),
  [{ unit: 1, kind: 'failed' }, { unit: 2, kind: 'running' }]);

// The same run a moment later, between forms: nothing is running, and the
// answer must still be the frontier rather than the end of the book.
check('between forms, the frontier is the first pending one',
  attentionPoints(book('admitted', 'failed', 'admitted', 'pending', 'pending')),
  [{ unit: 1, kind: 'failed' }, { unit: 3, kind: 'frontier' }]);

check('with nothing red, the frontier is the only stop',
  attentionPoints(book('admitted', 'admitted', 'pending')),
  [{ unit: 2, kind: 'frontier' }]);

// The end of the book means what it says: nothing red, running or pending.
check('a finished book ends at the end',
  attentionPoints(book('admitted', 'admitted')), [{ unit: null, kind: 'eof' }]);
check('and a finished book with reds stops at them first',
  attentionPoints(book('skipped', 'admitted', 'conditional')),
  [{ unit: 0, kind: 'skipped' }, { unit: null, kind: 'eof' }]);
check('an empty book has only the end', attentionPoints([]), [{ unit: null, kind: 'eof' }]);

// Both kinds of red are stops, in file order, and a running form still wins
// the last slot over a pending one behind it.
check('skipped and failed both stop, in order',
  attentionPoints(book('failed', 'skipped', 'running')),
  [{ unit: 0, kind: 'failed' }, { unit: 1, kind: 'skipped' }, { unit: 2, kind: 'running' }]);
check('a running form outranks a pending one',
  attentionPoints(book('pending', 'running')), [{ unit: 1, kind: 'running' }]);

// A conditional form is not an attention point: it went in, and the form that
// made it conditional is the red one already on the list.
check('conditional forms are not stops',
  attentionPoints(book('conditional', 'conditional')), [{ unit: null, kind: 'eof' }]);

// A form past an explicit target is not somewhere to send anyone. Before
// `beyond` existed these were `pending`, so a finished targeted run reported a
// frontier the session had no intention of ever reaching, and sitting there
// waiting did nothing.
check('everything up to the target done: nothing left to attend to',
  attentionPoints(book('admitted', 'admitted', 'beyond', 'beyond')),
  [{ unit: null, kind: 'eof' }]);
check('the frontier is the queued form, not the one past the barrier',
  attentionPoints(book('admitted', 'pending', 'beyond')),
  [{ unit: 1, kind: 'frontier' }]);
// A red form is by construction inside the barrier: the run stopped there, so
// it never reached past.
check('a failure still comes first, and the frontier after it is still queued',
  attentionPoints(book('admitted', 'failed', 'pending', 'beyond')),
  [{ unit: 1, kind: 'failed' }, { unit: 2, kind: 'frontier' }]);
// With no target nothing is `beyond`, so the split is invisible.
check('an untargeted book behaves exactly as before',
  attentionPoints(book('admitted', 'pending', 'pending')),
  [{ unit: 1, kind: 'frontier' }]);

console.log(failures === 0 ? 'attention: all green' : `attention: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
