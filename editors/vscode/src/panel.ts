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
import { pretty } from './pretty';

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

export interface PanelRuneField {
  key: string;
  value: string;
}

export interface PanelRune {
  rune: string;
  class?: string;
  enabled?: boolean;
  fields: PanelRuneField[];
}

/**
 * What one name a form introduced actually is, in the world.
 *
 * The point of showing it is that almost none of it is written in the file:
 * the left-hand side a `defthm` will rewrite, the trigger a
 * forward-chaining rule waits for, the four runes a `defun` quietly adds.
 * The definition rune's body is deliberately absent -- that part *is* in the
 * file, a few lines above.
 */
export interface PanelNameRules {
  name: string;
  symbolClass?: string;
  justification?: string;
  formals?: string;
  guard?: string;
  statement?: string;
  runes: PanelRune[];
}

/** Why the prover inducted -- see `InductionWire`. */
export interface PanelInduction {
  goal?: string;
  suggestedBy?: string;
  attribution?: string;
  scheme?: string;
  justifiedBy?: string;
  subgoals?: string;
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

/** The strings in an induction record that are ACL2 code rather than prose. */
function inductionStrings(records: PanelInduction[]): string[] {
  return records.flatMap((r) =>
    [r.scheme, r.suggestedBy].filter((x): x is string => x !== undefined)
  );
}

/**
 * Whether two rules records say the same thing.
 *
 * By serialisation rather than field by field: the shape is a small tree of
 * plain data whose fields are ACL2's and therefore open-ended, so a
 * hand-written comparison would quietly stop noticing whatever gets added
 * next.
 */
function same(a: PanelNameRules[], b: PanelNameRules[]): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** The strings in a rules record that are ACL2 code rather than prose. */
export function rulesStrings(entries: PanelNameRules[]): string[] {
  return entries.flatMap((e) => [
    ...(e.formals === undefined ? [] : [e.formals]),
    ...(e.guard === undefined ? [] : [e.guard]),
    ...(e.statement === undefined ? [] : [e.statement]),
    ...e.runes.flatMap((r) => [r.rune, ...r.fields.map((f) => f.value)]),
  ]);
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
  private lastRules?: { book: string; index: number; entries: PanelNameRules[] };

  onGoal?: (model: PanelModel, goal: string, slot: number) => void;

  /**
   * Set by the extension once the grammar and theme are loaded. Terms travel
   * to the webview already tokenized and already colored, because a webview
   * can do neither for itself.
   */
  tokenizer?: (text: string) => TokLines | undefined;

  /** The tab icon, so an ACL2 window is recognizable among a row of tabs. */
  iconPath?: { light: vscode.Uri; dark: vscode.Uri };

  /**
   * How wide the pane is, in characters of its monospace font.
   *
   * Reported by the page, because only it can measure. Terms are re-flowed
   * to this before being tokenized -- ACL2 laid them out for a 77-column
   * terminal, and this pane is rarely that.
   */
  private cols = 72;

  /**
   * The width terms are laid out at: the pane's, but never more than the
   * configured maximum.
   *
   * A pane can be 115 columns wide, and ACL2's own output is built for 77 --
   * so using the whole pane means nothing ever breaks and every term arrives
   * as one long line. No formatter sets its print width to the monitor
   * width. The measurement still earns its keep in the other direction: a
   * narrow pane breaks more, not less.
   */
  private layoutCols(): number {
    const max = vscode.workspace
      .getConfiguration('acl2-session')
      .get<number>('termWidth', 60);
    return Math.max(20, Math.min(this.cols, max));
  }
  private lastInduction?: { book: string; index: number; records: PanelInduction[] };

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
    this.panel.webview.onDidReceiveMessage(
      (m: { cmd?: string; goal?: string; slot?: number; cols?: number }) => {
      // Width first: it arrives before any form is shown, and re-flowing is
      // the whole reason the page reports it.
      if (m.cmd === 'cols' && typeof m.cols === 'number' && m.cols !== this.cols) {
        this.cols = m.cols;
        this.refresh();
        return;
      }
      if (!this.model) {
        return;
      }
      if (m.cmd === 'goal' && typeof m.goal === 'string' && typeof m.slot === 'number') {
        this.onGoal?.(this.model, m.goal, m.slot);
      }
    }
    );
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

  /**
   * Fill in what the form introduced.
   *
   * Keyed by book and unit like the induction narrative, and for the same
   * reason: the cursor may have moved on while this was being fetched, and
   * detail for the form you have left is noise.
   */
  rules(book: string, index: number, entries: PanelNameRules[]): void {
    const s = this.shown();
    if (!s || s.book !== book || s.index !== index) {
      return;
    }
    // A render replaces the whole page, which resets every dropdown the
    // reader has opened. So a post that would change nothing must not
    // happen: the panel follows the cursor, and the same form arriving again
    // is the common case, not the rare one.
    const l = this.lastRules;
    if (l && l.book === book && l.index === index && same(l.entries, entries)) {
      return;
    }
    this.lastRules = { book, index, entries };
    this.postRules(this.lastRules);
  }

  private postRules(r: { book: string; index: number; entries: PanelNameRules[] }): void {
    const laid = this.layOutRules(r.entries);
    void this.panel?.webview.postMessage({
      kind: 'rules',
      book: r.book,
      index: r.index,
      entries: laid,
      toks: tokenizeAll(rulesStrings(laid), this.tokenizer),
    });
  }

  /** Re-flow the terms to the pane's width, as the model's are. */
  private layOutRules(entries: PanelNameRules[]): PanelNameRules[] {
    const cols = this.layoutCols();
    return entries.map((e) => ({
      ...e,
      statement: e.statement === undefined ? undefined : pretty(e.statement, cols),
      runes: e.runes.map((u) => ({
        ...u,
        fields: u.fields.map((f) => ({ ...f, value: pretty(f.value, cols) })),
      })),
    }));
  }

  /** Re-send the current model, for when the theme changed under us. */
  refresh(): void {
    if (this.model) {
      this.post(this.model);
    }
  }

  /**
   * Re-flow, then tokenize, then send.
   *
   * In that order and not another: the tokens are keyed by the exact string
   * the page will look up, so anything that changes the text has to happen
   * before the tokenizer sees it.
   */
  private post(model: PanelModel): void {
    const laid: PanelModel = {
      ...model,
      checkpoints: model.checkpoints.map((c) => ({ ...c, term: pretty(c.term, this.layoutCols()) })),
    };
    // The narrative rides along rather than following in a second message.
    // It used to follow, and the page replaces its token map on a model --
    // so every state event rendered once with the scheme uncolored and again
    // a moment later with it colored. That is the flicker.
    const i = this.lastInduction;
    const records =
      i && i.book === model.book && i.index === model.index ? this.layOut(i.records) : undefined;
    const u = this.lastRules;
    const entries =
      u && u.book === model.book && u.index === model.index
        ? this.layOutRules(u.entries)
        : undefined;
    void this.panel?.webview.postMessage({
      kind: 'model',
      model: laid,
      records,
      entries,
      toks: tokenizeAll(
        [
          ...highlightable(laid),
          ...(records ? inductionStrings(records) : []),
          ...(entries ? rulesStrings(entries) : []),
        ],
        this.tokenizer
      ),
    });
  }

  private layOut(records: PanelInduction[]): PanelInduction[] {
    return records.map((r) => ({
      ...r,
      scheme: r.scheme === undefined ? undefined : pretty(r.scheme, this.layoutCols()),
    }));
  }

  tail(book: string, unit: number | null, elapsedMs: number, text: string): void {
    const s = this.shown();
    if (s && s.book === book && s.index === unit) {
      void this.panel?.webview.postMessage({ kind: 'tail', elapsedMs, text });
    }
  }

  /**
   * The induction narrative for the form on show.
   *
   * A separate message rather than a field of the model, because it is a
   * second round trip: the panel renders as soon as the envelope arrives,
   * and this fills in afterwards for the minority of forms that inducted.
   */
  induction(book: string, index: number, records: PanelInduction[]): void {
    const s = this.shown();
    if (s && s.book === book && s.index === index) {
      this.lastInduction = { book, index, records };
      this.postInduction(this.lastInduction);
    }
  }

  private postInduction(i: { book: string; index: number; records: PanelInduction[] }): void {
    const laid = this.layOut(i.records);
    void this.panel?.webview.postMessage({
      kind: 'induction',
      book: i.book,
      index: i.index,
      records: laid,
      toks: tokenizeAll(inductionStrings(laid), this.tokenizer),
    });
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
let inductions = [];
let ruleEntries = [];
let rulesKey = '';
// Which form the records belong to, so a re-render of the *same* form keeps
// them. The panel re-renders on every state event, and clearing on each one
// made the section blink out and back for the whole time a book was being
// checked. No backticks in here: this comment lives inside a template
// literal, and one would end the string.
let inductionKey = null;

window.addEventListener('message', (ev) => {
  const m = ev.data;
  if (m.kind === 'model') {
    model = m.model;
    toks = m.toks || {};
    // A different form: whatever induction the last one did is not this
    // one's. The same form again is just a refresh, and keeps it.
    const key = m.model.book + ':' + m.model.index;
    if (m.records) {
      // Carried with the model, so this render has both the terms and their
      // colors -- rendering once rather than twice is the whole point.
      inductions = m.records;
      inductionKey = key;
    } else if (key !== inductionKey) {
      inductions = [];
      inductionKey = key;
    }
    // Same reasoning for what the form introduced: carried when it is known,
    // dropped when the form changed, kept when this is the same form again.
    if (m.entries) {
      ruleEntries = m.entries;
      rulesKey = key;
    } else if (key !== rulesKey) {
      ruleEntries = [];
      rulesKey = key;
    }
    render();
  } else if (m.kind === 'rules') {
    Object.assign(toks, m.toks || {});
    ruleEntries = m.entries || [];
    rulesKey = m.book + ':' + m.index;
    render();
  } else if (m.kind === 'induction') {
    Object.assign(toks, m.toks || {});
    inductions = m.records || [];
    inductionKey = m.book + ':' + m.index;
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

/**
 * The induction narrative, when there is one.
 *
 * Folded open: for a proof that inducted this is usually the first thing
 * worth reading, and it is a handful of lines rather than a wall.
 */
function inductionHtml() {
  if (!inductions.length) { return ''; }
  // ACL2's attribution is prose -- "the :induction rule segment-listp" --
  // and the two things worth picking out of it are the rune class and the
  // name. A token opening with a colon is unambiguously the former; the
  // token after "rule"/"rules" is the latter. Everything else stays prose,
  // because it is prose.
  const runeWords = (phrase) => {
    // Doubled on purpose: this lives in a template literal, where a lone
    // \\s is not an escape and collapses to the letter s -- which split on
    // every "s" in the phrase and then rejoined into something that looked
    // exactly right.
    const words = String(phrase).split(/(\\s+)/);
    let nameNext = false;
    return words.map((w) => {
      if (/^\\s+$/.test(w)) { return w; }
      const bare = w.replace(/[.,;]+$/, '');
      const tail = w.slice(bare.length);
      if (bare.startsWith(':')) { nameNext = false; return '<code>' + esc(bare) + '</code>' + tail; }
      if (nameNext) { nameNext = false; return '<code class="lisp">' + hl(bare) + '</code>' + tail; }
      if (/^rules?$/.test(bare)) { nameNext = true; }
      return esc(w);
    }).join('');
  };
  const row = (label, value) =>
    '<tr><td class="meta">' + label + '</td><td>' + value + '</td></tr>';
  const one = (r) => {
    let h = '<div class="cp">';
    if (r.goal) { h += '<div class="meta">' + esc(r.goal) + '</div>'; }
    // A labelled table rather than one sentence: these are two different
    // facts -- the term it came from, and the rule that suggested it -- and
    // running them together made both harder to find.
    let rows = '';
    if (r.suggestedBy) {
      rows += row('suggested by', '<code class="lisp">' + hl(r.suggestedBy) + '</code>');
    }
    if (r.attribution) { rows += row('using', runeWords(r.attribution)); }
    if (rows) { h += '<table class="inner">' + rows + '</table>'; }
    // Laid out as ACL2 printed it -- the indentation is the structure.
    if (r.scheme) { h += '<pre>' + hl(r.scheme) + '</pre>'; }
    // Neither justifiedBy nor subgoals is shown. The first is the head
    // symbol of the term in the suggested-by row, so it only repeats it; the
    // second is about what happened next rather than about the scheme, and
    // the subgoals themselves are already the checkpoints above. Both stay
    // on the wire for callers that want them. (No backticks in comments
    // here -- this whole page is a template literal.)
    return h + '</div>';
  };
  return '<details open><summary><h3>Induction (' + inductions.length + ')</h3></summary>' +
    inductions.map(one).join('') + '</details>';
}

/**
 * What the form introduced, per name.
 *
 * Worth the space because almost none of it is in the buffer: the left-hand
 * side ACL2 derived, the trigger a forward-chaining rule waits for, the
 * runes a defun added without being asked. Fields come in ACL2's own order
 * and keep its keys, so nothing here has to know what a rule class carries.
 */
function rulesHtml() {
  if (!ruleEntries.length) { return ''; }
  const row = (label, value) =>
    '<tr><td class="meta">' + esc(label) + '</td><td>' + value + '</td></tr>';
  // A term goes in a pre when it has to wrap; short ones read better inline.
  const term = (t) =>
    String(t).includes('\\n')
      ? '<pre>' + hl(t) + '</pre>'
      : '<code class="lisp">' + hl(t) + '</code>';
  const rune = (u) => {
    let h = '<div class="cp">';
    // The enabled flag first: a rule that exists but is off explains a proof
    // that does not fire far more often than any of its other fields.
    const off = u.enabled === false ? ' <span class="status failed">disabled</span>' : '';
    h += '<div><code class="lisp">' + hl(u.rune) + '</code>' + off + '</div>';
    const rows = (u.fields || [])
      .map((f) => row(f.key.replace(/^:/, ''), term(f.value)))
      .join('');
    if (rows) { h += '<table class="inner">' + rows + '</table>'; }
    return h + '</div>';
  };
  const one = (e) => {
    let h = '<div class="cp"><div><code class="lisp">' + hl(e.name) + '</code>';
    if (e.formals) { h += ' <code class="lisp">' + hl(e.formals) + '</code>'; }
    // Guard verification is the fact a reader cannot see in the source at
    // all, and the one most likely to matter later.
    if (e.symbolClass === ':common-lisp-compliant') {
      h += ' <span class="meta">guards verified</span>';
    } else if (e.symbolClass === ':ideal') {
      h += ' <span class="meta">guards not verified</span>';
    } else if (e.symbolClass === ':program') {
      h += ' <span class="meta">program mode</span>';
    }
    h += '</div>';
    let rows = '';
    if (e.guard) { rows += row('guard', term(e.guard)); }
    if (e.justification) { rows += row('measure', term(e.justification)); }
    if (e.statement) { rows += row('statement', term(e.statement)); }
    if (rows) { h += '<table class="inner">' + rows + '</table>'; }
    h += (e.runes || []).map(rune).join('');
    return h + '</div>';
  };
  const n = ruleEntries.reduce((a, e) => a + (e.runes || []).length, 0);
  return '<details><summary><h3>Introduced (' + n + ' rune' + (n === 1 ? '' : 's') +
    ')</h3></summary>' + ruleEntries.map(one).join('') + '</details>';
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
  h += envelope(
    { ...m, failed: m.status === 'skipped' || m.status === 'failed' },
    'goal-',
    'goal'
  );
  h += inductionHtml();
  h += rulesHtml();
  // No actions row. Retracting and holding the target are editor verbs with
  // their own keybindings, and the full log has one too; a panel that follows
  // the cursor is a poor place to put a button whose effect is elsewhere.
  document.getElementById('root').innerHTML = h;
}

bindGoals(document, (b, slot) => 'goal-' + slot);

// Tell the extension how wide this pane is, in characters.
//
// Only the page can know: the width is pixels and the font is the editor's,
// so the column count is a measurement, not a setting. ACL2 laid its terms
// out for a 77-column terminal and this pane is rarely that, so without this
// the extension would have to guess -- and guess again every time the pane
// is dragged.
function reportCols() {
  const probe = document.createElement('pre');
  probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;margin:0;padding:0';
  probe.textContent = 'x'.repeat(100);
  document.body.appendChild(probe);
  const per = probe.getBoundingClientRect().width / 100;
  probe.remove();
  const room = document.getElementById('root').clientWidth;
  if (per > 0 && room > 0) {
    // A little slack: a term that lands exactly on the edge gets soft-wrapped
    // by the pane anyway, which is the thing being avoided.
    vscode.postMessage({ cmd: 'cols', cols: Math.max(20, Math.floor(room / per) - 2) });
  }
}
reportCols();
// Dragging the pane changes the answer. Debounced, because a drag is a
// stream of resizes and each reply re-flows and re-tokenizes every term.
let colsTimer;
window.addEventListener('resize', () => {
  clearTimeout(colsTimer);
  colsTimer = setTimeout(reportCols, 150);
});
</script>
</body>
</html>`;
