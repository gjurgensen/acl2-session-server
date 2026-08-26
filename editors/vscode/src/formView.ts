// The structured form view: an infoview-shaped webview beside the editor,
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
import { DocCode } from './xdoc';
import { pageHtml, SHARED_CSS, SHARED_JS } from './webviewParts';
import { MAX_TERM } from './policy';

/**
 * What the tab says.
 *
 * Bare, where its siblings are named -- `ACL2 Docs`, `ACL2 Output`, `ACL2
 * Eval`, `ACL2 Console`. This is the one you keep open, so it earns the plain
 * name; and it is the only one whose title carries *state*, since holding it
 * appends `(pinned)`. Tabs truncate, and a longer base name would cut off
 * exactly the part worth reading.
 */
const TITLE = 'ACL2';

export interface FormCheckpoint {
  kind: string;
  goal?: string;
  term: string;
}

export interface FormInnerEvent {
  name: string;
  kind?: string;
  time?: string;
  proverSteps?: number;
  rules?: string[];
  abortCauses?: string;
  /**
   * What this event left in the world, if it is known.
   *
   * Never set here: it arrives in its own message a round trip after the
   * model, and the page joins the two by name at render time. Declared all
   * the same, because `envelope()` reads it.
   */
  detail?: FormNameRules;
}

/**
 * What ACL2 had to say about one form. A unit and an eval carry the same
 * fields meaning the same things; each adds its own on top.
 */
export interface FormEnvelope {
  elapsedMs?: number;
  proverSteps?: number;
  outputLines?: number;
  errorLine?: string;
  abortCause?: string;
  checkpoints: FormCheckpoint[];
  rules: string[];
  warnings?: string;
  time?: string;
  /**
   * What the form evaluated to, as ACL2 printed it.
   *
   * Absent when there is nothing to show, and that is ACL2's judgement
   * rather than this end's: a triple whose error component is non-nil or
   * whose value is `:invisible` prints nothing at the REPL and arrives
   * missing here. Absent too when the value only repeats a name the form
   * introduced, since the event below already carries that name.
   */
  value?: string;
  /** Whether it came back as an error triple. Carried, not rendered. */
  valueTriple?: boolean;
  hintEvents?: string[];
  splitterRules?: string[];
  preRevert?: boolean;
  forced?: boolean;
  inner?: FormInnerEvent[];
  innerTotal?: number;
  nTop?: number;
  nSub?: number;
}

export interface FormRuneField {
  key: string;
  value: string;
}

export interface FormRune {
  rune: string;
  class?: string;
  enabled?: boolean;
  fields: FormRuneField[];
}

/**
 * What one name a form introduced actually is, in the world.
 *
 * The point of showing it is that almost none of it is written in the file:
 * the left-hand side a `defthm` will rewrite, the trigger a
 * forward-chaining rule waits for, the four runes a `defun` quietly adds.
 * The definition rune's body is deliberately absent -- that part *is* in the
 * file, a few lines above.
 *
 * It hangs off the event that introduced it, under "inside this form": these
 * are facts about one `defun` or one `defthm`, and a `define` that writes
 * four of them wants them beside their own names rather than pooled.
 */
export interface FormNameRules {
  name: string;
  /** Whether this name introduced anything. A name may be documented and not. */
  found?: boolean;
  /**
   * This name's xdoc summary, when a topic of this name exists -- as an HTML
   * fragment, already rendered and escaped by `xdocHtml`, not as the raw
   * markup the wire carries.
   *
   * Rendered before it gets here because a short is xdoc markup like any
   * other body: most of them carry `@('...')`, `@(see ...)` or `@(tsee ...)`,
   * which a reader should not be looking at. The page inserts this rather
   * than escaping it.
   */
  short?: string;
  /** The code lifted out of `short`, for the page to fill its holes with. */
  shortCode?: DocCode[];
  /**
   * This name's rendered xdoc body, when the topic has one. Its presence is
   * what gives a documented name something to open onto.
   */
  long?: string;
  /** The code lifted out of `long`, likewise. */
  longCode?: DocCode[];
  symbolClass?: string;
  measure?: string;
  relation?: string;
  formals?: string;
  guard?: string;
  statement?: string;
  runes: FormRune[];
}

