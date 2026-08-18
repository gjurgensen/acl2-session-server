// Client-side rules that are worth stating on their own, away from the
// editor API -- so they can be read, argued with, and tested without one.

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
