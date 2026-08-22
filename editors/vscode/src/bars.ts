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
// Two more sources close that gap, and both work by producing a *concrete*
// colour before any surface sees it -- which is the only arrangement in which
// the gutter can ever agree with the rest.
//
// A colour in the `acl2-session.colors` setting is one. The other is the
// active theme, read as a file: `themeIds` says what each status means in
// VSCode's vocabulary, `withThemeHue` takes the hue the theme gives and keeps
// the alpha from the literal below, and the gutter icon is generated from the
// result. The alpha has to come from here because a theme colour has none and
// cannot be given one -- and it is doing real work, since these differ per
// status on purpose.
//
// Reading the theme can fail, and ordinarily does in part: most themes define
// a fraction of the colour list and let VSCode's built-in defaults carry the
// rest, and those live in no file. Failing means falling back to the literals
// below, which are themselves VSCode's default-theme values -- so on the
// default themes resolution reproduces exactly what is shipped, and it is
// only elsewhere that it changes anything.
//
// Kept free of `vscode`, like `positions.ts` and `paintmap.ts`, so all of it
// is testable without an editor.

export interface Bar {
  /** The `media/<icon>-{light,dark}.svg` basename, and the map key. */
  icon: string;
  /**
   * What this status *means* in VSCode's own vocabulary, best candidate
   * first, per theme kind.
   *
   * Tried in order against the active theme's colours, so a theme that says
   * what its error red is gets used, and one that only sets the ANSI palette
   * still contributes something sensible. Nothing matching is the ordinary
   * case, not a failure: most themes define a fraction of the colour list and
   * let VSCode's built-in defaults carry the rest, and those live in no file.
   */
  themeIds: { light: string[]; dark: string[] };
  /**
   * The statuses this bar speaks for, which is what a *user* names it by.
   * `skipped` and `failed` share one bar, as they share a colour.
   */
  statuses: string[];
  /** The bar's own colour, which must match the SVG's fill and opacity. */
  light: string;
  dark: string;
}

/**
 * The shipped palette: VSCode's own semantic colours.
 *
 * `editorInfo`, `editorWarning` and `editorError`'s foregrounds for running,
 * conditional and red, `charts.green` for admitted, and a neutral grey for
 * pending -- the values the built-in light and dark themes use, so the status
 * column looks native rather than looking like whoever picked it.
 *
 * The alphas are the older choice and are kept: they are what stops the
 * column shouting, and they differ per status on purpose -- a running form is
 * the one to look at, an admitted one is a fact you should be able to ignore.
 */
