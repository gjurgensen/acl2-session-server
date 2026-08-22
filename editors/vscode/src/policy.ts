// Client-side rules that are worth stating on their own, away from the
// editor API -- so they can be read, argued with, and tested without one.

/**
 * The largest term this client will work on.
 *
 * One thing still happens here to every term that reaches a webview: it is
 * tokenized and colored. (Laying it out used to happen here too, and now
 * happens in the daemon -- see `acl2_session::pretty`.) Coloring is worth its
 * cost on a term a person is reading and has no value on one nobody can.
 * Above this, a term gets no token entry, and that *is* the fallback: `hl` in
 * the pages renders any string it cannot find as plain escaped text, so
 * nothing further has to be arranged.
 *
 * The number is a backstop, not a budget: it is far above any term ACL2
 * prints for a form a person wrote, and exists for the ones it prints for a
 * form a macro wrote. Measured on `kestrel/c/syntax/validator.lisp`, whose
 * `defines` at line 2775 induct on a 30-way flag function. The induction
 * scheme ACL2 printed for it was **6.83 MB**, and the form view's own handling of
 * one of them cost:
 *
 *   laying it out           394 ms   6.83 MB -> 15.3 MB, 319,743 lines
 *   tokenizeAll             3471 ms
 *   postMessage payload              88.6 MB of JSON
 *   the page's own HTML              54.4 MB, 563,184 <span> elements
 *
 * Three such schemes came in one `induction` reply (20.9 MB, against 1.6 MB
 * of real events for the whole book), and the form view re-rendered them on every
 * `state` event -- fifteen of them in the six seconds after that form landed.
 * The result was not a slow form view. The extension host stopped answering, so
 * every other extension stopped with it; the renderer took a quarter of a
 * gigabyte of structured clone and a million and a half DOM nodes in wrapping
 * `<pre>` blocks, and the window went to "not responding" and stayed there.
 * With the form view closed the same book checks cleanly, because nothing asks
 * for the narrative unless the form view is showing the form.
 *
 * The daemon caps the scheme itself (`SCHEME_LIMIT` in the editor crate's
 * `wire.rs`), which is what keeps 20 MB off the wire. This is the guard for
 * every other term: a checkpoint, a `:corollary`, a statement -- any of which
 * can take the same shape from a book nobody has opened yet.
 */
export const MAX_TERM = 64 * 1024;

/**
 * Holding the form view on one form.
 *
 * The form view follows the cursor. Locking stops it, so a form can be watched
 * while the edit meant to fix it happens somewhere else -- which is the
 * ordinary way to use it: hold the failure, change the lemma above it, watch
 * the checkpoints move.
 *
 * A possible step beyond this, deliberately not built: **freeze**. Lock holds
 * the form view on a form and lets its contents update as that form is re-proved;
 * freeze would hold the contents too. The two differ only while the locked
 * form is being re-proved, and in that window what a reader wants is "do not
 * take away what I was reading" rather than a snapshot. What freeze would add
 * beyond that is comparing an old result against a new one -- and that needs
 * both on screen at once, so it wants a second form view rather than a mode. If it
 * is ever built it should freeze the *facts* and not the presentation: a
 * frozen form view must still re-flow when the pane is dragged and still recolour
 * with the theme, or "frozen" quietly means "wrong width and wrong colours"
 * too. The state here would become a third value rather than a second flag.
 */

/**
 * A place in a book: where a form starts.
 *
 * Carries the same position twice because two parties count differently. The
 * daemon speaks 1-based lines and **byte** columns, which is what has to match
 * a unit's `line_start`/`col_start`. The editor speaks UTF-16 characters, which
 * is what an edit's range is expressed in. Converting on demand would mean
 * decoding a line that the edit has already changed, so both are carried and
 * both are refreshed together whenever the lock is re-confirmed.
 */
export interface Anchor {
  /** 1-based, as the wire counts. */
  line: number;
  /** 1-based byte column, as the wire counts. */
  column: number;
  /** The same column as the editor counts it: 0-based UTF-16. */
  character: number;
}

/** One edit, in the editor's coordinates with 1-based lines. */
export interface Edit {
  startLine: number;
  startChar: number;
  endLine: number;
  endChar: number;
  /** How many lines the replacement text occupies -- 1 when it has no newline. */
  newLines: number;
}

/** Whether `(l1, c1)` comes strictly before `(l2, c2)`. */
function before(l1: number, c1: number, l2: number, c2: number): boolean {
  return l1 < l2 || (l1 === l2 && c1 < c2);
}

/**
 * Where an anchor ends up after some edits, or `undefined` if it did not
 * survive them.
 *
 * An anchor names the *first character* of a form, and the question is only
 * ever whether that character moved somewhere this can work out. Three cases,
 * and the third is the one that matters:
 *
 * - The edit finishes above the anchor's line: whole lines were added or
 *   removed above, so the anchor slides by the difference. This is the case
 *   the feature exists for -- fixing a lemma above the form you are watching.
 * - The edit begins after the anchor: it is below or to the right, and the
 *   anchor has not moved.
 * - Anything else -- an edit that straddles the anchor, starts exactly on it,
 *   or merely finishes to its left on its own line -- gives up.
 *
 * That last case is deliberately blunt. Sliding an anchor *along* a line would
 * mean re-measuring bytes on a line whose old text the edit has already
 * replaced, and getting it subtly wrong would leave a lock pointing confidently
 * at the wrong form. Letting go instead costs a rare re-lock and can never lie.
 * It also settles the case nothing else can: deleting the locked form.
 */
