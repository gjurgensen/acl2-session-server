// The status column and the washes: two renderers over the same paint map.
//
// `gutter` (the default) is a continuous gutter status column of bars, the
// Dafny shape, and nothing else -- the buffer text stays entirely unmarked.
// Gray over lines queued but not yet processed (the load-bearing signal on
// a book that takes minutes), blue over the form being proved, green over
// admitted forms ("actually good" is a positive mark, not an absence),
// yellow over conditional ones, red over skipped/failed ones. Every status
// echoes in the overview ruler. The failure envelope's home is the form view;
// there are no diagnostics and no washes here, one mark per fact.
//
// `wash` draws whole-form background washes, reds included, and no
// diagnostics (the extension suppresses them in this mode).
//
// A wash covers a form's own extent and stops: whole lines through the block,
// and the line it ends on cut at its closing paren rather than running out to
// the viewport edge. The gutter is line-granular whatever the wash does -- one
// cell per line -- so it keeps its cell for the form that owns the start of a
// line and draws a second form's bar between the two. `paintmap.ts` decides
// all of that; this module only draws it.
//
// Nothing the two of them emit overlaps, which is the property to keep. An
// earlier version washed a shared line whole and painted the forms before it
// on top, and that depended on the order VSCode draws decoration types in --
// unspecified, since `DecorationRenderOptions` has no z-index. Painting only
// what a form covers needs no such assumption.
//
// Both fill the scrollbar through the overview ruler's left lane; diagnostics
// mark the right lane on their own.

import * as vscode from 'vscode';
import { UnitState } from './protocol';
import { charOffset } from './positions';
import { gutterMinus, paintMap, Span } from './paintmap';
import {
  Bar,
  BARS,
  ColorSetting,
  DecorationMode,
  INLINE_BAR,
  INLINE_TARGET,
  WASHES,
  barColorId,
  barFor,
  markFor,
  spinnerSvg,
  forTheme,
  firstDefined,
  gutterSvg,
  targetSvg,
  targetDownSvg,
  svgCanPaint,
  unreadableMark,
  washColorId,
  withAlpha,
  withThemeHue,
  WASH_ALPHA,
} from './bars';
import { themeColors } from './highlight';

// The modes and what each draws live in `bars.ts`, beside the table of bars
// they choose between; re-exported so a caller needs one import, not two.
export { DecorationMode };

export interface DecorationOptions {
  mode: DecorationMode;
  /** Where the held target's mark goes, so no status contests its cell. */
  target?: { line: number; inline: { line: number; col: number } | null } | null;
}

/**
 * The last step of choosing a colour: a concrete one if we have it, and
 * otherwise the contributed `ThemeColor`.
 *
 * `resolved` is whatever `build` worked out -- what the user wrote, or the
 * theme's own word for this status with our alpha kept. Falling through to
 * the `ThemeColor` is the case where the theme could not be read at all, and
 * it still gets `workbench.colorCustomizations` and the shipped default; the
 * gutter, which cannot take a `ThemeColor`, falls back to its SVG instead.
 */
function pick(resolved: string | undefined, id: string): string | vscode.ThemeColor {
  return resolved !== undefined ? resolved : new vscode.ThemeColor(id);
}

/**
 * The gutter bar, drawn between two forms that share a line.
 *
 * A coloured rectangle rather than the gutter's own SVG, and the reason is
 * geometry rather than taste. That SVG is square, with the bar four units of
 * eighteen across, so `contain` ties the bar's *thickness* to its *height*:
 * to be as thick as the gutter's it has to be as tall as the line. An inline
 * attachment sits on the text's baseline and grows upward, so a line-height
 * one reaches above the line box and pushes it open -- the whole line
 * visibly shifts. Drawing it with the real asset was tried and did that.
 *
 * A rectangle takes its width and height independently, so it can have the
 * gutter's thickness without its height. Both are in `em`, which is what the
 * first attempt got wrong: it was a border measured in pixels, matching the
 * SVG at one font size and drifting at every other.
 *
 * The colour has to live here as a value, since this is not an image.
 * `bars.ts` holds the shipped one once and `paintmap.mjs` asserts it still
 * matches `media/`.
 */
