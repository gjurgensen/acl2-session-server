// The console: a window per session for forms the file does not contain.
//
// One of these belongs to one book, and that is the point. The form view is
// per-form -- it follows the cursor and forgets what came before -- but an
// eval belongs to the *session*: it ran against the world that book has
// built, its answer means nothing against another book's world, and it stays
// interesting long after the cursor has moved. Sharing one transcript across
// books put one file's answers in another file's window, which is not a
// rendering bug but a category error.
//
// It is deliberately not a REPL: the session undoes every event an evaluated
// form introduces, so a `defun` typed here is gone by the next
// form. It is for everything that is *not* part of the book -- `pe`, `pl`,
// `pr`, `disabledp`, a throwaway `thm`, `set-gag-mode` -- where routing
// through the file would mean two edits per question and would make the
// question part of the prefix the daemon must admit.
//
// Output is bounded at three levels, because an eval's log runs from four
// lines to forty thousand: finished entries other than the newest fold to
// their summary line, the newest shows a scrolling excerpt, and the whole
// log is one button away in a document.

import * as vscode from 'vscode';
import { TokLines } from './highlight';
import { FormEnvelope, highlightable, tokenizeAll } from './formView';
import { SHARED_CSS, SHARED_JS } from './webviewParts';

export interface ConsoleResult extends FormEnvelope {
  /** `ok` or `failed`. */
  outcome: string;
  /** `none`, `undone`, or `dirty`. */
  worldChange: string;
  /** `proof_failure` or `rejected`, when it failed. */
  failStatus?: string;
}

/** One line of the transcript, kept for re-tokenizing on a theme change. */
interface Entry {
  seq: number;
  form: string;
  result?: ConsoleResult;
}

/** How many finished evals a transcript keeps, on both sides of the wire. */
export const HISTORY = 50;

export class Console {
  private panel?: vscode.WebviewPanel;
  private entries: Entry[] = [];
  /**
   * A webview's script runs asynchronously after its HTML is set, and a
   * message posted before it has registered its listener is simply dropped.
   * That is invisible until something posts *immediately* after opening --
   * which is exactly what evaluating a selection into a closed console does.
   * So the page says when it is listening, and until then posts queue.
   */
  private ready = false;
  private queued: unknown[] = [];

  /** A form typed into this console, or sent to it by a command. */
  onEval?: (seq: number, form: string) => void;
  onGoal?: (seq: number, goal: string, slot: number) => void;
  onFullLog?: (seq: number) => void;

  tokenizer?: (text: string) => TokLines | undefined;

  /** The tab icon, so an ACL2 window is recognizable among a row of tabs. */
  iconPath?: { light: vscode.Uri; dark: vscode.Uri };

  /**
   * @param book the daemon's id, for the extension's own bookkeeping
   * @param title what to call the window -- the file's name, since a user
   *   with two consoles open needs to tell them apart at a glance
   */
  constructor(
    readonly book: string,
    private title: string
  ) {}

  get visible(): boolean {
    return this.panel !== undefined;
  }

