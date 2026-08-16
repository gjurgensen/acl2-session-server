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
const events = [];
for (const m of ['state', 'unit', 'tail', 'settled', 'daemon-error']) {
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

const closed = await c.request('close', { book: opened.book });
assert(closed.closed === true, 'close closes');
c.dispose();
rmSync(dir, { recursive: true, force: true });
console.log('smoke: all green');
process.exit(0);