export interface FormModel extends FormEnvelope {
  book: string;
  index: number;
  total: number;
  lineStart: number;
  lineEnd: number;
  /**
   * The form's columns. Not rendered -- they identify the form for the
   * extension's own bookkeeping, which is the job they were always doing
   * underneath the line-and-column line the header used to carry.
   */
  colStart: number;
  colEnd: number;
  /** The paint status: `admitted`, `conditional`, `skipped`, `failed`, ... */
  status: string;
}

/**
 * Whether two records say the same thing.
 *
 * By serialisation rather than field by field: these are small trees of plain
 * data whose fields are ACL2's and therefore open-ended, so a hand-written
 * comparison would quietly stop noticing whatever gets added next.
 *
 * Sound to use on anything the daemon sends because all of it is JSON that
 * arrived over the wire -- there is nothing in here to serialise that did not
 * serialise on the way in.
 */
function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** The strings in a rules record that are ACL2 code rather than prose. */
export function rulesStrings(entries: FormNameRules[]): string[] {
  return entries.flatMap((e) => [
    // Code lifted out of a topic is code like any other on this page, and the
    // map is keyed by source text, so it is coloured by being listed here.
    ...(e.shortCode ?? []).map((c) => c.text),
    ...(e.longCode ?? []).map((c) => c.text),
    ...(e.formals === undefined ? [] : [e.formals]),
    ...(e.guard === undefined ? [] : [e.guard]),
    ...(e.measure === undefined ? [] : [e.measure]),
    ...(e.relation === undefined ? [] : [e.relation]),
    ...(e.statement === undefined ? [] : [e.statement]),
    ...e.runes.flatMap((r) => [r.rune, ...r.fields.map((f) => f.value)]),
  ]);
}