  open(focus = true): void {
    if (this.panel) {
      this.panel.reveal(undefined, !focus);
      return;
    }
    this.panel = vscode.window.createWebviewPanel(
      'acl2SessionConsole',
      `ACL2 Console — ${this.title}`,
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: !focus },
      { enableScripts: true, retainContextWhenHidden: true }
    );
    this.panel.iconPath = this.iconPath;
    this.panel.webview.html = html(this.title);
    this.panel.webview.onDidReceiveMessage(
      (m: {
        cmd?: string;
        seq?: number;
        form?: string;
        goal?: string;
        slot?: number;
        gen?: number;
        text?: string;
      }) => {
        // Answered here rather than through the extension: the console
        // already holds the tokenizer, and a keystroke should not have to
        // travel any further than it must.
        if (m.cmd === 'hl' && typeof m.gen === 'number') {
          this.post({ kind: 'inputToks', gen: m.gen, lines: this.tokenizer?.(m.text ?? '') });
          return;
        }
        if (m.cmd === 'ready') {
          this.ready = true;
          const held = this.queued;
          this.queued = [];
          for (const q of held) {
            void this.panel?.webview.postMessage(q);
          }
        } else if (m.cmd === 'eval' && typeof m.seq === 'number' && typeof m.form === 'string') {
          this.entries.push({ seq: m.seq, form: m.form });
          if (this.entries.length > HISTORY) {
            this.entries.splice(0, this.entries.length - HISTORY);
          }
          this.onEval?.(m.seq, m.form);
        } else if (m.cmd === 'goal' && typeof m.seq === 'number' && typeof m.goal === 'string') {
          this.onGoal?.(m.seq, m.goal, m.slot ?? 0);
        } else if (m.cmd === 'fullLog' && typeof m.seq === 'number') {
          this.onFullLog?.(m.seq);
        }
      }
    );
    this.panel.onDidDispose(() => {
      this.panel = undefined;
      this.ready = false;
      this.queued = [];
    });
  }

  private post(message: unknown): void {
    if (!this.panel) {
      return;
    }
    if (this.ready) {
      void this.panel.webview.postMessage(message);
    } else {
      this.queued.push(message);
    }
  }

  /**
   * Evaluate a form that came from somewhere other than the input line --
   * a selection in the buffer, say. The console opens if it was closed, so
   * the answer always has somewhere to land.
   */
  submit(form: string): void {
    this.open(false);
    this.post({ kind: 'submit', form });
  }

  state(seq: number, state: string, ahead: number): void {
    this.post({ kind: 'state', seq, state, ahead });
  }

  tail(elapsedMs: number, text: string): void {
    this.post({ kind: 'tail', elapsedMs, text });
  }

  done(seq: number, result: ConsoleResult, log: string): void {
    const e = this.entries.find((x) => x.seq === seq);
    if (e) {
      e.result = result;
    }
    this.post({
      kind: 'done',
      seq,
      result,
      log,
      // The form itself goes along: it was echoed into the transcript before
      // any of this existed, so its summary line is the one thing that would
      // otherwise sit uncolored until the next theme change.
      toks: tokenizeAll([...highlightable(result), ...(e ? [e.form] : [])], this.tokenizer),
    });
  }

  failed(seq: number, message: string): void {
    this.post({ kind: 'failed', seq, message });
  }

  goalResult(seq: number, slot: number, text: string): void {
    this.post({ kind: 'goalResult', seq, slot, text });
  }

  /**
   * Recolor the whole transcript, for when the theme changed under us.
   *
   * Terms are tokenized in the extension host, so the webview cannot redo
   * this for itself; without it the answers already on screen would keep the
   * old palette while everything around them changed.
   */
  refresh(): void {
    const strings: string[] = [];
    for (const e of this.entries) {
      strings.push(e.form);
      if (e.result) {
        strings.push(...highlightable(e.result));
      }
    }
    this.post({ kind: 'toks', toks: tokenizeAll(strings, this.tokenizer) });
  }

  dispose(): void {
    this.panel?.dispose();
    this.panel = undefined;
  }
}

/** Exported so `webview.mjs` can parse-check it; nothing else imports it. */
export function html(title: string): string {
  return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
${SHARED_CSS}
html, body { height: 100%; }
/* The input is pinned: a transcript that runs for three screens must not
   make the user scroll to the bottom to ask the next question. */
body { margin: 0; display: flex; flex-direction: column; }
#scroll { flex: 1 1 auto; overflow-y: auto; padding: 2px 12px 12px; }
#bar { flex: 0 0 auto; padding: 6px 10px;
       border-top: 1px solid var(--vscode-panel-border, rgba(128, 128, 128, 0.35));
       background: var(--vscode-editor-background); }
/* The input is a textarea with transparent text over a <pre> holding the
   same text, colored. Every metric that decides where a glyph lands -- font,
   size, line height, padding, border width, tab size, wrapping -- must be
   identical in both, or the colors drift off the characters they belong to. */
#wrap { position: relative; }
#form, #hlbox {
  box-sizing: border-box; width: 100%; margin: 0;
  font-family: var(--vscode-editor-font-family, monospace); font-size: 12px;
  line-height: 1.4; padding: 4px 6px; border: 1px solid transparent;
  border-radius: 3px; tab-size: 2;
  white-space: pre-wrap; overflow-wrap: break-word; word-break: normal;
}
/* Both positioned and both given a z-index, so they stack against each other
   rather than against the page. A negative z-index on the overlay does not
   work: #wrap has z-index auto and so is not a stacking context, which sends
   the overlay behind #bar's opaque background rather than behind the
   textarea -- and the text vanishes completely. */
