// Refuse to run a harness against a stale `out/`.
//
// Every `.mjs` harness tests the *compiled* module, which is what lets them
// run under plain node with no vscode -- but `out/` is a build artifact with
// no relationship to `src/` that anything checks. Both failure directions are
// real, and both have happened:
//
//   phantom failure  a fresh checkout has no `out/` at all, or an `out/`
//                    older than a commit that changed both sides. Observed:
//                    4ee8192 changed `webviewParts.ts` and `webview.mjs`
//                    together, and `webview.mjs` failed on a clean checkout
//                    until someone thought to recompile.
//   hidden failure   the worse one. `src/` regresses, nobody recompiles, and
//                    the stale `out/` still passes -- the harness reports
//                    green about code that is not the code.
//
// So: every `src/*.ts` must have an `out/*.js` at least as new. Anything else
// is a hard stop with the fix printed, never a warning, because a warning
// scrolls past.
//
// Orphans -- an `out/*.js` whose source is gone -- are only noted. `tsc` never
// cleans `outDir`, so a deleted module leaves its build behind for ever
// (`out/panel.js` outlived `src/panel.ts` by months); nothing requires one, so
// it cannot make a harness lie, but it can waste an afternoon of someone's
// reading.

import { readdirSync, statSync } from 'fs';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const HERE = dirname(fileURLToPath(import.meta.url));

/** Every `.ts` under `src/`, recursively, as paths relative to `src/`. */
function sources(dir = '', acc = []) {
  for (const e of readdirSync(join(HERE, 'src', dir), { withFileTypes: true })) {
    const rel = dir ? join(dir, e.name) : e.name;
    if (e.isDirectory()) {
      sources(rel, acc);
    } else if (e.name.endsWith('.ts')) {
      acc.push(rel);
    }
  }
  return acc;
}

/**
 * Die unless `out/` was built from `src/` as it stands.
 *
 * Called first thing by every harness, before any `require('./out/...')`.
 */
export function freshOrDie() {
  const stale = [];
  let missing = 0;
  for (const rel of sources()) {
    const src = statSync(join(HERE, 'src', rel));
    const out = statSync(join(HERE, 'out', rel.replace(/\.ts$/, '.js')), {
      throwIfNoEntry: false,
    });
    if (!out) {
      missing++;
      stale.push(`${rel} -- never compiled`);
    } else if (out.mtimeMs < src.mtimeMs) {
      stale.push(`${rel} -- newer than its build`);
    }
  }
  if (stale.length) {
    console.error(
      `\nout/ is stale: ${stale.length} module${stale.length === 1 ? '' : 's'}` +
        `${missing === stale.length ? ' (out/ looks empty -- a fresh checkout does not carry it)' : ''}\n` +
        stale.map((s) => `  ${s}`).join('\n') +
        `\n\nThis harness tests the compiled module, so running it now would report\n` +
        `on code that is not the code. Build first:\n\n  npm run compile\n`
    );
    process.exit(1);
  }

  const built = new Set(sources().map((r) => r.replace(/\.ts$/, '.js')));
  const orphans = readdirSync(join(HERE, 'out'), { recursive: true })
    .filter((f) => typeof f === 'string' && f.endsWith('.js') && !built.has(f))
    .sort();
  if (orphans.length) {
    console.error(
      `note: out/ carries ${orphans.length} build${orphans.length === 1 ? '' : 's'} of ` +
        `deleted source (${orphans.join(', ')});\n` +
        `      tsc never cleans outDir. \`rm -rf out && npm run compile\` clears them.`
    );
  }
}