/** The strings in an envelope that are ACL2 code rather than prose. */
export function highlightable(m: FormEnvelope): string[] {
  return [
    ...(m.value === undefined ? [] : [m.value]),
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
 *
 * A term over `MAX_TERM` gets no entry at all, which is the whole of the
 * fallback: `hl` in the pages renders any string it cannot find as plain
 * escaped text, so an oversized term arrives uncolored rather than as half a
 * million `<span>` elements. Nothing downstream has to know.
 */
export function tokenizeAll(
  strings: string[],
  tokenizer?: (text: string) => TokLines | undefined
): Record<string, TokLines> {
  const toks: Record<string, TokLines> = {};
  if (tokenizer) {
    for (const s of strings) {
      if (s.length > MAX_TERM) {
        continue;
      }
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

export class FormView {
  private panel?: vscode.WebviewPanel;
  /** Whether the page has said it is listening. See {@link FormView.send}. */
  private ready = false;
  private queued: unknown[] = [];
  private model?: FormModel;
  private lastRules?: { book: string; index: number; entries: FormNameRules[] };
  /** The serialised model last sent to the page. See `render`. */
  private postedModel?: string;

  /**
   * Asked for by the page's button while a form is running.
   *
   * Carries the book and unit rather than letting the extension work them
   * out. Clicking a button in here focuses the webview, so there is no
   * active text editor to find a book from -- the same reason the pin
   * commands do not go through `forBook`. The form view knows exactly which
   * form it is showing; it should say so.
   */
  onShowOutput?: (book: string, index: number) => void;

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

  /**
   * Told the width, so the daemon can lay terms out for it.
   *
   * Laying out happens there rather than here: it is a projection of a term
   * and nothing about it needs an editor, so one implementation serves every
   * client instead of one per client. What only this end can do is *measure*
   * -- the width is pixels and a font -- so what crosses is the number.
   */
  onCols?: (cols: number) => void;

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
      'acl2SessionFormView',
      TITLE,
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
      { enableScripts: true, retainContextWhenHidden: true }
    );
    this.panel.iconPath = this.iconPath;
    this.panel.webview.html = pageHtml(HTML, this.panel.webview);
    this.panel.webview.onDidReceiveMessage(
      (m: { cmd?: string; cols?: number }) => {
      // Width first: it arrives before any form is shown, and re-flowing is
      // the whole reason the page reports it.
      if (m.cmd === 'cols' && typeof m.cols === 'number' && m.cols !== this.cols) {
        this.cols = m.cols;
        this.onCols?.(this.layoutCols());
        return;
      }
      if (m.cmd === 'showOutput' && this.model) {
        this.onShowOutput?.(this.model.book, this.model.index);
        return;
      }
      if (m.cmd === 'ready') {
        this.ready = true;
        const held = this.queued;
        this.queued = [];
        for (const q of held) {
          void this.panel?.webview.postMessage(q);
        }
        return;
      }
    }
    );
    this.panel.onDidDispose(() => {
      this.panel = undefined;
      this.ready = false;
      this.queued = [];
      // The next panel is a fresh page that has been told nothing, so what
      // this one was last sent is no longer a reason to stay silent.
      this.postedModel = undefined;
    });
    if (this.model) {
      this.post(this.model);
    }
  }

  render(model: FormModel): void {
    this.model = model;
    // The guard `rules` carries, for the same reason and with more force:
    // this post is the larger of the two. It carries the model and the rules
    // records together and tokenizes both in one pass -- so a render that
    // changes nothing is the most expensive way the form view can do
    // nothing.
    //
    // It is not a rare case. `onState` follows the form view on every state
    // event, and a form landing produces a burst of them while the form on
    // screen is identical each time. Measured on the `defines` in
    // `kestrel/c/syntax/validator.lisp`: fifteen state events in the six
    // seconds after it landed, hence fifteen identical re-posts and fifteen
    // whole-page renders.
    //
    // `refresh` and the panel's first post call `post` directly, so a theme
    // change and a newly opened panel still send unconditionally. The page
    // keeps its state while hidden (`retainContextWhenHidden`), so there is
    // no reset behind our back to re-post for.
    const key = JSON.stringify(model);
    if (key === this.postedModel) {
      return;
    }
    this.post(model, key);
  }

  /**
   * Fill in what the form introduced.
   *
   * Keyed by book and unit: the cursor may have moved on while this was
   * being fetched, and detail for the form you have left is noise.
   */
  rules(book: string, index: number, entries: FormNameRules[]): void {
    const s = this.shown();
    if (!s || s.book !== book || s.index !== index) {
      return;
    }
    // A render replaces the whole page, which resets every dropdown the
    // reader has opened. So a post that would change nothing must not
    // happen: the form view follows the cursor, and the same form arriving again
    // is the common case, not the rare one.
    const l = this.lastRules;
    if (l && l.book === book && l.index === index && same(l.entries, entries)) {
      return;
    }
    this.lastRules = { book, index, entries };
    this.postRules(this.lastRules);
  }

  private postRules(r: { book: string; index: number; entries: FormNameRules[] }): void {
    this.send({
      kind: 'rules',
      book: r.book,
      index: r.index,
      entries: r.entries,
      toks: tokenizeAll(rulesStrings(r.entries), this.tokenizer),
    });
  }

  /**
   * Say the width again, for when the setting that caps it changed.
   *
   * The page's own measurement has not moved, so there is nothing to ask it;
   * what changed is the cap this end puts on it.
   */
  reportCols(): void {
    this.onCols?.(this.layoutCols());
  }

  /**
   * Mark the tab as held, or stop marking it.
   *
   * In the title because that is where it stays visible: the tab-bar icon
   * shows the state only while the form view's own tab has focus, and a reader
   * watching a form is by definition looking somewhere else. VSCode's own
   * markdown preview does the same thing, showing a pinned preview as
   * `[Preview] file.md`.
   */
  setPinLabel(label: string | undefined): void {
    if (this.panel) {
      this.panel.title = label ? `${TITLE} (${label})` : TITLE;
    }
  }

  /**
   * Say something to the reader in the page itself.
   *
   * For the one thing the form view has to volunteer rather than answer: that it
   * has stopped being held on the form it was held on. A notification would be
   * dismissed before it was read and a status-bar entry would be missed; this
   * is in the place the reader is already looking.
   */
  note(text: string | undefined): void {
    this.send({ kind: 'note', text });
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
  /**
   * Post, or hold until the page is listening.
   *
   * A webview's script runs asynchronously after its HTML is set, and a
   * message posted before it registers its listener is dropped with no error
   * anywhere -- the drop this codebase documents on `Console.ready` and which
   * `DocView` also handles. The form view was the one that did not, and it had
   * the worst consequence of the three, because `post` records what it sent:
   * a dropped model still set `postedModel`, so `render`'s dedup then
   * suppressed every identical re-post and the view sat on its placeholder
   * until the cursor moved to a *different* form.
   *
   * Reproducible: check a book, close the form-view tab, click the status bar.
   */
  private send(message: unknown): void {
    if (!this.panel) {
      return;
    }
    if (this.ready) {
      void this.panel.webview.postMessage(message);
    } else {
      this.queued.push(message);
    }
  }

  private post(model: FormModel, key = JSON.stringify(model)): void {
    // What was last actually sent, so `render` can tell a repeat from a
    // change. Recorded here rather than in `render` so the unconditional
    // callers -- `refresh`, and the panel's first post -- keep it honest.
    this.postedModel = key;
    // No re-flow here: terms arrive from the daemon already laid out for
    // the width this pane reported. The fold limit rides along because the
    // page decides per form whether its events open, and only this end can
    // read a setting.
    const laid = {
      ...model,
      eventsOpenLimit: vscode.workspace
        .getConfiguration('acl2-session')
        .get<number>('eventsOpenLimit', 4),
    };
    const u = this.lastRules;
    const entries =
      u && u.book === model.book && u.index === model.index
        ? u.entries
        : undefined;
    this.send({
      kind: 'model',
      model: laid,
      entries,
      toks: tokenizeAll(
        [...highlightable(laid), ...(entries ? rulesStrings(entries) : [])],
        this.tokenizer
      ),
    });
  }

  tail(book: string, unit: number | null, elapsedMs: number, text: string): void {
    const s = this.shown();
    if (s && s.book === book && s.index === unit) {
      this.send({ kind: 'tail', elapsedMs, text });
    }
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
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-%%NONCE%%';
               font-src %%CSPSOURCE%%;">
<style>
${SHARED_CSS}
html, body { height: 100%; }
body { padding: 2px 12px 12px; box-sizing: border-box; }
#tailtext { max-height: 45vh; overflow: auto; }
/* While a form is running the log is the only thing on the page, so it gets
   the pane rather than a fixed box with dead space under it.
   Grown, not stretched: \`0 1 auto\` lets the log take its content's height
   up to whatever is left, so a long proof fills the pane and scrolls while
   a proof that has printed two lines is two lines tall. Stretching it with
   \`flex: 1\` would fill the pane either way and leave the button stranded
   at the foot of an empty box, when where it belongs is under the output.
   \`min-height: 0\` on both, or a flex child refuses to shrink below the
   size of its content and the *pane* scrolls instead of the log. Only while
   the fold is open: a closed one keeps its own height and the button rises
   to meet it. */
#root { min-height: 100%; }
#root.running { display: flex; flex-direction: column; height: 100%; }
#root.running details.tail[open] {
  display: flex; flex-direction: column; flex: 0 1 auto; min-height: 0;
}
#root.running details.tail[open] #tailtext {
  flex: 0 1 auto; min-height: 0; max-height: none;
}
.openrow { margin: 8px 0 0; }
</style>
</head>
<body>
<div id="root"><p class="meta">Put the cursor in a form of a live book.</p></div>
<script nonce="%%NONCE%%">
const vscode = acquireVsCodeApi();
${SHARED_JS}
let model;
// Something the form view has to say on its own account -- that it stopped being
// held on a form. It outlives the render that follows it, which is the whole
// point: letting go is followed at once by showing whatever the cursor is in,
// and a message the next render wiped would never be seen. Cleared when the
// form view is held again.
let note;
let ruleEntries = [];
let rulesKey = '';

window.addEventListener('message', (ev) => {
  const m = ev.data;
  if (m.kind === 'model') {
    model = m.model;
    toks = m.toks || {};
    const key = m.model.book + ':' + m.model.index;
    // Folds belong to a form. Arriving at a different one forgets them, the
    // same as the rules records below.
    foldScopeIs(key);
    // What the form introduced rides with the model when it is known,
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
  } else if (m.kind === 'note') {
    note = m.text;
    render();
  } else if (m.kind === 'tail') {
    const t = document.getElementById('tailtext');
    if (t) {
      t.textContent = m.text || '';
      t.scrollTop = t.scrollHeight;
      const e = document.getElementById('elapsed');
      if (e) { e.textContent = Math.round(m.elapsedMs / 1000) + 's'; }
    }
  }
});


/**
 * Hang what each name turned out to be on the event that introduced it.
 *
 * The two arrive separately -- the envelope with the model, the scrape a
 * round trip later -- and are joined here rather than in the extension so
 * that either can land first. Case-insensitively and through a Map: the
 * names are ACL2 symbols, so one of them being spelled "constructor" is a
 * question of which book is open, not a hypothetical.
 */
function withDetail(m) {
  const inner = m.inner || [];
  if (!ruleEntries.length) { return m; }
  const by = new Map(ruleEntries.map((e) => [String(e.name).toLowerCase(), e]));
  const named = new Set(inner.map((e) => String(e.name).toLowerCase()));
  // Documentation belonging to no event of this form. A standalone defxdoc
  // records no event at all -- it writes a table -- so its topic matches
  // nothing above and would vanish if this only ever decorated events. It is
  // the case the whole form-level section exists for, and the reason this no
  // longer gives up when a form recorded nothing.
  const docs = ruleEntries.filter(
    (e) => e.short && !named.has(String(e.name).toLowerCase())
  );
  return { ...m, docs, inner: inner.map((e) => {
    const d = by.get(String(e.name).toLowerCase());
    return d ? { ...e, detail: d } : e;
  }) };
}

function render() {
  const m = model;
  if (!m) {
    const empty = document.getElementById('root');
    empty.className = '';
    empty.innerHTML = note ? '<div class="notice">' + esc(note) + '</div>' : '';
    return;
  }
  let h = note ? '<div class="notice">' + esc(note) + '</div>' : '';
  // The chip, and what the form cost. Neither the coordinates nor the
  // position in the book earned their place: a reader looking at this panel
  // has the form under their cursor, and the events below name it better than
  // an ordinal could.
  h += '<div class="h">' +
    '<span class="status ' + esc(m.status) + '">' + esc(statusLabel(m.status)) + '</span>';
  if (m.elapsedMs != null) {
    h += '<span class="meta">' + (m.elapsedMs / 1000).toFixed(m.elapsedMs < 10000 ? 1 : 0) + 's</span>';
  }
  if (m.proverSteps != null) { h += '<span class="meta">' + m.proverSteps + ' steps</span>'; }
  h += '</div>';
  if (m.status === 'running') {
    // Highlighting the tail here would mean a second grammar and a full
    // re-tokenize twice a second, because a webview cannot do what the
    // output view does -- that is a real TextDocument, colored by VSCode
    // from its language. So hand the reader to the thing that already works.
    h += fold('tail', true,
      '<h3>Prover output <span id="elapsed" class="meta"></span></h3>',
      '<pre id="tailtext">(waiting for output\\u2026)</pre>', 'tail') +
      '<p class="openrow"><button id="openoutput">Open in Output view</button></p>';
  }
  // pending and beyond render nothing below the chip. There is no further
  // information about either, and a sentence saying so is filler -- the
  // difference between them, that one is queued and one is not, is carried
  // by the word in the chip.
  h += envelope(
    withDetail({ ...m, failed: m.status === 'skipped' || m.status === 'failed' }),
    'goal-'
  );
  // No actions row. Retracting and holding the target are editor verbs with
  // their own keybindings, and the full log has one too; a form view that follows
  // the cursor is a poor place to put a button whose effect is elsewhere.
  const root = document.getElementById('root');
  // The one state whose content should fill the pane rather than flow down
  // it. Set on the element rather than baked into the markup because the
  // page is one document across every state.
  root.className = m.status === 'running' ? 'running' : '';
  root.innerHTML = h;
}

bindFolds(document);

// The output view is a command away, and the tail is the one thing this page
// deliberately does not render well.
document.addEventListener('click', (ev) => {
  if (ev.target && ev.target.id === 'openoutput') {
    vscode.postMessage({ cmd: 'showOutput' });
  }
});

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
// The listener above is installed, so anything held back may come now. A
// webview's script runs asynchronously after its HTML is set, and a message
// posted before this point is dropped without a word.
vscode.postMessage({ cmd: 'ready' });
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