function inlineBarType(
  light: string | vscode.ThemeColor,
  dark: string | vscode.ThemeColor
): vscode.TextEditorDecorationType {
  const mark = (color: string | vscode.ThemeColor) => ({
    before: {
      // A space, not an empty string: an empty attachment has no box for a
      // background to fill. `width` overrides what the space would take.
      contentText: ' ',
      backgroundColor: color,
      // Width, height and the margin that drops it below the baseline all
      // come together, because the last two only make sense as a pair.
      ...INLINE_BAR,
    },
  });
  return vscode.window.createTextEditorDecorationType({
    light: mark(light),
    dark: mark(dark),
  });
}

/**
 * The gutter cell: an icon generated for a known colour, or the shipped one.
 *
 * `colour` is concrete or absent, never a `ThemeColor`, because this is the
 * surface that cannot resolve one -- an image is its own document, with no
 * sight of the workbench's CSS variables. Everything else in this module can
 * take a `ThemeColor` and this cannot, which is why `build` works out one
 * concrete value for all four surfaces whenever it can: it is the only way
 * the status column and the rest of the paint ever agree.
 *
 * The generated icon travels as a `data:` URI rather than a written file --
 * nothing to clean up, and it changes whenever the theme or the setting does.
 */
function gutterType(
  context: vscode.ExtensionContext,
  bar: Bar,
  colour: { light?: string; dark?: string },
  ruler: string | vscode.ThemeColor
): vscode.TextEditorDecorationType {
  const icon = (theme: 'light' | 'dark'): vscode.Uri | string => {
    const c = colour[theme];
    // A colour an image cannot paint with -- a CSS variable, a `color-mix`
    // -- leaves the shipped icon in place rather than producing an SVG that
    // renders as nothing. The other three surfaces take it happily.
    return c === undefined || !svgCanPaint(c)
      ? context.asAbsolutePath(`media/${bar.icon}-${theme}.svg`)
      : vscode.Uri.parse(
          `data:image/svg+xml;base64,${Buffer.from(gutterSvg(c, bar.pulse), 'utf8').toString('base64')}`
        );
  };
  return vscode.window.createTextEditorDecorationType({
    overviewRulerLane: vscode.OverviewRulerLane.Left,
    overviewRulerColor: ruler,
    light: { gutterIconPath: icon('light'), gutterIconSize: 'contain' },
    dark: { gutterIconPath: icon('dark'), gutterIconSize: 'contain' },
  });
}

export class Decorations {
  private washes = new Map<string, vscode.TextEditorDecorationType>();
  /** The same washes, bounded by column: for a line two forms share. */
  private washParts = new Map<string, vscode.TextEditorDecorationType>();
  /** The status column: one gutter cell per line. */
  private gutterBars = new Map<string, vscode.TextEditorDecorationType>();
  /** The same bar inline, for a form with no gutter cell to claim. */
  private inlineBars = new Map<string, vscode.TextEditorDecorationType>();
  /** `minimal`'s one mark: a spinner at the head of the running form. */
  private spinner!: vscode.TextEditorDecorationType;
  private target!: vscode.TextEditorDecorationType;
  private targetInline!: vscode.TextEditorDecorationType;
  private unreadable!: vscode.TextEditorDecorationType;
  /**
   * The same tail in the status column, for the mode that speaks in bars.
   *
   * Its own type rather than the `red` one the failed forms use: setting a
   * decoration type replaces every range it holds, so sharing would mean
   * whichever of the two painted last erased the other.
   */
  private unreadableBar!: vscode.TextEditorDecorationType;

  constructor(private readonly context: vscode.ExtensionContext) {
    this.build();
  }

  /**
   * Rebuild every type from the settings as they stand now.
   *
   * A decoration type carries its colours from the moment it is created, so
   * a changed setting means new types -- there is nothing to mutate. Callers
   * must re-apply afterwards: disposing a type removes what it was drawing.
   */
  reload(): void {
    for (const t of this.everyType()) {
      t.dispose();
    }
    this.washes.clear();
    this.washParts.clear();
    this.gutterBars.clear();
    this.inlineBars.clear();
    this.build();
  }

