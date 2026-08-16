#!/usr/bin/env python3
"""Build the extension's mark.

The letters become outlines rather than <text>: the tab icons are shipped as
SVG and rendered on whatever machine the extension is installed on, where
DejaVu Sans may not exist. Converting here freezes the letterforms exactly as
they were designed and drops the font dependency entirely.

The design is `word('QED', y=162, x=22..234, size=100)` with
lengthAdjust="spacingAndGlyphs" -- i.e. the natural word scaled horizontally
to exactly 212 units. That scale is reproduced below rather than guessed.
"""
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.ttLib import TTFont

FONT = '/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf'
SIZE = 100.0          # font-size the design was drawn at
X1, X2 = 22.0, 234.0  # the textLength span
BASELINE = 162.0

font = TTFont(FONT)
upem = font['head'].unitsPerEm
glyphset = font.getGlyphSet()
cmap = font.getBestCmap()
hmtx = font['hmtx']

names = [cmap[ord(c)] for c in 'QED']
advances = [hmtx[n][0] for n in names]
natural = sum(advances) * SIZE / upem
squeeze = (X2 - X1) / natural   # what spacingAndGlyphs applies

# y flips: font units go up from the baseline, SVG goes down.
scale = SIZE / upem
paths, pen_x = [], 0.0
for name, adv in zip(names, advances):
    pen = SVGPathPen(glyphset)
    glyphset[name].draw(pen)
    d = pen.getCommands()
    if d:
        # Per glyph: place at its (squeezed) pen position, then scale.
        x = X1 + pen_x * scale * squeeze
        paths.append(
            f'<path transform="translate({x:.3f} {BASELINE}) '
            f'scale({scale * squeeze:.6f} {-scale:.6f})" d="{d}"/>'
        )
    pen_x += adv

BODY = '\n    '.join(paths)
print(f'natural width {natural:.2f} -> {X2 - X1} (squeeze {squeeze:.4f})')

HEAD = """<!-- The extension's mark: Q.E.D., which is what ACL2 prints when a proof
     closes.

     Original work, deliberately. ACL2's own logo is BSD-licensed and could be
     copied, but that licence forbids using the project's name to promote
     derived works without written permission, and no permissive licence
     grants trademark rights in a logo. This borrows neither.

     The letters are outlines, not <text>: these are rendered on whatever
     machine the extension is installed on, and DejaVu Sans may not be there.
     Regenerate with tools/mkicon.py, do not hand-edit the path data. -->
"""

def write(path, body, plate, fill):
    plate_rect = f'\n  <rect width="256" height="256" rx="56" fill="{plate}"/>' if plate else ''
    open(path, 'w').write(
        HEAD
        + f'<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256"'
        f' viewBox="0 0 256 256">{plate_rect}\n  <g fill="{fill}">\n    {body}\n  </g>\n</svg>\n'
    )
    print('wrote', path)

import sys
out = sys.argv[1]
# The marketplace icon: one asset for every surface, so it brings its own
# background -- a transparent mark cannot pick a green that reads on both
# white and near-black.
write(f'{out}/icon.svg', BODY, '#16181d', '#8ec07c')
# The tab icons sit on the themed tab bar, where a plate would look like a
# sticker. Two variants, so each green is tuned to its own background rather
# than splitting the difference.
write(f'{out}/window-dark.svg', BODY, None, '#8ec07c')
write(f'{out}/window-light.svg', BODY, None, '#3f7a35')