export const BARS: Bar[] = [
  {
    icon: 'pending',
    themeIds: {
      light: ['disabledForeground', 'descriptionForeground', 'terminal.ansiBrightBlack'],
      dark: ['disabledForeground', 'descriptionForeground', 'terminal.ansiBrightBlack'],
    },
    statuses: ['pending'],
    light: 'rgba(97, 97, 97, 0.4)',
    dark: 'rgba(204, 204, 204, 0.35)',
  },
  {
    icon: 'running',
    themeIds: {
      light: ['editorInfo.foreground', 'charts.blue', 'terminal.ansiBlue'],
      dark: ['editorInfo.foreground', 'charts.blue', 'terminal.ansiBrightBlue'],
    },
    statuses: ['running'],
    light: 'rgba(26, 133, 255, 0.85)',
    dark: 'rgba(55, 148, 255, 0.85)',
  },
  {
    icon: 'admitted',
    themeIds: {
      light: ['charts.green', 'testing.iconPassed', 'terminal.ansiGreen'],
      dark: ['charts.green', 'testing.iconPassed', 'terminal.ansiBrightGreen'],
    },
    statuses: ['admitted'],
    light: 'rgba(56, 138, 52, 0.55)',
    dark: 'rgba(137, 209, 133, 0.5)',
  },
  {
    icon: 'cond',
    themeIds: {
      light: ['editorWarning.foreground', 'charts.yellow', 'terminal.ansiYellow'],
      dark: ['editorWarning.foreground', 'charts.yellow', 'terminal.ansiBrightYellow'],
    },
    statuses: ['conditional'],
    light: 'rgba(191, 136, 3, 0.75)',
    dark: 'rgba(204, 167, 0, 0.7)',
  },
  {
    icon: 'red',
    themeIds: {
      light: ['editorError.foreground', 'charts.red', 'terminal.ansiRed'],
      dark: ['editorError.foreground', 'charts.red', 'terminal.ansiBrightRed'],
    },
    statuses: ['skipped', 'failed'],
    light: 'rgba(229, 20, 0, 0.7)',
    dark: 'rgba(241, 76, 76, 0.65)',
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

/**
 * The wash colours, by status: the bar's hue, faint.
 *
 * Translucent rather than opaque, which is the part that matters. An opaque
 * wash is a colour chosen against one editor background and fights every
 * other; a tint at this alpha sits on whatever background the theme has and
 * reads the same on all of them.
 */
export const WASHES: Record<string, { light: string; dark: string }> = {
  admitted: { light: 'rgba(56, 138, 52, 0.15)', dark: 'rgba(137, 209, 133, 0.1)' },
  conditional: { light: 'rgba(191, 136, 3, 0.15)', dark: 'rgba(204, 167, 0, 0.1)' },
  running: { light: 'rgba(26, 133, 255, 0.15)', dark: 'rgba(55, 148, 255, 0.1)' },
  skipped: { light: 'rgba(229, 20, 0, 0.15)', dark: 'rgba(241, 76, 76, 0.1)' },
  failed: { light: 'rgba(229, 20, 0, 0.15)', dark: 'rgba(241, 76, 76, 0.1)' },
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
 * Whether a colour is one an SVG can actually paint with.
 *
 * The gutter icon is an image, and an image is a separate document: it
 * cannot see the workbench's CSS variables, call `color-mix`, or resolve
 * anything but a literal colour. Those all work in the other three surfaces,
 * which are ordinary CSS -- so a value like
 * `color-mix(in srgb, var(--vscode-editorError-foreground) 65%, transparent)`
 * is a perfectly good override that this one surface has to decline.
 *
 * Declining means keeping the shipped icon, which is the honest outcome: a
 * status column that is a colour behind rather than a blank cell.
 */
export function svgCanPaint(color: string): boolean {
  return /^#[0-9a-f]{3,8}$/i.test(color) || /^rgba?\([\d\s.,%]+\)$/i.test(color) ||
    /^[a-z]+$/i.test(color);
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

/**
 * Take a colour's hue from the theme, keep the alpha from ours.
 *
 * The division of labour that makes reading the theme worth doing: the theme
 * says what red *is*, and the shipped literal says how loudly this status
 * should speak -- 0.85 for a running form, 0.5 for an admitted one. A theme
 * colour on its own carries no alpha and cannot be given one, so taking both
 * from the theme would mean a status column at full strength, and taking
 * neither is where this started.
 *
 * `SHIPPED` is one of our own `rgba(...)` literals, and is where the alpha
 * comes from; `hex` is whatever the theme had. Returns `undefined` if either
 * is not in the form expected, so a caller falls back rather than guesses.
 */
export function withThemeHue(shipped: string, hex: string): string | undefined {
  const alpha = /rgba?\([\d\s,]+?,\s*([0-9.]+)\)\s*$/.exec(shipped);
  const rgb = parseHex(hex);
  if (!alpha || !rgb) {
    return undefined;
  }
  // A theme colour may carry its own alpha (`#rrggbbaa`). Ours wins: it is
  // the part that is about this extension rather than about the theme.
  return `rgba(${rgb[0]}, ${rgb[1]}, ${rgb[2]}, ${alpha[1]})`;
}

/** `#rgb`, `#rrggbb` or `#rrggbbaa` as its three channels. */
function parseHex(hex: string): [number, number, number] | undefined {
  const m = /^#([0-9a-f]{3,8})$/i.exec(hex.trim());
  if (!m) {
    return undefined;
  }
  const h = m[1];
  const wide = h.length >= 6;
  if (!wide && h.length !== 3) {
    return undefined;
  }
  const at = (i: number) =>
    wide ? parseInt(h.slice(i * 2, i * 2 + 2), 16) : parseInt(h[i] + h[i], 16);
  const [r, g, b] = [at(0), at(1), at(2)];
  return [r, g, b].some(isNaN) ? undefined : [r, g, b];
}

/**
 * The first of `ids` the theme actually defines.
 *
 * Order is preference: what the status means, then a chart colour, then the
 * ANSI palette, which the most themes set and the `acl2Session.token.*`
 * fallbacks already lean on.
 */
export function firstDefined(
  colors: Record<string, string> | undefined,
  ids: string[]
): string | undefined {
  for (const id of ids) {
    const c = colors?.[id];
    if (typeof c === 'string' && parseHex(c)) {
      return c;
    }
  }
  return undefined;
}

/**
 * The wash's opacity, per theme kind.
 *
 * Separate from the wash colours themselves because it is the one number a
 * reader is likely to want to move: how far a tint reads depends on the
 * background under it, and a theme with low contrast can swallow what a high
 * contrast one shouts. The hue follows the theme now, so this is what is left
 * to tune. Overridable through `acl2-session.washOpacity`.
 */
export const WASH_ALPHA = { light: 0.15, dark: 0.1 };

/**
 * The same colour at a different opacity.
 *
 * Takes our own `rgba(...)` literals and the theme's `#rrggbb[aa]` alike,
 * since which one is in hand depends on how far down the chain we got.
 * `undefined` for anything else -- a CSS function, a named colour -- because
 * those cannot be taken apart, and a caller must leave them as they are.
 */
export function withAlpha(color: string, alpha: number): string | undefined {
  const m = /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*(?:,[\d.\s]+)?\)$/.exec(color.trim());
  const rgb = m ? [+m[1], +m[2], +m[3]] : parseHex(color);
  return rgb === undefined ? undefined : `rgba(${rgb[0]}, ${rgb[1]}, ${rgb[2]}, ${alpha})`;
}
