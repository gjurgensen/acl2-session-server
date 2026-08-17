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
// A third: the documentation for whatever name was asked about. Like the
// eval view it follows nothing, because a topic is something the user opened
// deliberately and should not vanish when the cursor moves.
export const DOC_SCHEME = 'acl2-session-doc';

export class OutputView implements vscode.TextDocumentContentProvider {
  private emitter = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.emitter.event;
  readonly uri: vscode.Uri;

  private content: string;

  /** What the view is currently showing, for refresh decisions. */
  shownBook: string | null = null;
  shownUnit: number | null = null;

  private readonly sub: vscode.Disposable;

  constructor(
    readonly scheme: string,
    title: string,
    placeholder: string
  ) {
    this.uri = vscode.Uri.parse(`${scheme}:${title}`);
    this.content = placeholder;
    // Scrolling has to follow the *document*, not `set`. Firing the change
    // event only asks VSCode for new content; the document has not grown by
    // the time `set` returns, so revealing there would scroll to the end of
    // the previous form's log. This event fires once the text is really in.
    this.sub = vscode.workspace.onDidChangeTextDocument((e) => {
      if (e.document.uri.toString() === this.uri.toString()) {
        this.revealTail();
      }
    });
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
    // Opening onto content that was set before the editor existed fires no
    // change event, so the reveal has to happen here too.
    this.revealTail();
  }

  /**
   * Put the end of the log on screen.
   *
   * The answer is always at the bottom -- the error line, `Q.E.D.`, the
   * summary -- and VSCode reuses the editor for a URI along with its scroll
   * position, so without this the view opens wherever the *previous* form's
   * log had been left.
   *
   * The last line is usually blank (ACL2 pads the end), so this lands on the
   * last line with something on it; `Default` then scrolls the minimum
   * needed, which puts that line at the foot of the viewport.
   */
  private revealTail(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.toString() !== this.uri.toString()) {
        continue;
      }
      const doc = editor.document;
      let line = doc.lineCount - 1;
      while (line > 0 && doc.lineAt(line).isEmptyOrWhitespace) {
        line--;
      }
      const pos = doc.lineAt(line).range.end;
      editor.selection = new vscode.Selection(pos, pos);
      editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.Default);
    }
  }

  dispose(): void {
    this.sub.dispose();
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
