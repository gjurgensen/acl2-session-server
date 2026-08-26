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

import { randomBytes } from 'crypto';
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
 * Fill in the three things a page cannot know for itself: where the icon font
 * lives, what this particular webview will accept loading it from, and this
 * render's script nonce.
 *
 * The first two are per-webview -- `asWebviewUri` rewrites a file path into
 * the scheme that webview will serve, and `cspSource` is the origin that goes
 * with it -- so neither can be baked into a page's HTML the way the rest of
 * it is. If the font fails to resolve the pages still render; the folds just
 * get no chevron.
 *
 * The nonce is per *render*, and is the layer that stands behind the
 * escaping. No interpolation in these pages is unescaped today -- `esc` is
 * applied at every one, attributes included -- but the threat is real and
 * specific: checkpoint terms, event names and xdoc markup are all
 * *book-controlled* text, the console webview can post `{cmd: 'eval'}`, and
 * an eval reaches the OS through a ttag. Under `script-src 'unsafe-inline'`
 * a single future slip in an HTML builder is remote code execution from
 * opening a hostile book. Under a nonce it is a broken page: the injected
 * script carries no nonce, so the webview declines to run it.
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
  // Fresh per render rather than per page: a nonce reused across renders is
  // one an attacker who saw a previous page already knows.
  const nonce = randomBytes(16).toString('base64');
  return html
    .replace(/%%CODICON%%/g, font)
    .replace(/%%CSPSOURCE%%/g, webview.cspSource)
    .replace(/%%NONCE%%/g, nonce);
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
   in an outlined box. The same box an event card uses (same border colour,
   same radius), on purpose: a box is this page's grouping idiom, so a block
   of output reads as one more grouped unit rather than a new kind of thing.
   An outline rather than a fill keeps it light where several stack in a
   failure. A field's own value is exempt -- see pre.field below. */
pre { background: none; margin: 4px 0; padding: 2px 6px;
      border: 1px solid var(--vscode-panel-border, rgba(128, 128, 128, 0.25));
      border-radius: 4px; overflow: auto; white-space: pre-wrap; }
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
/* A field's own value, however many lines it runs to. No rule down its edge:
   the label beside it is the structure, and the rule is for a standalone
   block -- a checkpoint, the value, the live log -- where nothing else bounds
   it. So the mark means "a top-level block of ACL2 output" rather than "code
   that happened to wrap". Kept in the value cell, not promoted to its own
   line, so it reads as this label's value and no more. */
pre.field { margin: 1px 0; border: 0; padding: 0; }
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
/* One number for every indent a fold produces.
 *
 * A chevron sits in a slot to the left of its label, and everything the fold
 * contains lines up with the *label* -- which is what a tree looks like, and
 * what four hand-written indents in this file were each approximating from a
 * different direction. They had drifted: the chevron's slot was 14px and the
 * body's indent 20px, so a card's title and its contents missed each other by
 * two pixels, which is the worst amount to be out by. Tied together they
 * cannot drift again, and nesting needs no new number. */
