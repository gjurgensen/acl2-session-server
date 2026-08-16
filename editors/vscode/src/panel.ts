// The structured panel: an infoview-shaped webview beside the editor,
// rendering the envelope for the form at the cursor -- status, the failure
// as structure (abort cause, checkpoints as terms with their prover output
// one click away), warnings, fired hints, splitter rules, the events inside
// a compound form, rules -- and streaming the live tail while that form
// proves. Plain HTML on the editor's theme variables, zero dependencies.
//
// Terms arrive already tokenized and already colored -- see highlight.ts,
// which runs them through the book grammar and the active theme's own rules,
// so a term here and the same term in the buffer look alike. The `acl2::`
// prefix on abort causes is stripped for display only; the wire keeps ACL2's
// own printing.

import * as vscode from 'vscode';
import { TokLines } from './highlight';

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
  nTop?: number;
  nSub?: number;
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

  /**
   * Every term and rune in the model, tokenized once and keyed by its own
   * text: the webview looks a string up rather than carrying a tokenizer,
   * and a rune repeated across a dozen events is tokenized once.
   */
  private post(model: PanelModel): void {
    const toks: Record<string, TokLines> = {};
    if (this.tokenizer) {
      for (const s of highlightable(model)) {
        if (!(s in toks)) {
          const t = this.tokenizer(s);
          if (t) {
            toks[s] = t;
          }
        }
      }
    }
    void this.panel?.webview.postMessage({ kind: 'model', model, toks });
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

/** The strings in a model that are ACL2 code rather than prose. */
function highlightable(m: PanelModel): string[] {
  return [
    ...m.checkpoints.map((c) => c.term),
    ...(m.rules ?? []),
    ...(m.hintEvents ?? []),
    ...(m.splitterRules ?? []),
    ...(m.inner ?? []).flatMap((e) => e.rules ?? []),
  ];
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
.status.conditional { background: rgba(212, 172, 13, 0.25); }
.status.skipped, .status.failed { background: rgba(198, 40, 40, 0.25); }
.status.running { background: rgba(90, 140, 210, 0.3); }
.status.pending { background: rgba(136, 136, 136, 0.2); }
.meta { opacity: 0.7; font-size: 12px; }
h3 { margin: 14px 0 4px; font-size: 11px; text-transform: uppercase;
     letter-spacing: 0.05em; opacity: 0.75; }
pre, code { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; }
pre { background: var(--vscode-textCodeBlock-background); padding: 6px 8px;
      border-radius: 4px; overflow: auto; margin: 4px 0; white-space: pre-wrap; }
/* Override the webview default: its light chip behind inline code is
   unreadable on dark themes. A faint neutral chip with the theme
   foreground works on both. */
code { background-color: rgba(128, 128, 128, 0.14);
       color: var(--vscode-foreground); padding: 0 4px; border-radius: 3px; }
pre code { background: none; padding: 0; }
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

// Terms arrive tokenized and colored by the extension, which ran them
// through the same grammar and the same theme rules the editor uses. Here
// they are only spans; a string with no entry is code we were not asked to
// highlight, or a tokenizer that has not finished loading.
let toks = {};
function hl(src) {
  const lines = toks[String(src)];
  if (!lines) { return esc(src); }
  return lines.map((line) => line.map((t) =>
    t.c
      ? '<span style="color:' + t.c + (t.i ? ';font-style:italic' : '') + '">' + esc(t.t) + '</span>'
      : esc(t.t)
  ).join('')).join('\\n');
}

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

function codes(list) {
  return '<p class="codes">' +
    list.map((r) => '<code class="lisp">' + hl(r) + '</code>').join('') + '</p>';
}

// ACL2 reports times as rationals of seconds; render them as labeled
// decimals instead of fractions.
function rat(tok) {
  const m = /^(-?[0-9]+)(?:\\/([0-9]+))?$/.exec(tok);
  if (m) { return Number(m[1]) / (m[2] ? Number(m[2]) : 1); }
  const f = parseFloat(tok);
  return isNaN(f) ? null : f;
}
function fmtSecs(v) {
  if (v === 0) { return '0s'; }
  if (v >= 10) { return v.toFixed(1) + 's'; }
  if (v >= 0.0005) { return v.toFixed(3) + 's'; }
  return '<0.001s';
}
function timeParts(t) {
  const toks = String(t || '').replace(/[()]/g, '').trim().split(/\\s+/);
  if (toks.length !== 4) { return null; }
  const vals = toks.map(rat);
  return vals.some((v) => v === null) ? null : vals;
}
function fmtTime(t) {
  const vals = timeParts(t);
  if (!vals) { return esc(t); }
  const names = ['prove', 'print', 'proof-tree', 'other'];
  return names
    .map((n, i) => ({ n, v: vals[i] }))
    .filter((x, i) => x.v > 0 || i === 0)
    .map((x) => x.n + ' ' + fmtSecs(x.v))
    .join(' \\u00b7 ');
}
function proveSecs(t) {
  const vals = timeParts(t);
  return vals ? fmtSecs(vals[0]) : '';
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
    const shownTop = (m.checkpoints || []).filter((c) => c.kind === 'top').length;
    const shownSub = (m.checkpoints || []).filter((c) => c.kind === 'sub').length;
    if ((m.nTop || 0) > shownTop || (m.nSub || 0) > shownSub) {
      h += '<p class="meta">showing ' + shownTop + ' of ' + (m.nTop || shownTop) +
        ' top and ' + shownSub + ' of ' + (m.nSub || shownSub) +
        ' subgoal checkpoints; the rest are in the full log.</p>';
    }
  }
  if (m.warnings) {
    // The daemon joins the kinds with ", ", so splitting counts them exactly.
    const kinds = String(m.warnings).split(', ');
    h += '<details open><summary><h3>Warnings (' + kinds.length + ')</h3></summary>' +
      '<pre>' + esc(m.warnings) + '</pre></details>';
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
      inner.map((e) => {
        // An empty steps cell has meant two different things: this event
        // never entered the prover, and this event's detail was never read.
        // A probed event has *something* -- kind, time, rules -- so a dash
        // can say "no steps" where blank keeps meaning "not looked at".
        const probed = !!(e.kind || e.time || (e.rules || []).length || e.proverSteps != null);
        const steps = e.proverSteps != null
          ? e.proverSteps + ' steps'
          : (probed ? '<span title="the prover was not entered">&mdash;</span>' : '');
        let row = '<tr><td>' + esc(e.kind || '') + '</td>' +
          '<td><code>' + esc(e.name) + '</code></td>' +
          '<td class="meta">' + proveSecs(e.time) + '</td>' +
          '<td class="meta">' + steps + '</td>' +
          '<td>' + (e.abortCauses ? '<span class="failtag">' + esc(strip(e.abortCauses)) + '</span>' : '') + '</td></tr>';
        if ((e.rules || []).length) {
          // Second-level detail, folded: the count keeps it visible that the
          // rules are there without a wall of chips between one event's row
          // and the next.
          row += '<tr><td></td><td colspan="4" class="innerrules">' +
            '<details><summary class="meta">rules (' + e.rules.length + ')</summary>' +
            codes(e.rules) + '</details></td></tr>';
        }
        return row;
      }).join('') + '</table>';
    const detailed = inner.filter(
      (e) => e.kind || e.time || e.proverSteps != null || (e.rules || []).length
    ).length;
    if (detailed < inner.length) {
      h += '<p class="meta">detail (kind, time, steps, rules) is probed for the first ' +
        detailed + ' events; the rest are names only.</p>';
    }
    h += '</details>';
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
