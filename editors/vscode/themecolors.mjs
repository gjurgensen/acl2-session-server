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

console.log(failures === 0 ? 'themecolors: all green' : `themecolors: ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
