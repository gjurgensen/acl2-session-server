// The status column and the washes: two renderers over the same paint map.
//
// `minimal` (the default) is the Lean-shaped one. A gutter bar marks every
// line that is queued but not yet processed -- the load-bearing signal on a
// book that takes minutes -- and turns bright over the form being proved. An
// admitted form gets a checkmark on its first line, so "actually good" is a
// positive mark, not an absence. Exactly one background survives: the
// conditional wash, because "admitted assuming an unproved claim" is a
// region-shaped soundness caveat, not a per-form status. Reds carry no paint
// here at all -- they are diagnostics, and painting them too would say the
// same thing twice.
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
  gutterChecks: boolean;
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
  private checkOk: vscode.TextEditorDecorationType;
  private checkCond: vscode.TextEditorDecorationType;
  private target: vscode.TextEditorDecorationType;

  constructor(context: vscode.ExtensionContext) {
    for (const [status, c] of Object.entries(WASH_COLORS)) {
      const ruler =
        status === 'conditional'
          ? 'rgba(184, 187, 38, 0.5)'
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
    this.pendingBar = gutterType(context, 'pending', 'rgba(136, 136, 136, 0.35)');
    this.runningBar = gutterType(context, 'running', 'rgba(255, 204, 51, 0.6)');
    this.checkOk = gutterType(context, 'check');
    this.checkCond = gutterType(context, 'check-cond');
    // The held-target indicator: a Q.E.D.-style box.
    this.target = vscode.window.createTextEditorDecorationType({
      after: { contentText: ' □', color: '#fabd2f', fontWeight: 'bold' },
    });
  }

  apply(editor: vscode.TextEditor, units: UnitState[], opts: DecorationOptions): void {
    const span = (u: UnitState) => new vscode.Range(u.line_start - 1, 0, u.line_end - 1, 0);
    const head = (u: UnitState) => new vscode.Range(u.line_start - 1, 0, u.line_start - 1, 0);
    const of = (status: string) => units.filter((u) => u.status === status);

    const washRanges = new Map<string, vscode.Range[]>();
    for (const s of this.washes.keys()) {
      washRanges.set(s, []);
    }
    let pending: vscode.Range[] = [];
    let running: vscode.Range[] = [];
    let ok: vscode.Range[] = [];
    let cond: vscode.Range[] = [];

    if (opts.mode === 'wash') {
      for (const u of units) {
        washRanges.get(u.status)?.push(span(u));
      }
    } else {
      pending = of('pending').map(span);
      running = of('running').map(span);
      if (opts.gutterChecks) {
        ok = of('admitted').map(head);
        cond = of('conditional').map(head);
      }
      // The one surviving background; reds stay diagnostics-only.
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
    editor.setDecorations(this.checkOk, ok);
    editor.setDecorations(this.checkCond, cond);
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
    for (const t of [this.pendingBar, this.runningBar, this.checkOk, this.checkCond, this.target]) {
      editor.setDecorations(t, []);
    }
  }

  dispose(): void {
    for (const t of this.washes.values()) {
      t.dispose();
    }
    for (const t of [this.pendingBar, this.runningBar, this.checkOk, this.checkCond, this.target]) {
      t.dispose();
    }
  }
}
