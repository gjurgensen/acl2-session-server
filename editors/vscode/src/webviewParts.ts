// The parts of a webview the form view and the console both need.
//
// Two webviews exist because they answer different questions. The form view is
// per-form: it follows the cursor and shows the envelope of whatever the
// cursor is in. The console is per-session: its transcript belongs to one
// book and outlives every cursor move. But a result is a result, and a
// failed `thm` typed into the console must render exactly as a failed
// `defthm` in the file does -- so the renderer lives here, in one copy,
// rather than in two that drift.
//
// A webview cannot import a module, so these are strings interpolated into
// each page. They are already-evaluated JS and CSS: interpolating them adds
// no further escaping, so what is written here is what the browser sees.

import * as vscode from 'vscode';

/**
 * Where this extension was installed, for building URLs into its own files.
 *
 * Module-level and set once at activation, because every page needs the same
 * answer and none of them is constructed with a context. It is only ever
 * read while building a page's HTML, which cannot happen before `activate`.
 */
let extensionUri: vscode.Uri | undefined;

export function setExtensionUri(uri: vscode.Uri): void {
  extensionUri = uri;
}

/**
 * Fill in the two things a page cannot know for itself: where the icon font
 * lives, and what this particular webview will accept loading it from.
 *
 * Both are per-webview -- `asWebviewUri` rewrites a file path into the
 * scheme that webview will serve, and `cspSource` is the origin that goes
 * with it -- so neither can be baked into a page's HTML the way the rest of
 * it is. If the font fails to resolve the pages still render; the folds just
 * get no chevron.
 */
export function pageHtml(html: string, webview: vscode.Webview): string {
  const font = extensionUri
    ? webview
        .asWebviewUri(
          vscode.Uri.joinPath(
            extensionUri, 'node_modules', '@vscode', 'codicons', 'dist', 'codicon.ttf'
          )
        )
        .toString()
    : '';
  return html.replace(/%%CODICON%%/g, font).replace(/%%CSPSOURCE%%/g, webview.cspSource);
}