  private build(): void {
    const set =
      vscode.workspace.getConfiguration('acl2-session').get<ColorSetting>('colors') ?? {};
    // What the active theme calls each of these, when it can be read. Read
    // once per build rather than per colour: it is a file parse.
    const theme = themeColors();
    // And what the reader has said about our own ids in
    // `workbench.colorCustomizations`. Read here rather than left to a
    // `ThemeColor` because resolving to a value is what lets the gutter
    // follow: referencing the id would customise three surfaces and leave the
    // status column behind. Theme-scoped entries win over global ones, the
    // same precedence VSCode applies and `highlight.ts` already honours for
    // the token colours.
    const custom = (() => {
      const all =
        vscode.workspace
          .getConfiguration('workbench')
          .get<Record<string, unknown>>('colorCustomizations') ?? {};
      const label = vscode.workspace.getConfiguration('workbench').get<string>('colorTheme');
      const scoped = (label ? all[`[${label}]`] : undefined) as
        | Record<string, unknown>
        | undefined;
      return (id: string): string | undefined => {
        const v = scoped?.[id] ?? all[id];
        return typeof v === 'string' ? v : undefined;
      };
    })();
    // How strong a wash is, which is the number a theme changes the feel of:
    // the same tint reads differently over different backgrounds.
    const washAlpha = (kind: 'light' | 'dark'): number => {
      const set = vscode.workspace
        .getConfiguration('acl2-session')
        .get<number | { light?: number; dark?: number }>('washOpacity');
      const v = typeof set === 'number' ? set : set?.[kind];
      return typeof v === 'number' && v >= 0 && v <= 1 ? v : WASH_ALPHA[kind];
    };
    // The colour to use, from the first source that has one: what the user
    // wrote, then the theme's own word for this status with our alpha kept,
    // and otherwise nothing -- which leaves the contributed theme colour to
    // the CSS surfaces and the shipped icon to the gutter.
    //
    // Deliberately one answer for all four surfaces. The gutter is an image
    // and cannot resolve a `ThemeColor` or a CSS variable, so the only way it
    // ever agrees with the other three is for the value to be concrete before
    // any of them see it.
    const resolve = (
      status: string,
      surface: 'bar' | 'wash',
      kind: 'light' | 'dark',
      shipped: string,
      ids: string[],
      id: string
    ): string | undefined => {
      // Ours first: it is the only setting that is about *this* extension,
      // and it is fully explicit, so not even the opacity dial rewrites it.
      const written = forTheme(set[status]?.[surface], kind);
      if (written !== undefined) {
        return written;
      }
      // Then what the reader customised the contributed id to, then the
      // theme's own word for this status, then what we ship. Each of the
      // three is a hue that our own opacity is applied to.
      const hue = custom(id) ?? firstDefined(theme, ids);
      const base = hue === undefined ? shipped : (withThemeHue(shipped, hue) ?? hue);
      if (surface === 'bar') {
        // No dial for the bars: their opacities differ per status on purpose
        // and are not a matter of taste about the background.
        return hue === undefined ? undefined : base;
      }
      const alpha = washAlpha(kind);
      return withAlpha(base, alpha) ?? (hue === undefined ? undefined : base);
    };
    const barOfStatus = (status: string) =>
      BARS.find((b) => b.statuses.includes(status)) ?? BARS[BARS.length - 1];
    const over = (status: string, surface: 'bar' | 'wash', kind: 'light' | 'dark') => {
      const bar = barOfStatus(status);
      const shipped = surface === 'wash' ? WASHES[status]?.[kind] : bar[kind];
      const id = surface === 'wash' ? washColorId(status) : barColorId(status);
      return resolve(status, surface, kind, shipped ?? bar[kind], bar.themeIds[kind], id);
    };
    // The ruler follows the *bar*, whatever the surface: it is the same
    // status seen from the scrollbar, and an override that recoloured the
    // column while leaving the ruler behind would be half a change.
    const rulerOf = (status: string) =>
      pick(over(status, 'bar', 'dark') ?? over(status, 'bar', 'light'), barColorId(status));

    for (const status of Object.keys(WASHES)) {
      // Admitted alone gets no ruler mark: on a book that goes green it
      // would be a solid bar down the scrollbar saying nothing.
      const paint = {
        ...(status === 'admitted'
          ? {}
          : {
              overviewRulerLane: vscode.OverviewRulerLane.Left,
              overviewRulerColor: rulerOf(status),
            }),
        light: { backgroundColor: pick(over(status, 'wash', 'light'), washColorId(status)) },
        dark: { backgroundColor: pick(over(status, 'wash', 'dark'), washColorId(status)) },
      };
      // Two types, one colour: whole lines for the block a multi-line form
      // fills, and a column-bounded one for the lines it only partly covers
      // -- the line it ends on, and a line it starts partway along.
      this.washes.set(
        status,
        vscode.window.createTextEditorDecorationType({ isWholeLine: true, ...paint })
      );
      this.washParts.set(status, vscode.window.createTextEditorDecorationType(paint));
    }

    // The bars: neutral grey for queued work and, pulsing, for the form
    // being proved -- admitted green, conditional YELLOW, red for the reds.
    // Running used to be blue; the pulse says it better than a fifth hue did.
    for (const bar of BARS) {
      // The target is in this table for its colour only. It is not a status
      // and has no status bar; its own two shapes are built below.
      if (bar.icon === 'target') {
        continue;
      }
      // A bar can speak for two statuses (skipped and failed share one), and
      // an override on either recolours it.
      const named = bar.statuses.find((st) => set[st]?.bar !== undefined) ?? bar.statuses[0];
      // One bar, possibly two statuses: an override on either names it.
      const light = over(named, 'bar', 'light');
      const dark = over(named, 'bar', 'dark');
      this.gutterBars.set(
        bar.icon,
        gutterType(this.context, bar, { light, dark }, rulerOf(named))
      );
      this.inlineBars.set(
        bar.icon,
        inlineBarType(pick(light, barColorId(named)), pick(dark, barColorId(named)))
      );
    }

    // The spinner, built from the running bar's own resolved colour so the two
    // modes agree about what "working" looks like, whatever the theme said.
    {
      const c = { light: over('running', 'bar', 'light'), dark: over('running', 'bar', 'dark') };
      const spin = (theme: 'light' | 'dark'): vscode.Uri | string => {
        const c0 = c[theme];
        return c0 === undefined || !svgCanPaint(c0)
          ? this.context.asAbsolutePath(`media/spinner-${theme}.svg`)
          : vscode.Uri.parse(
              `data:image/svg+xml;base64,${Buffer.from(spinnerSvg(c0), 'utf8').toString('base64')}`
            );
      };
      this.spinner = vscode.window.createTextEditorDecorationType({
        overviewRulerLane: vscode.OverviewRulerLane.Left,
        overviewRulerColor: pick(c.dark, barColorId('running')),
        light: { gutterIconPath: spin('light'), gutterIconSize: 'contain' },
        dark: { gutterIconPath: spin('dark'), gutterIconSize: 'contain' },
      });
    }

    // The held target, in two shapes for the two places it can fall.
    //
    // It replaces a gold phantom square drawn at whatever column the target
    // was set from. That mark said "something is here" without saying what,
    // sat in a hue a hand's breadth from the conditional yellow, and pointed
    // at a position that was the barrier only by coincidence.
    {
      const c = { light: over('target', 'bar', 'light'), dark: over('target', 'bar', 'dark') };
      const rule = (theme: 'light' | 'dark'): vscode.Uri | string => {
        const c0 = c[theme];
        return c0 === undefined || !svgCanPaint(c0)
          ? this.context.asAbsolutePath(`media/target-${theme}.svg`)
          : vscode.Uri.parse(
              `data:image/svg+xml;base64,${Buffer.from(targetSvg(c0), 'utf8').toString('base64')}`
            );
      };
      // The ruler mark is line-resolution, which is all a scrollbar has: two
      // forms on a line share a tick. That is the right precision for "look
      // around here" and the wrong precision for the barrier itself, which is
      // why the two marks below exist and the tick does not replace them.
      this.target = vscode.window.createTextEditorDecorationType({
        overviewRulerLane: vscode.OverviewRulerLane.Left,
        overviewRulerColor: pick(c.dark ?? c.light, barColorId('target')),
        light: { gutterIconPath: rule('light'), gutterIconSize: 'contain' },
        dark: { gutterIconPath: rule('dark'), gutterIconSize: 'contain' },
      });
      // And where the gutter cannot reach. A gutter cell addresses a line,
      // so a barrier falling between two forms written on one line has no
      // cell of its own: the cell belongs to the form that starts the line,
      // which is on the *other* side of the barrier. Drawn in the text
      // instead, at the column the second form begins -- the same escape
      // hatch `paintmap` already gives a status in that position.
      const down = (theme: 'light' | 'dark'): vscode.Uri | string => {
        const c0 = c[theme];
        return c0 === undefined || !svgCanPaint(c0)
          ? this.context.asAbsolutePath(`media/target-down-${theme}.svg`)
          : vscode.Uri.parse(
              `data:image/svg+xml;base64,` +
                `${Buffer.from(targetDownSvg(c0), 'utf8').toString('base64')}`
            );
      };
      this.targetInline = vscode.window.createTextEditorDecorationType({
        light: { before: { ...INLINE_TARGET, contentIconPath: down('light') } },
        dark: { before: { ...INLINE_TARGET, contentIconPath: down('dark') } },
      });
    }
    // The unreadable tail, from the paren that never closes to end of file.
    // Deliberately the same red as a failure: both mean "none of this is
    // going in". The hover says which, because the remedies are nothing
    // alike -- one wants a proof, the other wants a bracket.
    this.unreadableBar = gutterType(
      this.context,
      BARS.find((b) => b.icon === 'red') ?? BARS[BARS.length - 1],
      {},
      rulerOf('failed')
    );
    this.unreadable = vscode.window.createTextEditorDecorationType({
      isWholeLine: true,
      overviewRulerLane: vscode.OverviewRulerLane.Left,
      overviewRulerColor: rulerOf('failed'),
      light: { backgroundColor: pick(over('failed', 'wash', 'light'), washColorId('failed')) },
      dark: { backgroundColor: pick(over('failed', 'wash', 'dark'), washColorId('failed')) },
    });
  }