#form { position: relative; z-index: 1; resize: none; display: block;
        background: transparent; color: transparent;
        caret-color: var(--vscode-editorCursor-foreground, var(--vscode-input-foreground));
        border-color: var(--vscode-input-border, rgba(128, 128, 128, 0.35));
        max-height: 30vh; overflow-y: auto; }
#form:focus { outline: 1px solid var(--vscode-focusBorder); }
/* Transparent text would take the placeholder with it. */
#form::placeholder { color: var(--vscode-input-placeholderForeground, rgba(128, 128, 128, 0.8)); }
#hlbox { position: absolute; inset: 0; z-index: 0; overflow: hidden;
         background: var(--vscode-input-background);
         color: var(--vscode-input-foreground); pointer-events: none; }
/* One eval. The whole entry folds to its summary line, and the log inside it
   is bounded on its own -- neither a long transcript nor one long answer can
   push everything else off the window. */
.ev { border-top: 1px solid rgba(128, 128, 128, 0.25); padding: 2px 0; }
.ev > summary { list-style: none; padding: 4px 0; display: flex; gap: 8px;
                align-items: baseline; }
.ev > summary::-webkit-details-marker { display: none; }
.ev > summary .sform { flex: 1 1 auto; overflow: hidden; text-overflow: ellipsis;
                       white-space: nowrap;
                       font-family: var(--vscode-editor-font-family, monospace);
                       font-size: 12px; }
.evlog { max-height: 32vh; overflow: auto; }
.wc { font-size: 11px; padding: 1px 7px; border-radius: 9px;
      background: rgba(212, 172, 13, 0.22); }
.wc.dirty { background: rgba(198, 40, 40, 0.3); font-weight: 600; }
#hint { padding: 10px 0; }
</style>
</head>
<body>
<div id="scroll">
  <p id="hint" class="meta">Forms evaluated against <b>${esc(title)}</b> as the session
  has it so far. Nothing typed here is added to the book: settings stick, but
  events are undone, so this is a place for <code>pe</code>, <code>pl</code>,
  <code>pr</code>, <code>disabledp</code> and throwaway experiments &mdash; not
  for definitions you want to keep.</p>
  <div id="evals"></div>
</div>
<div id="bar">
  <div id="wrap">
    <pre id="hlbox" aria-hidden="true"></pre>
    <textarea id="form" rows="1" spellcheck="false" autocomplete="off"
      placeholder="Enter evaluates · Up/Down recall"></textarea>
  </div>
</div>
<script>
const vscode = acquireVsCodeApi();
${SHARED_JS}

const evals = [];
let nextSeq = 1;
const history = [];
let histAt = 0;

const evalAt = (seq) => evals.find((e) => e.seq === seq);
const pending = () => evals.find((e) => e.state !== 'done');

// Only the newest result stays open. An expanded transcript would push the
// answer to the question just asked off the screen within a few queries.
function collapseOthers(keep) {
  for (const e of evals) {
    if (e !== keep) { e.open = false; }
  }
  keep.open = true;
}

window.addEventListener('message', (ev) => {
  const m = ev.data;
  if (m.kind === 'toks') {
    toks = m.toks || {};
    render();
  } else if (m.kind === 'submit') {
    submit(m.form);
  } else if (m.kind === 'state') {
    const e = evalAt(m.seq);
    if (e) { e.state = m.state; e.ahead = m.ahead; render(); }
  } else if (m.kind === 'tail') {
    const e = pending();
    if (e) { e.state = 'running'; e.tail = m.text || ''; e.elapsedMs = m.elapsedMs; render(); }
  } else if (m.kind === 'done') {
    const e = evalAt(m.seq);
    if (e) {
      Object.assign(toks, m.toks || {});
      e.state = 'done'; e.result = m.result; e.log = m.log;
      collapseOthers(e);
      render();
    }
  } else if (m.kind === 'failed') {
    const e = evalAt(m.seq);
    if (e) { e.state = 'done'; e.error = m.message; collapseOthers(e); render(); }
  } else if (m.kind === 'goalResult') {
    const d = document.getElementById('egoal-' + m.seq + '-' + m.slot);
    if (d) { d.textContent = m.text || '(nothing captured under this goal)'; }
  } else if (m.kind === 'inputToks') {
    // Stale by the time it arrived, or the grammar never loaded: either way
    // the plain text already in the box is the right thing to leave there.
    if (m.gen === hlGen && m.lines) {
      hlbox.innerHTML = spans(m.lines) + (input.value.endsWith('\\n') ? ' ' : '');
    }
  }
});

