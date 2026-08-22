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
 * The inline bar's geometry: how wide, how tall, and how far below the
 * baseline it hangs.
 *
 * All in `em`, so it grows with the font exactly as the gutter's SVG grows
 * with the line -- a bar measured in pixels matched at one font size and no
 * other, which is what made the two marks visibly different to begin with.
 *
 * The height is the load-bearing number, and it is bounded by where the
 * baseline is rather than by the line. An inline attachment's bottom margin
 * edge sits *on* the text baseline and the box extends upward, so anything
 * taller than the space above the baseline -- roughly the ascent plus half
 * the leading, about 0.9em in a 1.35em line -- pushes the line box open and
 * shifts the whole line. Drawing the gutter's own square SVG at
 * `height: '100%'` did exactly that.
 *
 * Hence the negative bottom margin. It drops the box below the baseline into
 * the descender space, which the line box already reserves and nothing else
 * here uses, buying height that costs nothing. The invariant to preserve when
 * adjusting these is the *difference*:
 *
 *     height - drop = how far the bar rises above the baseline
 *
 * Keep that at or under about 0.9em and the line cannot be pushed open; the
 * drop itself has roughly 0.35em of descender to spend. Raising both by the
 * same amount is therefore always safe, and is how to make the bar taller.
 */
const DROP = '0.25em';
export const INLINE_BAR = {
  width: '0.25em',
  height: '1.15em',
  // Top, right, bottom, left. The bottom is the drop; the sides just keep
  // the bar off the two forms it sits between.
  margin: `0 0.15em -${DROP} 0.1em`,
};
