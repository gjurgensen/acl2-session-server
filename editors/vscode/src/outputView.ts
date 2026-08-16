// A read-only virtual document showing one unit's captured output --
// "what did ACL2 say about *this* form?" -- following the cursor. Slices
// come from the daemon's OutputStore, which is served without the book
// lock, so fetching while a proof grinds costs nothing.

import * as vscode from 'vscode';

export const SCHEME = 'acl2-session-output';

export class OutputView implements vscode.TextDocumentContentProvider {
  private emitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.emitter.event;
  readonly uri = vscode.Uri.parse(`${SCHEME}:ACL2 Output`);

  private content = '(no form under the cursor has been checked yet)';

  /** What the view is currently showing, for refresh decisions. */
  shownBook: string | null = null;
  shownUnit: number | null = null;

  provideTextDocumentContent(): string {
    return this.content;
  }

  // The view shows ACL2's output verbatim: which unit it belongs to and how
  // it fared are already in the gutter, the status bar and the panel, so a
  // banner here would only push the real text down.
  set(book: string, unit: number | null, text: string): void {
    this.shownBook = book;
    this.shownUnit = unit;
    this.content = text;
    this.emitter.fire(this.uri);
  }

  get visible(): boolean {
    return vscode.window.visibleTextEditors.some((e) => e.document.uri.scheme === SCHEME);
  }

  async open(): Promise<void> {
    const doc = await vscode.workspace.openTextDocument(this.uri);
    await vscode.window.showTextDocument(doc, {
      viewColumn: vscode.ViewColumn.Beside,
      preserveFocus: true,
      preview: true,
    });
  }
}
