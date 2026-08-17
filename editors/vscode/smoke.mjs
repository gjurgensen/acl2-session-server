// Protocol smoke test outside VSCode: drives the compiled protocol client
// (out/protocol.js) against a real daemon and a real ACL2, exercising the
// whole client-side wire -- spawn, framing, correlation, the event stream --
// with the editor UI as the only untested layer.
//
//   npm run compile   (or tsc -p .)
//   node smoke.mjs <path-to-acl2-session-editor> <path-to-saved_acl2>

import { mkdtempSync, writeFileSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const { DaemonClient } = require('./out/protocol.js');

const [daemon, acl2] = process.argv.slice(2);
if (!daemon || !acl2) {
  console.error('usage: node smoke.mjs <daemon> <saved_acl2>');
  process.exit(2);
}

const dir = mkdtempSync(join(tmpdir(), 'acl2-vscode-smoke-'));
const book = join(dir, 'demo.lisp');
const text =
  '(in-package "ACL2")\n\n' +
  '(defun alpha (x) (if (consp x) (alpha (cdr x)) nil))\n\n' +
  '(defthm vs-bad (equal (alpha x) (cons 1 x)))\n\n' +
  '(defthm vs-below (equal (alpha (cons a b)) (cons 1 (cons a b))))\n';
writeFileSync(book, text);

const c = new DaemonClient(daemon, ['--default', acl2]);
c.on('stderr', (s) => process.stderr.write(s));
// However this script exits -- assertion failure, timeout, success -- the
// daemon must not be stranded with a live ACL2 under it.
process.on('exit', () => {
  try {
    process.kill(c.pid, 'SIGKILL');
  } catch {
    /* already gone */
  }
});
const events = [];
for (const m of ['state', 'unit', 'tail', 'settled', 'eval_state', 'daemon-error']) {
  c.on(m, (p) => events.push([m, p]));
}

function until(pred, ms = 60000) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const iv = setInterval(() => {
      const hit = pred();
      if (hit) {
        clearInterval(iv);
        resolve(hit);
      } else if (Date.now() - t0 > ms) {
        clearInterval(iv);
        reject(new Error('timeout waiting for events'));
      }
    }, 20);
  });
}

function assert(ok, what) {
  if (!ok) {
    console.error(`FAIL: ${what}`);
    console.error(JSON.stringify(events, null, 1));
    process.exit(1);
  }
  console.log(`ok: ${what}`);
}

const opened = await c.request('open', { path: book });
assert(typeof opened.book === 'string' && opened.image.length > 0, 'open returns a book and image');

c.notify('update', { book: opened.book, text, revision: 1 });
const settled = await until(() => events.find(([m]) => m === 'settled'));
assert(settled[1].reason === 'reached_target', 'the update settles at the target');
assert(settled[1].revision === 1, 'events carry the update revision');

const states = events.filter(([m]) => m === 'state');
const last = states[states.length - 1][1];
assert(last.conditional_from === 2, 'conditional_from names the red unit');
assert(
  last.units.map((u) => u.status).join(',') === 'admitted,admitted,skipped,conditional',
  'the paint map triages the book'
);

const red = events.find(([m, p]) => m === 'unit' && p.outcome === 'skipped')[1];
assert(red.status === 'proof_failure', 'the red unit carries its envelope');
const below = events.find(([m, p]) => m === 'unit' && p.index === 3)[1];
assert(
  (below.rules || []).join(' ').includes('vs-bad'),
  'the conditional proof reports the rules it leaned on'
);
const out = await c.request('output', {
  book: opened.book,
  output_ref: red.output_ref,
  mode: 'tail',
  lines: 80,
});
assert(
  out.total_lines > 0 && out.text.toLowerCase().includes('vs-bad'),
  'an output slice serves the red log'
);

// Eval: the three things the verb has to get right.
//
// A query answers from the world the file built. A failed experiment comes
// back with the same envelope a red unit carries, so one renderer serves
// both. And a `defun` evaluated here is undone -- which is the contract, and
// the reason this is a query console rather than a REPL.
const q = await c.request('eval', { book: opened.book, form: "(pe 'alpha)" });
assert(q.outcome === 'ok' && q.world_change === 'none', 'a query evaluates without touching the world');
const qout = await c.request('output', {
  book: opened.book,
  output_ref: q.output_ref,
  mode: 'all',
});
assert(qout.text.toLowerCase().includes('alpha'), 'the query log holds the answer');

// The claim deliberately says nothing about `alpha`. `vs-bad` was admitted
// with its proofs skipped, so `(equal (alpha x) (cons 1 x))` is a rewrite
// rule in this very world: every claim about `alpha` collapses through it to
// the empty clause, and the prover pushes no term for a goal it refuted
// outright -- which would fail this assertion on a perfectly good envelope.
// Commutativity of `append` reverts to induction and leaves a checkpoint at
// each level, which is the shape being tested.
const bad = await c.request('eval', {
  book: opened.book,
  form: '(thm (equal (append x y) (append y x)))',
});
assert(bad.outcome === 'failed', 'a failed experiment reports failure');
assert(bad.status === 'proof_failure', 'and reports it as a proof failure');
assert(
  (bad.checkpoints || []).some((k) => k.kind === 'top') &&
    (bad.checkpoints || []).some((k) => k.kind === 'sub'),
  'and carries its checkpoints as terms, as a red unit does'
);

