// Which form owns which piece of which line.
//
// Two forms on one line is rare and legal, and it is the case both renderers
// here were built to be unable to express: a gutter has one cell per line,
// and a whole-line wash is whole-line. Precision about *where* a form sits
// does not by itself buy either of them anything -- something has to decide
// what to draw when a line holds two answers.
//
// The wash's answer is to paint a form's own extent and no more, so it never
// has to arbitrate: two forms on a line each take their own half, and the
// space between them belongs to neither. Only the gutter, which genuinely
// has one cell to give, still has to choose.
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
 * `endCol` of `null` means the end of the line's text: an opening line of a
 * form that carries on past it, where there is nothing after the form to
 * leave unpainted.
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
  /**
   * The form's first line: where a mark that is a single icon goes.
   *
   * Carried plainly rather than derived from `gutter`, which is the *lines
   * this form owns the gutter of* and is `null` for a form written second on
   * its opening line. A mark at the head of a form wants that head either
   * way, and there is nothing for it to collide with -- `minimal`, the only
   * mode that draws one, marks nothing else at all.
   */
  head: number;
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
 * A form is painted over exactly its own extent and no further. The lines it
 * spans in full are whole lines -- washed to the edge of the viewport, which
 * is what makes a multi-line form read as a block -- and the line it *ends*
 * on stops at its closing paren, so the wash does not run on past the form
 * into whatever follows. A line it begins partway through starts at its
 * opening paren for the same reason.
 *
 * Nothing overlaps, which is worth stating because an earlier version relied
 * on overlap and on the undocumented order VSCode draws decoration types in.
 * Painting only what a form covers removes the question: two forms on one
 * line paint their own halves, and the gap between them belongs to neither.
 */
export function paintMap(units: Placed[]): Painted[] {
  const owner = lineOwners(units);

  return units.map((u) => {
    const spans: Span[] = [];
    // Leading indentation goes to the form that owns the line, as a
    // whole-line wash would have given it; a form that starts partway along
    // begins at its own column, since the space before it is not its.
    const owns = owner.get(u.line_start) === u.index;
    const from = owns ? 1 : u.col_start;
    if (u.line_start === u.line_end) {
      spans.push({ kind: 'part', line: u.line_start, col: from, endCol: u.col_end });
    } else {
      // The opening line runs to the end of its text: the form continues, so
      // there is nothing beyond it on this line to leave alone.
      if (owns) {
        spans.push({ kind: 'lines', from: u.line_start, to: u.line_end - 1 });
      } else {
        spans.push({ kind: 'part', line: u.line_start, col: from, endCol: null });
        if (u.line_start + 1 <= u.line_end - 1) {
          spans.push({ kind: 'lines', from: u.line_start + 1, to: u.line_end - 1 });
        }
      }
      spans.push({ kind: 'part', line: u.line_end, col: 1, endCol: u.col_end });
    }
    // The gutter is a separate question: a unit can only ever lose its
    // *first* cell, since a later line of its own is shared only with a unit
    // that comes after it, and the earlier one wins.
    const cells = owns ? u.line_start : u.line_start + 1;
    return {
      index: u.index,
      status: u.status,
      head: u.line_start,
      spans,
      gutter: cells <= u.line_end ? { from: cells, to: u.line_end } : null,
      inlineAt: owns ? null : { line: u.line_start, col: u.col_start },
    };
  });
}