  /**
   * Paint from `line` to the end of the buffer as unreadable, or clear it
   * with `null`.
   *
   * The whole tail rather than the one line, because that is what is true:
   * the reader stopped at that paren, so nothing below it has been seen, and
   * marking only the opening line would suggest the rest is merely waiting.
   */
  applyUnreadable(
    editor: vscode.TextEditor,
    line: number | null,
    mode: DecorationMode,
    why?: string
  ): void {
    const how = line === null ? undefined : unreadableMark(mode);
    if (line === null || how === undefined) {
      // Both, always: the mode may have just changed, and the mark it used to
      // wear has to come off even though nothing about the file did.
      editor.setDecorations(this.unreadable, []);
      editor.setDecorations(this.unreadableBar, []);
      return;
    }
    const doc = editor.document;
    const from = Math.max(0, Math.min(line, doc.lineCount) - 1);
    const last = Math.max(from, doc.lineCount - 1);
    const range = new vscode.Range(from, 0, last, doc.lineAt(last).range.end.character);
    const marks = [
      {
        range,
        hoverMessage: `ACL2: ${why ?? 'the file cannot be read from here'} \u2014 nothing below this point has been checked.`,
      },
    ];
    editor.setDecorations(this.unreadable, how === 'wash' ? marks : []);
    editor.setDecorations(this.unreadableBar, how === 'bar' ? marks : []);
  }