:root { --twist: 14px; }
details[data-fold] > summary::before {
  font-family: "codicon";
  content: "\\eab6";
  display: inline-block;
  width: var(--twist);
  font-size: 13px;
  line-height: 1;
  /* Middle, not baseline, and the font's own metrics say why: codicon is
     unitsPerEm 300 with ascender 300 and descender 0, so its baseline is the
     *bottom* of the em box and the chevron's ink centre sits 0.498em above
     it -- about 6.5px here. Text's optical centre is half an x-height up,
     nearer 3.4px, so a baseline-aligned chevron rides some three pixels high
     beside the word it belongs to. \`middle\` puts the box's centre at the
     parent's baseline plus half an x-height, which is where the ink wants to
     be. The icon font's own stylesheet leaves this at baseline, which is
     harmless for VSCode because its codicons sit in flex rows that centre
     them anyway; ours sits in flowing text, where it is not. */
  vertical-align: middle;
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
details.event > summary { padding: 5px 8px; }
/* An event with nothing under it has no chevron, and still reserves the slot:
   otherwise its name sits a chevron's width left of every name around it. */
.event.bare .esum { padding: 5px 8px 5px calc(8px + var(--twist)); }
.einside { padding: 2px 9px 8px calc(8px + var(--twist)); }
.ekind { font-size: 11px; opacity: 0.65; margin-right: 6px; }
.ename { font-weight: 600; }
/* On its own line: a signature and four facts do not fit beside each other
   in a pane this narrow, and wrapping them mid-phrase reads worse. Indented
   because it is a *block* inside the summary, and a \`::before\` only offsets
   the first inline run -- without this it starts back at the chevron, a slot
   to the left of the name it is about. */
.efacts { display: block; margin-top: 2px; padding-left: var(--twist); }
/* The summary on a documented card's face: its own line under the name, not
   trailing off the side of it. A short is a sentence, and a sentence set
   beside a name reads as an afterthought where it is in fact the point of
   the card.

   Not italicised. The italic was marking it as prose among terms, which is a
   job its own line already does -- and a sentence of italic prose is harder
   to read than the same sentence upright, which is a poor trade for a
   distinction nothing needs any more. */
.dsum { display: block; margin-top: 2px; padding-left: var(--twist);
  opacity: 0.85; }
/* Rendered markup can carry a topic link, and this page has nothing to open
   one with -- the doc view listens for a click on \`a.see\`, and this is a
   different webview. They read as what \`@(tsee ...)\` is in xdoc anyway, a
   name in code font, rather than as a link that goes nowhere when clicked. */
.dsum a.see, .doclong a.see { color: inherit; text-decoration: none; }
/* An event's documentation, boxed like the card it describes and like the
   standalone card for a topic with no event. A box is this page's grouping
   idiom, and a block of prose sitting loose among an event's facts was the
   one thing inside a card that had no edge of its own. */
details.docfold, .docfold.bare {
  border: 1px solid var(--vscode-panel-border, rgba(128, 128, 128, 0.25));
  border-radius: 4px; margin: 4px 0;
}
details.docfold > summary { padding: 4px 8px; }
.docfold.bare > .esum { padding: 4px 8px 4px calc(8px + var(--twist)); }
.docfold > .dinside { padding: 0 9px 6px calc(8px + var(--twist)); }
/* A block beside a chevron indents by the chevron's slot, because the chevron
   is drawn inline and only offsets the first run. Where there is no chevron
   the slot is reserved by the container's own padding instead -- so indenting
   again puts the block a slot to the *right* of the label it belongs to.
   That is what a bare event's facts line had been doing: 8+14 from the box
   and 14 more of its own, against a name at 8+14. */
.event.bare > .esum > .efacts { padding-left: 0; }
.docfold.bare > .esum > .dsum { padding-left: 0; }
/* A rendered body. Code inside it is the page's own \`pre\`/\`code\` -- an
   outlined box and a faint chip -- because a term is a term wherever it is
   written, and a documented topic should not introduce a third way of
   drawing one. */
.doclong p { margin: 4px 0; }
/* A name standing in for a directive that would have printed a definition.
   A name, so it is drawn as one. */
code.xname { background: none; padding: 0; }
/* And the same for a fold nested inside one: its body lines up under its own
   label, not under its own chevron. \`.einside\` says this for itself, having a
   padding on all four sides. */
details[data-fold]:not(.event) > *:not(summary) { padding-left: var(--twist); }
/* A rule that exists but is off. Blue, not red: a disabled rule is a fact
   worth surfacing -- it is the field most likely to explain a proof that did
   not fire -- but it is a state, not a fault, and red reads as something
   having gone wrong. The same blue the running chip uses, to stay within the
   palette. */
.off {
  background: rgba(90, 140, 210, 0.3); border-radius: 3px; padding: 0 5px;
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

/**
 * One labelled fact -- a guard, a measure, a rule's left-hand side.
 *
 * The value sits beside its label in the value column either way. A term that
 * fits is inline; a term that runs to several lines keeps ACL2's own layout
 * in a preformatted block, still in the cell, still to the right of the label
 * -- it does not get promoted onto a line of its own. Neither is marked: the
 * label is the structure, and the rule down the edge is for a standalone
 * block, not a field's own value. (No backticks in here: this is inside a
 * template literal.)
 */
function factRow(label, term) {
  const t = String(term);
  const value = t.includes('\\n')
    ? '<pre class="field">' + hl(t) + '</pre>'
    : '<code class="lisp">' + hl(t) + '</code>';
  return '<tr><td class="meta">' + esc(label) + '</td><td>' + value + '</td></tr>';
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

/** How many of a clique's names fit on its title line before the count. */
const TITLE_NAMES = 3;

/**
 * A clique event's title: its members, or enough of them.
 *
 * The whole list when it is short enough to read. Otherwise the first few and
 * how many more there are -- the order is the source's, so the first few are
 * the ones written first. Never a bare truncation: a reader who cannot tell a
 * list ended early will take it for the whole clique.
 */
function cliqueTitle(names) {
  if (names.length <= TITLE_NAMES + 1) { return names.map(esc).join(', '); }
  const shown = names.slice(0, TITLE_NAMES).map(esc).join(', ');
  return shown + '<span class="meta"> +' + (names.length - TITLE_NAMES) + ' more</span>';
}

/**
 * One function of a recursion clique, as a block inside its event's.
 *
 * Built like an event card and not like one: a member is not an event -- ACL2
 * recorded one for the clique, not one apiece -- so it has no kind, no time
 * and no steps of its own. What it does have is its own record: its
 * signature, its guard and measure, and the rules it wrote, which is the half
 * a clique event used to lose.
 *
 * The proof is the clique's and stays on the event above: one termination
 * argument admits all of them, so a copy under each member would be the same
 * list said twice over.
 */
function memberHtml(d, p) {
  const runes = d.runes || [];
  const off = runes.filter((u) => u.enabled === false).length;
  const facts = [];
  const note = guardNote(d.symbolClass);
  if (note) { facts.push(note); }
  if (runes.length) {
    facts.push(runes.length + ' rule' + (runes.length === 1 ? '' : 's') +
               (off ? ', ' + off + ' off' : ''));
  }
  const head = '<code class="ename">' + esc(d.name) + '</code>' +
    (d.formals ? ' <code class="lisp">' + hl(d.formals) + '</code>' : '') +
    (facts.length ? '<span class="efacts meta">' + facts.join(' \u00b7 ') + '</span>' : '');
  let body = nameFacts(d);
  if (runes.length) {
    body += fold(p + 'mem:' + d.name + ':introduced', false,
      '<span class="meta">introduced (' + runes.length + ')</span>',
      introducedHtml(d));
  }
  return body
    ? fold(p + 'mem:' + d.name, false, head,
           '<div class="einside">' + body + '</div>', 'event')
    : '<div class="event bare"><div class="esum">' + head + '</div></div>';
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
function eventHtml(e, p, openByDefault, at) {
  // What the name's own record says, where this event is the one that made
  // the record. \`verify-guards\` is not: it proves a guard obligation about a
  // function some earlier event defined, and the record it looks up under
  // that name is the *definition's* -- its arguments, its documentation, its
  // guard and measure, the rules it wrote, the class it left the name in.
  // Reported here, all of that is the defun's card said a second time under a
  // heading that did none of it. What belongs to this event is what its own
  // proof consumed -- the used fold -- and what that cost.
  const members = e.members || [];
  // \`verify-guards\` proves about a name it did not define, and a clique event
  // has a row per member below -- either way the record filed under this
  // event's own name is not this card's to show.
  const d = (/^verify-guards\+?$/i.test(e.kind || '') || members.length)
    ? undefined : e.detail;
  // A \`thm\` has no name -- it proves a goal and introduces nothing, so ACL2
  // keys its record under none. Its kind carries the line on its own; an
  // empty \`code\` would draw the chip that marks a name with no name in it.
  // Its fold is keyed by where it sits, there being nothing else to key it
  // by, which is why every *named* event still keys by its name.
  const key = e.name || (e.kind || 'ev') + '@' + at;

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
  // The count in parens, like every other section heading -- Events (2),
  // Rules (5). How many are disabled is not repeated here: the event's own
  // facts line above already says "N rules, M off", and each disabled rule
  // carries its badge once the fold is open. (No backticks: template literal.)
  if (runes.length) {
    body += fold(p + 'ev:' + key + ':introduced', true,
      '<span class="meta">introduced (' + runes.length + ')</span>',
      introducedHtml(d));
  }
  // One row per function the clique admitted. ACL2 records the whole clique
  // as a single event keyed under its first definition, so without this a
  // \`mutual-recursion\` read as though it had defined that one function and
  // written only its rules -- the others' rules appeared nowhere at all.
  body += members.map((mem) => memberHtml(mem, p + 'ev:' + key + ':')).join('');
  if (used.length) {
    body += fold(p + 'ev:' + key + ':used', false,
      '<span class="meta">used (' + used.length + ')</span>',
      usedHtml(used));
  }
  // Boxed, and labelled on the line the chevron is on. The summary used to be
  // the short itself, which is a block -- so it began on the line *after* the
  // chevron, leaving the chevron pointing at nothing and the box opening with
  // a blank line. A label beside the chevron is what every other fold on this
  // page does, and the short then reads as the first line of the contents,
  // which is what it is.
  //
  // The name is not repeated: the card this sits in is already the name. Nor
  // is there a "documented" row above it any more, which said the summary a
  // second time.
  if (d && (d.short || d.long)) {
    // The same face the standalone card has, with a label where that one has
    // a name: something beside the chevron on its line, then the short on its
    // own beneath. Both parts are the *summary*, so the short is readable
    // without opening anything -- a sentence is what the reader wants first,
    // and having to open a fold to learn it is the thing worth avoiding.
    const head = '<span class="meta">documentation</span>' +
      (d.short ? '<div class="dsum">' + withCode(d.short, d.shortCode) + '</div>' : '');
    body = (d.long
      ? fold(p + 'ev:' + key + ':documentation', false, head,
             '<div class="dinside doclong">' + withCode(d.long, d.longCode) + '</div>',
             'docfold')
      // Nothing to open: a chevron onto nothing is worse than no chevron.
      : '<div class="docfold bare"><div class="esum">' + head + '</div></div>') + body;
  }

  // The formals sit beside the name because together they are the signature,
  // which is what a reader looking a function up came for -- and for a
  // macro-generated one there is nothing in the buffer to read it off.
  // A clique event is keyed under its first definition, but it admitted all
  // of them, and naming one of the several reads as though the form defined
  // that one. So the title is the members -- but a clique can be a dozen
  // functions with long names, and a title that wraps to four lines is not a
  // title. Capped, and the cap says what it left out: the rest are rows of
  // this very card, a chevron away, so nothing is lost by not listing them
  // here. That is what makes a cap honest where a silent truncation would
  // not be.
  const title = members.length
    ? cliqueTitle(members.map((mem) => mem.name))
    : esc(e.name);
  const head = '<span class="ekind">' + esc(e.kind || '') + '</span>' +
    (title ? '<code class="ename">' + title + '</code>' : '') +
    (d && d.formals ? ' <code class="lisp">' + hl(d.formals) + '</code>' : '') +
    causes(e.abortCauses || '')
      .map((c) => ' <span class="failtag">' + esc(c) + '</span>').join('') +
    (facts.length ? '<span class="efacts meta">' + facts.join(' \\u00b7 ') + '</span>' : '');

  // An event with nothing under it gets no fold: a chevron that opens onto
  // nothing is worse than no chevron.
  return body
    ? fold(p + 'ev:' + key, openByDefault, head,
           '<div class="einside">' + body + '</div>', 'event')
    : '<div class="event bare"><div class="esum">' + head + '</div></div>';
}

/**
 * Topics this form documented that belong to none of its events.
 *
 * At the end, and last on purpose: everything above is what the form did, and
 * this is what it says about itself. A form whose only product is
 * documentation -- a standalone defxdoc -- has nothing above at all, which is
 * why this is reachable from both paths.
 *
 * A card each, the same shape the events above use: the name and its summary
 * on the face of it, the body inside. A topic reads like an event here --
 * something the form produced, with a line about it and more underneath --
 * so it is built out of the same fold rather than a list that looks like a
 * different kind of thing.
 *
 * The summary is the whole of the collapsed card, and it is where the short
 * belongs: one sentence, which is what a short is for. The body is the
 * rendered long, which is a document.
 */
function docsHtml(m, p) {
  const docs = m.docs || [];
  if (!docs.length) { return ''; }
  return '<h3>Documented (' + docs.length + ')</h3>' +
    docs.map((d) => docCard(d, p)).join('');
}

/**
 * One documented topic.
 *
 * Closed by default however few there are, unlike an event: an event's block
 * is a handful of facts, and a topic's is prose written to be read on its
 * own. Several open at once is a wall of text where the reader wanted a list
 * of what this form documents.
 *
 * A topic with no body gets no fold, on the same rule the events follow: a
 * chevron that opens onto nothing is worse than no chevron. A \`defxdoc\` with
 * a \`:short\` and no \`:long\` is an ordinary thing to write.
 */
/**
 * A rendered topic with its code put back, coloured.
 *
 * The renderer lifts code out and leaves a numbered hole, so that whoever
 * shows it can colour it with the editor's own grammar rather than a second
 * approximate one -- the same bargain the terms on this page already make.
 * This is that page keeping its side of it: \`hl\` looks the source text up in
 * the token map the host sends, and the host lists these texts among the
 * strings it tokenizes.
 *
 * A hole with nothing to fill it is left exactly as it is. The renderer puts
 * the escaped source inside the hole as well as beside it, so an unfilled one
 * reads as plain text rather than coming up blank.
 */
function withCode(html, code) {
  const by = {};
  for (const c of code || []) { by[String(c.slot)] = c.text; }
  return String(html || '').replace(
    /<(pre|code) data-code="(\\d+)">[\\s\\S]*?<\\/\\1>/g,
    (m, tag, n) => (n in by ? '<' + tag + '>' + hl(by[n]) + '</' + tag + '>' : m)
  );
}

function docCard(d, p) {
  const head = '<code class="ename">' + esc(d.name) + '</code>' +
    (d.short ? '<div class="dsum">' + withCode(d.short, d.shortCode) + '</div>' : '');
  return d.long
    ? fold(p + 'doc:' + d.name, false, head,
           '<div class="einside doclong">' + withCode(d.long, d.longCode) + '</div>',
           'event')
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
  if (!inner.length) { return head + docsHtml(m, p); }
  const total = m.innerTotal != null && m.innerTotal > inner.length
    ? inner.length + ' shown of ' + m.innerTotal
    : String(inner.length);
  // Open by default only while there are few enough to read past. The
  // defines in kestrel/c/syntax/validator.lisp records 620 events, and 620
  // open blocks is a page that cannot be scrolled.
  const limit = m.eventsOpenLimit != null ? m.eventsOpenLimit : 4;
  const openByDefault = inner.length <= limit;
  let h = head + '<h3>Events (' + total + ')</h3>';
  h += inner.map((e, i) => eventHtml(e, p, openByDefault, i)).join('');
  h += docsHtml(m, p);
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
function failureHtml(m, p, on, goals) {
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
        (on
          ? goals && goals[i] !== undefined
            ? '<pre id="' + p + i + '">' + esc(goals[i]) + '</pre>'
            : '<pre id="' + p + i + '" hidden></pre>'
          : '') + '</div>';
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
      before.map((e, i) => eventHtml(e, p, false, i)).join(''));
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
// \`goals\` is what has already been fetched for this envelope's checkpoints,
// by slot. It has to be rendered from a model rather than written into the DOM
// once, because every render regenerates these elements: the console re-renders
// on a tail tick, on another eval's state, and on a theme refresh, and each one
// wiped the prover output a reader had just asked for. (No backticks in here.)
function envelope(m, p, on, goals) {
  const failed = !!(m.failed || m.errorLine || m.abortCause ||
                    (m.checkpoints || []).length);
  return (failed ? failureHtml(m, p, on, goals) : productiveHtml(m, p)) + warningsHtml(m, p);
}

// Goal buttons can sit in any number of envelopes on one page, so they are
// bound once here and dispatch on the command their own markup names.
// \`onFetch(seq, slot, text)\` lets a page record the placeholder on its own
// model as well as in the DOM, so a render arriving before the reply does not
// put the pre back to empty-and-hidden and leave the button looking dead.
function bindGoals(root, slotId, onFetch) {
  root.addEventListener('click', (ev) => {
    const b = ev.target.closest && ev.target.closest('button[data-goal]');
    if (!b) { return; }
    const slot = Number(b.dataset.slot);
    const pre = document.getElementById(slotId(b, slot));
    const waiting = '(fetching\\u2026)';
    if (pre) { pre.hidden = false; pre.textContent = waiting; }
    if (onFetch) { onFetch(seqOf(b), slot, waiting); }
    vscode.postMessage({ cmd: b.dataset.on, goal: b.dataset.goal, slot, seq: seqOf(b) });
  });
}
function seqOf(b) {
  const d = b.closest('details.ev');
  return d ? Number(d.dataset.seq) : undefined;
}
`;
