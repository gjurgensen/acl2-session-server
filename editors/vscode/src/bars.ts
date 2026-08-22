// The status palette: what colour each status is, and where that colour is
// allowed to come from.
//
// Three sources, in order. A **contributed theme colour** is the default and
// the one a VSCode user expects -- overridable through
// `workbench.colorCustomizations`, restylable by a theme, with high-contrast
// variants -- and it is what the wash, the inline bar and the overview ruler
// use. The **gutter bar cannot take one**: `gutterIconPath` wants an image,
// and a `ThemeColor` is an opaque id with no API to resolve it to a value, so
// the gutter falls back to the shipped `media/<icon>-<theme>.svg`, whose fill
// is baked in. That is why the same colour exists here as a literal too, and
// why `paintmap.mjs` asserts the two still agree.
//
// The third source closes that gap: a concrete colour in the
// `acl2-session.colors` setting. Given one, the gutter icon is *generated*
// from it, so an override reaches the one surface a theme colour cannot --
// and reaches the other three as well, since a status column whose two
// renderings disagree is the bug this module was written to prevent.
//
// Kept free of `vscode`, like `positions.ts` and `paintmap.ts`, so all of it
// is testable without an editor.

export interface Bar {
  /** The `media/<icon>-{light,dark}.svg` basename, and the map key. */
  icon: string;
  /**
   * The statuses this bar speaks for, which is what a *user* names it by.
   * `skipped` and `failed` share one bar, as they share a colour.
   */
  statuses: string[];
  /** The overview ruler's colour for this status. */
  ruler: string;
  /** The bar's own colour, which must match the SVG's fill and opacity. */
  light: string;
  dark: string;
}

export const BARS: Bar[] = [
  {
    icon: 'pending',
    statuses: ['pending'],
    ruler: 'rgba(136, 136, 136, 0.35)',
    light: 'rgba(97, 97, 97, 0.4)',
    dark: 'rgba(204, 204, 204, 0.35)',
  },
  {
    icon: 'running',
    statuses: ['running'],
    ruler: 'rgba(90, 140, 210, 0.6)',
    light: 'rgba(21, 101, 192, 0.85)',
    dark: 'rgba(106, 159, 221, 0.85)',
  },
  {
    icon: 'admitted',
    statuses: ['admitted'],
    ruler: 'rgba(80, 160, 90, 0.35)',
    light: 'rgba(46, 125, 50, 0.55)',
    dark: 'rgba(142, 192, 124, 0.5)',
  },
  {
    icon: 'cond',
    statuses: ['conditional'],
    ruler: 'rgba(212, 172, 13, 0.5)',
    light: 'rgba(212, 172, 13, 0.75)',
    dark: 'rgba(255, 214, 51, 0.7)',
  },
  {
    icon: 'red',
    statuses: ['skipped', 'failed'],
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

/** The wash colours, by status. Backgrounds, so far paler than the bars. */
export const WASHES: Record<string, { light: string; dark: string }> = {
  admitted: { light: '#c8e6c9', dark: '#1c2e15' },
  conditional: { light: '#d9e8f5', dark: '#2c2908' },
  skipped: { light: '#ffb3b3', dark: '#3d1e19' },
  failed: { light: '#ffb3b3', dark: '#3d1e19' },
  running: { light: '#fff59d', dark: '#1a2841' },
};

/** The contributed theme-colour id for a status's bar, and for its wash. */
export const barColorId = (status: string): string => `acl2Session.bar.${status}`;
export const washColorId = (status: string): string => `acl2Session.wash.${status}`;

/**
 * One colour as `acl2-session.colors` may give it: for both theme kinds at
 * once, or one per kind. Anything absent falls through to the theme colour.
 */
export type ColorOverride = string | { light?: string; dark?: string };

/** The `acl2-session.colors` setting: per status, per surface. */
export type ColorSetting = Record<string, { bar?: ColorOverride; wash?: ColorOverride }>;

/** The half of an override that applies to one theme kind. */
export function forTheme(c: ColorOverride | undefined, theme: 'light' | 'dark'): string | undefined {
  if (c === undefined) {
    return undefined;
  }
  return typeof c === 'string' ? c : c[theme];
}

/**
 * The gutter icon's SVG, generated for a colour the setting supplied.
 *
 * Deliberately the same geometry as the shipped files -- an 18-unit cell with
 * a 4-unit bar inset by one -- because an override should change the colour
 * of the status column and nothing else about it. `fill` takes the whole
 * colour, opacity included, rather than splitting it the way the files do:
 * a user writing `#fb4934` or `rgba(...)` should not have to know that the
 * shipped assets happen to carry their alpha in a separate attribute.
 */
export function gutterSvg(color: string): string {
  return (
    '<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 18 18">' +
    `<rect x="1" y="0" width="4" height="18" fill="${escapeAttr(color)}"/>` +
    '</svg>'
  );
}

/** A colour string is user input, and it is about to go inside an attribute. */
function escapeAttr(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}
