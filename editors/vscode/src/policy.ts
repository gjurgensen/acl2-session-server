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
 * scheme ACL2 printed for it was **6.83 MB**, and the panel's own handling of
 * one of them cost:
 *
 *   laying it out           394 ms   6.83 MB -> 15.3 MB, 319,743 lines
 *   tokenizeAll             3471 ms
 *   postMessage payload              88.6 MB of JSON
 *   the page's own HTML              54.4 MB, 563,184 <span> elements
 *
 * Three such schemes came in one `induction` reply (20.9 MB, against 1.6 MB
 * of real events for the whole book), and the panel re-rendered them on every
 * `state` event -- fifteen of them in the six seconds after that form landed.
 * The result was not a slow panel. The extension host stopped answering, so
 * every other extension stopped with it; the renderer took a quarter of a
 * gigabyte of structured clone and a million and a half DOM nodes in wrapping
 * `<pre>` blocks, and the window went to "not responding" and stayed there.
 * With the panel closed the same book checks cleanly, because nothing asks
 * for the narrative unless the panel is showing the form.
 *
 * The daemon caps the scheme itself (`SCHEME_LIMIT` in the editor crate's
 * `wire.rs`), which is what keeps 20 MB off the wire. This is the guard for
 * every other term: a checkpoint, a `:corollary`, a statement -- any of which
 * can take the same shape from a book nobody has opened yet.
 */
export const MAX_TERM = 64 * 1024;

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
 * From the outside that is a frozen editor, not a slow panel.
 *
 * Depth on demand is a different question and still allowed: opening a view,
 * or a form landing, is a *user-paced* event, and reading the handle there
 * costs one reply rather than one per tick.
 */
export interface TailPlan {
  /** Where the text the event already carries should go. */
  target: 'console' | 'output-view' | 'panel-only';
  /** Never true. See above. */
  requestsMore: false;
}

/**
 * `unit` is null for an eval's tail, which belongs to the console rather than
 * to any form of the file. Otherwise the output view takes it when it is
 * showing that same form, and the panel always gets it.
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
  return { target: 'panel-only', requestsMore: false };
}