  apply(editor: vscode.TextEditor, units: UnitState[], opts: DecorationOptions): void {
    const doc = editor.document;
    // Where each form's paint goes, decided in `paintmap.ts` -- which form
    // owns which gutter cell, and which lines have to be painted to their
    // columns because two forms share them.
    const painted = paintMap(units);

    const range = (sp: Span): vscode.Range => {
      if (sp.kind === 'lines') {
        return new vscode.Range(sp.from - 1, 0, sp.to - 1, 0);
      }
      const line = Math.min(Math.max(sp.line, 1), doc.lineCount) - 1;
      const text = doc.lineAt(line).text;
      const from = charOffset(text, sp.col);
      const to = sp.endCol === null ? text.length : charOffset(text, sp.endCol);
      return new vscode.Range(line, from, line, Math.max(from, to));
    };
    // A point, not a span: the mark is a `before` attachment on the second
    // form's opening paren, so the icon is the content and the range only
    // says where to hang it.
    const at = (p: { line: number; col: number }): vscode.Range => {
      const line = Math.min(Math.max(p.line, 1), doc.lineCount) - 1;
      const c = charOffset(doc.lineAt(line).text, p.col);
      return new vscode.Range(line, c, line, c);
    };

    const washRanges = new Map<string, vscode.Range[]>();
    const partRanges = new Map<string, vscode.Range[]>();
    for (const st of this.washes.keys()) {
      washRanges.set(st, []);
      partRanges.set(st, []);
    }
    const bars = new Map<string, vscode.Range[]>();
    const inline = new Map<string, vscode.Range[]>();
    for (const bar of BARS) {
      bars.set(bar.icon, []);
      inline.set(bar.icon, []);
    }

    // The cell the target's mark takes, so no status tries to share it.
    // Null in `wash` and `minimal`, which draw no bars for it to displace.
    const contested = opts.mode === 'gutter' ? (opts.target ?? null) : null;
    const targetLine = contested && !contested.inline ? contested.line : null;
    const targetInline = contested?.inline ?? null;
    const sameSpot = (a: { line: number; col: number }, b: { line: number; col: number }) =>
      a.line === b.line && a.col === b.col;

    // Spinners: at the head of the form, one line, never a column.
    const spins: vscode.Range[] = [];

    for (const p of painted) {
      if (opts.mode === 'wash') {
        for (const sp of p.spans) {
          const into = sp.kind === 'lines' ? washRanges : partRanges;
          into.get(p.status)?.push(range(sp));
        }
        continue;
      }
      // What this status is drawn as in this mode: `bars.ts`, where the three
      // modes differ and can be tested without an editor.
      const mark = markFor(opts.mode, p.status);
      if (mark === undefined) {
        continue;
      }
      if (mark.kind === 'spinner') {
        // The form's own first line, whether or not it owns that line's gutter
        // cell -- there is nothing else competing for it in this mode, so the
        // shared-line question the bars have to answer does not arise.
        spins.push(new vscode.Range(p.head - 1, 0, p.head - 1, 0));
        continue;
      }
      // Minus whatever cell the target's mark has claimed: one icon per cell,
      // and the mark is the one worth seeing there.
      const g = gutterMinus(p.gutter, targetLine);
      if (g) {
        bars.get(mark.icon)?.push(new vscode.Range(g.from - 1, 0, g.to - 1, 0));
      }
      // No gutter cell of its own: the bar goes between the two forms, at
      // the column this one starts in. A form that has both -- it opens on
      // a line it does not own and runs on past it -- gets both, which is
      // right: the mark says where it starts, the cells say how far it runs.
      // Same rule inline. Two `before` attachments at one column stack rather
      // than overdraw, so this is legibility rather than necessity -- but a
      // boundary said twice, in two shapes, is a boundary said worse.
      if (p.inlineAt && !(targetInline && sameSpot(p.inlineAt, targetInline))) {
        inline.get(mark.icon)?.push(at(p.inlineAt));
      }
    }
    editor.setDecorations(this.spinner, spins);

    // Every type is always set, so switching modes cleans up after itself.
    for (const [status, t] of this.washes) {
      editor.setDecorations(t, washRanges.get(status) ?? []);
    }
    for (const [status, t] of this.washParts) {
      editor.setDecorations(t, partRanges.get(status) ?? []);
    }
    for (const [icon, t] of this.gutterBars) {
      editor.setDecorations(t, bars.get(icon) ?? []);
    }
    for (const [icon, t] of this.inlineBars) {
      editor.setDecorations(t, inline.get(icon) ?? []);
    }
  }

