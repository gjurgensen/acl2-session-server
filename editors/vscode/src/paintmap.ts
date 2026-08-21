// Which form owns which piece of which line.
//
// Two forms on one line is rare and legal, and it is the case both renderers
// here were built to be unable to express: a gutter has one cell per line,
// and a whole-line wash is whole-line. Precision about *where* a form sits
// does not by itself buy either of them anything -- something has to decide
// what to draw when a line holds two answers.
//
// This module is that decision, kept apart from the drawing so it can be
// tested without an editor. It takes the units and returns, per unit, the
// spans to wash and where (if anywhere) to put an inline status mark.
//
// Columns in and out are the daemon's: 1-based, counted in bytes. Turning
// them into editor positions is `positions.ts`'s job, at the point of
// drawing.

/** The part of a unit the paint map cares about. */
export interface Placed {
  index: number;
  line_start: number;
  line_end: number;
  col_start: number;
  col_end: number;
  status: string;
}

/** A run of whole lines, washed edge to edge as they always have been. */
export interface WholeLines {
  kind: 'lines';
  from: number;
  to: number;
}

/**
 * Part of one line, from `col` to `endCol`.
 *
 * `endCol` of `null` means "to the end of the line" -- the opening line of a
 * form that runs on past it.
 */
export interface PartOfLine {
  kind: 'part';
  line: number;
  col: number;
  endCol: number | null;
}

export type Span = WholeLines | PartOfLine;

export interface Painted {
  index: number;
  status: string;
  /** What to wash for this unit. */
  spans: Span[];
  /**
   * The lines whose gutter cell is this unit's to claim, or `null` when it
   * has none -- a form written second on a line it therefore does not own.
   */
  gutter: { from: number; to: number } | null;
  /**
   * Where to draw this unit's status inline, when the gutter cannot.
   *
   * Set only for a form that does *not* own the start of its first line --
   * the gutter cell there belongs to the form that does, so this one's
   * status goes between the two forms instead, where the second one
   * actually begins. `null` for the owner, which is the overwhelmingly
   * common case and keeps the gutter exactly as it always was.
   */
  inlineAt: { line: number; col: number } | null;
}

/**
 * Lines touched by more than one unit.
 *
 * Units are contiguous and non-overlapping, so only a *first* or *last* line
 * can be shared -- an interior line of a multi-line form is that form's
 * alone. Sharing therefore comes in two shapes, and both matter: two forms
 * written side by side, and a form that ends where the next one begins.
 */
function sharedLines(units: Placed[]): Set<number> {
  const seen = new Set<number>();
  const shared = new Set<number>();
  for (const u of units) {
    // A one-line form must not collide with *itself*: its first and last
    // lines are the same line, and counting both would mark every ordinary
    // form's line as shared and cost the whole book its whole-line wash.
    const ends = u.line_start === u.line_end ? [u.line_start] : [u.line_start, u.line_end];
    for (const line of ends) {
      if (seen.has(line)) {
        shared.add(line);
      }
      seen.add(line);
    }
  }
  return shared;
}

/**
 * The unit that owns the start of each line: the first one, in file order,
 * whose extent covers it. That is the form a reader's eye starts the line
 * on, so it is the one the gutter should speak for.
 */
function lineOwners(units: Placed[]): Map<number, number> {
  const owner = new Map<number, number>();
  for (const u of units) {
    for (let line = u.line_start; line <= u.line_end; line++) {
      if (!owner.has(line)) {
        owner.set(line, u.index);
      }
    }
  }
  return owner;
}

/**
 * Lay the units out for painting.
 *
 * A unit whose first and last lines are both its own is one whole-line run,
 * exactly as before this existed. A unit sharing either end is split, so the
 * wash changes colour mid-line at the boundary between the two forms rather
 * than one of them silently painting over the other.
 */
export function paintMap(units: Placed[]): Painted[] {
  const shared = sharedLines(units);
  const owner = lineOwners(units);
  return units.map((u) => {
    const spans: Span[] = [];
    const firstShared = shared.has(u.line_start);
    const lastShared = shared.has(u.line_end);
    if (!firstShared && !lastShared) {
      spans.push({ kind: 'lines', from: u.line_start, to: u.line_end });
    } else if (u.line_start === u.line_end) {
      spans.push({ kind: 'part', line: u.line_start, col: u.col_start, endCol: u.col_end });
    } else {
      // The opening line runs to its end; the closing line starts at its
      // own. Only the ends can be shared, so the block between them is
      // whole lines whatever happens at the edges.
      if (firstShared) {
        spans.push({ kind: 'part', line: u.line_start, col: u.col_start, endCol: null });
      }
      if (lastShared) {
        spans.push({ kind: 'part', line: u.line_end, col: 1, endCol: u.col_end });
      }
      const from = firstShared ? u.line_start + 1 : u.line_start;
      const to = lastShared ? u.line_end - 1 : u.line_end;
      if (from <= to) {
        spans.push({ kind: 'lines', from, to });
      }
    }
    // A unit can only ever lose its *first* line: a later line of its own is
    // shared only with a unit that comes after it, and the earlier one wins.
    const owns = owner.get(u.line_start) === u.index;
    const from = owns ? u.line_start : u.line_start + 1;
    return {
      index: u.index,
      status: u.status,
      spans,
      gutter: from <= u.line_end ? { from, to: u.line_end } : null,
      inlineAt: owns ? null : { line: u.line_start, col: u.col_start },
    };
  });
}
