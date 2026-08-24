// The documentation view: one topic at a time, with its cross-references
// followed in place.
//
// Its own window rather than a section of the form view, for the same reason
// the console moved out: the form view is *per-form* and about proof
// state, and a topic is neither. It is opened deliberately and should outlive
// the cursor moving, which is also why it follows nothing.

import * as vscode from 'vscode';
import { pageHtml, SHARED_CSS, SHARED_JS } from './webviewParts';
import { DocCode } from './xdoc';

/** What the view is asked to show. */
export interface DocPage {
  name: string;
  /** Rendered body, or a message when there is nothing to render. */
  html: string;
  /**
   * Code blocks as *tokens*, coloured by the extension with the editor's own
   * grammar and theme. The page turns them into spans with the same helper
   * the form view uses -- there is no second tokenizer anywhere on this page.
   */
  code: { slot: number; lines: unknown }[];
  /** Whether anything is behind us in the history. */
  canGoBack: boolean;
}

export class DocView {
  private panel?: vscode.WebviewPanel;
  private page?: DocPage;
  iconPath?: { light: vscode.Uri; dark: vscode.Uri };

  /** Follow a cross-reference. Set by the extension. */
  onTopic?: (name: string) => void;
  /** Go back one topic. Set by the extension. */
  onBack?: () => void;

  open(): void {
    if (this.panel) {
      this.panel.reveal(undefined, true);
      return;
    }
    this.panel = vscode.window.createWebviewPanel(
      'acl2SessionDocs',
      'ACL2 Docs',
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
      { enableScripts: true, retainContextWhenHidden: true }
    );
    this.panel.iconPath = this.iconPath;
    this.panel.webview.html = pageHtml(HTML, this.panel.webview);
    this.panel.webview.onDidReceiveMessage((m: { cmd?: string; topic?: string }) => {
      if (m.cmd === 'topic' && m.topic) {
        this.onTopic?.(m.topic);
      } else if (m.cmd === 'back') {
        this.onBack?.();
      } else if (m.cmd === 'ready' && this.page) {
        // The page's script runs after its HTML is set, and a message posted
        // before it registers its listener is dropped silently -- the trap
        // recorded for the console, which bites here too because the
        // command opens the window and renders into it in one breath.
        this.post(this.page);
      }
    });
    this.panel.onDidDispose(() => {
      this.panel = undefined;
    });
  }

  show(page: DocPage): void {
    this.page = page;
    this.open();
    this.post(page);
  }

  private post(page: DocPage): void {
    void this.panel?.webview.postMessage({ kind: 'page', ...page });
  }

  dispose(): void {
    this.panel?.dispose();
  }
}

export const HTML = `<!DOCTYPE html>
<html><head><meta charset="utf-8">
<!-- The same policy the form view and the console carry, and this page has the
     better claim to it: it is the only one whose body is markup derived from
     a book rather than assembled here. -->
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';
               font-src %%CSPSOURCE%%;">
<style>
${SHARED_CSS}
body { padding: 0 14px 20px; line-height: 1.5; }
.bar { display: flex; gap: 8px; align-items: baseline; position: sticky; top: 0;
       background: var(--vscode-editor-background); padding: 10px 0 6px; }
.name { font-weight: 600; font-size: 15px; }
a.see { color: var(--vscode-textLink-foreground); text-decoration: none; cursor: pointer; }
a.see:hover { text-decoration: underline; }
.math { font-family: var(--vscode-editor-font-family, monospace);
        background: rgba(128,128,128,0.10); padding: 0 4px; border-radius: 3px; }
div.math { padding: 6px 8px; margin: 6px 0; }
.unresolved { opacity: 0.65; font-style: italic; }
.box { border-left: 3px solid rgba(128,128,128,0.4); padding-left: 10px; margin: 8px 0; }
table { border-collapse: collapse; margin: 6px 0; }
th, td { border: 1px solid rgba(128,128,128,0.35); padding: 2px 8px; text-align: left; }
li { margin: 2px 0; }
</style></head><body>
<div class="bar"><button id="back">Back</button><span class="name" id="name"></span></div>
<div id="body"></div>
<script>
${SHARED_JS}
const vscode = acquireVsCodeApi();
const back = document.getElementById('back');
back.addEventListener('click', () => vscode.postMessage({ cmd: 'back' }));

// Delegated, because the body is replaced wholesale on every topic.
document.getElementById('body').addEventListener('click', (e) => {
  const a = e.target.closest && e.target.closest('a.see');
  if (a) {
    e.preventDefault();
    const t = a.getAttribute('data-topic');
    if (t) { vscode.postMessage({ cmd: 'topic', topic: t }); }
  }
});

window.addEventListener('message', (ev) => {
  const m = ev.data;
  if (!m || m.kind !== 'page') { return; }
  document.getElementById('name').textContent = m.name;
  back.style.display = m.canGoBack ? '' : 'none';
  const body = document.getElementById('body');
  body.innerHTML = m.html;
  // Code blocks arrive already coloured by the extension, which ran them
  // through the same grammar and theme the editor uses. Here they are only
  // spans -- there is no second tokenizer on this page.
  for (const c of m.code || []) {
    // Any element with the slot, not just <pre>: inline @('...') spans are
    // code too, and in a real topic most of the code is inline.
    const slot = body.querySelector('[data-code="' + c.slot + '"]');
    // Only when there are tokens. The escaped text is already in the
    // element, and putting textContent back through innerHTML would read
    // markup *inside the code* as markup -- the one thing lifting code
    // blocks out was supposed to prevent.
    if (slot && c.lines) { slot.innerHTML = spans(c.lines); }
  }
  window.scrollTo(0, 0);
});
vscode.postMessage({ cmd: 'ready' });
</script></body></html>`;

/** A block the tokenizer could not colour: the page escapes it as-is. */
export function plainSpans(c: DocCode): { slot: number; lines: unknown } {
  return { slot: c.slot, lines: undefined };
}