  /**
   * Draw (or clear, with null) the mark for the held target.
   *
   * `at` is the *first form beyond* the barrier, not the last form inside it,
   * and the reason is which gutter has room. Beside an included form the
   * column is fully spoken for -- green, yellow and red each say something
   * nothing else says -- so marking there would displace a verdict rather
   * than add a mark. Beyond it every form is the same grey, hundreds of them,
   * so specialising the first costs nothing and supplies the one fact that
   * region needs: where it starts.
   *
   * `inline` when the form does not own its line's gutter cell. See the
   * construction of the two types.
   */
  applyTarget(
    editor: vscode.TextEditor,
    at: { line: number; inline: { line: number; col: number } | null } | null
  ): void {
    if (at === null) {
      editor.setDecorations(this.target, []);
      editor.setDecorations(this.targetInline, []);
      return;
    }
    if (at.inline) {
      const l = Math.min(Math.max(at.inline.line, 1), editor.document.lineCount) - 1;
      const c = charOffset(editor.document.lineAt(l).text, at.inline.col);
      editor.setDecorations(this.target, []);
      editor.setDecorations(this.targetInline, [new vscode.Range(l, c, l, c)]);
      return;
    }
    const l = Math.min(Math.max(at.line, 1), editor.document.lineCount) - 1;
    editor.setDecorations(this.target, [new vscode.Range(l, 0, l, 0)]);
    editor.setDecorations(this.targetInline, []);
  }

  clear(editor: vscode.TextEditor): void {
    for (const t of this.everyType()) {
      editor.setDecorations(t, []);
    }
    editor.setDecorations(this.unreadable, []);
  }

  dispose(): void {
    for (const t of this.everyType()) {
      t.dispose();
    }
    this.unreadable.dispose();
  }

  private everyType(): vscode.TextEditorDecorationType[] {
    return [
      ...this.washes.values(),
      ...this.washParts.values(),
      ...this.gutterBars.values(),
      ...this.inlineBars.values(),
      this.unreadableBar,
      this.target,
    ];
  }
}
