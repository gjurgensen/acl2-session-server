// Taking a hue from the theme while keeping our own alpha.
//
// The division of labour this whole feature rests on: the theme says what
// red *is*, and the shipped literal says how loudly a status speaks. Getting
// it wrong is invisible -- a status column that is the right colour at the
// wrong strength, or silently not the theme's colour at all -- so it is
// checked here rather than by looking at it.
//
//   npm run compile   (or tsc -p .)
//   node themecolors.mjs

import { createRequire } from 'module';

const require = createRequire(import.meta.url);
import { readFileSync } from 'fs';
const { BARS, WASHES, WASH_ALPHA, firstDefined, withAlpha, withThemeHue, svgCanPaint } =
  require('./out/bars.js');

let failures = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) {
    failures++;
  }
  console.log(
    `${ok ? 'ok' : 'FAIL'}: ${what}` +
      (ok ? '' : ` (got ${JSON.stringify(got)}, want ${JSON.stringify(want)})`)
  );
};

// The hue comes from the theme, the alpha stays ours.
check('a theme hue keeps our alpha', withThemeHue('rgba(241, 76, 76, 0.65)', '#00ff00'),
  'rgba(0, 255, 0, 0.65)');
check('short hex expands', withThemeHue('rgba(1, 2, 3, 0.5)', '#0f8'), 'rgba(0, 255, 136, 0.5)');
// A theme colour may carry its own alpha. Ours is about this extension rather
// than about the theme, so ours wins.
check('a theme alpha is discarded', withThemeHue('rgba(1, 2, 3, 0.4)', '#11223380'),
  'rgba(17, 34, 51, 0.4)');
check('a hue that is not a colour gives nothing', withThemeHue('rgba(1, 2, 3, 0.4)', 'red'),
  undefined);
check('a shipped value with no alpha gives nothing', withThemeHue('#aabbcc', '#001122'), undefined);

// Preference order: what the status means, then charts, then ANSI.
const theme = { 'terminal.ansiRed': '#aa0000', 'charts.red': '#bb0000' };
check('the best available id wins',
  firstDefined(theme, ['editorError.foreground', 'charts.red', 'terminal.ansiRed']), '#bb0000');
check('and falls through to the last',
  firstDefined(theme, ['editorError.foreground', 'terminal.ansiRed']), '#aa0000');
check('an unreadable theme resolves nothing', firstDefined(undefined, ['charts.red']), undefined);
check('a non-colour value is not a colour',
  firstDefined({ 'charts.red': 'inherit' }, ['charts.red']), undefined);

// Every shipped colour must be able to donate its alpha, or reading the theme
// would silently do nothing for that status.
for (const bar of BARS) {
  for (const kind of ['light', 'dark']) {
    check(`${bar.icon} (${kind}) can take a theme hue`,
      typeof withThemeHue(bar[kind], '#123456'), 'string');
    check(`${bar.icon} (${kind}) names theme colours to try`, bar.themeIds[kind].length > 0, true);
  }
}
for (const [status, c] of Object.entries(WASHES)) {
  for (const kind of ['light', 'dark']) {
    check(`wash ${status} (${kind}) can take a theme hue`,
      typeof withThemeHue(c[kind], '#123456'), 'string');
  }
}

// And whatever comes out has to be paintable by the gutter's SVG, or the one
// surface this feature exists for would keep falling back to the shipped icon.
check('a resolved colour is one an SVG can paint',
  svgCanPaint(withThemeHue('rgba(241, 76, 76, 0.65)', '#00ff00')), true);

// The wash's opacity is a dial over whatever hue won, so it has to take both
// forms a hue arrives in: ours, and the theme's.
check('an opacity replaces ours', withAlpha('rgba(1, 2, 3, 0.13)', 0.3), 'rgba(1, 2, 3, 0.3)');
check('and applies to a bare theme hex', withAlpha('#89d185', 0.3), 'rgba(137, 209, 133, 0.3)');
check('and to one carrying its own alpha', withAlpha('#89d18580', 0.3), 'rgba(137, 209, 133, 0.3)');
check('but declines what it cannot take apart',
  withAlpha('color-mix(in srgb, var(--x) 50%, transparent)', 0.3), undefined);
check('and a named colour, which has no channels to keep', withAlpha('red', 0.3), undefined);
check('an adjusted wash is still paintable by the SVG',
  svgCanPaint(withAlpha('#89d185', 0.3)), true);
for (const kind of ['light', 'dark']) {
  check(`the shipped ${kind} wash opacity is a fraction`,
    WASH_ALPHA[kind] > 0 && WASH_ALPHA[kind] < 1, true);
}