const def = await c.request('eval', { book: opened.book, form: '(defun ephemeral (x) x)' });
assert(def.outcome === 'ok' && def.world_change === 'undone', 'an event evaluated here is undone');
const gone = await c.request('eval', { book: opened.book, form: '(ephemeral 1)' });
assert(gone.outcome === 'failed', 'and is not there for the next eval');

// The busy policy: an eval submitted while the file is being checked waits
// for the form in flight rather than being refused. Sending the eval right
// behind the update means it lands mid-check; it must still answer.
events.length = 0;
c.notify('update', { book: opened.book, text, target_line: null, revision: 2 });
const queued = c.request('eval', { book: opened.book, form: '(+ 1 1)' });
assert((await queued).outcome === 'ok', 'an eval sent mid-check still answers');
assert(
  events.some(([m, p]) => m === 'eval_state' && p.state === 'running'),
  'and says when it went'
);

// describe: the hover answer. The interesting cases are the two that are not
// "a name was found" -- a token that is no name at all (which is what every
// hover over a variable or a comment word is), and the busy reply, which must
// be distinguishable from "not found" or a hover during a proof would render
// as an authoritative "no such thing".
const { hoverMarkdown } = require('./out/names.js');

const fn = await c.request('describe', { book: opened.book, name: 'alpha' });
assert(fn.available && fn.found, 'describe finds a name from this file');
assert(fn.kind === 'function', 'and says what kind it is');
assert(fn.formals === '(x)', 'with its formals');
assert(fn.book === undefined, 'a name defined here reports no book');
assert(hoverMarkdown(fn).includes('(alpha x)'), 'the hover shows the call form');
assert(hoverMarkdown(fn).includes('this file'), 'and says it is defined here');

const thm = await c.request('describe', { book: opened.book, name: 'vs-bad' });
assert(thm.found && thm.kind === 'theorem', 'a theorem is a theorem');
assert(hoverMarkdown(thm).includes('alpha'), 'and its statement is shown');

const missing = await c.request('describe', { book: opened.book, name: 'not-a-name-here' });
assert(missing.available && !missing.found, 'an unknown token answers "not found"');
assert(hoverMarkdown(missing) === undefined, 'and renders no tooltip');

const unsafe = await c.request('describe', { book: opened.book, name: '(' });
assert(unsafe.available && !unsafe.found, 'an unsafe token never reaches a probe');

// A hover must never wait on the prover. Sent right behind an update, it
// either answers from cache or declines -- but it comes back promptly either
// way, and a decline is `available: false`, not `found: false`.
events.length = 0;
c.notify('update', { book: opened.book, text, target_line: null, revision: 3 });
const t0 = Date.now();
const hovered = await c.request('describe', { book: opened.book, name: 'alpha' });
assert(Date.now() - t0 < 20000, 'a hover sent mid-check does not wait for the check');
assert(
  hovered.available ? hovered.found : hovered.found === false,
  'a busy hover declines rather than reporting the name absent'
);
if (!hovered.available) {
  assert(hoverMarkdown(hovered) === undefined, 'and a declined hover renders nothing');
}

// definition: this file, by unit and line, located in the buffer rather than
// in the world -- so it is right even for a form the session has not admitted.
const dhere = await c.request('definition', { book: opened.book, name: 'alpha' });
assert(dhere.available && dhere.found, 'definition finds a name defined here');
assert(dhere.path === undefined, 'a name defined here answers with no path');
assert(dhere.unit === 1, 'and names the unit that defines it');
assert(dhere.line === 3, 'and the line the form starts on');
assert(dhere.how === 'definer', 'reported as exact, since the name is written there');

const dnone = await c.request('definition', { book: opened.book, name: 'not-a-name-here' });
assert(dnone.available && !dnone.found, 'an unknown token has no definition');

// rules: what the form introduced, which is mostly what its author never
// wrote. Served from the scrape taken at admission, so unlike describe it
// has no busy case to distinguish -- it never asks the prover anything.
const ru = await c.request('rules', { book: opened.book, name: 'alpha' });
assert(ru.found, 'rules finds a name from this file');
assert(ru.formals === '(x)', 'and carries its signature');
assert(ru.measure === '(acl2-count x)',
       'and the measure as a term, not the justification record around it');
assert(ru.relation === undefined, 'o< is every defun\u2019s, so it is not reported');
const classes = (ru.runes ?? []).map((r) => r.class);
assert(classes.includes(':definition'), 'a defun introduces a definition rune');
assert(classes.includes(':induction'), 'and an induction rune it never wrote');
const defRune = ru.runes.find((r) => r.class === ':definition');
assert(defRune.enabled === true, 'enabled is reported, since a disabled rule explains a lot');
const defKeys = defRune.fields.map((f) => f.key);
assert(!defKeys.includes(':rhs'), 'the body is not carried -- it is in the buffer');
assert(defKeys.includes(':clique'), 'but the rest of the rule is');

const rnone = await c.request('rules', { book: opened.book, name: 'binary-append' });
assert(!rnone.found, 'a name this file never introduced is not ours to explain');

const closed = await c.request('close', { book: opened.book });
assert(closed.closed === true, 'close closes');
c.dispose();
rmSync(dir, { recursive: true, force: true });
console.log('smoke: all green');
process.exit(0);
