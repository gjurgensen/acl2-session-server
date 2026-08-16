// The status column and the washes: two renderers over the same paint map.
//
// `minimal` (the default) is a continuous gutter status column of bars, the
// Dafny shape: gray over lines queued but not yet processed -- the
// load-bearing signal on a book that takes minutes -- amber over the form
// being proved, green over admitted forms ("actually good" is a positive
// mark, not an absence), olive over conditional ones, red over
// skipped/failed ones. Exactly one background survives: the conditional
// wash, because "admitted assuming an unproved claim" is a region-shaped
// soundness caveat, not a per-form status. Reds additionally carry the
// diagnostic -- gutter presence plus a tight head-line squiggle is exactly
// vscode-lean4's error treatment, not a double-marking.
//
// `wash` draws whole-form background washes, reds included, and no
// diagnostics (the extension suppresses them in this mode).
//
// Both fill the scrollbar through the overview ruler's left lane; diagnostics
// mark the right lane on their own.

import * as vscode from 'vscode';
import { UnitState } from './protocol';

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
    // admitted green,
    // conditional YELLOW (the bar only; the conditional wash keeps its
    // olive family), red for the reds.
    this.pendingBar = gutterType(context, 'pending', 'rgba(136, 136, 136, 0.35)');
    this.runningBar = gutterType(context, 'running', 'rgba(90, 140, 210, 0.6)');
    this.admittedBar = gutterType(context, 'admitted', 'rgba(80, 160, 90, 0.35)');
    // The conditional wash already marks the ruler; the red diagnostic marks
    // its own lane. Neither bar doubles it.
    this.condBar = gutterType(context, 'cond');
    this.redBar = gutterType(context, 'red');
    // The held-target indicator: a Q.E.D.-style box.
    this.target = vscode.window.createTextEditorDecorationType({
      after: { contentText: ' □', color: '#fabd2f', fontWeight: 'bold' },
    });
  }

  apply(editor: vscode.TextEditor, units: UnitState[], opts: DecorationOptions): void {
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
      // The one surviving background; reds are the diagnostic plus the bar.
      washRanges.set(
        'conditional',
        of('conditional').map(span)
      );
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

  /** Draw (or clear, with null) the held-target glyph. */
  applyTarget(editor: vscode.TextEditor, line: number | null): void {
    if (line === null) {
      editor.setDecorations(this.target, []);
      return;
    }
    if (line < 1) {
      // Held before the first form: the glyph leads the first line.
      editor.setDecorations(this.target, [new vscode.Range(0, 0, 0, 0)]);
      return;
    }
    const l = Math.min(line, editor.document.lineCount) - 1;
    const end = editor.document.lineAt(l).range.end;
    editor.setDecorations(this.target, [new vscode.Range(end, end)]);
  }

  clear(editor: vscode.TextEditor): void {
    for (const t of this.washes.values()) {
      editor.setDecorations(t, []);
    }
    for (const t of this.bars()) {
      editor.setDecorations(t, []);
    }
  }

  dispose(): void {
    for (const t of this.washes.values()) {
      t.dispose();
    }
    for (const t of this.bars()) {
      t.dispose();
    }
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
