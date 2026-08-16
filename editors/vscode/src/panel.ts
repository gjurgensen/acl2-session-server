// The structured panel: an infoview-shaped webview beside the editor,
// rendering the envelope for the form at the cursor -- status, the failure
// as structure (abort cause, checkpoints as terms with their prover output
// one click away), warnings, fired hints, splitter rules, the events inside
// a compound form, rules -- and streaming the live tail while that form
// proves. Plain HTML on the editor's theme variables, zero dependencies.
//
// Per-form, and only per-form: what it shows is whatever the cursor is in,
// and it forgets the previous form entirely. Forms evaluated outside the
// file belong to the session rather than to any cursor position, so they
// live in the console (console.ts) instead. The renderer they share is in
// webviewParts.ts.
//
// Terms arrive already tokenized and already colored -- see highlight.ts,
// which runs them through the book grammar and the active theme's own rules,
// so a term here and the same term in the buffer look alike. The `acl2::`
// prefix on abort causes is stripped for display only; the wire keeps ACL2's
// own printing.

import * as vscode from 'vscode';
import { TokLines } from './highlight';
import { SHARED_CSS, SHARED_JS } from './webviewParts';

export interface PanelCheckpoint {
  kind: string;
  goal?: string;
  term: string;
}

export interface PanelInnerEvent {
  name: string;
  kind?: string;
  time?: string;
  proverSteps?: number;
  rules?: string[];
  abortCauses?: string;
}

/**
 * What ACL2 had to say about one form. A unit and an eval carry the same
 * fields meaning the same things; each adds its own on top.
 */
export interface PanelEnvelope {
  elapsedMs?: number;
  proverSteps?: number;
  outputLines?: number;
  errorLine?: string;
  abortCause?: string;
  checkpoints: PanelCheckpoint[];
  rules: string[];
  warnings?: string;
  time?: string;
  hintEvents?: string[];
  splitterRules?: string[];
  preRevert?: boolean;
  forced?: boolean;
  inner?: PanelInnerEvent[];
  innerTotal?: number;
  nTop?: number;
  nSub?: number;
}

export interface PanelModel extends PanelEnvelope {
  book: string;
  index: number;
  total: number;
  lineStart: number;
  lineEnd: number;
  /** The paint status: `admitted`, `conditional`, `skipped`, `failed`, ... */
  status: string;
  skipped: boolean;
}

/** The strings in an envelope that are ACL2 code rather than prose. */
export function highlightable(m: PanelEnvelope): string[] {
  return [
    ...m.checkpoints.map((c) => c.term),
    ...(m.rules ?? []),
    ...(m.hintEvents ?? []),
    ...(m.splitterRules ?? []),
    ...(m.inner ?? []).flatMap((e) => e.rules ?? []),
  ];
}

/**
 * Tokenize each string once, keyed by its own text: a webview looks a string
 * up rather than carrying a tokenizer, and a rune repeated across a dozen
 * events is tokenized once.
 */
export function tokenizeAll(
  strings: string[],
  tokenizer?: (text: string) => TokLines | undefined
): Record<string, TokLines> {
  const toks: Record<string, TokLines> = {};
  if (tokenizer) {
    for (const s of strings) {
      if (!(s in toks)) {
        const t = tokenizer(s);
        if (t) {
          toks[s] = t;
        }
      }
    }
  }
  return toks;
}

export class Panel {
  private panel?: vscode.WebviewPanel;
  private model?: PanelModel;

  onGoal?: (model: PanelModel, goal: string, slot: number) => void;
  onFullLog?: (model: PanelModel) => void;
  onRetract?: (model: PanelModel) => void;
  onTarget?: (model: PanelModel) => void;

  /**
   * Set by the extension once the grammar and theme are loaded. Terms travel
   * to the webview already tokenized and already colored, because a webview
   * can do neither for itself.
   */
  tokenizer?: (text: string) => TokLines | undefined;

  /** The tab icon, so an ACL2 window is recognizable among a row of tabs. */
  iconPath?: { light: vscode.Uri; dark: vscode.Uri };

  get visible(): boolean {
    return this.panel !== undefined;
  }

  shown(): { book: string; index: number } | undefined {
    return this.model && this.panel
      ? { book: this.model.book, index: this.model.index }
      : undefined;
  }