/** Typography, status chips, code blocks and folds -- the shared look. */
export const SHARED_CSS = `
body { font-family: var(--vscode-font-family); color: var(--vscode-foreground);
       font-size: 13px; }
.h { display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; margin: 10px 0 4px; }
.status { font-weight: 600; padding: 1px 8px; border-radius: 9px; }
.status.admitted, .status.ok { background: rgba(46, 125, 50, 0.25); }
.status.conditional { background: rgba(212, 172, 13, 0.25); }
.status.skipped, .status.failed { background: rgba(198, 40, 40, 0.25); }
.status.running { background: rgba(90, 140, 210, 0.3); }
.status.pending, .status.queued, .status.beyond { background: rgba(136, 136, 136, 0.2); }
.meta { opacity: 0.7; font-size: 12px; }
h3 { margin: 14px 0 4px; font-size: 11px; text-transform: uppercase;
     letter-spacing: 0.05em; opacity: 0.75; }
pre, code { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; }
/* A block of ACL2's own text -- a checkpoint term, the live log, a value --
   marked by a rule down its left edge rather than by a filled slab. The
   slab read as heavy against a panel that is mostly code already, and three
   of them in one failure made the page look built out of boxes. The rule
   says the same thing (this is one block, and it is ACL2 speaking) at a
   fraction of the weight, and keeps working when several sit in a row. */
pre { background: none; margin: 4px 0; padding: 1px 0 1px 10px;
      border-left: 2px solid var(--vscode-panel-border, rgba(128, 128, 128, 0.3));
      overflow: auto; white-space: pre-wrap; }
/* Override the webview default: its light chip behind inline code is
   unreadable on dark themes. A faint neutral chip with the theme
   foreground works on both. */
code { background-color: rgba(128, 128, 128, 0.14);
       color: var(--vscode-foreground); padding: 0 4px; border-radius: 3px; }
pre code { background: none; padding: 0; }
/* The chip is for a token being called out on its own -- an abort cause, a
   name in a sentence. It is not for the places that are *made* of code: a
   rule per line, a table of a rule's fields, the name and signature at the
   head of an event. Left on there, every one of them became a grey pill and
   the shading stopped meaning anything, since what it is supposed to mark is
   a block of ACL2's own text you might select as a unit -- a checkpoint, the
   live tail -- and those are pre blocks, which keep it. */
ul.rules code,
table.inner code,
details.event > summary code,
.event.bare .esum code,
.failed-what code { background: none; padding: 0; }
button { background: var(--vscode-button-secondaryBackground);
         color: var(--vscode-button-secondaryForeground); border: none;
         padding: 2px 9px; border-radius: 3px; cursor: pointer; font-size: 11px; }
button:hover { background: var(--vscode-button-secondaryHoverBackground); }
.note { background: rgba(198, 40, 40, 0.15); padding: 6px 9px; border-radius: 4px;
        margin: 8px 0; font-size: 12px; }
/* The form view speaking for itself rather than about a form. Neutral on purpose:
   red in here already means a form went red. */
.notice { border-left: 3px solid var(--vscode-focusBorder, #888); padding: 4px 9px;
          margin: 8px 0; font-size: 12px; opacity: 0.85; }
/* Rules one per line, the way :pr prints them. No chip behind each: the
   line break delimits them, which is all the chip was ever doing. */
ul.rules { list-style: none; margin: 3px 0 0; padding: 0; }
ul.rules > li { padding: 1px 0; }
ul.rules table.inner { margin: 2px 0 5px 14px; }
td.blockrow { padding: 0 0 3px; }
td.blockrow pre { margin: 2px 0 4px; }
.cp { margin: 8px 0; }
details > summary { cursor: pointer; }
details > summary h3 { display: inline; }
/* VSCode's own icon font, so a fold in here opens with the same chevron the
   explorer's twisties use. A webview receives no icons for free, so the file
   is shipped with the extension and its URL is filled in per webview -- see
   \`pageHtml\`. Scoped to \`data-fold\`, which is exactly the folds \`fold()\`
   emits: the console draws its own entry summaries and is left alone. */
@font-face {
  font-family: "codicon";
  src: url("%%CODICON%%") format("truetype");
}
details[data-fold] > summary { list-style: none; }
details[data-fold] > summary::-webkit-details-marker { display: none; }
details[data-fold] > summary::before {
  font-family: "codicon";
  content: "\\eab6";
  display: inline-block;
  width: 14px;
  font-size: 13px;
  line-height: 1;
  vertical-align: baseline;
  opacity: 0.75;
}
details[data-fold][open] > summary::before { content: "\\eab4"; }
table.inner { border-collapse: collapse; font-size: 12px; }
table.inner td { padding: 1px 10px 1px 0; vertical-align: baseline; }
.failtag { color: var(--vscode-terminal-ansiRed, #cc6666); font-weight: 600; }
/* One event a form recorded. Classed "event" and not "ev": the console
   already uses "ev" for a whole eval entry, and these pages share a
   stylesheet. */
details.event, .event.bare {
  border: 1px solid var(--vscode-panel-border, rgba(128, 128, 128, 0.25));
  border-radius: 4px; margin: 6px 0;
}
details.event > summary, .event.bare .esum { padding: 5px 8px; }
.einside { padding: 2px 9px 8px 20px; }
.ekind { font-size: 11px; opacity: 0.65; margin-right: 6px; }
.ename { font-weight: 600; }
/* On its own line: a signature and four facts do not fit beside each other
   in a pane this narrow, and wrapping them mid-phrase reads worse. */
.efacts { display: block; margin-top: 2px; }
/* A rule that exists but is off. Red because it is the field most likely to
   explain a proof that did not fire. */
.off {
  background: rgba(198, 40, 40, 0.25); border-radius: 3px; padding: 0 5px;
  font-size: 10px; text-transform: uppercase; letter-spacing: 0.06em;
  margin-left: 5px;
}
.failed-what { margin: 10px 0 2px; }
`;

/**
 * Escaping, the little ACL2-isms, the term colorizer, and `envelope()` --
 * the renderer for what ACL2 had to say about one form.
 *
 * Every page that uses this must define `vscode` (from `acquireVsCodeApi`)
 * before it, and must assign `toks` from the extension's `toks` payload.
 */