// The running bar breathes, and both routes to it have to carry the animation:
// the SVG generated from a resolved colour, and the shipped icon that stands in
// when the colour is one an image cannot paint with. Losing it on one route
// would show up as a bar that pulses until you pick a theme, or after.
{
  const { gutterSvg } = require('./out/bars.js');
  const running = BARS.find((b) => b.statuses.includes('running'));
  check('the running bar is the one marked to pulse', running.pulse, true);
  check('and it is the only one', BARS.filter((b) => b.pulse).length, 1);

  const animated = gutterSvg('#3794ff', true);
  check('a pulsing bar carries a keyframes rule', /@keyframes/.test(animated), true);
  check('and drives it from CSS, not script',
    /<script/.test(animated), false);
  check('a plain bar carries none', /@keyframes/.test(gutterSvg('#3794ff')), false);

  for (const theme of ['dark', 'light']) {
    const shipped = readFileSync(new URL(`./media/running-${theme}.svg`, import.meta.url), 'utf8');
    check(`the shipped running-${theme} icon pulses too`, /@keyframes/.test(shipped), true);
  }}

// The three modes, as one table. Each marks each fact once: `gutter` draws
// every status and leaves the text alone, `wash` draws backgrounds and no
// bars, `minimal` draws only the form being proved and leaves what is *wrong*
// to a diagnostic.
{
  const { barFor, markFor, problemFor, spinnerSvg } = require('./out/bars.js');
  const ALL = ['pending', 'running', 'admitted', 'conditional', 'skipped', 'failed'];

  const expect = {
    gutter:  { pending: 'pending', running: 'running', admitted: 'admitted',
               conditional: 'cond', skipped: 'red', failed: 'red' },
    minimal: { pending: undefined, running: undefined, admitted: undefined,
               conditional: undefined, skipped: undefined, failed: undefined },
    wash:    { pending: undefined, running: undefined, admitted: undefined,
               conditional: undefined, skipped: undefined, failed: undefined },
  };
  for (const mode of Object.keys(expect)) {
    for (const st of ALL) {
      check(`${mode}: ${st}`, barFor(mode, st), expect[mode][st]);
    }
  }


  // Every bar the table can name has to exist, or a status silently loses its
  // mark at the point of drawing.
  for (const mode of Object.keys(expect)) {
    for (const st of ALL) {
      const icon = barFor(mode, st);
      if (icon !== undefined) {
        check(`${mode}/${st} names a real bar`, BARS.some((b) => b.icon === icon), true);
      }
    }
  }

  // What each mode *draws*: a bar down the form, a spinner at its head, or
  // nothing. `minimal` draws one thing and it is not a bar.
  check('minimal marks the running form with a spinner',
    markFor('minimal', 'running').kind, 'spinner');
  for (const st of ['pending', 'admitted', 'conditional', 'skipped', 'failed']) {
    check(`minimal draws nothing for ${st}`, markFor('minimal', st), undefined);
  }
  check('gutter marks it with a bar', markFor('gutter', 'running').kind, 'bar');
  check('and that bar is the running one', markFor('gutter', 'running').icon, 'running');
  check('wash draws no marks at all', markFor('wash', 'running'), undefined);

  // A buffer that will not lex marks its whole tail, but which mark is still
  // the mode's to choose -- it used to be a red wash in all three, which put
  // a wash in the two modes built around not having one.
  const { unreadableMark } = require('./out/bars.js');
  check('wash washes the unreadable tail', unreadableMark('wash'), 'wash');
  check('gutter marks it in the status column', unreadableMark('gutter'), 'bar');
  check('minimal leaves it to the diagnostic', unreadableMark('minimal'), undefined);

  // The spinner turns, where the bar can only pulse -- a bar is tiled down
  // every line of the form and would march in step; this is one icon.
  const spun = spinnerSvg('#cccccc');
  check('the spinner rotates', /@keyframes acl2-spin/.test(spun), true);
  check('and does it without script', /<script/.test(spun), false);
  check('an arc, not a ring -- the gap is what makes turning visible',
    /<path[^>]*fill="none"/.test(spun), true);

  // The running bar is grey now, like the queued one: the pulse says running,
  // not a fifth hue. Only the wash still says it in blue.
  {
    const bar = BARS.find((b) => b.statuses.includes('running'));
    const queued = BARS.find((b) => b.statuses.includes('pending'));
    for (const kind of ['light', 'dark']) {
      const [br, bg, bb] = bar[kind].match(/\d+/g).slice(0, 3);
      const [qr, qg, qb] = queued[kind].match(/\d+/g).slice(0, 3);
      check(`the running bar shares the queued hue (${kind})`, `${br},${bg},${bb}`, `${qr},${qg},${qb}`);
    }
    check('and is the one that pulses', bar.pulse, true);
  }

  // The held target's mark says what happens at a line rather than where a
  // boundary lies, which is the third answer this question got. A gutter cell
  // addresses a line, so anything drawn in one reads as being about that line;
  // a rule along the top edge could not be seen, and a mark that could be seen
  // read as a status of the form beside it. An octagon says "stops here" and
  // does not need the geometry to say anything.
  {
    const { targetSvg, gutterSvg, INLINE_TARGET } = require('./out/bars.js');
    const stop = targetSvg('#3794ff');
    check('the target mark is a shape, not a bar', /<path/.test(stop) && !/<rect/.test(stop), true);
    check('and a status bar is a bar', /<rect/.test(gutterSvg('#3794ff')), true);

    const pts = [...(/<path[^>]*d="([^"]+)"/.exec(stop)[1])
      .matchAll(/(-?[\d.]+)\s+(-?[\d.]+)/g)].map((m) => [+m[1], +m[2]]);
    const xs = pts.map((p) => p[0]);
    const ys = pts.map((p) => p[1]);
    check('it is an octagon', pts.length, 8);
    // Regular, and centred: a stop sign leaning or sitting off to one side
    // reads as a mistake rather than as a sign.
    check('centred horizontally', (Math.min(...xs) + Math.max(...xs)) / 2, 9);
    check('centred vertically', (Math.min(...ys) + Math.max(...ys)) / 2, 9);
    check('and as wide as it is tall',
      Math.max(...xs) - Math.min(...xs), Math.max(...ys) - Math.min(...ys));

    // Visibility is the thing the first two attempts lost, so it is asserted:
    // the mark fills most of the cell, where a status bar is four eighteenths
    // of it across.
    check('it fills most of the cell', Math.max(...xs) - Math.min(...xs) >= 12, true);
    check('and it does not pulse -- a barrier is not working',
      /@keyframes/.test(stop), false);

    // Inline it is a glyph, because an image there pushes the line box open --
    // which this file's neighbour already recorded, and which an inline icon
    // turned out to do no less than an inline image. A shape this simple loses
    // nothing to being drawn a second way: at this size an octagon and a
    // filled circle are the same dot.
    check('the inline mark is a glyph', typeof INLINE_TARGET.contentText, 'string');
    check('and a round one, to match', INLINE_TARGET.contentText, '\u25cf');
    check('with no box geometry to push the line open',
      'width' in INLINE_TARGET || 'height' in INLINE_TARGET, false);
    check('and no negative margin, which only a box needs',
      /-/.test(INLINE_TARGET.margin), false);
  }

  // Its hue must be free: it is not a verdict, and it shares the gutter with
  // four that are. Blue, which the column freed when running stopped needing
  // a hue of its own.
  {
    const target = BARS.find((b) => b.icon === 'target');
    check('the target has a colour of its own', target !== undefined, true);
    check('and is no status, so no status can select it',
      ['pending', 'running', 'admitted', 'conditional', 'skipped', 'failed']
        .some((st) => target.statuses.includes(st)), false);
    check('it is never returned as a bar', barFor('gutter', 'beyond'), 'pending');
    for (const kind of ['light', 'dark']) {
      const hue = (c) => c.match(/\d+/g).slice(0, 3).join(',');
      const mine = hue(target[kind]);
      for (const other of BARS.filter((b) => b.icon !== 'target')) {
        check(`the target hue is not the ${other.icon} hue (${kind})`, hue(other[kind]) === mine, false);
      }
      // Blue is the one thing wash still says running in, and the target is
      // never a wash -- the two live on different surfaces and never meet.
      check(`the target is blue (${kind})`, hue(WASHES.running[kind]), mine);
    }
    check('it is not last in the table, which is the unknown-status fallback',
      BARS[BARS.length - 1].icon === 'target', false);
  }

  // What goes to the Problems view, in every mode.

  // Severity, which is where a conditional form differs: it was proved, and
  // proved correctly, on a claim that was not. That is worth saying, but it
  // is not an error and there can be hundreds of them, so it is a warning and
  // it is off unless asked for.
  {
    check('a failure is an error', problemFor('failed', false), 'error');
    check('and so is a skip', problemFor('skipped', false), 'error');
    check('with the setting off, a conditional form says nothing',
      problemFor('conditional', false), undefined);
    check('with it on, it is a warning and not an error',
      problemFor('conditional', true), 'warning');
    check('a failure is an error either way', problemFor('failed', true), 'error');
    for (const st of ['pending', 'admitted', 'running', 'beyond']) {
      check(`${st} says nothing even with it on`, problemFor(st, true), undefined);
    }
  }
}

console.log(failures === 0 ? 'themecolors: all green' : `themecolors: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
