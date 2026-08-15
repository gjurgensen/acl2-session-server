// Whole-line washes per unit status. Line-granular deliberately: the daemon
// reports 1-based line spans, so no byte-to-UTF-16 mapping exists to get
// wrong, and isWholeLine gives the fill-to-edge look.
//
// Color families: green for admitted, red for skipped and failed alike (a
// form admitted only with its
// proofs skipped still needs fixing), yellow/blue for the unit being proved,
// and conditional -- admitted below a red form, assuming its unproved claim --
// as *the other green*: unmistakably the green family, visibly not the same
// green.

import * as vscode from 'vscode';
import { UnitState } from './protocol';

const COLORS: Record<string, { light: string; dark: string }> = {
  admitted: { light: '#c8e6c9', dark: '#1c2e15' },
  conditional: { light: '#d9e8f5', dark: '#2c2908' },
  skipped: { light: '#ffb3b3', dark: '#3d1e19' },
  failed: { light: '#ffb3b3', dark: '#3d1e19' },
  running: { light: '#fff59d', dark: '#1a2841' },
};

export class Decorations {
  private types = new Map<string, vscode.TextEditorDecorationType>();

  constructor() {
    for (const [status, c] of Object.entries(COLORS)) {
      this.types.set(
        status,
        vscode.window.createTextEditorDecorationType({
          isWholeLine: true,
          light: { backgroundColor: c.light },
          dark: { backgroundColor: c.dark },
        })
      );
    }
  }

  apply(editor: vscode.TextEditor, units: UnitState[]): void {
    const byStatus = new Map<string, vscode.Range[]>();
    for (const status of this.types.keys()) {
      byStatus.set(status, []);
    }
    for (const u of units) {
      byStatus.get(u.status)?.push(new vscode.Range(u.line_start - 1, 0, u.line_end - 1, 0));
    }
    for (const [status, ranges] of byStatus) {
      editor.setDecorations(this.types.get(status)!, ranges);
    }
  }

  clear(editor: vscode.TextEditor): void {
    for (const t of this.types.values()) {
      editor.setDecorations(t, []);
    }
  }

  dispose(): void {
    for (const t of this.types.values()) {
      t.dispose();
    }
  }
}
