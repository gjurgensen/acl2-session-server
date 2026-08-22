// The status column and the washes: two renderers over the same paint map.
//
// `minimal` (the default) is a continuous gutter status column of bars, the
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
// Both renderers are line-granular by nature -- one gutter cell per line, and
// a wash that reaches the viewport edge -- so a line holding two forms needs
// each of them to say something it cannot say by default. The last form on
// such a line keeps the whole-line wash, so it still reaches the edge like
// every other line, and the forms before it are painted over it, each ending
// where the next begins; the gutter keeps its cell for the form that owns the
// start of the line and draws the second form's bar between the two.
// `paintmap.ts` decides all of that; this module only draws it.
//
// That overlap is deliberate and is the only way to say "from this column to
// the viewport edge": a whole-line decoration always begins at column 0, and
// `DecorationRenderOptions` has no z-index. What puts the column-bounded
// pieces on top is that their decoration types are created *after* the
// whole-line ones, which is the order VSCode renders them in -- unspecified,
// but relied on widely. If it ever renders the other way the symptom is
// visible and confined: on a line holding two forms, and only there, the
// earlier form's colour is swallowed by the later one's.
//
// Both fill the scrollbar through the overview ruler's left lane; diagnostics
// mark the right lane on their own.

import * as vscode from 'vscode';
import { UnitState } from './protocol';
import { charOffset } from './positions';
import { paintMap, Span } from './paintmap';
import {
  Bar,
  BARS,
  ColorSetting,
  INLINE_BAR,
  WASHES,
  barColorId,
  forTheme,
  gutterSvg,
  washColorId,
} from './bars';

export type DecorationMode = 'minimal' | 'wash';

export interface DecorationOptions {
  mode: DecorationMode;
  /** Mark admitted forms with the green bar; off, absence means admitted. */
  gutterAdmitted: boolean;
}

/**
 * A colour for one surface of one status, from the first source that has one.
 *
 * An override from the setting wins, because it is the only thing a user
 * wrote by hand; otherwise the contributed theme colour, which is what makes
 * a theme and `workbench.colorCustomizations` work. There is no third case
 * here -- the shipped literals reach only the gutter icon, which is an image
 * and takes no colour at all. See `gutterType`.
 */
