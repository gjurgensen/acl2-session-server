// The status column and the washes: two renderers over the same paint map.
//
// `minimal` (the default) is a continuous gutter status column of bars, the
// Dafny shape, and nothing else -- the buffer text stays entirely unmarked.
// Gray over lines queued but not yet processed (the load-bearing signal on
// a book that takes minutes), blue over the form being proved, green over
// admitted forms ("actually good" is a positive mark, not an absence),
// yellow over conditional ones, red over skipped/failed ones. Every status
// echoes in the overview ruler. The failure envelope's home is the panel;
// there are no diagnostics and no washes here, one mark per fact.
//
// `wash` draws whole-form background washes, reds included, and no
// diagnostics (the extension suppresses them in this mode).
//
// Both fill the scrollbar through the overview ruler's left lane; diagnostics
// mark the right lane on their own.

import * as vscode from 'vscode';
import { UnitState } from './protocol';
import { charOffset } from './positions';

export type DecorationMode = 'minimal' | 'wash';

export interface DecorationOptions {
  mode: DecorationMode;
  /** Mark admitted forms with the green bar; off, absence means admitted. */
  gutterAdmitted: boolean;
}

const WASH_COLORS: Record<string, { light: string; dark: string }> = {
  admitted: { light: '#c8e6c9', dark: '#1c2e15' },
  conditional: { light: '#d9e8f5', dark: '#2c2908' },
  skipped: { light: '#ffb3b3', dark: '#3d1e19' },
  failed: { light: '#ffb3b3', dark: '#3d1e19' },
  running: { light: '#fff59d', dark: '#1a2841' },
};

function gutterType(
  context: vscode.ExtensionContext,
  icon: string,
  ruler?: string
): vscode.TextEditorDecorationType {
  return vscode.window.createTextEditorDecorationType({
    ...(ruler
      ? { overviewRulerLane: vscode.OverviewRulerLane.Left, overviewRulerColor: ruler }
      : {}),
    light: {
      gutterIconPath: context.asAbsolutePath(`media/${icon}-light.svg`),
      gutterIconSize: 'contain',
    },
    dark: {
      gutterIconPath: context.asAbsolutePath(`media/${icon}-dark.svg`),
      gutterIconSize: 'contain',
    },
  });
}

export class Decorations {
  private washes = new Map<string, vscode.TextEditorDecorationType>();
  private pendingBar: vscode.TextEditorDecorationType;
  private runningBar: vscode.TextEditorDecorationType;
  private admittedBar: vscode.TextEditorDecorationType;
  private condBar: vscode.TextEditorDecorationType;
  private redBar: vscode.TextEditorDecorationType;
  private target: vscode.TextEditorDecorationType;
  private unreadable: vscode.TextEditorDecorationType;

  constructor(context: vscode.ExtensionContext) {
    for (const [status, c] of Object.entries(WASH_COLORS)) {
      const ruler =
        status === 'conditional'
          ? 'rgba(212, 172, 13, 0.5)'
          : status === 'skipped' || status === 'failed'
            ? 'rgba(220, 60, 60, 0.6)'
            : status === 'running'
              ? 'rgba(255, 204, 51, 0.6)'
              : undefined;
      this.washes.set(
        status,
        vscode.window.createTextEditorDecorationType({
          isWholeLine: true,
          ...(ruler
            ? { overviewRulerLane: vscode.OverviewRulerLane.Left, overviewRulerColor: ruler }
            : {}),
          light: { backgroundColor: c.light },
          dark: { backgroundColor: c.dark },
        })
      );
    }
    // The bars: pending neutral gray, running BLUE (true blue, hue 262),
    // admitted green, conditional YELLOW, red for the reds.
    this.pendingBar = gutterType(context, 'pending', 'rgba(136, 136, 136, 0.35)');
    this.runningBar = gutterType(context, 'running', 'rgba(90, 140, 210, 0.6)');
    this.admittedBar = gutterType(context, 'admitted', 'rgba(80, 160, 90, 0.35)');
    this.condBar = gutterType(context, 'cond', 'rgba(212, 172, 13, 0.5)');
    this.redBar = gutterType(context, 'red', 'rgba(220, 60, 60, 0.6)');
    // The held-target indicator: a Q.E.D.-style box.
    this.target = vscode.window.createTextEditorDecorationType({
      after: { contentText: ' □', color: '#fabd2f', fontWeight: 'bold' },
    });
    // The unreadable tail, from the paren that never closes to end of file.
    // Deliberately the same red as a failure: both mean "none of this is
    // going in". The hover says which, because the remedies are nothing
    // alike -- one wants a proof, the other wants a bracket.
    this.unreadable = vscode.window.createTextEditorDecorationType({
      isWholeLine: true,
      overviewRulerLane: vscode.OverviewRulerLane.Left,
      overviewRulerColor: 'rgba(220, 60, 60, 0.6)',
      light: { backgroundColor: WASH_COLORS.failed.light },
      dark: { backgroundColor: WASH_COLORS.failed.dark },
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
    // Whole lines, columns and all: a gutter has one cell per line and a wash
    // is `isWholeLine`, so neither can show two statuses on one line however
    // exactly the forms are located. Two forms on a line therefore paint over
    // each other -- rare, and the panel and the target glyph, which *can* be
    // exact, are where that line's forms are told apart.
    const span = (u: UnitState) => new vscode.Range(u.line_start - 1, 0, u.line_end - 1, 0);
    const of = (status: string) => units.filter((u) => u.status === status);

    const washRanges = new Map<string, vscode.Range[]>();
    for (const s of this.washes.keys()) {
      washRanges.set(s, []);
    }
    let pending: vscode.Range[] = [];
    let running: vscode.Range[] = [];
    let admitted: vscode.Range[] = [];
    let cond: vscode.Range[] = [];
    let red: vscode.Range[] = [];

    if (opts.mode === 'wash') {
      for (const u of units) {
        washRanges.get(u.status)?.push(span(u));
      }
    } else {
      pending = of('pending').map(span);
      running = of('running').map(span);
      if (opts.gutterAdmitted) {
        admitted = of('admitted').map(span);
        cond = of('conditional').map(span);
      }
      red = [...of('skipped'), ...of('failed')].map(span);
    }

    // Every type is always set, so switching modes cleans up after itself.
    for (const [status, t] of this.washes) {
      editor.setDecorations(t, washRanges.get(status) ?? []);
    }
    editor.setDecorations(this.pendingBar, pending);
    editor.setDecorations(this.runningBar, running);
    editor.setDecorations(this.admittedBar, admitted);
    editor.setDecorations(this.condBar, cond);
    editor.setDecorations(this.redBar, red);
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
    for (const t of this.washes.values()) {
      editor.setDecorations(t, []);
    }
    for (const t of this.bars()) {
      editor.setDecorations(t, []);
    }
    editor.setDecorations(this.unreadable, []);
  }

  dispose(): void {
    for (const t of this.washes.values()) {
      t.dispose();
    }
    for (const t of this.bars()) {
      t.dispose();
    }
    this.unreadable.dispose();
  }

  private bars(): vscode.TextEditorDecorationType[] {
    return [
      this.pendingBar,
      this.runningBar,
      this.admittedBar,
      this.condBar,
      this.redBar,
      this.target,
    ];
  }
}