function entryHtml(e) {
  const r = e.result;
  const chip = e.error ? 'failed' : e.state !== 'done' ? (e.state || 'queued') : r.outcome;
  const secs = r && r.elapsedMs != null
    ? (r.elapsedMs / 1000).toFixed(r.elapsedMs < 10000 ? 1 : 0) + 's'
    : e.elapsedMs != null ? Math.round(e.elapsedMs / 1000) + 's' : '';
  let head = '<summary><span class="status ' + esc(chip) + '">' + esc(chip) + '</span>' +
    '<span class="sform lisp">' + hl(e.form) + '</span>' +
    '<span class="meta">' + esc(secs) + '</span>';
  if (r && r.worldChange && r.worldChange !== 'none') {
    // \`undone\` is the contract working and says so quietly; \`dirty\` is the
    // rollback having failed, which costs a rebuild and is not quiet.
    head += '<span class="wc ' + esc(r.worldChange) + '">' +
      (r.worldChange === 'dirty' ? 'world dirty' : 'world change undone') + '</span>';
  }
  head += '</summary>';

  let body = '';
  if (e.state !== 'done') {
    body += '<p class="meta">' +
      (e.state === 'running'
        ? 'running\\u2026'
        : 'queued' + (e.ahead ? ' behind ' + e.ahead + ' more' : ' \\u2014 the session is busy')) +
      '</p>';
    if (e.tail) { body += '<pre class="evlog">' + esc(e.tail) + '</pre>'; }
  } else if (e.error) {
    body += '<div class="note">' + esc(e.error) + '</div>';
  } else {
    body += envelope({ ...r, failed: r.outcome === 'failed' }, 'egoal-' + e.seq + '-', 'goal');
    // The printed output *is* the answer for a query, so it is shown rather
    // than linked -- but bounded, and with the whole log one click away.
    if (e.log) {
      body += '<h3>Output</h3><pre class="evlog">' + esc(e.log) + '</pre>';
    }
    if (r.outputLines) {
      body += '<p><button data-log="' + e.seq + '">full log (' + r.outputLines +
        ' lines)</button></p>';
    }
  }
  return '<details class="ev" data-seq="' + e.seq + '"' + (e.open ? ' open' : '') + '>' +
    head + body + '</details>';
}

function render() {
  const box = document.getElementById('evals');
  box.innerHTML = evals.map(entryHtml).join('');
  const hint = document.getElementById('hint');
  if (hint && evals.length) { hint.style.display = 'none'; }
  for (const d of box.querySelectorAll('details.ev')) {
    d.addEventListener('toggle', () => {
      const e = evalAt(Number(d.dataset.seq));
      if (e) { e.open = d.open; }
    });
  }
  for (const b of box.querySelectorAll('button[data-log]')) {
    b.addEventListener('click', () =>
      vscode.postMessage({ cmd: 'fullLog', seq: Number(b.dataset.log) }));
  }
  // Each excerpt is already the tail of its log, but the box is shorter than
  // the excerpt -- so start it at the end, where the answer is, rather than
  // 200 lines above it.
  for (const log of box.querySelectorAll('.evlog')) {
    log.scrollTop = log.scrollHeight;
  }
}

bindGoals(document, (b, slot) => 'egoal-' + seqOf(b) + '-' + slot);

