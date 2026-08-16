// The structured panel: an infoview-shaped webview beside the editor,
// rendering the envelope for the form at the cursor -- status, the failure
// as structure (abort cause, checkpoints as terms with their prover output
// one click away), warnings, fired hints, splitter rules, the events inside
// a compound form, rules -- and streaming the live tail while that form
// proves. Plain HTML on the editor's theme variables, zero dependencies.
//
// Terms are highlighted by a small Lisp tokenizer using the theme's
// *terminal* palette: webviews do not receive the editor's token colors, but
// the ANSI variables track the same theme family, so a gruvbox editor gets
// gruvbox terms. The `acl2::` prefix on abort causes is stripped for display
// only; the wire keeps ACL2's own printing.

import * as vscode from 'vscode';

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
  abortCauses?: string;
}

export interface PanelModel {
  book: string;
  index: number;
  total: number;
  lineStart: number;
  lineEnd: number;
  status: string;
  elapsedMs?: number;
  proverSteps?: number;
  outputLines?: number;
  errorLine?: string;
  abortCause?: string;
  checkpoints: PanelCheckpoint[];
  rules: string[];
  skipped: boolean;
  warnings?: string;
  time?: string;
  hintEvents?: string[];
  splitterRules?: string[];
  preRevert?: boolean;
  forced?: boolean;
  inner?: PanelInnerEvent[];
  innerTotal?: number;
}

export class Panel {
  private panel?: vscode.WebviewPanel;
  private model?: PanelModel;

