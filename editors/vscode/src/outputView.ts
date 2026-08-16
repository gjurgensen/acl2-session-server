// Read-only virtual documents showing what ACL2 actually printed. Slices
// come from the daemon's OutputStore, which is served without the book lock,
// so fetching while a proof grinds costs nothing.
//
// Two of these exist. The unit view answers "what did ACL2 say about *this*
// form?" and follows the cursor. The eval view holds the log of the last
// form evaluated outside the file, and follows nothing: an eval is something
// the user asked for by hand, and having it vanish on the next cursor move
// -- which is what sharing one document would do -- makes it unreadable.

import * as vscode from 'vscode';

export const SCHEME = 'acl2-session-output';
export const EVAL_SCHEME = 'acl2-session-eval';

export class OutputView implements vscode.TextDocumentContentProvider {
  private emitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.emitter.event;
  readonly uri: vscode.Uri;

  private content: string;

  /** What the view is currently showing, for refresh decisions. */
  shownBook: string | null = null;
  shownUnit: number | null = null;

  constructor(
    readonly scheme: string,
    title: string,
    placeholder: string
  ) {
    this.uri = vscode.Uri.parse(`${scheme}:${title}`);
    this.content = placeholder;
  }

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
    return vscode.window.visibleTextEditors.some((e) => e.document.uri.scheme === this.scheme);
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
