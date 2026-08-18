// The parts of a webview the panel and the console both need.
//
// Two webviews exist because they answer different questions. The panel is
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
.status.pending, .status.queued { background: rgba(136, 136, 136, 0.2); }
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
.rules code, .codes code { display: inline-block; margin: 1px 5px 1px 0; }
.cp { margin: 8px 0; }
details > summary { cursor: pointer; }
details > summary h3 { display: inline; }
table.inner { border-collapse: collapse; font-size: 12px; }
table.inner td { padding: 1px 10px 1px 0; vertical-align: baseline; }
.failtag { color: var(--vscode-terminal-ansiRed, #cc6666); font-weight: 600; }
/* The second-level row under an event: indented under its own name, and
   given room to breathe, because what is in it is paragraphs rather than a
   cell. */
td.detail { padding: 0 0 6px 12px; }
td.detail table.inner { margin: 2px 0; }
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

function codes(list) {
  return '<p class="codes">' +
    list.map((r) => '<code class="lisp">' + hl(r) + '</code>').join('') + '</p>';
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
 * What one name a form introduced actually is, in the world.
 *
 * Almost none of it is in the buffer: the guard and measure ACL2 worked out,
 * the left-hand side a rewrite rule will match, the trigger a
 * forward-chaining rule waits for, the four runes a defun adds without being
 * asked. Fields arrive in ACL2's own order and keep its keys, so nothing
 * here has to know what a given rule class carries. The definition rune's
 * body is deliberately absent -- that part *is* in the buffer, a few lines
 * above.
 */
function nameDetail(d) {
  const row = (label, value) =>
    '<tr><td class="meta">' + esc(label) + '</td><td>' + value + '</td></tr>';
  // A term goes in a pre when it has to wrap; short ones read better inline.
  const term = (t) =>
    String(t).includes('\\n')
      ? '<pre>' + hl(t) + '</pre>'
      : '<code class="lisp">' + hl(t) + '</code>';
  let rows = '';
  if (d.guard) { rows += row('guard', term(d.guard)); }
  if (d.measure) { rows += row('measure', term(d.measure)); }
  if (d.relation) { rows += row('decreasing under', term(d.relation)); }
  if (d.statement) { rows += row('statement', term(d.statement)); }
  let h = rows ? '<table class="inner">' + rows + '</table>' : '';
  h += (d.runes || []).map((u) => {
    // The enabled flag first: a rule that exists but is off explains a proof
    // that does not fire far more often than any of its other fields.
    const off = u.enabled === false ? ' <span class="status failed">disabled</span>' : '';
    const fs = (u.fields || [])
      .map((f) => row(f.key.replace(/^:/, ''), term(f.value))).join('');
    return '<div class="cp"><div><code class="lisp">' + hl(u.rune) + '</code>' + off + '</div>' +
      (fs ? '<table class="inner">' + fs + '</table>' : '') + '</div>';
  }).join('');
  return h;
}

/**
 * What ACL2 had to say about one form: why it failed, what it warned about,
 * what fired, what it used, and what was inside it. A file's form and an
 * eval'd one carry the same fields, so this renders either.
 *
 * \`p\` prefixes the ids of the per-goal output slots, and \`on\` names the
 * command a goal button posts -- a page may hold several envelopes at once,
 * and their slots must not collide.
 */
function envelope(m, p, on) {
  let h = '';
  if (m.errorLine || m.abortCause || (m.checkpoints || []).length) {
    h += '<h3>Failure</h3>';
    if (m.errorLine) { h += '<pre>' + esc(m.errorLine) + '</pre>'; }
    if (m.abortCause) {
      const cs = causes(m.abortCause);
      h += '<p class="meta">abort cause' + (cs.length > 1 ? 's' : '') + ': ' +
        cs.map((c) => '<code>' + esc(c) + '</code>').join(' ') + '</p>';
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
          ? '<button data-goal="' + esc(c.goal) + '" data-slot="' + i +
            '" data-on="' + on + '">prover output</button>'
          : '') +
        '</div><pre>' + hl(c.term) + '</pre><pre id="' + p + i + '" hidden></pre></div>';
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
    const label = m.failed ? 'Rules the attempt used' : 'Rules';
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
        const probed = !!(e.kind || e.time || (e.rules || []).length ||
                          e.proverSteps != null || e.detail);
        const steps = e.proverSteps != null
          ? e.proverSteps + ' steps'
          : (probed ? '<span title="the prover was not entered">&mdash;</span>' : '');
        const d = e.detail;
        // The formals sit beside the name because together they are the
        // signature, which is what a reader is looking for when they look up
        // a function at all -- and for a macro-generated one there is nothing
        // in the buffer to read it off.
        const sig = '<code>' + esc(e.name) + '</code>' +
          (d && d.formals ? ' <code class="lisp">' + hl(d.formals) + '</code>' : '');
        let row = '<tr><td>' + esc(e.kind || '') + '</td>' +
          '<td>' + sig + '</td>' +
          '<td class="meta">' + proveSecs(e.time) + '</td>' +
          '<td class="meta">' + steps + '</td>' +
          '<td>' + causes(e.abortCauses || '')
            .map((c) => '<span class="failtag">' + esc(c) + '</span>').join(', ') + '</td></tr>';
        // Second-level detail, folded: what this event introduced, then what
        // its proof consumed. The summary carries the two facts worth having
        // without opening it -- whether the guards were verified, and how
        // many runes came with the event -- because a wall of terms between
        // one row and the next is what the fold is there to prevent.
        const bits = [];
        const note = d ? guardNote(d.symbolClass) : '';
        if (note) { bits.push(note); }
        const nr = d ? (d.runes || []).length : 0;
        if (nr) { bits.push(nr + ' rune' + (nr === 1 ? '' : 's')); }
        const used = (e.rules || []).length;
        if (used) { bits.push(used + ' rule' + (used === 1 ? '' : 's') + ' used'); }
        const body = (d ? nameDetail(d) : '') +
          (used ? '<div class="meta">rules the proof used</div>' + codes(e.rules) : '');
        if (body || bits.length) {
          row += '<tr><td></td><td colspan="4" class="detail">' +
            '<details><summary class="meta">' + (bits.join(' \\u00b7 ') || 'detail') +
            '</summary>' + body + '</details></td></tr>';
        }
        return row;
      }).join('') + '</table>';
    const detailed = inner.filter(
      (e) => e.kind || e.time || e.proverSteps != null || (e.rules || []).length || e.detail
    ).length;
    if (detailed < inner.length) {
      h += '<p class="meta">detail (kind, time, steps, rules) is probed for the first ' +
        detailed + ' events; the rest are names only.</p>';
    }
    h += '</details>';
  }
  return h;
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