export const SHARED_JS = `
const esc = (s) => String(s).replace(/[&<>"]/g,
  (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
// Not anchored: the outer abort cause is a bare symbol, but an inner event's
// causes arrive as a printed list -- \`(ACL2::INDUCTION-DEPTH-LIMIT-EXCEEDED)\`
// -- where a leading-only strip never fires. Only \`acl2::\` goes, and only
// where it is a whole package prefix, so \`my-acl2::\` is left alone.
const strip = (s) => String(s).replace(/(?<![\\w-])acl2::/gi, '');

// ACL2 hands us one cause or a list of them, printed as it prints lists. The
// parentheses are a Lisp-ism the reader did not ask for and the plural is
// the point, so peel the list and report the causes as causes.
function causes(s) {
  const t = String(s).trim();
  const body = t.startsWith('(') && t.endsWith(')') ? t.slice(1, -1) : t;
  return body.split(/\\s+/).filter(Boolean).map(strip);
}

// Terms arrive tokenized and colored by the extension, which ran them
// through the same grammar and the same theme rules the editor uses. Here
// they are only spans; a string with no entry is code we were not asked to
// highlight, or a tokenizer that has not finished loading.
let toks = {};
function spans(lines) {
  return lines.map((line) => line.map((t) =>
    t.c
      ? '<span style="color:' + t.c + (t.i ? ';font-style:italic' : '') + '">' + esc(t.t) + '</span>'
      : esc(t.t)
  ).join('')).join('\\n');
}
function hl(src) {
  const lines = toks[String(src)];
  return lines ? spans(lines) : esc(src);
}

/**
 * Which folds the reader has opened or closed, by id, and what those ids
 * belong to.
 *
 * A render replaces the page's markup wholesale, and \`open\` is state on the
 * \`<details>\` element -- so without this every re-render returns every fold
 * to its authored default. That is not a rare event: the form view re-renders
 * on every state event, and a form landing produced fifteen of them in six
 * seconds in \`kestrel/c/syntax/validator.lisp\`. The rules scrape and the
 * induction narrative each arrive as their own later message besides. Open a
 * rule list, let the scrape land two hundred milliseconds later, and it shuts
 * under you.
 *
 * A map rather than a set, because the three states are distinct: opened,
 * closed, and never touched -- and only the last should take the section's
 * default.
 */
let folds = {};
let foldScope = '';

/**
 * Say what the folds now belong to, forgetting them if it changed.
 *
 * The form view follows the cursor, so folds opened on the form you have left
 * mean nothing on the one you arrived at. Same lifecycle as \`inductionKey\`
 * and \`rulesKey\` in the form view, and for the same reason. The console
 * never calls this: its transcript is cumulative, so its folds are too.
 */
function foldScopeIs(scope) {
  if (scope !== foldScope) {
    foldScope = scope;
    folds = {};
  }
}

/**
 * A \`<details>\` whose open state survives the next render.
 *
 * \`id\` must be derived from what is inside it -- \`4:event:my-app:introduced\`
 * -- and never from its position. A positional id is wrong the moment a list
 * changes length, which is exactly when a form is re-probed and re-rendered.
 */
function fold(id, openByDefault, summaryHtml, bodyHtml, cls) {
  const open = id in folds ? folds[id] : openByDefault;
  return '<details data-fold="' + esc(id) + '"' +
    (cls ? ' class="' + cls + '"' : '') + (open ? ' open' : '') + '>' +
    '<summary>' + summaryHtml + '</summary>' + bodyHtml + '</details>';
}

/**
 * Record every fold the reader touches.
 *
 * In the capture phase on purpose: \`toggle\` does not bubble, so a delegated
 * listener on the document never sees it otherwise. One listener for the page
 * rather than one per element, because the elements are replaced on every
 * render and their listeners would go with them.
 */
function bindFolds(root) {
  root.addEventListener('toggle', (ev) => {
    const d = ev.target;
    if (d && d.tagName === 'DETAILS' && d.dataset && d.dataset.fold) {
      folds[d.dataset.fold] = d.open;
    }
  }, true);
}

/**
 * What a status is called, where a reader can see it.
 *
 * The chips stopped echoing the wire, and not for taste: \`admitted\` is
 * simply wrong for half of what it covers -- \`(+ 1 2)\` is not admitted, it
 * is evaluated -- and the console had already needed a second word,
 * \`ok\`, for the same green state, which is why the stylesheet still
 * carries the two class names as a pair. Both land on one label here.
 *
 * The words are verbs -- what happened to this form -- rather than nouns for
 * what it is. That is what keeps the set consistent: past participle for a
 * form that finished, present participle for one that has not, which is one
 * grammatical family across all eight. Nouns agree for
 * success-against-failure and then fall apart, since \`running\` and
 * \`pending\` have no one-word noun and \`conditional\` has none at all.
 *
 * \`conditional\` is the deliberate holdout: it is not an outcome but a
 * qualification of one. The form did succeed; what is conditional is the
 * world underneath it.
 *
 * Total on purpose, with anything unrecognized falling through to the wire's
 * own word: a status the daemon grows next should render as itself rather
 * than as nothing. Lowercase throughout, matching the rest of the surface.
 */
function statusLabel(status) {
  const s = String(status);
  if (s === 'admitted' || s === 'ok') { return 'succeeded'; }
  return s;
}


// A length of ACL2 text, said the way a reader would say it. Only ever used
// to explain that something was cut, so the units are the ones that make the
// two numbers comparable at a glance rather than exact: "showing 16 KB of
// 6.8 MB" is the sentence, and "16,365 of 6,831,709 characters" is not.
function fmtChars(n) {
  if (n >= 1000000) { return (n / 1000000).toFixed(1) + ' MB'; }
  if (n >= 1000) { return Math.round(n / 1000) + ' KB'; }
  return n + ' characters';
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
  const parts = String(t || '').replace(/[()]/g, '').trim().split(/\\s+/);
  if (parts.length !== 4) { return null; }
  const vals = parts.map(rat);
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

// A symbol-class is ACL2's word for a fact the source does not show: whether
// this function's guards were verified. Worth spelling out, since ":ideal"
// tells a reader nothing and is the case they most need to notice.
function guardNote(c) {
  if (c === ':common-lisp-compliant') { return 'guards verified'; }
  if (c === ':ideal') { return 'guards not verified'; }
  if (c === ':program') { return 'program mode'; }
  return '';
}

/** A term inline when it fits on one line, in a block when it does not. */
/**
 * One labelled fact -- a guard, a measure, a rule's left-hand side.
 *
 * A term that fits on a line stays inline in the value column, unmarked:
 * beside its label it reads as one row of a key/value grid. A term that
 * needs more than a line takes its *own* line instead -- the label above it,
 * the block full width below -- because a marked block wedged into the value
 * column next to inline one-liners is the one thing that looked wrong here.
 * Own-line is what earns the rule down the left; inline never does.
 */
function factRow(label, term) {
  const t = String(term);
  if (t.includes('\\n')) {
    return '<tr><td class="meta" colspan="2">' + esc(label) + '</td></tr>' +
      '<tr><td colspan="2" class="blockrow"><pre>' + hl(t) + '</pre></td></tr>';
  }
  return '<tr><td class="meta">' + esc(label) + '</td>' +
    '<td><code class="lisp">' + hl(t) + '</code></td></tr>';
}

/**
 * What a name turned out to be -- the part of it that is not in the buffer.
 *
 * The guard and measure ACL2 worked out, the relation the measure decreases
 * under, the statement of a theorem. Not the rules it wrote: those are a
 * list rather than a fact about the name, and they get their own fold.
 */
function nameFacts(d) {
  let rows = '';
  if (d.guard) { rows += factRow('guard', d.guard); }
  if (d.measure) { rows += factRow('measure', d.measure); }
  if (d.relation) { rows += factRow('decreasing under', d.relation); }
  if (d.statement) { rows += factRow('statement', d.statement); }
  return rows ? '<table class="inner">' + rows + '</table>' : '';
}

/**
 * The rules an event added to the world.
 *
 * One per line, the way :pr prints them, because a single event can write
 * dozens and a wrapped row of them reads as one long rule. A rule that
 * exists but is *off* says so first: nothing in the source shows it, and it
 * explains a later proof not firing more often than any other field. That
 * is not a corner case -- a plain std::define, measured, disables two of the
 * four rules it writes.
 *
 * A rule with no fields under it is not a gap in the scrape. :pr prints
 * nothing at all for :executable-counterpart or :induction, and the
 * definition rune's body is deliberately absent, since that part *is* in the
 * buffer a few lines above.
 */
function introducedHtml(d) {
  return '<ul class="rules">' + (d.runes || []).map((u) => {
    const off = u.enabled === false ? ' <span class="off">disabled</span>' : '';
    const fs = (u.fields || [])
      .map((f) => factRow(f.key.replace(/^:/, ''), f.value)).join('');
    return '<li><code class="lisp">' + hl(u.rune) + '</code>' + off +
      (fs ? '<table class="inner">' + fs + '</table>' : '') + '</li>';
  }).join('') + '</ul>';
}

/** The rules a proof consumed -- the other direction from introducedHtml. */
function usedHtml(rules) {
  return '<ul class="rules">' +
    rules.map((r) => '<li><code class="lisp">' + hl(r) + '</code></li>').join('') +
    '</ul>';
}

/**
 * One event a form recorded, as a block of its own.
 *
 * The probe reads event-data-fal, which records *every* event, so a plain
 * defun arrives here exactly as a defines does and there is no
 * simple-against-compound case to write. The summary carries what a reader
 * would otherwise have opened the block to find -- what it cost, whether its
 * guards were verified, how many rules it wrote and how many of those are
 * off. The block carries what is written nowhere in the source.
 *
 * Introduced against used is the distinction the two folds exist for, and
 * they run in opposite directions: what the event added to the world, and
 * what its proof consumed to get admitted. A defun that writes four rules
 * and uses one is the case that makes it plain.
 */
function eventHtml(e, p, openByDefault) {
  const d = e.detail;
  // A probed event has *something* -- kind, time, rules. Saying "no steps"
  // where the probe never reached is a claim we cannot make, so a blank cell
  // keeps meaning "not looked at".
  const probed = !!(e.kind || e.time || (e.rules || []).length ||
                    e.proverSteps != null || d);
  const facts = [];
  const secs = proveSecs(e.time);
  if (secs) { facts.push('prove ' + secs); }
  if (e.proverSteps != null) {
    facts.push(e.proverSteps + ' steps');
  } else if (probed) {
    facts.push('<span title="the prover was not entered">no steps</span>');
  }
  const note = d ? guardNote(d.symbolClass) : '';
  if (note) { facts.push(note); }
  const runes = d ? (d.runes || []) : [];
  const off = runes.filter((u) => u.enabled === false).length;
  if (runes.length) {
    facts.push(runes.length + ' rule' + (runes.length === 1 ? '' : 's') +
               (off ? ', ' + off + ' off' : ''));
  }
  const used = e.rules || [];

  let body = d ? nameFacts(d) : '';
  if (runes.length) {
    body += fold(p + 'ev:' + e.name + ':introduced', true,
      '<span class="meta">introduced \\u2014 ' + runes.length +
        ' rule' + (runes.length === 1 ? '' : 's') +
        (off ? ', ' + off + ' disabled' : '') + '</span>',
      introducedHtml(d));
  }
  if (used.length) {
    body += fold(p + 'ev:' + e.name + ':used', false,
      '<span class="meta">used \\u2014 ' + used.length +
        ' rule' + (used.length === 1 ? '' : 's') + '</span>',
      usedHtml(used));
  }

  // The formals sit beside the name because together they are the signature,
  // which is what a reader looking a function up came for -- and for a
  // macro-generated one there is nothing in the buffer to read it off.
  const head = '<span class="ekind">' + esc(e.kind || '') + '</span>' +
    '<code class="ename">' + esc(e.name) + '</code>' +
    (d && d.formals ? ' <code class="lisp">' + hl(d.formals) + '</code>' : '') +
    causes(e.abortCauses || '')
      .map((c) => ' <span class="failtag">' + esc(c) + '</span>').join('') +
    (facts.length ? '<span class="efacts meta">' + facts.join(' \\u00b7 ') + '</span>' : '');

  // An event with nothing under it gets no fold: a chevron that opens onto
  // nothing is worse than no chevron.
  return body
    ? fold(p + 'ev:' + e.name, openByDefault, head,
           '<div class="einside">' + body + '</div>', 'event')
    : '<div class="event bare"><div class="esum">' + head + '</div></div>';
}

/**
 * A form that produced something: its events, in the order the source has
 * them.
 *
 * The wire sends them newest first, which is how the event data is kept and
 * the wrong way to read it -- a define writes its function before the
 * theorems about it, and someone following the form from the top wants the
 * same order the buffer has.
 */
function productiveHtml(m, p) {
  // What the form evaluated to, when ACL2 would have shown it. Whether it
  // says anything is already decided upstream -- a value that only repeats
  // an event's own name never arrives, and neither does one ACL2 itself
  // declines to print.
  let head = '';
  if (m.value) {
    head += '<h3>Value</h3><pre>' + hl(m.value) + '</pre>';
  }
  const inner = (m.inner || []).slice().reverse();
  if (!inner.length) { return head; }
  const total = m.innerTotal != null && m.innerTotal > inner.length
    ? inner.length + ' shown of ' + m.innerTotal
    : String(inner.length);
  // Open by default only while there are few enough to read past. The
  // defines in kestrel/c/syntax/validator.lisp records 620 events, and 620
  // open blocks is a page that cannot be scrolled.
  const limit = m.eventsOpenLimit != null ? m.eventsOpenLimit : 4;
  const openByDefault = inner.length <= limit;
  let h = head + '<h3>Events (' + total + ')</h3>';
  h += inner.map((e) => eventHtml(e, p, openByDefault)).join('');
  const detailed = inner.filter(
    (e) => e.kind || e.time || e.proverSteps != null || (e.rules || []).length || e.detail
  ).length;
  if (detailed < inner.length) {
    h += '<p class="meta">detail (kind, time, steps, rules) is probed for the first ' +
      detailed + ' events; the rest are names only.</p>';
  }
  return h;
}

/**
 * A form that failed: what failed, then the goals, then what explains them.
 *
 * The abort cause is last on purpose. In practice it is nearly always
 * induction-depth-limit-exceeded, the fix is nearly never the depth limit,
 * and the first thing anyone does with a failed proof is read the subgoals.
 * Nothing above it is left empty when the form never reached the prover:
 * the top slot is the *name* of what failed, and for a translate error or a
 * name clash that name and the error line are the whole story.
 *
 * Naming what failed is not decoration. A define expands to a progn, so its
 * checkpoints mention the return variable and never mention the theorem --
 * without the name a reader goes looking at the function.
 */
function failureHtml(m, p, on) {
  let h = '';
  const inner = m.inner || [];
  // The wire marks the failing event with its abort causes. A form with one
  // event and no causes -- a translate-level failure -- is still that event.
  const bad = inner.find((e) => e.abortCauses) ||
    (inner.length === 1 ? inner[0] : undefined);
  if (bad) {
    h += '<div class="failed-what"><span class="ekind">' + esc(bad.kind || '') +
      '</span><code class="ename">' + esc(bad.name) + '</code></div>';
  }

  const cps = m.checkpoints || [];
  if (cps.length) {
    h += '<h3>Checkpoints (' + cps.length + ')</h3>';
    // Both of these change how the checkpoints below are to be read, so they
    // sit with them rather than in a footer.
    if (m.preRevert) {
      h += '<p class="meta">the top checkpoints are from before the proof ' +
        'reverted to induction.</p>';
    }
    if (m.forced) { h += '<p class="meta">forced assumptions are present.</p>'; }
    cps.forEach((c, i) => {
      // No kind tag: the goal name already says which pool it came from,
      // Goal against Subgoal *1/1.
      h += '<div class="cp"><div class="meta">' + esc(c.goal || '') +
        (on && c.goal
          ? ' <button data-goal="' + esc(c.goal) + '" data-slot="' + i +
            '" data-on="' + on + '">prover output</button>'
          : '') +
        '</div><pre>' + hl(c.term) + '</pre>' +
        (on ? '<pre id="' + p + i + '" hidden></pre>' : '') + '</div>';
    });
    const shownTop = cps.filter((c) => c.kind === 'top').length;
    const shownSub = cps.filter((c) => c.kind === 'sub').length;
    if ((m.nTop || 0) > shownTop || (m.nSub || 0) > shownSub) {
      h += '<p class="meta">showing ' + shownTop + ' of ' + (m.nTop || shownTop) +
        ' top and ' + shownSub + ' of ' + (m.nSub || shownSub) +
        ' subgoal checkpoints; the rest are in the full log.</p>';
    }
  }

  if ((m.rules || []).length) {
    h += fold(p + 'rules', true, '<h3>Rules (' + m.rules.length + ')</h3>',
      usedHtml(m.rules));
  }
  if ((m.hintEvents || []).length) {
    h += '<h3>Hints that fired</h3>' + usedHtml(m.hintEvents);
  }
  if ((m.splitterRules || []).length) {
    h += '<h3>Splitter rules</h3>' + usedHtml(m.splitterRules);
  }
  if (m.time) { h += '<h3>Time</h3><p class="meta">' + fmtTime(m.time) + '</p>'; }

  if (m.errorLine || m.abortCause) {
    h += '<h3>Failure</h3>';
    if (m.abortCause) {
      const cs = causes(m.abortCause);
      h += '<p class="meta">abort cause' + (cs.length > 1 ? 's' : '') + ': ' +
        cs.map((c) => '<code>' + esc(c) + '</code>').join(' ') + '</p>';
    }
    if (m.errorLine) { h += '<pre>' + esc(m.errorLine) + '</pre>'; }
  }

  // What got in before the thing that did not. Useful and not urgent, so it
  // is folded and it is last: a compound form can have admitted half its
  // events before one of them failed.
  const before = inner.filter((e) => e !== bad).slice().reverse();
  if (before.length) {
    h += fold(p + 'before', false,
      '<h3>Admitted before the failure (' + before.length + ')</h3>',
      before.map((e) => eventHtml(e, p, false)).join(''));
  }
  return h;
}

/** Warnings. Form-level -- the wire carries no per-event field for them. */
function warningsHtml(m, p) {
  if (!m.warnings) { return ''; }
  // The daemon joins the kinds with ", ", so splitting counts them exactly.
  const kinds = String(m.warnings).split(', ');
  return fold(p + 'warnings', true, '<h3>Warnings (' + kinds.length + ')</h3>',
    '<pre>' + esc(m.warnings) + '</pre>');
}

/**
 * What ACL2 had to say about one form.
 *
 * Two shapes rather than one. A form that produced something shows what it
 * produced; a form that failed leads with the goals. A file's form and an
 * eval'd one carry the same fields, so this renders either.
 *
 * p prefixes the fold ids and the per-goal output slots, since a page may
 * hold several envelopes at once and their ids must not collide. on names
 * the command a goal button posts, and giving no on means no buttons --
 * which is how the form view asks for the drill-down to be left out.
 */
function envelope(m, p, on) {
  const failed = !!(m.failed || m.errorLine || m.abortCause ||
                    (m.checkpoints || []).length);
  return (failed ? failureHtml(m, p, on) : productiveHtml(m, p)) + warningsHtml(m, p);
}

// Goal buttons can sit in any number of envelopes on one page, so they are
// bound once here and dispatch on the command their own markup names.
function bindGoals(root, slotId) {
  root.addEventListener('click', (ev) => {
    const b = ev.target.closest && ev.target.closest('button[data-goal]');
    if (!b) { return; }
    const slot = Number(b.dataset.slot);
    const pre = document.getElementById(slotId(b, slot));
    if (pre) { pre.hidden = false; pre.textContent = '(fetching\\u2026)'; }
    vscode.postMessage({ cmd: b.dataset.on, goal: b.dataset.goal, slot, seq: seqOf(b) });
  });
}
function seqOf(b) {
  const d = b.closest('details.ev');
  return d ? Number(d.dataset.seq) : undefined;
}
`;
