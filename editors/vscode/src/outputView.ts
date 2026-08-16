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
    const doc = await this.applyLanguage(await vscode.workspace.openTextDocument(this.uri));
    await vscode.window.showTextDocument(doc, {
      viewColumn: vscode.ViewColumn.Beside,
      preserveFocus: true,
      preview: true,
    });
  }

  /** Re-read the setting for a document that is already open. */
  async refreshLanguage(): Promise<void> {
    for (const e of vscode.window.visibleTextEditors) {
      if (e.document.uri.scheme === this.scheme) {
        await this.applyLanguage(e.document);
      }
    }
  }

  /**
   * These are the one surface where highlighting cannot be done by shipping
   * spans -- a TextDocument is colored by a grammar chosen from its language,
   * and nothing else. So the log's language is `acl2-output` when the setting
   * is on and plain text when it is off.
   *
   * `setTextDocumentLanguage` returns the document under its new language;
   * the old handle is stale, so the result is what gets shown.
   */
  private async applyLanguage(doc: vscode.TextDocument): Promise<vscode.TextDocument> {
    const want = highlightOutput() ? 'acl2-output' : 'plaintext';
    return doc.languageId === want ? doc : vscode.languages.setTextDocumentLanguage(doc, want);
  }
}

function highlightOutput(): boolean {
  return vscode.workspace.getConfiguration('acl2-session').get<boolean>('highlightOutput', true);
}