// A form is complete when its parens close. Strings, character literals,
// bar-quoted symbols and both comment syntaxes all hide parens that must not
// be counted -- and getting this wrong is not cosmetic: an unbalanced form
// sent to the session swallows the marker that ends it, and ACL2 waits for
// the rest of the input forever.
function complete(s) {
  let depth = 0;
  let i = 0;
  while (i < s.length) {
    const c = s[i];
    if (c === ';') {
      while (i < s.length && s[i] !== '\\n') { i++; }
    } else if (c === '#' && s[i + 1] === '\\\\') {
      i += 3; // #\\( and friends: the paren is a character, not a delimiter
    } else if (c === '#' && s[i + 1] === '|') {
      let n = 1;
      i += 2;
      while (i < s.length && n > 0) {
        if (s[i] === '#' && s[i + 1] === '|') { n++; i += 2; }
        else if (s[i] === '|' && s[i + 1] === '#') { n--; i += 2; }
        else { i++; }
      }
    } else if (c === '"' || c === '|') {
      const q = c;
      i++;
      while (i < s.length && s[i] !== q) { i += s[i] === '\\\\' ? 2 : 1; }
      i++;
    } else {
      if (c === '(') { depth++; }
      else if (c === ')') { depth--; }
      i++;
    }
  }
  // A negative depth is a typo, not an unfinished form: let ACL2 say so.
  return depth <= 0;
}

const input = document.getElementById('form');
const hlbox = document.getElementById('hlbox');

function grow() {
  input.style.height = 'auto';
  input.style.height = Math.min(input.scrollHeight, window.innerHeight * 0.3) + 'px';
}

// What is typed is colored by the same grammar and theme the buffer uses,
// which lives in the extension host -- a webview has neither. So each edit
// asks, and the answer is stamped: replies arrive in order, but a slow one
// for text three keystrokes old must not overwrite the current line.
let hlGen = 0;
function paintInput() {
  const text = input.value;
  // A <pre> gives a trailing newline no height, so a text ending in one
  // would sit a line short of the textarea and drag every later line out of
  // register. The space is invisible and restores the line.
  hlbox.textContent = text.endsWith('\\n') ? text + ' ' : text;
  vscode.postMessage({ cmd: 'hl', gen: ++hlGen, text });
}
function syncScroll() {
  hlbox.scrollTop = input.scrollTop;
  hlbox.scrollLeft = input.scrollLeft;
}
input.addEventListener('input', () => { grow(); paintInput(); syncScroll(); });
input.addEventListener('scroll', syncScroll);

input.addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter' && !ev.shiftKey) {
    const text = input.value;
    // Enter finishes a form and newlines an unfinished one -- the REPL
    // behavior, and here also the guard that keeps a half-typed form from
    // wedging the session.
    if (!text.trim() || !complete(text)) { return; }
    ev.preventDefault();
    setInput('');
    submit(text.trim());
    return;
  }
  // History only where it cannot fight editing: at the very start or end of
  // what is typed.
  if (ev.key === 'ArrowUp' && histAt > 0 && input.selectionStart === 0) {
    ev.preventDefault();
    setInput(history[--histAt]);
  } else if (ev.key === 'ArrowDown' && input.selectionStart === input.value.length) {
    if (histAt < history.length - 1) {
      ev.preventDefault();
      setInput(history[++histAt]);
    } else if (histAt < history.length) {
      ev.preventDefault();
      histAt = history.length;
      setInput('');
    }
  }
});

/** Set the text and bring the overlay, the height and the scroll along. */
function setInput(v) {
  input.value = v;
  grow();
  paintInput();
  syncScroll();
}

function submit(form) {
  const seq = nextSeq++;
  const e = { seq, form, state: 'queued', ahead: 0, open: true };
  evals.push(e);
  // The same ceiling the extension keeps, so a long session's transcript
  // does not turn into an unbounded pile of DOM.
  if (evals.length > ${HISTORY}) { evals.splice(0, evals.length - ${HISTORY}); }
  collapseOthers(e);
  if (history[history.length - 1] !== form) { history.push(form); }
  histAt = history.length;
  render();
  vscode.postMessage({ cmd: 'eval', seq, form });
  const s = document.getElementById('scroll');
  s.scrollTop = s.scrollHeight;
}

input.focus();
// Last, so nothing the extension sends in reply can arrive before the
// listener above is installed.
vscode.postMessage({ cmd: 'ready' });
</script>
</body>
</html>`;
}

/** The title goes into the page, so it is escaped like anything else would be. */
function esc(s: string): string {
  return s.replace(
    /[&<>"]/g,
    (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c] ?? c
  );
}
