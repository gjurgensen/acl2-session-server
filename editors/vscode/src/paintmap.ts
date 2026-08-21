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
 * Part of one line, from `col` up to (not including) `endCol`.
 *
 * Always a form that has a later form beside it on that line, so `endCol` is
 * where that one starts -- never the end of the text, which is the whole
 * point: the form on the right owns the rest of the line.
 */
export interface PartOfLine {
  kind: 'part';
  line: number;
  col: number;
  endCol: number;
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
 * A unit whose lines are all its own is one whole-line run, exactly as
 * before this existed. On a line two forms share, the **last** form keeps
 * the whole-line run -- so the wash still reaches the edge of the viewport,
 * as it does on every other line in the book -- and the forms before it are
 * painted over it, each ending where the next begins. The colour therefore
 * changes once per boundary and not once more at the end of the text.
 *
 * That overlap is the only way to say "from this column to the edge": a
 * whole-line decoration always starts at column 0, and the API has no
 * z-index. The pieces are drawn by decoration types created after the
 * whole-line ones, which is what puts them on top. See `decorations.ts`.
 */
export function paintMap(units: Placed[]): Painted[] {
  const owner = lineOwners(units);
  // The next form to begin on a given line, which is where the form before
  // it stops: the gap between `)` and `(` belongs to the form on its left,
  // so the colour changes at a form boundary rather than a space.
  const nextStartOn = (line: number, after: number): Placed | undefined =>
    units.find((v) => v.index > after && v.line_start === line);

  return units.map((u) => {
    const spans: Span[] = [];
    // A form is painted as pieces only where a *later* form starts on one of
    // its lines. Only its last line can be in that position: an interior
    // line is its own, and a later form starting on its first line would
    // have to start after it, which the file order forbids.
    const next = nextStartOn(u.line_end, u.index);
    if (next === undefined) {
      spans.push({ kind: 'lines', from: u.line_start, to: u.line_end });
    } else {
      // Leading indentation belongs to the form that owns the line, the
      // same as a whole-line wash would give it.
      const col =
        u.line_start === u.line_end && owner.get(u.line_end) !== u.index ? u.col_start : 1;
      spans.push({ kind: 'part', line: u.line_end, col, endCol: next.col_start });
      if (u.line_start < u.line_end) {
        spans.push({ kind: 'lines', from: u.line_start, to: u.line_end - 1 });
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