  onGoal?: (model: PanelModel, goal: string, slot: number) => void;
  onFullLog?: (model: PanelModel) => void;
  onRetract?: (model: PanelModel) => void;
  onTarget?: (model: PanelModel) => void;

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
      void this.panel.webview.postMessage({ kind: 'model', model: this.model });
    }
  }

  render(model: PanelModel): void {
    this.model = model;
    void this.panel?.webview.postMessage({ kind: 'model', model });
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

const HTML = `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
body { font-family: var(--vscode-font-family); color: var(--vscode-foreground);
       font-size: 13px; padding: 2px 12px 12px; }
.h { display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; margin: 10px 0 4px; }
.status { font-weight: 600; padding: 1px 8px; border-radius: 9px; }
.status.admitted { background: rgba(46, 125, 50, 0.25); }
.status.conditional { background: rgba(160, 150, 20, 0.25); }
.status.skipped, .status.failed { background: rgba(198, 40, 40, 0.25); }
.status.running { background: rgba(196, 138, 0, 0.3); }
.status.pending { background: rgba(136, 136, 136, 0.2); }
.meta { opacity: 0.7; font-size: 12px; }
h3 { margin: 14px 0 4px; font-size: 11px; text-transform: uppercase;
     letter-spacing: 0.05em; opacity: 0.75; }
pre, code { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; }
pre { background: var(--vscode-textCodeBlock-background); padding: 6px 8px;
      border-radius: 4px; overflow: auto; margin: 4px 0; white-space: pre-wrap; }
button { background: var(--vscode-button-secondaryBackground);
         color: var(--vscode-button-secondaryForeground); border: none;
         padding: 2px 9px; border-radius: 3px; cursor: pointer; font-size: 11px; }
button:hover { background: var(--vscode-button-secondaryHoverBackground); }
.note { background: rgba(198, 40, 40, 0.15); padding: 6px 9px; border-radius: 4px;
        margin: 8px 0; font-size: 12px; }
#tailtext { max-height: 45vh; overflow: auto; }
.rules code, .codes code { display: inline-block; margin: 1px 5px 1px 0; }
.cp { margin: 8px 0; }
details > summary { cursor: pointer; }
details > summary h3 { display: inline; }
table.inner { border-collapse: collapse; font-size: 12px; }
table.inner td { padding: 1px 10px 1px 0; vertical-align: baseline; }
.failtag { color: var(--vscode-terminal-ansiRed, #cc6666); font-weight: 600; }
/* Lisp token colors ride the theme's terminal palette. */
.p { color: var(--vscode-terminal-ansiBrightBlack, #888888); }
.hd { color: var(--vscode-terminal-ansiBlue, #6a9fb5); }
.kw { color: var(--vscode-terminal-ansiMagenta, #b48ead); }
.st { color: var(--vscode-terminal-ansiGreen, #a3be8c); }
.nu { color: var(--vscode-terminal-ansiCyan, #7fbfb4); }
.qu { color: var(--vscode-terminal-ansiYellow, #d8b26a); }
.cm { color: var(--vscode-terminal-ansiBrightBlack, #888888); font-style: italic; }
</style>
</head>
<body>
<div id="root"><p class="meta">Put the cursor in a form of a live book.</p></div>
<script>
const vscode = acquireVsCodeApi();
let model;
const esc = (s) => String(s).replace(/[&<>"]/g,
  (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const strip = (s) => String(s).replace(/^acl2::/i, '');

// A small Lisp tokenizer for term display. The head symbol of each list,
// keywords, strings, numbers and quotes get theme-terminal colors.
function hl(src) {
  const s = String(src);
  let out = '';
  let expectHead = false;
  const re = /([()])|("(?:[^"\\\\]|\\\\.)*"?)|(;[^\\n]*)|('+)|([^\\s()'";]+)|(\\s+)/g;
  let m;
  while ((m = re.exec(s))) {
    if (m[1]) {
      out += '<span class="p">' + m[1] + '</span>';
      expectHead = m[1] === '(';
    } else if (m[2]) {
      out += '<span class="st">' + esc(m[2]) + '</span>';
      expectHead = false;
    } else if (m[3]) {
      out += '<span class="cm">' + esc(m[3]) + '</span>';
    } else if (m[4]) {
      out += '<span class="qu">' + esc(m[4]) + '</span>';
    } else if (m[5]) {
      const t = m[5];
      let cls = '';
      if (expectHead) { cls = 'hd'; }
      else if (t.startsWith(':')) { cls = 'kw'; }
      else if (/^[+-]?[0-9]/.test(t)) { cls = 'nu'; }
      else if (/^(nil|t)$/i.test(t)) { cls = 'kw'; }
      out += cls ? '<span class="' + cls + '">' + esc(t) + '</span>' : esc(t);
      expectHead = false;
    } else if (m[6]) {
      out += esc(m[6]); // whitespace keeps a pending head position
    }
  }
  return out;
}

window.addEventListener('message', (ev) => {
  const m = ev.data;
  if (m.kind === 'model') {
    model = m.model;
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

function codes(list) {
  return '<p class="codes">' +
    list.map((r) => '<code class="lisp">' + hl(r) + '</code>').join('') + '</p>';
}

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
    h += '<p class="meta">time (prove print proof-tree other): ' + esc(m.time) + '</p>';
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
  if (m.errorLine || m.abortCause || (m.checkpoints || []).length) {
    h += '<h3>Failure</h3>';
    if (m.errorLine) { h += '<pre>' + esc(m.errorLine) + '</pre>'; }
    if (m.abortCause) {
      h += '<p class="meta">abort cause: <code>' + esc(strip(m.abortCause)) + '</code></p>';
    }
    if (m.forced) {
      h += '<p class="meta">forced assumptions are present.</p>';
    }
    if (m.preRevert) {
      h += '<p class="meta">top checkpoints are from before the proof reverted to induction.</p>';
    }
    (m.checkpoints || []).forEach((c, i) => {
      const kind = c.kind === 'top' && m.preRevert ? 'top!' : c.kind;
      h += '<div class="cp"><div class="meta">' + esc(kind) + ' ' + esc(c.goal || '') + ' ' +
        (c.goal
          ? '<button data-goal="' + esc(c.goal) + '" data-slot="' + i + '">prover output</button>'
          : '') +
        '</div><pre>' + hl(c.term) + '</pre><pre id="goal-' + i + '" hidden></pre></div>';
    });
  }
  if (m.warnings) {
    h += '<h3>Warnings</h3><pre>' + esc(m.warnings) + '</pre>';
  }
  if ((m.hintEvents || []).length) {
    h += '<h3>Hints that fired</h3>' + codes(m.hintEvents);
  }
  if ((m.splitterRules || []).length) {
    h += '<h3>Splitter rules</h3>' + codes(m.splitterRules);
  }
  if ((m.rules || []).length) {
    const label = m.status === 'skipped' || m.status === 'failed'
      ? 'Rules the attempt used'
      : 'Rules';
    h += '<details open><summary><h3>' + label + ' (' + m.rules.length + ')</h3></summary>' +
      codes(m.rules) + '</details>';
  }
  const inner = m.inner || [];
  if (inner.length) {
    const total = m.innerTotal != null && m.innerTotal > inner.length
      ? inner.length + ' shown of ' + m.innerTotal
      : String(inner.length);
    h += '<details open><summary><h3>Inside this form (' + total + ')</h3></summary>' +
      '<table class="inner">' +
      inner.map((e) => '<tr><td>' + esc(e.kind || '') + '</td>' +
        '<td><code>' + esc(e.name) + '</code></td>' +
        '<td class="meta">' + (e.proverSteps != null ? e.proverSteps + ' steps' : '') + '</td>' +
        '<td>' + (e.abortCauses ? '<span class="failtag">' + esc(strip(e.abortCauses)) + '</span>' : '') + '</td></tr>'
      ).join('') + '</table></details>';
  }
  h += '<h3>Actions</h3><p>' +
    (m.outputLines
      ? '<button id="fulllog">full log (' + m.outputLines + ' lines)</button> '
      : '') +
    '<button id="retract">retract this form</button> ' +
    '<button id="target">hold target here</button></p>';
  document.getElementById('root').innerHTML = h;
  for (const b of document.querySelectorAll('button[data-goal]')) {
    b.addEventListener('click', () => {
      const pre = document.getElementById('goal-' + b.dataset.slot);
      pre.hidden = false;
      pre.textContent = '(fetching\\u2026)';
      vscode.postMessage({ cmd: 'goal', goal: b.dataset.goal, slot: Number(b.dataset.slot) });
    });
  }
  const on = (id, cmd) => {
    const el = document.getElementById(id);
    if (el) { el.addEventListener('click', () => vscode.postMessage({ cmd })); }
  };
  on('fulllog', 'fullLog');
  on('retract', 'retract');
  on('target', 'target');
}
</script>
</body>
</html>`;
