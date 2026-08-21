// The five status bars, in the one place both renderings read them from.
//
// A bar is drawn twice: as a gutter cell, from `media/<icon>-<theme>.svg`,
// and -- for a form that shares a line and so has no cell to claim -- inline,
// between the two forms. The gutter takes an image and the inline mark cannot
// (see `decorations.ts`), so the colours have to exist here as values as well
// as in those files, and `paintmap.mjs` asserts the two agree. That check is
// the point of this module: an earlier version kept the same table by hand
// and had no way of noticing if `media/` moved.
//
// Kept free of `vscode`, like `positions.ts` and `paintmap.ts`, so the check
// can run outside an editor.

export interface Bar {
  /** The `media/<icon>-{light,dark}.svg` basename, and the map key. */
  icon: string;
  /** The overview ruler's colour for this status. */
  ruler: string;
  /** The bar's own colour, which must match the SVG's fill and opacity. */
  light: string;
  dark: string;
}

export const BARS: Bar[] = [
  {
    icon: 'pending',
    ruler: 'rgba(136, 136, 136, 0.35)',
    light: 'rgba(97, 97, 97, 0.4)',
    dark: 'rgba(204, 204, 204, 0.35)',
  },
  {
    icon: 'running',
    ruler: 'rgba(90, 140, 210, 0.6)',
    light: 'rgba(21, 101, 192, 0.85)',
    dark: 'rgba(106, 159, 221, 0.85)',
  },
  {
    icon: 'admitted',
    ruler: 'rgba(80, 160, 90, 0.35)',
    light: 'rgba(46, 125, 50, 0.55)',
    dark: 'rgba(142, 192, 124, 0.5)',
  },
  {
    icon: 'cond',
    ruler: 'rgba(212, 172, 13, 0.5)',
    light: 'rgba(212, 172, 13, 0.75)',
    dark: 'rgba(255, 214, 51, 0.7)',
  },
  {
    icon: 'red',
    ruler: 'rgba(220, 60, 60, 0.6)',
    light: 'rgba(198, 40, 40, 0.7)',
    dark: 'rgba(251, 73, 52, 0.65)',
  },
];

/**
 * How wide the inline bar is, and how tall.
 *
 * In `em`, so it grows with the font exactly as the gutter's SVG grows with
 * the line -- a bar measured in pixels matched at one font size and no other,
 * which is what made the two marks visibly different in the first place.
 *
 * The height is the load-bearing number. An inline attachment sits on the
 * text's baseline and extends upward, so anything taller than the space above
 * the baseline pushes the line box open and the whole line shifts. A full
 * line-height bar -- which is what matching the gutter's proportions exactly
 * would need -- does that visibly. Just under the ascent does not, and costs
 * a bar a little shorter than the gutter's.
 */
export const INLINE_BAR = { width: '0.25em', height: '0.9em' };