function pick(override: string | undefined, id: string): string | vscode.ThemeColor {
  return override !== undefined ? override : new vscode.ThemeColor(id);
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
 * `bars.ts` holds it once and `paintmap.mjs` asserts it still matches
 * `media/`.
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
 * The gutter cell: the shipped SVG, or one generated for an overridden colour.
 *
 * The generated one travels as a `data:` URI rather than a written file --
 * there is nothing to clean up afterwards, and the icon changes whenever the
 * setting does. It is the *only* path in this module that a `ThemeColor`
 * cannot serve, which is the whole reason the setting exists.
 */
function gutterType(
  context: vscode.ExtensionContext,
  bar: Bar,
  override: { light?: string; dark?: string },
  ruler: string | vscode.ThemeColor
): vscode.TextEditorDecorationType {
  const icon = (theme: 'light' | 'dark'): vscode.Uri | string => {
    const c = override[theme];
    return c === undefined
      ? context.asAbsolutePath(`media/${bar.icon}-${theme}.svg`)
      : vscode.Uri.parse(
          `data:image/svg+xml;base64,${Buffer.from(gutterSvg(c), 'utf8').toString('base64')}`
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
  private target!: vscode.TextEditorDecorationType;
  private unreadable!: vscode.TextEditorDecorationType;

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
    const over = (status: string, surface: 'bar' | 'wash', theme: 'light' | 'dark') =>
      forTheme(set[status]?.[surface], theme);
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
      // Two types, one colour. Whole-line is the look this mode is for --
      // the wash reaching the edge of the viewport is what makes a form read
      // as a block. It is also the one thing that cannot express a line
      // holding two forms, so that line, and only that line, is painted to
      // its columns instead and the colour changes where the forms do.
      this.washes.set(
        status,
        vscode.window.createTextEditorDecorationType({ isWholeLine: true, ...paint })
      );
      this.washParts.set(status, vscode.window.createTextEditorDecorationType(paint));
    }

    // The bars: pending neutral gray, running BLUE (true blue, hue 262),
    // admitted green, conditional YELLOW, red for the reds.
    for (const bar of BARS) {
      // A bar can speak for two statuses (skipped and failed share one), and
      // an override on either recolours it.
      const named = bar.statuses.find((st) => set[st]?.bar !== undefined) ?? bar.statuses[0];
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

    // The held-target indicator: a Q.E.D.-style box.
    this.target = vscode.window.createTextEditorDecorationType({
      after: { contentText: ' \u25a1', color: '#fabd2f', fontWeight: 'bold' },
    });
    // The unreadable tail, from the paren that never closes to end of file.
    // Deliberately the same red as a failure: both mean "none of this is
    // going in". The hover says which, because the remedies are nothing
    // alike -- one wants a proof, the other wants a bracket.
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
  applyUnreadable(editor: vscode.TextEditor, line: number | null, why?: string): void {
    if (line === null) {
      editor.setDecorations(this.unreadable, []);
      return;
    }
    const doc = editor.document;
    const from = Math.max(0, Math.min(line, doc.lineCount) - 1);
    const last = Math.max(from, doc.lineCount - 1);
    editor.setDecorations(this.unreadable, [
      {
        range: new vscode.Range(from, 0, last, doc.lineAt(last).range.end.character),
        hoverMessage: `ACL2: ${why ?? 'the file cannot be read from here'} \u2014 nothing below this point has been checked.`,
      },
    ]);
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
      const to = charOffset(text, sp.endCol);
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

    // Which bar a status wears, and whether it wears one at all.
    const barOf = (status: string): string | undefined => {
      switch (status) {
        case 'pending':
          return 'pending';
        case 'running':
          return 'running';
        case 'admitted':
          return opts.gutterAdmitted ? 'admitted' : undefined;
        case 'conditional':
          return opts.gutterAdmitted ? 'cond' : undefined;
        case 'skipped':
        case 'failed':
          return 'red';
        default:
          return undefined;
      }
    };

    for (const p of painted) {
      if (opts.mode === 'wash') {
        for (const sp of p.spans) {
          const into = sp.kind === 'lines' ? washRanges : partRanges;
          into.get(p.status)?.push(range(sp));
        }
        continue;
      }
      const bar = barOf(p.status);
      if (bar === undefined) {
        continue;
      }
      if (p.gutter) {
        bars.get(bar)?.push(new vscode.Range(p.gutter.from - 1, 0, p.gutter.to - 1, 0));
      }
      // No gutter cell of its own: the bar goes between the two forms, at
      // the column this one starts in. A form that has both -- it opens on
      // a line it does not own and runs on past it -- gets both, which is
      // right: the mark says where it starts, the cells say how far it runs.
      if (p.inlineAt) {
        inline.get(bar)?.push(at(p.inlineAt));
      }
    }

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
   * Draw (or clear, with null) the held-target glyph.
   *
   * At the target's own column when it has one, rather than at the end of
   * its line: a target holding after the first of two forms on a line would
   * otherwise put the glyph after the second, which reads as including it.
   */
  applyTarget(editor: vscode.TextEditor, target: { line: number; column: number | null } | null): void {
    if (target === null) {
      editor.setDecorations(this.target, []);
      return;
    }
    if (target.line < 1) {
      // Held before the first form: the glyph leads the first line.
      editor.setDecorations(this.target, [new vscode.Range(0, 0, 0, 0)]);
      return;
    }
    const l = Math.min(target.line, editor.document.lineCount) - 1;
    const text = editor.document.lineAt(l).text;
    const at =
      target.column === null
        ? editor.document.lineAt(l).range.end
        : new vscode.Position(l, charOffset(text, target.column));
    editor.setDecorations(this.target, [new vscode.Range(at, at)]);
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
      this.target,
    ];
  }
}