export function keepAnchor(a: Anchor, edits: readonly Edit[]): Anchor | undefined {
  // Every range in one change event is expressed against the document as it
  // was *before* the event, so each is judged against the original position
  // and the shifts are summed. Walking them while moving the anchor would
  // measure the later ones against a document none of them saw.
  let shift = 0;
  for (const e of edits) {
    if (before(e.endLine, e.endChar, a.line, a.character)) {
      if (e.endLine >= a.line) {
        // Finished on our own line, to our left: our column moved, and this
        // does not track columns.
        return undefined;
      }
      shift += e.newLines - (e.endLine - e.startLine + 1);
    } else if (!before(a.line, a.character, e.startLine, e.startChar)) {
      // It did not finish before us and does not start after us, so it covers
      // where we point.
      return undefined;
    }
  }
  const line = a.line + shift;
  return line < 1 ? undefined : { line, column: a.column, character: a.character };
}

/**
 * The unit a lock is on, or `undefined` if the lock can no longer be trusted.
 *
 * The rule the whole feature rests on: **the daemon's split is the truth and
 * the anchor is a guess.** A lock holds the position a form *started* at, so a
 * lock that is still good is one whose anchor is still exactly where a form
 * starts. Anything else -- no form there, or a form starting elsewhere -- means
 * the guess and the split have diverged, and the honest response is to let go
 * rather than show a form nobody asked for.
 *
 * Deliberately stricter than "some form covers this position": a position
 * inside a *different* form covers it just as well, and staying locked has to
 * mean the same form.
 */
export function lockedIndex(
  anchor: Anchor,
  units: readonly { index: number; line_start: number; col_start: number }[]
): number | undefined {
  const u = units.find((x) => x.line_start === anchor.line && x.col_start === anchor.column);
  return u?.index;
}

/**
 * What a `tail` event should cause.
 *
 * A tail is a **push**: the daemon sends it on its own timer and it already
 * carries the text to show. Nothing here asks the daemon for anything, and
 * `requestsMore` is typed as `false` rather than `boolean` so that saying
 * otherwise does not compile.
 *
 * That is not stylistic. A tail arrives about twice a second for as long as a
 * form runs, so anything issued per tail is issued at that cadence forever --
 * and a reply carrying *lines* is only as small as the lines happen to be.
 * Asking for four hundred of them from `kestrel/c/syntax/validator.lisp`, a
 * book that prints long ones, returned about 117 KB a time. Measured on a
 * live session before this rule existed:
 *
 *   daemon wrote  245 MB in eleven minutes
 *   real events   ~1.5 MB for the whole book
 *
 * A hundred and sixty times the legitimate traffic. The daemon's writer
 * thread stalled on a socket the client could not drain, and the extension
 * host -- which every other extension shares -- spent its life parsing JSON.
 * From the outside that is a frozen editor, not a slow form view.
 *
 * Depth on demand is a different question and still allowed: opening a view,
 * or a form landing, is a *user-paced* event, and reading the handle there
 * costs one reply rather than one per tick.
 */
export interface TailPlan {
  /** Where the text the event already carries should go. */
  target: 'console' | 'output-view' | 'form-view-only';
  /** Never true. See above. */
  requestsMore: false;
}

/**
 * `unit` is null for an eval's tail, which belongs to the console rather than
 * to any form of the file. Otherwise the output view takes it when it is
 * showing that same form, and the form view always gets it.
 */
export function planTail(args: {
  unit: number | null;
  sameBook: boolean;
  shownUnit: number | null;
}): TailPlan {
  if (args.unit === null) {
    return { target: 'console', requestsMore: false };
  }
  if (args.sameBook && args.shownUnit === args.unit) {
    return { target: 'output-view', requestsMore: false };
  }
  return { target: 'form-view-only', requestsMore: false };
}

/** One place `Go to Next Attention Point` will stop. */
export interface Attention {
  /** The unit to go to, or `null` for the end of the book. */
  unit: number | null;
  /** `skipped`, `failed`, `running`, `frontier`, or `eof`. */
  kind: string;
}

/**
 * Where a reader is sent, in order: every red form, then the frontier.
 *
 * The frontier is the form being proved, and *failing that the first one not
 * yet checked* -- which is the case this was missing. A form is `running`
 * only while ACL2 is actually working on it, and the driver reports state far
 * more often than that: after every step, after a rollback, after settling.
 * At any of those moments nothing is running while plenty is still pending,
 * and treating "nothing running" as "nothing left" sent the cursor to the end
 * of the file in the middle of a run, seemingly at random.
 *
 * The end of the book is the last resort and means what it says: nothing red,
 * nothing running, nothing pending.
 */
export function attentionPoints(
  units: readonly { index: number; status: string }[]
): Attention[] {
  const points: Attention[] = units
    .filter((u) => u.status === 'skipped' || u.status === 'failed')
    .map((u) => ({ unit: u.index, kind: u.status }));
  const running = units.find((u) => u.status === 'running');
  const pending = units.find((u) => u.status === 'pending');
  if (running) {
    points.push({ unit: running.index, kind: 'running' });
  } else if (pending) {
    points.push({ unit: pending.index, kind: 'frontier' });
  } else {
    points.push({ unit: null, kind: 'eof' });
  }
  return points;
}