  open(): void {
    if (this.panel) {
      this.panel.reveal(undefined, true);
      return;
    }
    this.panel = vscode.window.createWebviewPanel(
      'acl2SessionPanel',
      'ACL2',
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
      { enableScripts: true, retainContextWhenHidden: true }
    );
    this.panel.iconPath = this.iconPath;
    this.panel.webview.html = HTML;
    this.panel.webview.onDidReceiveMessage((m: { cmd?: string; goal?: string; slot?: number }) => {
      if (!this.model) {
        return;
      }
      switch (m.cmd) {
        case 'goal':
          if (typeof m.goal === 'string' && typeof m.slot === 'number') {
            this.onGoal?.(this.model, m.goal, m.slot);
          }
          break;
        case 'fullLog':
          this.onFullLog?.(this.model);
          break;
        case 'retract':
          this.onRetract?.(this.model);
          break;
        case 'target':
          this.onTarget?.(this.model);
          break;
      }
    });
    this.panel.onDidDispose(() => {
      this.panel = undefined;
    });
    if (this.model) {
      this.post(this.model);
    }
  }

  render(model: PanelModel): void {
    this.model = model;
    this.post(model);
  }

  /** Re-send the current model, for when the theme changed under us. */
  refresh(): void {
    if (this.model) {
      this.post(this.model);
    }
  }

  private post(model: PanelModel): void {
    void this.panel?.webview.postMessage({
      kind: 'model',
      model,
      toks: tokenizeAll(highlightable(model), this.tokenizer),
    });
  }

  tail(book: string, unit: number | null, elapsedMs: number, text: string): void {
    const s = this.shown();
    if (s && s.book === book && s.index === unit) {
      void this.panel?.webview.postMessage({ kind: 'tail', elapsedMs, text });
    }
  }

  goalResult(slot: number, text: string): void {
    void this.panel?.webview.postMessage({ kind: 'goal', slot, text });
  }

  dispose(): void {
    this.panel?.dispose();
    this.panel = undefined;
  }
}

/** Exported so `webview.mjs` can parse-check it; nothing else imports it. */
export const HTML = `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
${SHARED_CSS}
body { padding: 2px 12px 12px; }
#tailtext { max-height: 45vh; overflow: auto; }
</style>
</head>
<body>
<div id="root"><p class="meta">Put the cursor in a form of a live book.</p></div>
<script>
const vscode = acquireVsCodeApi();
${SHARED_JS}
let model;

window.addEventListener('message', (ev) => {
  const m = ev.data;
  if (m.kind === 'model') {
    model = m.model;
    toks = m.toks || {};
    render();
  } else if (m.kind === 'tail') {
    const t = document.getElementById('tailtext');
    if (t) {
      t.textContent = m.text || '';
      t.scrollTop = t.scrollHeight;
      const e = document.getElementById('elapsed');
      if (e) { e.textContent = Math.round(m.elapsedMs / 1000) + 's'; }
    }
  } else if (m.kind === 'goal') {
    const d = document.getElementById('goal-' + m.slot);
    if (d) { d.textContent = m.text || '(nothing captured under this goal)'; }
  }
});

function render() {
  const m = model;
  let h = '<div class="h">' +
    '<span class="status ' + esc(m.status) + '">' + esc(m.status) + '</span>' +
    '<span>form ' + (m.index + 1) + '/' + m.total + '</span>' +
    '<span class="meta">lines ' + m.lineStart + '\\u2013' + m.lineEnd + '</span>';
  if (m.elapsedMs != null) {
    h += '<span class="meta">' + (m.elapsedMs / 1000).toFixed(m.elapsedMs < 10000 ? 1 : 0) + 's</span>';
  }
  if (m.proverSteps != null) { h += '<span class="meta">' + m.proverSteps + ' steps</span>'; }
  h += '</div>';
  if (m.time) {
    h += '<p class="meta">time: ' + fmtTime(m.time) + '</p>';
  }
  if (m.skipped) {
    h += '<div class="note">Admitted with its proofs skipped \\u2014 checking continued. ' +
         'Every form below is conditional on this unproved claim.</div>';
  }
  if (m.status === 'running') {
    h += '<h3>Proving <span id="elapsed" class="meta"></span></h3>' +
         '<pre id="tailtext">(waiting for output\\u2026)</pre>';
  }
  if (m.status === 'pending') {
    h += '<p class="meta">Not processed yet.</p>';
  }
  h += envelope(
    { ...m, failed: m.status === 'skipped' || m.status === 'failed' },
    'goal-',
    'goal'
  );
  h += '<h3>Actions</h3><p>' +
    (m.outputLines
      ? '<button id="fulllog">full log (' + m.outputLines + ' lines)</button> '
      : '') +
    '<button id="retract">retract this form</button> ' +
    '<button id="target">hold target here</button></p>';
  document.getElementById('root').innerHTML = h;
  const on = (id, cmd) => {
    const el = document.getElementById(id);
    if (el) { el.addEventListener('click', () => vscode.postMessage({ cmd })); }
  };
  on('fulllog', 'fullLog');
  on('retract', 'retract');
  on('target', 'target');
}

bindGoals(document, (b, slot) => 'goal-' + slot);
</script>
</body>
</html>`;
